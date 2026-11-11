// SPDX-License-Identifier: AGPL-3.0-or-later

// A small client for a deployed Koidrive server over HTTPS and WebSocket, built on the generated protobuf code. It
// counts every request for the cost summary. Error messages name the method and path only: chunk targets carry
// their tickets in the query string.

import { create, type DescMessage, fromBinary, fromJson, type MessageInitShape, type MessageShape, toBinary, toJson } from "@bufbuild/protobuf";
import { AccessTokenSchema, DeviceCredentialsSchema, PairStartResponseSchema, TokenRequestSchema } from "../../src/gen/koidrive/v1/auth_pb";
import { type CommitResult, type Op, OpSchema, type UploadReceipt, UploadReceiptSchema } from "../../src/gen/koidrive/v1/commit_pb";
import { ClientMessageSchema, RpcRequestSchema, type RpcResponse, RpcResponseSchema, ServerMessageSchema, type Welcome } from "../../src/gen/koidrive/v1/messages_pb";
import { ChunkRefSchema, type Content, ContentSchema, type NodeState } from "../../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, ErrorSchema } from "../../src/gen/koidrive/v1/rpc_pb";
import { toHex, uuidv7 } from "../../src/ids";
import type { Credentials, Stats } from "./state.ts";

const REQUEST_MS = 60_000;

export const stats: Stats = { requests: 0, bytesUp: 0, bytesDown: 0 };

export class HttpError extends Error {
  readonly status: number;
  readonly code: string;
  constructor(message: string, status: number, code: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function sha256(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

export { toHex, uuidv7 };

/** Sends a request, counting it, and returns the response whatever its status. */
export async function send(url: string, init: RequestInit = {}): Promise<Response> {
  const body = init.body;
  stats.requests++;
  if (body instanceof Uint8Array) stats.bytesUp += body.byteLength;
  else if (typeof body === "string") stats.bytesUp += body.length;
  try {
    return await fetch(url, { ...init, signal: AbortSignal.timeout(REQUEST_MS) });
  } catch (error) {
    throw new Error((init.method ?? "GET") + " " + new URL(url).pathname + " failed: " + (error instanceof Error ? error.message : String(error)));
  }
}

export async function readBytes(response: Response): Promise<Uint8Array> {
  const bytes = new Uint8Array(await response.arrayBuffer());
  stats.bytesDown += bytes.byteLength;
  return bytes;
}

async function failure(what: string, response: Response): Promise<HttpError> {
  const bytes = await readBytes(response);
  let code = "";
  let message = new TextDecoder().decode(bytes).slice(0, 300);
  try {
    const error = (response.headers.get("Content-Type") ?? "").includes("json")
      ? fromJson(ErrorSchema, JSON.parse(new TextDecoder().decode(bytes)))
      : fromBinary(ErrorSchema, bytes);
    code = String(error.code);
    message = error.message;
  } catch {}
  return new HttpError(what + " returned HTTP " + response.status + ": " + message, response.status, code);
}

/** A deployed server at base, for example https://koidrive-test-1-1.example.workers.dev. */
export class Server {
  readonly base: string;
  constructor(base: string) {
    this.base = base.replace(/\/$/, "");
  }

  /** A JSON request with a protobuf message body; returns the reply parsed with schema. Throws HttpError otherwise. */
  async json<O extends DescMessage>(
    method: string,
    path: string,
    output: O,
    init: { body?: unknown; token?: string; status?: number; headers?: Record<string, string> } = {},
  ): Promise<MessageShape<O>> {
    const response = await this.request(method, path, init);
    if (response.status !== (init.status ?? 200)) throw await failure(method + " " + path.split("?")[0], response);
    return fromJson(output, JSON.parse(new TextDecoder().decode(await readBytes(response))), { ignoreUnknownFields: true });
  }

  async request(method: string, path: string, init: { body?: unknown; token?: string; headers?: Record<string, string> } = {}): Promise<Response> {
    return send(this.base + path, {
      method,
      headers: {
        Accept: "application/json",
        ...(init.body === undefined ? {} : { "Content-Type": "application/json" }),
        ...(init.token ? { Authorization: "Bearer " + init.token } : {}),
        ...init.headers,
      },
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
    });
  }
}

/** One device of the deployment's owner, refreshing its access token before it expires. */
export class Device {
  readonly server: Server;
  readonly credentials: Credentials;
  private access: { token: string; expiresAtMs: number } | undefined;

  constructor(server: Server, credentials: Credentials) {
    this.server = server;
    this.credentials = credentials;
  }

  async token(): Promise<string> {
    if (!this.access || this.access.expiresAtMs - Date.now() < 2 * 60_000) {
      const token = await this.server.json("POST", "/v1/token", AccessTokenSchema, {
        body: toJson(TokenRequestSchema, create(TokenRequestSchema, { deviceId: this.credentials.deviceId, refreshCredential: this.credentials.refreshCredential })),
      });
      this.access = { token: token.token, expiresAtMs: Number(token.expiresAtMs) };
    }
    return this.access.token;
  }

  /** Pairs a new device of the same owner (spec v0 section 15.2). */
  async pair(deviceName: string): Promise<Device> {
    const started = await this.server.json("POST", "/v1/pair/start", PairStartResponseSchema, { token: await this.token() });
    const credentials = await this.server.json("POST", "/v1/pair/complete", DeviceCredentialsSchema, {
      body: { code: started.code, deviceName, platform: "linux" },
    });
    return new Device(this.server, { userId: credentials.userId, deviceId: credentials.deviceId, refreshCredential: credentials.refreshCredential });
  }
}

/** A file's bytes split into chunks, and the Content that describes them. */
export interface FileData {
  parts: Uint8Array[];
  content: Content;
  sha256: string;
  size: number;
}

export async function fileData(parts: Uint8Array[]): Promise<FileData> {
  const chunks = [];
  for (const part of parts) chunks.push(create(ChunkRefSchema, { hash: await sha256(part), size: part.byteLength }));
  const list = new Uint8Array(chunks.length * 36);
  const view = new DataView(list.buffer);
  chunks.forEach((chunk, i) => {
    list.set(chunk.hash, i * 36);
    view.setUint32(i * 36 + 32, chunk.size, true);
  });
  const whole = new Uint8Array(parts.reduce((sum, part) => sum + part.byteLength, 0));
  let offset = 0;
  for (const part of parts) {
    whole.set(part, offset);
    offset += part.byteLength;
  }
  return {
    parts,
    size: whole.byteLength,
    sha256: toHex(await sha256(whole)),
    content: create(ContentSchema, {
      size: BigInt(whole.byteLength),
      mtimeNs: BigInt(Date.now()) * 1_000_000n,
      chunks,
      chunkCount: chunks.length,
      manifestDigest: await sha256(list),
      chunker: "fastcdc2020-1m-4m-16m",
    }),
  };
}

export function randomBytes(size: number): Uint8Array {
  const bytes = new Uint8Array(size);
  for (let i = 0; i < size; i += 65536) crypto.getRandomValues(bytes.subarray(i, Math.min(size, i + 65536)));
  return bytes;
}

export const op = {
  mkdir: (nodeId: string, parentId: string, name: string): Op => create(OpSchema, { op: { case: "createDir", value: { nodeId, parentId, name } } }),
  create: (nodeId: string, parentId: string, name: string, content: Content): Op =>
    create(OpSchema, { op: { case: "createFile", value: { nodeId, parentId, name, content } } }),
  update: (nodeId: string, content: Content): Op => create(OpSchema, { op: { case: "updateContent", value: { nodeId, content } } }),
  delete: (nodeId: string): Op => create(OpSchema, { op: { case: "delete", value: { nodeId } } }),
};

type Call = MessageInitShape<typeof RpcRequestSchema>["call"];

/** One device working in one space: Welcome over the WebSocket, everything else over the HTTP RPC endpoint. */
export class SpaceClient {
  readonly device: Device;
  readonly spaceId: string;
  historyId = "";
  baseSeq = 0n;
  acceptedSeq = 0n;
  private nextReq = 1n;

  constructor(device: Device, spaceId: string) {
    this.device = device;
    this.spaceId = spaceId;
  }

  /** Opens the space's WebSocket, sends Hello and returns Welcome, which carries the current history_id. */
  async welcome(): Promise<Welcome> {
    const url = this.device.server.base.replace(/^http/, "ws") + "/v1/spaces/" + this.spaceId + "/ws";
    const token = await this.device.token();
    stats.requests++;
    const socket = new WebSocket(url, { headers: { Authorization: "Bearer " + token } });
    socket.binaryType = "arraybuffer";
    try {
      const welcome = await new Promise<Welcome>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("no Welcome within 30 s on /v1/spaces/" + this.spaceId + "/ws")), 30_000);
        socket.addEventListener("open", () => {
          const hello = create(ClientMessageSchema, { msg: { case: "hello", value: { clientName: "koidrive-deploy-test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } } });
          const frame = toBinary(ClientMessageSchema, hello);
          stats.bytesUp += frame.byteLength;
          socket.send(frame);
        });
        socket.addEventListener("message", (event) => {
          if (!(event.data instanceof ArrayBuffer)) return;
          stats.bytesDown += event.data.byteLength;
          const message = fromBinary(ServerMessageSchema, new Uint8Array(event.data));
          clearTimeout(timer);
          if (message.msg.case === "welcome") resolve(message.msg.value);
          else reject(new Error("expected Welcome, got " + message.msg.case + (message.msg.case === "goodbye" ? " " + message.msg.value.code + " " + message.msg.value.reason : "")));
        });
        socket.addEventListener("error", () => {
          clearTimeout(timer);
          reject(new Error("the WebSocket to /v1/spaces/" + this.spaceId + "/ws failed"));
        });
        socket.addEventListener("close", (event) => {
          clearTimeout(timer);
          reject(new Error("the WebSocket to /v1/spaces/" + this.spaceId + "/ws closed with " + event.code + " " + event.reason));
        });
      });
      this.historyId = welcome.historyId;
      this.baseSeq = welcome.headSeq;
      this.acceptedSeq = welcome.deviceSeq;
      return welcome;
    } finally {
      socket.close(1000);
    }
  }

  async rpc(call: Call): Promise<RpcResponse["result"]> {
    const request = toBinary(RpcRequestSchema, create(RpcRequestSchema, { reqId: this.nextReq++, call }));
    const what = "POST /v1/spaces/" + this.spaceId + "/rpc";
    const response = await send(this.device.server.base + "/v1/spaces/" + this.spaceId + "/rpc", {
      method: "POST",
      headers: { Authorization: "Bearer " + (await this.device.token()), "Content-Type": "application/x-protobuf" },
      body: request,
    });
    if (!response.ok) throw await failure(what, response);
    const result = fromBinary(RpcResponseSchema, await readBytes(response)).result;
    if (result.case === "error") throw new HttpError(what + " (" + call?.case + ") failed: " + result.value.message, 200, String(result.value.code));
    return result;
  }

  /** Asks which chunks are missing, uploads them to the targets the server returns, and returns the receipts. */
  async upload(...files: FileData[]): Promise<{ receipts: UploadReceipt[]; uploaded: number }> {
    const byHash = new Map<string, Uint8Array>();
    for (const file of files) file.content.chunks.forEach((chunk, i) => byHash.set(toHex(chunk.hash), file.parts[i]!));
    const chunks = files.flatMap((file) => file.content.chunks);
    const result = await this.rpc({ case: "prepareUpload", value: { chunks } });
    if (result.case !== "prepareUpload") throw new Error("prepareUpload answered " + result.case);
    const receipts: UploadReceipt[] = [];
    for (const plan of result.value.chunks) {
      if (plan.action === ChunkPlan_Action.PRESENT) continue;
      if (plan.action !== ChunkPlan_Action.UPLOAD || !plan.target) throw new Error("prepareUpload planned action " + plan.action + " for chunk " + toHex(plan.hash));
      const bytes = byHash.get(toHex(plan.hash))!;
      const response = await send(plan.target.url, { method: plan.target.method || "PUT", headers: { ...plan.target.headers, "Content-Type": "application/octet-stream" }, body: bytes });
      if (!response.ok) throw await failure("PUT chunk " + toHex(plan.hash), response);
      receipts.push(fromBinary(UploadReceiptSchema, await readBytes(response)));
    }
    return { receipts, uploaded: receipts.length };
  }

  /** Commits in the current history with the next device slot; throws when the commit is refused. */
  async commit(ops: Op[], receipts: UploadReceipt[] = []): Promise<CommitResult> {
    const deviceSeq = this.acceptedSeq + 1n;
    const result = await this.rpc({
      case: "commit",
      value: { commitId: uuidv7(), baseSeq: this.baseSeq, ops, receipts, historyId: this.historyId, deviceSeq, requireHead: false },
    });
    if (result.case !== "commit") throw new Error("commit answered " + result.case);
    this.acceptedSeq = deviceSeq;
    if (result.value.seq > this.baseSeq) this.baseSeq = result.value.seq;
    return result.value;
  }

  /** The live tree, node_id to state, read page by page. */
  async tree(): Promise<Map<string, NodeState>> {
    const nodes = new Map<string, NodeState>();
    let cursor = "";
    do {
      const result = await this.rpc({ case: "listTree", value: { cursor, limit: 100 } });
      if (result.case !== "tree") throw new Error("listTree answered " + result.case);
      for (const node of result.value.nodes) nodes.set(node.nodeId, node);
      cursor = result.value.nextCursor;
    } while (cursor);
    return nodes;
  }

  /** Downloads a version's chunks from the targets the server returns and returns the SHA-256 of the whole file. */
  async download(content: Content): Promise<string> {
    const result = await this.rpc({ case: "getDownloadTargets", value: { hashes: content.chunks.map((chunk) => chunk.hash) } });
    if (result.case !== "downloadTargets") throw new Error("getDownloadTargets answered " + result.case);
    const targets = new Map(result.value.targets.map((target) => [toHex(target.hash), target.target!]));
    const parts: Uint8Array[] = [];
    for (const chunk of content.chunks) {
      const hex = toHex(chunk.hash);
      const target = targets.get(hex);
      if (!target) throw new Error("no download target for chunk " + hex);
      const response = await send(target.url, { method: target.method || "GET", headers: target.headers });
      if (!response.ok) throw await failure("GET chunk " + hex, response);
      const bytes = await readBytes(response);
      if (toHex(await sha256(bytes)) !== hex) throw new Error("chunk " + hex + " came back with other bytes");
      parts.push(bytes);
    }
    return (await fileData(parts)).sha256;
  }
}

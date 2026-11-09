// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  ServerInfoSchema,
  SpaceInfoSchema,
  SpaceListSchema,
  TokenRequestSchema,
} from "../src/gen/flarebox/v1/auth_pb";
import { OpResult_Code, UploadReceiptSchema } from "../src/gen/flarebox/v1/commit_pb";
import {
  type ClientMessage,
  ClientMessageSchema,
  ResyncRequired_Reason,
  RpcRequestSchema,
  RpcResponseSchema,
  type ServerMessage,
  ServerMessageSchema,
  type Welcome,
} from "../src/gen/flarebox/v1/messages_pb";
import type { CommitRecord, Content } from "../src/gen/flarebox/v1/model_pb";
import { Error_Code } from "../src/gen/flarebox/v1/rpc_pb";
import { deriveSpaceKey, signChunkGrant } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import { Close } from "../src/space-do";
import { chunkKey } from "../src/space/store";
import { content, op } from "./helpers";

const BASE = "https://flarebox.test";

afterEach(() => {
  vi.restoreAllMocks();
});

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

async function call<Res extends DescMessage>(
  path: string,
  init: { method?: string; token?: string; body?: Uint8Array },
  responseSchema: Res,
): Promise<{ status: number; message?: MessageShape<Res> }> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.method ?? "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  return { status: response.status, message: response.ok ? fromBinary(responseSchema, bytes) : undefined };
}

function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

let owner: DeviceCredentials | undefined;

/**
 * Durable Object storage is shared by the tests in this file, so the deployment is claimed once (by the first
 * test) and the owner's credentials are reused.
 */
async function getOwner(): Promise<DeviceCredentials> {
  owner ??= await claimOwner();
  return owner;
}

async function claimOwner(): Promise<DeviceCredentials> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Flarebox claim code"));
  const code = line!.split(": ")[1]!;
  const claimed = await call("/v1/claim", { body: body(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop A", platform: "macos" }) }, DeviceCredentialsSchema);
  return claimed.message!;
}

async function pairDevice(inviter: DeviceCredentials, deviceName: string): Promise<DeviceCredentials> {
  const started = await call("/v1/pair/start", { token: inviter.access!.token }, PairStartResponseSchema);
  const paired = await call("/v1/pair/complete", { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName, platform: "linux" }) }, DeviceCredentialsSchema);
  return paired.message!;
}

async function createSpace(owner: DeviceCredentials): Promise<string> {
  const created = await call("/v1/spaces", { token: owner.access!.token, body: body(CreateSpaceRequestSchema, { name: "Home" }) }, SpaceInfoSchema);
  expect(created.status).toBe(201);
  return created.message!.spaceId;
}

/** A WebSocket client speaking the binary session protocol. */
class Socket {
  private readonly queue: ServerMessage[] = [];
  private readonly texts: string[] = [];
  private wake: (() => void) | undefined;
  private nextReq = 1n;
  closed: { code: number; reason: string } | undefined;

  private constructor(readonly ws: WebSocket) {
    ws.addEventListener("message", (event) => {
      if (typeof event.data === "string") this.texts.push(event.data);
      else this.queue.push(fromBinary(ServerMessageSchema, new Uint8Array(event.data as ArrayBuffer)));
      this.wake?.();
    });
    ws.addEventListener("close", (event) => {
      this.closed = { code: event.code, reason: event.reason };
      this.wake?.();
    });
  }

  static async open(spaceId: string, token: string | undefined): Promise<Socket | number> {
    const headers: Record<string, string> = { Upgrade: "websocket" };
    if (token) headers.Authorization = "Bearer " + token;
    const response = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/ws", { headers });
    if (!response.webSocket) return response.status;
    response.webSocket.binaryType = "arraybuffer";
    response.webSocket.accept();
    return new Socket(response.webSocket);
  }

  static async connect(spaceId: string, creds: DeviceCredentials): Promise<Socket> {
    const socket = await Socket.open(spaceId, creds.access!.token);
    if (typeof socket === "number") throw new Error("upgrade failed with " + socket);
    return socket;
  }

  send(msg: MessageInitShape<typeof ClientMessageSchema>["msg"]): void {
    this.ws.send(toBinary(ClientMessageSchema, create(ClientMessageSchema, { msg } as MessageInitShape<typeof ClientMessageSchema>)));
  }

  private async until<T>(take: () => T | undefined, what: string): Promise<T> {
    const deadline = Date.now() + 3000;
    for (;;) {
      const value = take();
      if (value !== undefined) return value;
      if (this.closed) throw new Error("socket closed while waiting for " + what);
      if (Date.now() > deadline) throw new Error("timed out waiting for " + what);
      await new Promise<void>((resolve) => {
        this.wake = resolve;
        setTimeout(resolve, 50);
      });
    }
  }

  next(): Promise<ServerMessage["msg"]> {
    return this.until(() => this.queue.shift()?.msg, "a message");
  }

  text(): Promise<string> {
    return this.until(() => this.texts.shift(), "a text frame");
  }

  async waitClosed(): Promise<{ code: number; reason: string }> {
    return this.until(() => this.closed, "close").catch(() => this.closed!);
  }

  async hello(maxFormat = 1): Promise<Welcome> {
    this.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat } });
    const msg = await this.next();
    if (msg.case !== "welcome") throw new Error("expected Welcome, got " + msg.case);
    return msg.value;
  }

  /** Sends Resume and collects the backlog until CaughtUp. */
  async resume(historyId: string, sinceSeq: bigint): Promise<{ records: CommitRecord[]; head: bigint }> {
    this.send({ case: "resume", value: { historyId, sinceSeq } });
    const records: CommitRecord[] = [];
    for (;;) {
      const msg = await this.next();
      if (msg.case === "changes") records.push(...msg.value.commits);
      else if (msg.case === "caughtUp") return { records, head: msg.value.headSeq };
      else throw new Error("unexpected " + msg.case);
    }
  }

  async rpc(call: MessageInitShape<typeof RpcRequestSchema>["call"]) {
    const reqId = this.nextReq++;
    this.send({ case: "rpc", value: { reqId, call } });
    const msg = await this.next();
    if (msg.case !== "rpc" || msg.value.reqId !== reqId) throw new Error("expected RpcResponse " + reqId + ", got " + msg.case);
    return msg.value.result;
  }

  async commit(baseSeq: bigint, ops: ReturnType<typeof op.mkdir>[], receipts: MessageInitShape<typeof UploadReceiptSchema>[] = []) {
    const result = await this.rpc({ case: "commit", value: { commitId: uuidv7(), baseSeq, ops, receipts } });
    if (result.case !== "commit") throw new Error("commit failed: " + (result.case === "error" ? result.value.message : result.case));
    return result.value;
  }

  async pushed(): Promise<CommitRecord[]> {
    const msg = await this.next();
    if (msg.case !== "changes") throw new Error("expected Changes, got " + msg.case);
    return msg.value.commits;
  }

  /** Hello and a full Resume from seq 0. */
  async start(): Promise<Welcome> {
    const welcome = await this.hello();
    await this.resume(welcome.historyId, 0n);
    return welcome;
  }
}

/** Stores chunks in R2 and signs receipts the way the relay upload route does. */
async function upload(spaceId: string, parts: { content: Content; bytes: Uint8Array[] }) {
  const spaceKey = await deriveSpaceKey((await account().serverKey()).secret, spaceId);
  const receipts = [];
  for (const [i, chunk] of parts.content.chunks.entries()) {
    const hex = toHex(chunk.hash);
    await env.BUCKET.put(chunkKey(spaceId, hex), parts.bytes[i]!);
    const token = await signChunkGrant(spaceKey, "receipt", spaceId, hex, chunk.size, Date.now() + 3_600_000);
    receipts.push(create(UploadReceiptSchema, { hash: chunk.hash, size: chunk.size, token }));
  }
  return receipts;
}

async function prepared(socket: Socket, spaceId: string, text: string) {
  const parts = await content(text);
  const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
  expect(plan.case).toBe("prepareUpload");
  return { content: parts.content, receipts: await upload(spaceId, parts) };
}

function nodeIds(records: CommitRecord[]): string[] {
  return records.flatMap((r) => r.changes.map((c) => (c.change.case === "node" ? c.change.value.nodeId : "")));
}

describe("owner claim, pairing and tokens", () => {
  it("claims the server once with the logged code, pairs a device and refreshes tokens", async () => {
    const before = await call("/v1/server", { method: "GET" }, ServerInfoSchema);
    expect(before.message!.claimed).toBe(false);
    const wrong = await call("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", deviceName: "x" }) }, DeviceCredentialsSchema);
    expect(wrong.status).toBe(403);

    const owner = await getOwner();
    expect((await call("/v1/server", { method: "GET" }, ServerInfoSchema)).message!.claimed).toBe(true);
    expect((await exports.default.fetch(BASE + "/setup").then((r) => r.text())).includes("already has an owner")).toBe(true);
    const again = await call("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", deviceName: "x" }) }, DeviceCredentialsSchema);
    expect(again.status).toBe(403);

    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    expect(started.message!.code).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4}$/);
    const pairBody = body(PairCompleteRequestSchema, { code: started.message!.code.toLowerCase(), deviceName: "Desktop B", platform: "linux" });
    const paired = await call("/v1/pair/complete", { body: pairBody }, DeviceCredentialsSchema);
    expect(paired.message!.userId).toBe(owner.userId);
    expect(paired.message!.deviceId).not.toBe(owner.deviceId);
    expect((await call("/v1/pair/complete", { body: pairBody }, DeviceCredentialsSchema)).status).toBe(403);

    const refreshed = await call(
      "/v1/token",
      { body: body(TokenRequestSchema, { deviceId: paired.message!.deviceId, refreshCredential: paired.message!.refreshCredential }) },
      AccessTokenSchema,
    );
    expect(refreshed.message!.expiresAtMs).toBeGreaterThan(BigInt(Date.now()));
    const spaces = await call("/v1/spaces", { method: "GET", token: refreshed.message!.token }, SpaceListSchema);
    expect(spaces.status).toBe(200);

    const forged = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: paired.message!.deviceId, refreshCredential: "nope" }) }, AccessTokenSchema);
    expect(forged.status).toBe(401);
    expect((await call("/v1/spaces", { method: "GET" }, SpaceListSchema)).status).toBe(401);
    expect((await call("/v1/spaces", { method: "GET", token: owner.access!.token.slice(0, -1) + "x" }, SpaceListSchema)).status).toBe(401);
  });

  it("stops accepting a revoked device", async () => {
    const owner = await getOwner();
    const laptop = await pairDevice(owner, "Lost laptop");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, laptop);
    await socket.start();
    await account().revokeDevice(laptop.deviceId);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const refresh = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: laptop.deviceId, refreshCredential: laptop.refreshCredential }) }, AccessTokenSchema);
    expect(refresh.status).toBe(401);
  });
});

describe("WebSocket session", () => {
  async function twoDevices() {
    const a = await getOwner();
    const b = await pairDevice(a, "Desktop B");
    const spaceId = await createSpace(a);
    return { a, b, spaceId, stub: env.SPACE.get(env.SPACE.idFromName(spaceId)) };
  }

  it("lets two devices commit and receive each other's changes", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const sb = await Socket.connect(spaceId, b);
    const welcome = await sa.start();
    expect(welcome).toMatchObject({ proto: 1, formatVersion: 1, headSeq: 0n, readOnly: false });
    await sb.start();

    const docs = uuidv7();
    const first = await sa.commit(0n, [op.mkdir(docs, spaceId, "Docs")]);
    expect(first.seq).toBe(1n);
    const atB = await sb.pushed();
    expect(atB.map((r) => r.seq)).toEqual([1n]);
    expect(atB[0]!.deviceId).toBe(a.deviceId);
    expect(nodeIds(atB)).toEqual([docs]);

    const notes = uuidv7();
    const second = await sb.commit(1n, [op.mkdir(notes, docs, "Notes")]);
    expect(second.seq).toBe(2n);
    expect(nodeIds(await sa.pushed())).toEqual([notes]);
  });

  it("resumes from the last seq after a dropped socket and across hibernation", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.start();
    let sb = await Socket.connect(spaceId, b);
    await sb.start();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    expect((await sb.pushed())[0]!.seq).toBe(1n);

    sb.ws.close(1000, "network drop");
    await sb.waitClosed();
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    await sa.commit(2n, [op.mkdir(uuidv7(), spaceId, "three")]);

    sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const caughtUp = await sb.resume(welcome.historyId, 1n);
    expect(caughtUp.records.map((r) => r.seq)).toEqual([2n, 3n]);
    expect(caughtUp.head).toBe(3n);

    await evictDurableObject(stub);
    await sa.commit(3n, [op.mkdir(uuidv7(), spaceId, "after hibernation")]);
    expect((await sb.pushed()).map((r) => r.seq)).toEqual([4n]);
  });

  it("turns a conflicting edit into a conflict copy and pushes it", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const sb = await Socket.connect(spaceId, b);
    await sa.start();
    await sb.start();
    const file = uuidv7();
    const v1 = await prepared(sa, spaceId, "v1");
    await sa.commit(0n, [op.create(file, spaceId, "plan.txt", v1.content)], v1.receipts);
    await sb.pushed();

    const fromB = await prepared(sb, spaceId, "B's edit");
    await sb.commit(1n, [op.update(file, fromB.content)], fromB.receipts);
    await sa.pushed();
    const fromA = await prepared(sa, spaceId, "A's edit");
    const result = await sa.commit(1n, [op.update(file, fromA.content)], fromA.receipts);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(result.results[0]!.name).toMatch(/^plan \(conflict from Laptop A \d{4}-\d{2}-\d{2}\)\.txt$/);

    const pushed = await sb.pushed();
    const copy = pushed[0]!.changes[0]!.change;
    expect(copy.case === "node" && copy.value).toMatchObject({ nodeId: result.results[0]!.nodeId, name: result.results[0]!.name, parentId: spaceId });
  });

  it("asks for a resync on another history or a seq ahead of the server", async () => {
    const { a, spaceId } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    const welcome = await socket.hello();
    socket.send({ case: "resume", value: { historyId: uuidv7(), sinceSeq: 0n } });
    expect(await socket.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.HISTORY_CHANGED } });
    socket.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq: 9n } });
    expect(await socket.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.AHEAD_OF_SERVER } });
  });

  it("closes a connection whose token expired, and one that reauthenticates as another device", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    await sa.start();
    await runInDurableObject(stub, (instance: import("../src/space-do").SpaceDO) => instance.closeExpired(Date.now() + 16 * 60_000 + 1000));
    expect((await sa.waitClosed()).code).toBe(Close.TOKEN_EXPIRED);

    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    sb.send({ case: "reauth", value: { accessToken: b.access!.token } });
    sb.send({ case: "reauth", value: { accessToken: a.access!.token } });
    const goodbye = await sb.next();
    expect(goodbye).toMatchObject({ case: "goodbye", value: { code: Close.TOKEN_EXPIRED } });
  });

  it("requires Hello first, answers text pings, and refuses unknown users", async () => {
    const { a, spaceId } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    socket.ws.send("ping");
    expect(await socket.text()).toBe("pong");
    socket.send({ case: "resume", value: { historyId: "", sinceSeq: 0n } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.PROTOCOL } });

    expect(await Socket.open(spaceId, undefined)).toBe(401);
    const stranger = uuidv7();
    await env.SPACE.get(env.SPACE.idFromName(stranger)).init({ spaceId: stranger, name: "x", ownerUserId: uuidv7(), spaceKey: new Uint8Array(32) });
    expect(await Socket.open(stranger, a.access!.token)).toBe(403);
  });

  it("serves the same RPCs over HTTP and pushes HTTP commits", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    const commit = body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "via http")] } } });
    const response = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: commit }, RpcResponseSchema);
    expect(response.message!.result.case).toBe("commit");
    expect((await sb.pushed())[0]!.seq).toBe(1n);

    const list = body(RpcRequestSchema, { reqId: 2n, call: { case: "listTree", value: {} } });
    const tree = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: list }, RpcResponseSchema);
    const page = tree.message!.result;
    expect(page.case === "tree" && page.value.nodes.map((n) => n.name).sort()).toEqual(["", "via http"]);
  });
});

describe("share links", () => {
  it("checks the password, locks after repeated failures, and honours expiry and revocation", async () => {
    const links = account();
    const spaceId = uuidv7();
    const nodeId = uuidv7();
    const open = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7() });
    expect(await links.openShareLink(open, undefined)).toEqual({ ok: true, value: { spaceId, nodeId } });
    expect(open).toMatch(/^[A-Za-z0-9_-]{22}$/);

    const locked = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7(), password: "correct horse" });
    const started = Date.now();
    expect((await links.openShareLink(locked, "correct horse")).ok).toBe(true);
    console.log("PBKDF2-SHA256 100000 iterations, one share-link verification through the Account DO: " + (Date.now() - started) + " ms");
    for (let i = 0; i < 5; i++) expect((await links.openShareLink(locked, "wrong")).ok).toBe(false);
    expect(await links.openShareLink(locked, "correct horse")).toMatchObject({ ok: false, message: "too many wrong passwords" });

    const row = await runInDurableObject(links, (_instance, state) =>
      state.storage.sql.exec("SELECT password_kdf, password_salt, password_hash, password_iterations FROM share_links WHERE link_id = ?", locked).one(),
    );
    expect(row.password_kdf).toBe("pbkdf2-sha256");
    expect(row.password_iterations).toBe(100_000);
    expect((row.password_salt as ArrayBuffer).byteLength).toBe(16);
    expect((row.password_hash as ArrayBuffer).byteLength).toBe(32);

    const expired = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7(), expiresAtMs: Date.now() - 1 });
    expect((await links.openShareLink(expired, undefined)).ok).toBe(false);
    await links.revokeShareLink(open);
    expect((await links.openShareLink(open, undefined)).ok).toBe(false);
  });

  it("counts consecutive failures per link, including a missing password, and starts over after success or a lock", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    const codes = async (attempts: (string | undefined)[]) => {
      const out = [];
      for (const attempt of attempts) {
        const result = await links.openShareLink(link, attempt);
        out.push(result.ok ? "ok" : Error_Code[result.code]);
      }
      return out;
    };
    expect(await codes(["x", undefined, "x", "x", "pw", "x", "x", "x", "x"])).toEqual([
      "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "ok",
      "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED",
    ]);
    expect(await codes(["x", "pw", "x"])).toEqual(["UNAUTHENTICATED", "RETRY_LATER", "RETRY_LATER"]);
    const state = await runInDurableObject(links, (_instance, s) =>
      s.storage.sql.exec("SELECT failed_attempts FROM share_links WHERE link_id = ?", link).one(),
    );
    expect(state.failed_attempts).toBe(0);
    await runInDurableObject(links, (_instance, s) => {
      s.storage.sql.exec("UPDATE share_links SET locked_until_ms = ? WHERE link_id = ?", Date.now() - 1, link);
    });
    expect(await codes(["x", "x", "x", "x", "pw"])).toEqual(["UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "ok"]);
  });

  it("counts concurrent wrong passwords one by one, so they cannot get past the lock", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    const results = await Promise.all(Array.from({ length: 12 }, () => links.openShareLink(link, "x")));
    const codes = results.map((r) => (r.ok ? "ok" : Error_Code[r.code]));
    expect(codes.filter((c) => c === "UNAUTHENTICATED")).toHaveLength(5);
    expect(codes.filter((c) => c === "RETRY_LATER")).toHaveLength(7);
    expect(await links.openShareLink(link, "pw")).toMatchObject({ ok: false, code: Error_Code.RETRY_LATER });
  });

  it("refuses to open a link whose password scheme the server does not know", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    await runInDurableObject(links, (_instance, s) => {
      s.storage.sql.exec("UPDATE share_links SET password_kdf = 'argon2id' WHERE link_id = ?", link);
    });
    expect(await links.openShareLink(link, "pw")).toMatchObject({ ok: false, code: Error_Code.INTERNAL });
  });
});

// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { env } from "cloudflare:workers";
import { type CommitResult, type Op, OpSchema, UploadReceiptSchema, type UploadReceipt } from "../src/gen/flarebox/v1/commit_pb";
import { type RpcRequest, RpcRequestSchema, type RpcResponse, RpcResponseSchema } from "../src/gen/flarebox/v1/messages_pb";
import { type ChunkRef, ChunkRefSchema, type Content, ContentSchema, type NodeState } from "../src/gen/flarebox/v1/model_pb";
import { ChunkPlan_Action, type Error as RpcError } from "../src/gen/flarebox/v1/rpc_pb";
import { sha256, signChunkGrant } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import type { Session } from "../src/space-do";
import { chunkKey, encodeChunkList } from "../src/space/store";

export const ORIGIN = "https://flarebox.test";
export const OWNER = uuidv7();
export const SPACE_KEY = crypto.getRandomValues(new Uint8Array(32));
const encoder = new TextEncoder();

export async function newSpace(name = "Test space") {
  const spaceId = uuidv7();
  const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
  await stub.init({ spaceId, name, ownerUserId: OWNER, spaceKey: SPACE_KEY });
  return { spaceId, stub };
}

export function session(deviceName: string, overrides: Partial<Session> = {}): Session {
  return { deviceId: uuidv7(), userId: OWNER, deviceName, role: "owner", maxFormat: 1, origin: ORIGIN, ...overrides };
}

export async function content(...parts: string[]): Promise<{ content: Content; bytes: Uint8Array[] }> {
  const bytes = parts.map((part) => encoder.encode(part));
  const chunks: ChunkRef[] = [];
  for (const part of bytes) chunks.push(create(ChunkRefSchema, { hash: await sha256(part), size: part.length }));
  return {
    bytes,
    content: create(ContentSchema, {
      size: BigInt(bytes.reduce((sum, part) => sum + part.length, 0)),
      mtimeNs: 1_700_000_000_000_000_000n,
      chunks,
      chunkCount: chunks.length,
      manifestDigest: await sha256(encodeChunkList(chunks)),
      chunker: "fastcdc2020-1m-4m-16m",
    }),
  };
}

export const op = {
  mkdir: (nodeId: string, parentId: string, name: string): Op =>
    create(OpSchema, { op: { case: "createDir", value: { nodeId, parentId, name } } }),
  create: (nodeId: string, parentId: string, name: string, content: Content): Op =>
    create(OpSchema, { op: { case: "createFile", value: { nodeId, parentId, name, content } } }),
  update: (nodeId: string, content: Content): Op => create(OpSchema, { op: { case: "updateContent", value: { nodeId, content } } }),
  move: (nodeId: string, newParentId: string, newName: string): Op =>
    create(OpSchema, { op: { case: "move", value: { nodeId, newParentId, newName } } }),
  delete: (nodeId: string): Op => create(OpSchema, { op: { case: "delete", value: { nodeId } } }),
  restore: (nodeId: string, init: { versionId?: string; targetParentId?: string; name?: string } = {}): Op =>
    create(OpSchema, { op: { case: "restore", value: { nodeId, ...init } } }),
};

type Stub = Awaited<ReturnType<typeof newSpace>>["stub"];

/** A device talking to one space through the binary RPC entry point. */
export class TestClient {
  baseSeq = 0n;
  /** Time the receipts are signed for; set it when the Space DO clock is moved. */
  nowMs: number | undefined;
  private nextReq = 1n;

  constructor(
    readonly stub: Stub,
    readonly spaceId: string,
    readonly session: Session,
  ) {}

  async call(call: MessageInitShape<typeof RpcRequestSchema>["call"]): Promise<RpcResponse["result"]> {
    const request: RpcRequest = create(RpcRequestSchema, { reqId: this.nextReq++, call });
    const response = fromBinary(RpcResponseSchema, await this.stub.rpc(this.session, toBinary(RpcRequestSchema, request)));
    if (response.reqId !== request.reqId) throw new Error("req_id mismatch");
    return response.result;
  }

  /** Prepares the chunks, stores the ones the server asks for in R2 the way the Worker would, and returns receipts. */
  async upload(...contents: Content[]): Promise<UploadReceipt[]> {
    return this.uploadBytes(contents.flatMap((c) => c.chunks), new Map());
  }

  async uploadParts(parts: { content: Content; bytes: Uint8Array[] }): Promise<UploadReceipt[]> {
    const bytes = new Map(parts.content.chunks.map((chunk, i) => [toHex(chunk.hash), parts.bytes[i]!]));
    return this.uploadBytes(parts.content.chunks, bytes);
  }

  private async uploadBytes(chunks: ChunkRef[], bytes: Map<string, Uint8Array>): Promise<UploadReceipt[]> {
    const result = await this.call({ case: "prepareUpload", value: { chunks } });
    if (result.case !== "prepareUpload") throw new Error(`prepareUpload failed: ${JSON.stringify(result.value)}`);
    const receipts: UploadReceipt[] = [];
    for (const plan of result.value.chunks) {
      if (plan.action !== ChunkPlan_Action.UPLOAD) continue;
      const hex = toHex(plan.hash);
      const chunk = chunks.find((c) => toHex(c.hash) === hex)!;
      await env.BUCKET.put(chunkKey(this.spaceId, hex), bytes.get(hex) ?? new Uint8Array(chunk.size));
      const token = await signChunkGrant(SPACE_KEY, "receipt", this.spaceId, hex, chunk.size, (this.nowMs ?? Date.now()) + 3_600_000);
      receipts.push(create(UploadReceiptSchema, { hash: plan.hash, size: chunk.size, token }));
    }
    return receipts;
  }

  async commitRaw(ops: Op[], init: { baseSeq?: bigint; receipts?: UploadReceipt[]; commitId?: string } = {}): Promise<RpcResponse["result"]> {
    return this.call({
      case: "commit",
      value: { commitId: init.commitId ?? uuidv7(), baseSeq: init.baseSeq ?? this.baseSeq, ops, receipts: init.receipts ?? [] },
    });
  }

  /** Commits and advances baseSeq; throws when the call fails. */
  async commit(ops: Op[], init: { baseSeq?: bigint; receipts?: UploadReceipt[]; commitId?: string } = {}): Promise<CommitResult> {
    const result = await this.commitRaw(ops, init);
    if (result.case !== "commit") throw new Error(`commit failed: ${JSON.stringify((result.value as RpcError | undefined)?.message)}`);
    if (result.value.seq > this.baseSeq) this.baseSeq = result.value.seq;
    return result.value;
  }

  async tree(): Promise<Map<string, NodeState>> {
    const nodes = new Map<string, NodeState>();
    let cursor = "";
    do {
      const result = await this.call({ case: "listTree", value: { cursor, limit: 2 } });
      if (result.case !== "tree") throw new Error("listTree failed");
      for (const node of result.value.nodes) nodes.set(node.nodeId, node);
      cursor = result.value.nextCursor;
    } while (cursor);
    return nodes;
  }
}

export async function client(space: { spaceId: string; stub: Stub }, deviceName: string, overrides: Partial<Session> = {}) {
  return new TestClient(space.stub, space.spaceId, session(deviceName, overrides));
}

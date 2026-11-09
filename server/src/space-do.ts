// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { DurableObject } from "cloudflare:workers";
import { type Commit, type CommitResult, CommitResultSchema, type Op } from "./gen/flarebox/v1/commit_pb";
import {
  CaughtUpSchema,
  ChangesSchema,
  ClientMessageSchema,
  GoodbyeSchema,
  type Hello,
  type Resume,
  ResyncRequired_Reason,
  ResyncRequiredSchema,
  type RpcRequest,
  type RpcResponse,
  RpcRequestSchema,
  RpcResponseSchema,
  type ServerMessage,
  ServerMessageSchema,
  WelcomeSchema,
} from "./gen/flarebox/v1/messages_pb";
import {
  type ChangeRecord,
  ChangeRecordSchema,
  type ChunkRef,
  type CommitRecord,
  CommitRecordSchema,
  type Content,
  NodePurgedSchema,
  NodeStateSchema,
  VersionInfoSchema,
} from "./gen/flarebox/v1/model_pb";
import {
  ChunkPlan_Action,
  ChunkPlanSchema,
  ChunkTargetSchema,
  DownloadTargetsSchema,
  Error_Code,
  ErrorSchema,
  type GetDownloadTargets,
  type GetVersionChunks,
  type ListTrash,
  type ListTree,
  type ListVersions,
  type PrepareUpload,
  PrepareUploadResultSchema,
  TargetSchema,
  TrashPageSchema,
  TreePageSchema,
  VersionChunksSchema,
  VersionListSchema,
} from "./gen/flarebox/v1/rpc_pb";
import { type Identity, verifyAccessToken } from "./auth";
import { sha256, signChunkGrant, verifyChunkGrant } from "./crypto";
import { fromHex, isUuidV7, toHex, uuidv7 } from "./ids";
import { type ApplyContext, CallFailure, CommitApplier } from "./space/commit";
import { ChunkState, migrate } from "./space/schema";
import {
  chunkKey,
  encodeChunkList,
  getMeta,
  getNode,
  getVersion,
  type NodeRow,
  nodeState,
  rows,
  setMeta,
  type VersionRow,
  versionChunkPage,
  versionChunks,
  versionInfo,
  versionPageCount,
} from "./space/store";
import { PROTO_MAX, PROTO_MIN, SERVER_VERSION } from "./version";

export const FORMAT_VERSION = 1;
const HOUR_MS = 3_600_000;
const DAY_MS = 24 * HOUR_MS;
/** How long a PrepareUpload reservation protects a chunk from GC (spec v0 section 12). */
export const HOLD_MS = DAY_MS;
/** Lifetime of chunk tickets and upload receipts. */
export const GRANT_MS = HOUR_MS;
const MAX_OPS = 1000;
const MAX_CHUNKS_PER_CALL = 1000;
/** A MISSING_CHUNKS error lists at most this many hashes, the most one PrepareUpload takes; a retry after uploading them lists the rest. */
export const MAX_MISSING_LISTED = MAX_CHUNKS_PER_CALL;
export const MAX_CHUNK_BYTES = 16 * 1024 * 1024;
/** Spec v0 section 7.2: a ServerMessage is at most 1 MiB, a ClientMessage at most 8 MiB. */
const MAX_SERVER_FRAME = 1024 * 1024;
export const MAX_CLIENT_FRAME = 8 * 1024 * 1024;
const MAX_REQ_ID = 2n ** 64n - 1n;
const RETRY_LATER_MS = 60_000;
/** Room for the ServerMessage, RpcResponse and page fields around a page's items: tags, lengths, req_id, cursor and head_seq. */
const PAGE_ENVELOPE_BYTES = 1024;
const GC_BATCH = 1000;
const GC_RETRY_MS = 10 * 60_000;
const R2_DELETE_BATCH = 1000;
const ORPHAN_SWEEP_MS = 7 * DAY_MS;

export type Role = "owner" | "editor" | "viewer";

/** Header the Worker sets on the WebSocket upgrade it forwards: JSON of {@link Connect}. */
export const CONNECT_HEADER = "X-Flarebox-Connect";
/** How long past its access token's expiry a connection may stay open (spec v0 section 7.1). */
export const TOKEN_GRACE_MS = 60_000;

/** WebSocket close codes. */
export const Close = { TRY_AGAIN_LATER: 1013, PROTOCOL: 4000, TOKEN_EXPIRED: 4001, UNSUPPORTED_VERSION: 4002, DEVICE_REVOKED: 4003 } as const;

export interface Connect {
  identity: Identity;
  origin: string;
}

/** Per-connection state kept in the WebSocket attachment so it survives hibernation. */
interface Attachment {
  connId: string;
  identity: Identity;
  origin: string;
  role: Role;
  maxFormat: number;
  hello: boolean;
  live: boolean;
}

/** Who is calling, as established by the Worker from the access token and by the WebSocket Hello. */
export interface Session {
  deviceId: string;
  userId: string;
  deviceName: string;
  role: Role;
  maxFormat: number;
  /** Public origin of the deployment, used to build chunk URLs. */
  origin: string;
}

export interface SpaceSettings {
  version_retention_days: number;
  trash_retention_days: number;
  chunk_grace_days: number;
}

const DEFAULT_SETTINGS: SpaceSettings = { version_retention_days: 30, trash_retention_days: 30, chunk_grace_days: 30 };

export interface GcReport {
  expiredVersions: number;
  purgedNodes: number;
  deletedChunks: number;
  sweptChunks: number;
  more: boolean;
}

function fail(code: Error_Code, message: string): never {
  throw new CallFailure(code, message);
}

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i]);
}

function checkChunkRef(chunk: ChunkRef): void {
  if (chunk.hash.length !== 32) fail(Error_Code.INVALID_ARGUMENT, "chunk hash must be 32 bytes");
  if (chunk.size < 1 || chunk.size > MAX_CHUNK_BYTES) fail(Error_Code.INVALID_ARGUMENT, "chunk size out of range");
  if (chunk.encoding !== 0) fail(Error_Code.INVALID_ARGUMENT, "unsupported chunk encoding");
}

async function checkContent(content: Content | undefined): Promise<Content> {
  if (!content) fail(Error_Code.INVALID_ARGUMENT, "content is required");
  if (content.chunks.length !== content.chunkCount) fail(Error_Code.INVALID_ARGUMENT, "commits must list every chunk inline");
  content.chunks.forEach(checkChunkRef);
  const total = content.chunks.reduce((sum, chunk) => sum + BigInt(chunk.size), 0n);
  if (total !== content.size) fail(Error_Code.INVALID_ARGUMENT, "content size does not match its chunks");
  if (!sameBytes(await sha256(encodeChunkList(content.chunks)), content.manifestDigest)) {
    fail(Error_Code.INVALID_ARGUMENT, "manifest_digest does not match the chunk list");
  }
  return content;
}

function checkIds(op: Op, index: number): void {
  const required: string[] = [];
  const optional: string[] = [];
  switch (op.op.case) {
    case "createDir":
    case "createFile":
      required.push(op.op.value.nodeId, op.op.value.parentId);
      break;
    case "updateContent":
    case "delete":
      required.push(op.op.value.nodeId);
      break;
    case "move":
      required.push(op.op.value.nodeId, op.op.value.newParentId);
      break;
    case "restore":
      required.push(op.op.value.nodeId);
      optional.push(op.op.value.versionId, op.op.value.targetParentId);
      break;
    default:
      fail(Error_Code.INVALID_ARGUMENT, `op ${index} is empty`);
  }
  if (!required.every(isUuidV7) || !optional.every((id) => id === "" || isUuidV7(id))) {
    fail(Error_Code.INVALID_ARGUMENT, `op ${index} has a malformed id`);
  }
}

function opContent(op: Op): Content | undefined | null {
  if (op.op.case === "createFile" || op.op.case === "updateContent") return op.op.value.content;
  return null;
}

/** The size of a reply as a WebSocket ServerMessage, which is a few bytes more than the HTTP body. */
function frameBytes(response: RpcResponse): number {
  return toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: response } })).length;
}

/** Wraps a non-commit result, refusing it if its WebSocket frame would exceed 1 MiB. Commit replies are bounded when stored. */
function reply(reqId: bigint, result: RpcResponse["result"]): { response: RpcResponse } {
  const response = create(RpcResponseSchema, { reqId, result });
  if (frameBytes(response) > MAX_SERVER_FRAME) fail(Error_Code.LIMIT_EXCEEDED, "the reply does not fit in one 1 MiB message");
  return { response };
}

/**
 * Takes items from the front while a page of them fits in one 1 MiB ServerMessage, counting each item's field tag and
 * length prefix and leaving room for the envelope. The first item is always taken; the commit size limit keeps it small enough.
 */
function fitPage<R, T>(rows: R[], build: (row: R) => T, encodedSize: (item: T) => number): T[] {
  const page: T[] = [];
  let bytes = PAGE_ENVELOPE_BYTES;
  for (const row of rows) {
    const item = build(row);
    bytes += encodedSize(item) + 6;
    if (page.length > 0 && bytes > MAX_SERVER_FRAME) break;
    page.push(item);
  }
  return page;
}

/** One per space: tree, versions, chunks and the commit log. */
export class SpaceDO extends DurableObject<Env> {
  private readonly sql: SqlStorage;
  private spaceKeyCache: Uint8Array | undefined;
  /** The time source for holds, retention and tickets. Tests replace it to move through days. */
  clock: () => number = () => Date.now();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    ctx.storage.transactionSync(() => migrate(this.sql));
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  /** Creates the space on first call; later calls with the same id do nothing. */
  async init(args: { spaceId: string; name: string; ownerUserId: string; spaceKey: Uint8Array }): Promise<void> {
    const existing = getMeta(this.sql, "space_id");
    if (existing !== undefined) {
      if (existing !== args.spaceId) throw new Error("this Space DO already holds another space");
      return;
    }
    this.ctx.storage.transactionSync(() => {
      setMeta(this.sql, "space_id", args.spaceId);
      setMeta(this.sql, "name", args.name);
      setMeta(this.sql, "format_version", FORMAT_VERSION);
      setMeta(this.sql, "history_id", uuidv7());
      setMeta(this.sql, "head_seq", 0);
      setMeta(this.sql, "log_floor_seq", 0);
      setMeta(this.sql, "space_key", args.spaceKey.slice().buffer);
      setMeta(this.sql, "settings", JSON.stringify(DEFAULT_SETTINGS));
      this.sql.exec("INSERT INTO members (user_id, role, added_seq) VALUES (?, 'owner', 0)", args.ownerUserId);
      this.sql.exec(
        `INSERT INTO nodes (node_id, parent_id, name, name_key, kind, status, content_rev, content_dev, meta_rev, meta_dev)
         VALUES (?, NULL, '', '', 1, 1, 0, '', 0, '')`,
        args.spaceId,
      );
    });
    await this.schedule("gc", this.clock() + DAY_MS);
  }

  /** Answers one binary RpcRequest with a binary RpcResponse (the HTTP path, and tests). */
  async rpc(session: Session, request: Uint8Array): Promise<Uint8Array> {
    const { response } = await this.call(session, fromBinary(RpcRequestSchema, request));
    return toBinary(RpcResponseSchema, response);
  }

  /** The HTTP RPC path: the role comes from the member list, the format from the device's last Hello. */
  async httpRpc(connect: Connect, request: Uint8Array): Promise<Uint8Array> {
    const req = fromBinary(RpcRequestSchema, request);
    const role = this.memberRole(connect.identity.userId);
    if (!role || this.revoked(connect.identity.deviceId)) {
      const error = role
        ? create(ErrorSchema, { code: Error_Code.UNAUTHENTICATED, message: "device revoked" })
        : create(ErrorSchema, { code: Error_Code.FORBIDDEN, message: "not a member of this space" });
      return toBinary(RpcResponseSchema, create(RpcResponseSchema, { reqId: req.reqId, result: { case: "error", value: error } }));
    }
    const device = this.sql.exec("SELECT max_format FROM devices WHERE device_id = ?", connect.identity.deviceId).toArray()[0];
    const session = this.sessionOf(connect, role, device ? Number(device.max_format) : FORMAT_VERSION);
    const { response, record } = await this.call(session, req);
    if (record) this.broadcast(record);
    return toBinary(RpcResponseSchema, response);
  }

  /** Accepts a WebSocket upgrade forwarded by the Worker after it checked the access token. */
  override async fetch(request: Request): Promise<Response> {
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") return new Response("expected a WebSocket upgrade", { status: 426 });
    const connect = JSON.parse(request.headers.get(CONNECT_HEADER) ?? "null") as Connect | null;
    if (!connect) return new Response("missing identity", { status: 401 });
    const role = this.memberRole(connect.identity.userId);
    if (!role) return new Response("not a member of this space", { status: 403 });
    if (this.revoked(connect.identity.deviceId)) return new Response("device revoked", { status: 401 });
    const [client, server] = Object.values(new WebSocketPair()) as [WebSocket, WebSocket];
    this.ctx.acceptWebSocket(server, [connect.identity.deviceId]);
    const attachment: Attachment = { connId: uuidv7(), identity: connect.identity, origin: connect.origin, role, maxFormat: 0, hello: false, live: false };
    server.serializeAttachment(attachment);
    await this.scheduleTokenSweep(connect.identity.expiresAtMs + TOKEN_GRACE_MS);
    return new Response(null, { status: 101, webSocket: client });
  }

  override async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    if (ws.readyState !== WebSocket.OPEN) return;
    if (typeof message === "string") return this.goodbye(ws, Close.PROTOCOL, "text messages other than ping are not supported");
    if (message.byteLength > MAX_CLIENT_FRAME) return this.goodbye(ws, Close.PROTOCOL, "message larger than 8 MiB");
    const attachment = ws.deserializeAttachment() as Attachment;
    if (this.clock() > attachment.identity.expiresAtMs + TOKEN_GRACE_MS) return this.goodbye(ws, Close.TOKEN_EXPIRED, "access token expired");
    const active = await this.deviceActive(attachment.identity.deviceId);
    if (active === undefined) return this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
    if (!active) return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
    if (ws.readyState !== WebSocket.OPEN) return;
    let msg;
    try {
      msg = fromBinary(ClientMessageSchema, new Uint8Array(message)).msg;
    } catch {
      return this.goodbye(ws, Close.PROTOCOL, "malformed ClientMessage");
    }
    if (msg.case === "hello") return this.hello(ws, attachment, msg.value);
    if (msg.case === "reauth") return this.reauth(ws, attachment, msg.value.accessToken);
    if (!attachment.hello) return this.goodbye(ws, Close.PROTOCOL, "Hello must come first");
    if (msg.case === "resume") return this.resume(ws, attachment, msg.value);
    if (msg.case === "rpc") {
      const { response, record } = await this.call(this.sessionOf(attachment, attachment.role, attachment.maxFormat), msg.value);
      this.send(ws, { case: "rpc", value: response });
      if (record) this.broadcast(record, attachment.connId);
      return;
    }
    return this.goodbye(ws, Close.PROTOCOL, "empty ClientMessage");
  }

  /** Closes every connection of a revoked device with 4003. */
  async closeDevice(deviceId: string): Promise<void> {
    this.recordRevoked(deviceId);
    for (const ws of this.ctx.getWebSockets(deviceId)) this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
  }

  private recordRevoked(deviceId: string): void {
    this.sql.exec("INSERT INTO revoked_devices (device_id, revoked_at_ms) VALUES (?, ?) ON CONFLICT DO NOTHING", deviceId, Date.now());
  }

  private revoked(deviceId: string): boolean {
    return this.sql.exec("SELECT 1 FROM revoked_devices WHERE device_id = ?", deviceId).toArray().length > 0;
  }

  /**
   * Asks the Account DO whether the device is still active, so a revocation stops the device even before its notice
   * reaches this space, and remembers a refusal. Other events wait, so messages of one connection keep their order.
   * Returns undefined when the Account DO cannot answer; the error is caught inside the callback, because an
   * exception there would reset this object and drop every connection.
   */
  private async deviceActive(deviceId: string): Promise<boolean | undefined> {
    if (this.revoked(deviceId)) return false;
    const account = this.env.ACCOUNT.get(this.env.ACCOUNT.idFromName("account"));
    const active = await this.ctx.blockConcurrencyWhile(async () => {
      try {
        return await account.isDeviceActive(deviceId);
      } catch (error) {
        console.error("account device check failed", error);
        return undefined;
      }
    });
    if (active === false) this.recordRevoked(deviceId);
    return active;
  }

  /** Closes connections whose access token expired more than a minute ago; returns when to check next. */
  closeExpired(nowMs: number): number | undefined {
    let next: number | undefined;
    for (const ws of this.ctx.getWebSockets()) {
      const deadline = (ws.deserializeAttachment() as Attachment).identity.expiresAtMs + TOKEN_GRACE_MS;
      if (deadline < nowMs) this.goodbye(ws, Close.TOKEN_EXPIRED, "access token expired");
      else next = Math.min(next ?? deadline, deadline);
    }
    return next;
  }

  private memberRole(userId: string): Role | undefined {
    const row = this.sql.exec("SELECT role FROM members WHERE user_id = ?", userId).toArray()[0];
    return row ? (String(row.role) as Role) : undefined;
  }

  private sessionOf(connect: Connect, role: Role, maxFormat: number): Session {
    const { identity, origin } = connect;
    return { deviceId: identity.deviceId, userId: identity.userId, deviceName: identity.deviceName, role, maxFormat, origin };
  }

  /** Sends one ServerMessage; a socket this server already closed gets nothing. */
  private send(ws: WebSocket, msg: ServerMessage["msg"]): void {
    if (ws.readyState !== WebSocket.OPEN) return;
    ws.send(toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg })));
  }

  private goodbye(ws: WebSocket, code: number, reason: string): void {
    try {
      this.send(ws, { case: "goodbye", value: create(GoodbyeSchema, { code, reason }) });
      ws.close(code, reason);
    } catch {
      // Already closed.
    }
  }

  /** Pushes a new commit to every caught-up connection except the one that made it. */
  private broadcast(record: CommitRecord, exceptConnId?: string): void {
    const frame = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "changes", value: { commits: [record] } } }));
    for (const ws of this.ctx.getWebSockets()) {
      const attachment = ws.deserializeAttachment() as Attachment;
      if (!attachment.live || attachment.connId === exceptConnId) continue;
      try {
        ws.send(frame);
      } catch {
        // The socket is closing; the client catches up with Resume when it reconnects.
      }
    }
  }

  private hello(ws: WebSocket, attachment: Attachment, hello: Hello): void {
    const proto = Math.min(PROTO_MAX, hello.protoMax);
    if (proto < Math.max(PROTO_MIN, hello.protoMin)) {
      return this.goodbye(ws, Close.UNSUPPORTED_VERSION, "this server speaks protocol 1." + PROTO_MIN + " to 1." + PROTO_MAX);
    }
    attachment.hello = true;
    attachment.maxFormat = hello.maxFormat;
    ws.serializeAttachment(attachment);
    const { identity } = attachment;
    this.sql.exec(
      `INSERT INTO devices (device_id, user_id, name, max_format, last_seen_ms) VALUES (?, ?, ?, ?, ?)
       ON CONFLICT (device_id) DO UPDATE SET name = excluded.name, max_format = excluded.max_format, last_seen_ms = excluded.last_seen_ms`,
      identity.deviceId,
      identity.userId,
      identity.deviceName,
      hello.maxFormat,
      this.clock(),
    );
    const formatVersion = Number(getMeta(this.sql, "format_version"));
    const welcome = create(WelcomeSchema, {
      proto,
      serverVersion: SERVER_VERSION,
      formatVersion,
      historyId: String(getMeta(this.sql, "history_id")),
      headSeq: BigInt(this.headSeq),
      logFloorSeq: BigInt(Number(getMeta(this.sql, "log_floor_seq"))),
      deviceSeq: this.acceptedDeviceSeq(identity.deviceId),
      readOnly: attachment.role === "viewer" || hello.maxFormat < formatVersion,
    });
    this.send(ws, { case: "welcome", value: welcome });
  }

  /** Sends every commit after since_seq, then CaughtUp, then marks the connection live (spec v0 section 7.3). */
  private resume(ws: WebSocket, attachment: Attachment, resume: Resume): void {
    const head = this.headSeq;
    let reason: ResyncRequired_Reason | undefined;
    if (resume.historyId !== getMeta(this.sql, "history_id")) reason = ResyncRequired_Reason.HISTORY_CHANGED;
    else if (resume.sinceSeq > BigInt(head)) reason = ResyncRequired_Reason.AHEAD_OF_SERVER;
    else if (resume.sinceSeq < BigInt(Number(getMeta(this.sql, "log_floor_seq")))) reason = ResyncRequired_Reason.LOG_TRUNCATED;
    if (reason !== undefined) {
      const resync = create(ResyncRequiredSchema, { reason });
      if (reason === ResyncRequired_Reason.HISTORY_CHANGED) {
        resync.restoreSeq = BigInt(Number(getMeta(this.sql, "restore_seq") ?? 0));
        resync.restorePointMs = BigInt(Number(getMeta(this.sql, "restore_point_ms") ?? 0));
      }
      return this.send(ws, { case: "resyncRequired", value: resync });
    }

    let batch: CommitRecord[] = [];
    let bytes = 0;
    const flush = () => {
      if (batch.length > 0) this.send(ws, { case: "changes", value: create(ChangesSchema, { commits: batch }) });
      batch = [];
      bytes = 0;
    };
    for (const row of this.sql.exec("SELECT record FROM commits WHERE seq > ? ORDER BY seq", Number(resume.sinceSeq))) {
      const record = new Uint8Array(row.record as ArrayBuffer);
      const framed = record.length + 4;
      if (bytes + framed > MAX_SERVER_FRAME - 8) flush();
      batch.push(fromBinary(CommitRecordSchema, record));
      bytes += framed;
    }
    flush();
    this.send(ws, { case: "caughtUp", value: create(CaughtUpSchema, { headSeq: BigInt(head) }) });
    attachment.live = true;
    ws.serializeAttachment(attachment);
  }

  private async reauth(ws: WebSocket, attachment: Attachment, token: string): Promise<void> {
    const account = this.env.ACCOUNT.get(this.env.ACCOUNT.idFromName("account"));
    const identity = await verifyAccessToken(await account.serverKey(), token, this.clock());
    if (!identity || identity.deviceId !== attachment.identity.deviceId || identity.userId !== attachment.identity.userId) {
      return this.goodbye(ws, Close.TOKEN_EXPIRED, "invalid access token");
    }
    const active = await this.deviceActive(identity.deviceId);
    if (active === undefined) return this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
    if (!active) return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
    const current = ws.deserializeAttachment() as Attachment;
    current.identity = identity;
    ws.serializeAttachment(current);
    await this.scheduleTokenSweep(identity.expiresAtMs + TOKEN_GRACE_MS);
  }

  private async scheduleTokenSweep(dueMs: number): Promise<void> {
    const current = this.sql.exec("SELECT due_ms FROM schedule WHERE task = 'tokens'").toArray()[0];
    if (!current || Number(current.due_ms) > dueMs) await this.schedule("tokens", dueMs);
  }
  /** Runs one RPC. A new commit also returns its CommitRecord so the caller can push it. */
  async call(session: Session, request: RpcRequest): Promise<{ response: RpcResponse; record?: CommitRecord }> {
    const reqId = request.reqId;
    try {
      const call = request.call;
      switch (call.case) {
        case "commit": {
          const { result, record } = await this.commit(session, call.value);
          return { response: create(RpcResponseSchema, { reqId, result: { case: "commit", value: result } }), record };
        }
        case "prepareUpload":
          return reply(reqId, { case: "prepareUpload", value: await this.prepareUpload(session, call.value) });
        case "getDownloadTargets":
          return reply(reqId, { case: "downloadTargets", value: await this.downloadTargets(session, call.value) });
        case "getVersionChunks":
          return reply(reqId, { case: "versionChunks", value: this.versionChunks(call.value) });
        case "listVersions":
          return reply(reqId, { case: "versions", value: this.listVersions(call.value) });
        case "listTree":
          return reply(reqId, { case: "tree", value: this.listTree(call.value) });
        case "listTrash":
          return reply(reqId, { case: "trash", value: this.listTrash(call.value) });
        default:
          return fail(Error_Code.INVALID_ARGUMENT, "empty request");
      }
    } catch (error) {
      if (!(error instanceof CallFailure)) {
        console.error("space rpc failed", error);
        error = new CallFailure(Error_Code.INTERNAL, "internal error");
      }
      const failure = error as CallFailure;
      const value = create(ErrorSchema, {
        code: failure.code,
        message: failure.message,
        missingChunks: failure.missingChunks,
        retryAfterMs: BigInt(failure.retryAfterMs),
      });
      const response = create(RpcResponseSchema, { reqId, result: { case: "error", value } });
      if (frameBytes(response) <= MAX_SERVER_FRAME) return { response };
      const limit = create(ErrorSchema, { code: Error_Code.LIMIT_EXCEEDED, message: "the error reply does not fit in one 1 MiB message" });
      return { response: create(RpcResponseSchema, { reqId, result: { case: "error", value: limit } }) };
    }
  }

  private get spaceId(): string {
    const id = getMeta(this.sql, "space_id");
    if (typeof id !== "string") fail(Error_Code.INVALID_ARGUMENT, "space does not exist");
    return id;
  }

  private get spaceKey(): Uint8Array {
    this.spaceKeyCache ??= new Uint8Array(getMeta(this.sql, "space_key") as ArrayBuffer);
    return this.spaceKeyCache;
  }

  private get headSeq(): number {
    return Number(getMeta(this.sql, "head_seq"));
  }

  private settings(): SpaceSettings {
    return { ...DEFAULT_SETTINGS, ...JSON.parse(String(getMeta(this.sql, "settings"))) };
  }

  private checkCanWrite(session: Session): void {
    if (session.role === "viewer") fail(Error_Code.FORBIDDEN, "viewers cannot change this space");
    if (session.maxFormat < Number(getMeta(this.sql, "format_version"))) {
      fail(Error_Code.READ_ONLY, "this client is older than the space format");
    }
  }

  /** The result stored for a commit, in the log or as its device's latest accepted commit (RFC 0002 section 2). */
  private storedResult(commitId: string): CommitResult | undefined {
    const row =
      this.sql.exec("SELECT results FROM commits WHERE commit_id = ?", commitId).toArray()[0] ??
      this.sql.exec("SELECT results FROM device_commits WHERE commit_id = ?", commitId).toArray()[0];
    return row ? fromBinary(CommitResultSchema, new Uint8Array(row.results as ArrayBuffer)) : undefined;
  }

  private acceptedDeviceSeq(deviceId: string): bigint {
    const row = this.sql.exec("SELECT device_seq FROM device_commits WHERE device_id = ?", deviceId).toArray()[0];
    return BigInt(Number(row?.device_seq ?? 0));
  }

  /**
   * RFC 0002 section 3, steps 1 to 4: the history fence, the stored result, the device counter and the base.
   * Returns the stored result of a commit that was already accepted; throws for a refused one.
   */
  private checkCommitOrder(deviceId: string, commit: Commit): CommitResult | undefined {
    if (commit.historyId !== getMeta(this.sql, "history_id")) fail(Error_Code.HISTORY_MISMATCH, "the commit was built in another history of this space");
    const stored = this.storedResult(commit.commitId);
    if (stored) return stored;
    const accepted = this.acceptedDeviceSeq(deviceId);
    if (commit.deviceSeq === 0n) fail(Error_Code.INVALID_ARGUMENT, "device_seq is required");
    if (commit.deviceSeq <= accepted) {
      fail(Error_Code.STALE_DEVICE_STATE, `device_seq ${commit.deviceSeq} is at or below this device's highest accepted one, ${accepted}`);
    }
    if (commit.deviceSeq > accepted + 1n) fail(Error_Code.INVALID_ARGUMENT, `device_seq must be ${accepted + 1n}`);
    const head = BigInt(this.headSeq);
    if (commit.baseSeq > head) fail(Error_Code.AHEAD_OF_SERVER, "base_seq is ahead of the server");
    if (commit.requireHead && commit.baseSeq < head) fail(Error_Code.HEAD_MOVED, "the space changed after base_seq");
    return undefined;
  }

  private appendCommit(record: CommitRecord, result: CommitResult): void {
    const recordBytes = toBinary(CommitRecordSchema, record);
    const resultBytes = toBinary(CommitResultSchema, result);
    const pushed = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "changes", value: { commits: [record] } } }));
    const answered = toBinary(
      ServerMessageSchema,
      create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: MAX_REQ_ID, result: { case: "commit", value: result } } } }),
    );
    if (Math.max(pushed.length, answered.length) > MAX_SERVER_FRAME) {
      fail(Error_Code.LIMIT_EXCEEDED, "the commit's change records do not fit in one 1 MiB message; split the commit");
    }
    this.sql.exec(
      "INSERT INTO commits (seq, commit_id, device_id, user_id, committed_at_ms, record, results) VALUES (?, ?, ?, ?, ?, ?, ?)",
      Number(record.seq),
      record.commitId,
      record.deviceId,
      record.userId,
      Number(record.committedAtMs),
      recordBytes,
      resultBytes,
    );
    setMeta(this.sql, "head_seq", Number(record.seq));
  }

  private async commit(session: Session, commit: Commit): Promise<{ result: CommitResult; record?: CommitRecord }> {
    this.checkCanWrite(session);
    if (!isUuidV7(commit.commitId)) fail(Error_Code.INVALID_ARGUMENT, "commit_id must be a UUIDv7");
    const stored = this.checkCommitOrder(session.deviceId, commit);
    if (stored) return { result: stored };
    if (commit.ops.length === 0) fail(Error_Code.INVALID_ARGUMENT, "a commit needs at least one op");
    if (commit.ops.length > MAX_OPS) fail(Error_Code.LIMIT_EXCEEDED, `at most ${MAX_OPS} ops per commit`);

    commit.ops.forEach(checkIds);
    const referenced = new Map<string, ChunkRef>();
    for (const op of commit.ops) {
      const content = opContent(op);
      if (content === null) continue;
      for (const chunk of (await checkContent(content)).chunks) {
        const hex = toHex(chunk.hash);
        if ((referenced.get(hex)?.size ?? chunk.size) !== chunk.size) fail(Error_Code.INVALID_ARGUMENT, "one chunk hash is listed with two sizes");
        referenced.set(hex, chunk);
      }
    }
    const spaceId = this.spaceId;
    const nowMs = this.clock();
    const receipted = new Set<string>();
    for (const receipt of commit.receipts) {
      const hex = toHex(receipt.hash);
      const chunk = referenced.get(hex);
      if (chunk && chunk.size === receipt.size && (await verifyChunkGrant(this.spaceKey, "receipt", spaceId, hex, receipt.size, receipt.token, nowMs))) {
        receipted.add(hex);
      }
    }

    return this.ctx.storage.transactionSync(() => {
      if (this.revoked(session.deviceId)) fail(Error_Code.UNAUTHENTICATED, "device revoked");
      const again = this.checkCommitOrder(session.deviceId, commit);
      if (again) return { result: again };
      const head = this.headSeq;

      const missing: Uint8Array[] = [];
      for (const [hex, chunk] of referenced) {
        const row = this.sql.exec("SELECT size, state FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
        const ready = row && row.size === chunk.size && (row.state === ChunkState.LIVE || (row.state === ChunkState.PENDING && receipted.has(hex)));
        if (!ready) missing.push(chunk.hash);
      }
      if (missing.length > 0) {
        const listed = missing.slice(0, MAX_MISSING_LISTED);
        throw new CallFailure(Error_Code.MISSING_CHUNKS, `${missing.length} chunks are not uploaded; ${listed.length} are listed`, listed);
      }

      const ctx: ApplyContext = {
        seq: head + 1,
        baseSeq: Number(commit.baseSeq),
        nowMs,
        committer: { deviceId: session.deviceId, userId: session.userId, deviceName: session.deviceName },
      };
      const { results, changes } = new CommitApplier(this.sql, ctx).apply(commit.ops);
      const record = create(CommitRecordSchema, {
        seq: BigInt(ctx.seq),
        commitId: commit.commitId,
        deviceId: session.deviceId,
        userId: session.userId,
        committedAtMs: BigInt(nowMs),
        changes,
      });
      const result = create(CommitResultSchema, { seq: BigInt(ctx.seq), results, changes });
      this.appendCommit(record, result);
      this.sql.exec(
        `INSERT INTO device_commits (device_id, device_seq, commit_id, results) VALUES (?, ?, ?, ?)
         ON CONFLICT (device_id) DO UPDATE SET device_seq = excluded.device_seq, commit_id = excluded.commit_id, results = excluded.results`,
        session.deviceId,
        Number(commit.deviceSeq),
        commit.commitId,
        toBinary(CommitResultSchema, result),
      );
      return { result, record };
    });
  }

  private chunkUrl(session: Session, hex: string, ticket: string): string {
    return `${session.origin}/v1/spaces/${this.spaceId}/chunks/${hex}?ticket=${ticket}`;
  }

  private async prepareUpload(session: Session, request: PrepareUpload) {
    this.checkCanWrite(session);
    if (request.chunks.length > MAX_CHUNKS_PER_CALL) fail(Error_Code.LIMIT_EXCEEDED, `at most ${MAX_CHUNKS_PER_CALL} chunks per call`);
    request.chunks.forEach(checkChunkRef);
    const byHash = new Map<string, ChunkRef>();
    for (const chunk of request.chunks) {
      const hex = toHex(chunk.hash);
      if ((byHash.get(hex)?.size ?? chunk.size) !== chunk.size) fail(Error_Code.INVALID_ARGUMENT, "one chunk hash is listed with two sizes");
      byHash.set(hex, chunk);
    }
    const chunks = [...byHash.values()];
    const nowMs = this.clock();
    const holdUntil = nowMs + HOLD_MS;
    const actions = this.ctx.storage.transactionSync(() =>
      chunks.map((chunk) => {
        const row = this.sql.exec("SELECT size, state FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
        if (!row) {
          this.sql.exec(
            "INSERT INTO chunks (hash, size, state, refcount, hold_until_ms) VALUES (?, ?, ?, 0, ?)",
            chunk.hash,
            chunk.size,
            ChunkState.PENDING,
            holdUntil,
          );
          return ChunkPlan_Action.UPLOAD;
        }
        if (row.size !== chunk.size) fail(Error_Code.INVALID_ARGUMENT, "chunk size differs from the stored chunk");
        if (row.state === ChunkState.DELETING) return ChunkPlan_Action.RETRY_LATER;
        this.sql.exec("UPDATE chunks SET hold_until_ms = MAX(hold_until_ms, ?) WHERE hash = ?", holdUntil, chunk.hash);
        return row.state === ChunkState.LIVE ? ChunkPlan_Action.PRESENT : ChunkPlan_Action.UPLOAD;
      }),
    );
    const expiresAtMs = nowMs + GRANT_MS;
    const spaceId = this.spaceId;
    const plans = [];
    for (const [i, chunk] of chunks.entries()) {
      const action = actions[i]!;
      const plan = create(ChunkPlanSchema, { hash: chunk.hash, action });
      if (action === ChunkPlan_Action.RETRY_LATER) plan.retryAfterMs = BigInt(RETRY_LATER_MS);
      if (action === ChunkPlan_Action.UPLOAD) {
        const hex = toHex(chunk.hash);
        const ticket = await signChunkGrant(this.spaceKey, "put", spaceId, hex, chunk.size, expiresAtMs);
        plan.target = create(TargetSchema, { url: this.chunkUrl(session, hex, ticket), method: "PUT", expiresAtMs: BigInt(expiresAtMs) });
      }
      plans.push(plan);
    }
    return create(PrepareUploadResultSchema, { chunks: plans });
  }

  private async downloadTargets(session: Session, request: GetDownloadTargets) {
    if (request.hashes.length > MAX_CHUNKS_PER_CALL) fail(Error_Code.LIMIT_EXCEEDED, `at most ${MAX_CHUNKS_PER_CALL} chunks per call`);
    const missing = request.hashes.filter((hash) => {
      const row = this.sql.exec("SELECT state FROM chunks WHERE hash = ?", hash).toArray()[0];
      return row?.state !== ChunkState.LIVE;
    });
    if (missing.length > 0) throw new CallFailure(Error_Code.MISSING_CHUNKS, "some chunks are not stored", missing);
    const expiresAtMs = this.clock() + GRANT_MS;
    const spaceId = this.spaceId;
    const targets = [];
    for (const hash of request.hashes) {
      const hex = toHex(hash);
      const ticket = await signChunkGrant(this.spaceKey, "get", spaceId, hex, 0, expiresAtMs);
      const target = create(TargetSchema, { url: this.chunkUrl(session, hex, ticket), method: "GET", expiresAtMs: BigInt(expiresAtMs) });
      targets.push(create(ChunkTargetSchema, { hash, target }));
    }
    return create(DownloadTargetsSchema, { targets });
  }

  private versionChunks(request: GetVersionChunks) {
    const version = getVersion(this.sql, request.versionId);
    if (!version) fail(Error_Code.INVALID_ARGUMENT, "unknown version");
    const pageCount = versionPageCount(version);
    if (request.page >= pageCount) fail(Error_Code.INVALID_ARGUMENT, "page out of range");
    return create(VersionChunksSchema, { chunks: versionChunkPage(this.sql, version, request.page), page: request.page, pageCount });
  }

  private listVersions(request: ListVersions) {
    const limit = Math.min(request.limit || 100, 1000);
    const [seqText, versionId = ""] = request.cursor ? request.cursor.split("/") : [String(Number.MAX_SAFE_INTEGER)];
    const seq = Number(seqText);
    if (!Number.isSafeInteger(seq)) fail(Error_Code.INVALID_ARGUMENT, "malformed cursor");
    const found = rows<VersionRow>(
      this.sql.exec(
        `SELECT * FROM versions WHERE node_id = ? AND (seq < ? OR (seq = ? AND version_id < ?))
         ORDER BY seq DESC, version_id DESC LIMIT ?`,
        request.nodeId,
        seq,
        seq,
        versionId,
        limit + 1,
      ),
    );
    const page = fitPage(found.slice(0, limit), (version) => versionInfo(this.sql, version), (info) => toBinary(VersionInfoSchema, info).length);
    const last = found[page.length - 1];
    return create(VersionListSchema, {
      versions: page,
      nextCursor: found.length > page.length && last ? `${last.seq}/${last.version_id}` : "",
    });
  }

  private nodePage(status: number, cursor: string, requested: number) {
    const limit = Math.min(requested || 1000, 5000);
    const found = rows<NodeRow>(
      this.sql.exec("SELECT * FROM nodes WHERE status = ? AND node_id > ? ORDER BY node_id LIMIT ?", status, cursor, limit + 1),
    );
    const nodes = fitPage(found.slice(0, limit), (node) => nodeState(this.sql, node), (state) => toBinary(NodeStateSchema, state).length);
    const more = nodes.length < found.length;
    return { nodes, nextCursor: more ? nodes.at(-1)!.nodeId : "" };
  }

  private listTree(request: ListTree) {
    return create(TreePageSchema, { ...this.nodePage(1, request.cursor, request.limit), headSeq: BigInt(this.headSeq) });
  }

  private listTrash(request: ListTrash) {
    return create(TrashPageSchema, this.nodePage(2, request.cursor, request.limit));
  }

  /** Lowers the refcounts of a version's distinct chunks and deletes it. */
  private dropVersion(version: VersionRow, nowMs: number): void {
    const distinct = new Map(versionChunks(this.sql, version).map((chunk) => [toHex(chunk.hash), chunk.hash]));
    for (const hash of distinct.values()) {
      this.sql.exec(
        "UPDATE chunks SET refcount = refcount - 1, zero_since_ms = CASE WHEN refcount = 1 THEN ? ELSE zero_since_ms END WHERE hash = ?",
        nowMs,
        hash,
      );
    }
    this.sql.exec("DELETE FROM version_chunk_pages WHERE version_id = ?", version.version_id);
    this.sql.exec("DELETE FROM versions WHERE version_id = ?", version.version_id);
  }

  /**
   * Expires old versions, purges old trash, and deletes unreferenced chunks in two phases (spec v0 section 12).
   * Each run handles at most one batch of each kind; `more` asks for another run soon.
   */
  async collectGarbage(nowMs: number): Promise<GcReport> {
    const settings = this.settings();
    const spaceId = this.spaceId;
    const purge = this.ctx.storage.transactionSync(() => {
      const expired = rows<VersionRow>(
        this.sql.exec(
          "SELECT * FROM versions WHERE superseded_at_ms IS NOT NULL AND superseded_at_ms < ? LIMIT ?",
          nowMs - settings.version_retention_days * DAY_MS,
          GC_BATCH,
        ),
      );
      for (const version of expired) this.dropVersion(version, nowMs);

      // Leaves first, so that a node is never left behind with a purged parent.
      const changes: ChangeRecord[] = [];
      for (let budget = GC_BATCH; budget > 0; ) {
        const due = rows<NodeRow>(
          this.sql.exec(
            `SELECT * FROM nodes n WHERE status = 2 AND trashed_at_ms < ?
               AND NOT EXISTS (SELECT 1 FROM nodes c WHERE c.parent_id = n.node_id) LIMIT ?`,
            nowMs - settings.trash_retention_days * DAY_MS,
            budget,
          ),
        );
        if (due.length === 0) break;
        budget -= due.length;
        for (const node of due) {
          for (const version of rows<VersionRow>(this.sql.exec("SELECT * FROM versions WHERE node_id = ?", node.node_id))) {
            this.dropVersion(version, nowMs);
          }
          this.sql.exec("DELETE FROM nodes WHERE node_id = ?", node.node_id);
          changes.push(create(ChangeRecordSchema, { change: { case: "purged", value: create(NodePurgedSchema, { nodeId: node.node_id }) } }));
        }
      }
      let record: CommitRecord | undefined;
      if (changes.length > 0) {
        const seq = BigInt(this.headSeq + 1);
        record = create(CommitRecordSchema, { seq, commitId: uuidv7(nowMs), committedAtMs: BigInt(nowMs), changes });
        this.appendCommit(record, create(CommitResultSchema, { seq, changes }));
      }
      return { expiredVersions: expired.length, purgedNodes: changes.length, record };
    });
    if (purge.record) this.broadcast(purge.record);

    const marked = this.ctx.storage.transactionSync(() => {
      const hashes = this.sql
        .exec(
          `SELECT hash FROM chunks WHERE refcount = 0 AND hold_until_ms < ?
             AND (state = 1 OR (state = 2 AND zero_since_ms < ?)) LIMIT ?`,
          nowMs,
          nowMs - settings.chunk_grace_days * DAY_MS,
          GC_BATCH,
        )
        .toArray();
      for (const { hash } of hashes) this.sql.exec("UPDATE chunks SET state = 3 WHERE hash = ?", hash);
      return hashes.length;
    });

    let sweptChunks = 0;
    let sweeping = false;
    if (nowMs >= Number(getMeta(this.sql, "orphan_sweep_due_ms") ?? 0)) {
      const listed = await this.env.BUCKET.list({
        prefix: `spaces/${spaceId}/chunks/`,
        cursor: String(getMeta(this.sql, "orphan_sweep_cursor") ?? "") || undefined,
        limit: GC_BATCH,
      });
      this.ctx.storage.transactionSync(() => {
        for (const object of listed.objects) {
          const hash = fromHex(object.key.slice(object.key.lastIndexOf("/") + 1));
          if (this.sql.exec("SELECT 1 FROM chunks WHERE hash = ?", hash).toArray().length > 0) continue;
          this.sql.exec("INSERT INTO chunks (hash, size, state, refcount, hold_until_ms) VALUES (?, ?, ?, 0, ?)", hash, object.size, ChunkState.DELETING, nowMs);
          sweptChunks++;
        }
        setMeta(this.sql, "orphan_sweep_cursor", listed.truncated ? listed.cursor : "");
        if (!listed.truncated) setMeta(this.sql, "orphan_sweep_due_ms", nowMs + ORPHAN_SWEEP_MS);
      });
      sweeping = listed.truncated;
    }

    let deletedChunks = 0;
    for (;;) {
      const batch = this.sql
        .exec("SELECT hash FROM chunks WHERE state = 3 LIMIT ?", R2_DELETE_BATCH)
        .toArray()
        .map((row) => new Uint8Array(row.hash as ArrayBuffer));
      if (batch.length === 0) break;
      await this.env.BUCKET.delete(batch.map((hash) => chunkKey(spaceId, toHex(hash))));
      this.ctx.storage.transactionSync(() => {
        for (const hash of batch) this.sql.exec("DELETE FROM chunks WHERE hash = ? AND state = 3", hash);
      });
      deletedChunks += batch.length;
    }
    const { expiredVersions, purgedNodes } = purge;
    return {
      expiredVersions,
      purgedNodes,
      deletedChunks,
      sweptChunks,
      more: expiredVersions === GC_BATCH || purgedNodes === GC_BATCH || marked === GC_BATCH || sweeping,
    };
  }

  private async schedule(task: string, dueMs: number): Promise<void> {
    this.sql.exec("INSERT INTO schedule (task, due_ms) VALUES (?, ?) ON CONFLICT (task) DO UPDATE SET due_ms = excluded.due_ms", task, dueMs);
    await this.armAlarm();
  }

  private async armAlarm(): Promise<void> {
    const next = this.sql.exec("SELECT MIN(due_ms) AS due FROM schedule").one().due;
    if (next === null) await this.ctx.storage.deleteAlarm();
    else await this.ctx.storage.setAlarm(Number(next));
  }

  override async alarm(): Promise<void> {
    const nowMs = this.clock();
    const due = this.sql.exec("SELECT task FROM schedule WHERE due_ms <= ?", nowMs).toArray().map((row) => String(row.task));
    for (const task of due) {
      if (task === "gc") {
        let nextMs = nowMs + GC_RETRY_MS;
        try {
          const report = await this.collectGarbage(nowMs);
          nextMs = nowMs + (report.more ? 60_000 : DAY_MS);
        } catch (error) {
          console.error("space gc failed", error);
        }
        this.sql.exec("UPDATE schedule SET due_ms = ? WHERE task = 'gc'", nextMs);
      }
      if (task === "tokens") {
        const next = this.closeExpired(nowMs);
        if (next === undefined) this.sql.exec("DELETE FROM schedule WHERE task = 'tokens'");
        else this.sql.exec("UPDATE schedule SET due_ms = ? WHERE task = 'tokens'", next);
      }
    }
    await this.armAlarm();
  }
}

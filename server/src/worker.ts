// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { type Identity, type ServerKey, bearerToken, verifyAccessToken } from "./auth";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  DeviceCredentialsSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  RewindRequestSchema,
  RewindResultSchema,
  ServerInfo_DataPath,
  ServerInfoSchema,
  SpaceInfoSchema,
  SpaceListSchema,
  TokenRequestSchema,
} from "./gen/flarebox/v1/auth_pb";
import { UploadReceiptSchema } from "./gen/flarebox/v1/commit_pb";
import { Error_Code } from "./gen/flarebox/v1/rpc_pb";
import { BadRequest, errorReply, readMessage, reply } from "./http";
import { type ChunkObject, deriveSpaceKey, signChunkGrant, verifyChunkGrant } from "./crypto";
import { fromHex, isUuidV7 } from "./ids";
import { CONNECT_HEADER, type Connect, GRANT_MS, MAX_CHUNK_BYTES, MAX_CLIENT_FRAME } from "./space-do";
import { chunkKey, INCARNATION } from "./space/store";
import { PROTO_MAX, PROTO_MIN, SERVER_VERSION } from "./version";

export { AccountDO } from "./account-do";
export { SpaceDO } from "./space-do";

const serverKeys = new WeakMap<object, Promise<ServerKey>>();

function account(env: Env) {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

/** The server key, read once per isolate and Account DO binding so that tokens verify without a DO call. A failed read is not kept. */
function key(env: Env): Promise<ServerKey> {
  let cached = serverKeys.get(env.ACCOUNT);
  if (!cached) {
    cached = account(env)
      .serverKey()
      .catch((error: unknown) => {
        serverKeys.delete(env.ACCOUNT);
        throw error;
      });
    serverKeys.set(env.ACCOUNT, cached);
  }
  return cached;
}

/** Checks the access token and the device. With a space, it also refuses a space that a rewind holds closed (RFC 0003 section 4). */
async function authenticate(request: Request, env: Env, spaceId?: string): Promise<Identity | Response> {
  const token = bearerToken(request);
  const identity = token ? await verifyAccessToken(await key(env), token, Date.now()) : undefined;
  if (!identity) return errorReply(request, Error_Code.UNAUTHENTICATED, "missing or invalid access token");
  const check = await account(env).checkSession(identity.deviceId, spaceId);
  if (!check.active) return errorReply(request, Error_Code.UNAUTHENTICATED, "this device was revoked");
  if (check.closed) return errorReply(request, Error_Code.RETRY_LATER, "the space is being rewound; retry later", 503);
  return identity;
}

/** HTTP statuses of the rewind endpoint's errors (RFC 0003 section 1). */
const REWIND_STATUS: Partial<Record<Error_Code, number>> = { [Error_Code.HISTORY_MISMATCH]: 409, [Error_Code.RETRY_LATER]: 503 };

async function rewind(request: Request, env: Env, spaceId: string): Promise<Response> {
  const identity = await authenticate(request, env);
  if (identity instanceof Response) return identity;
  const body = await readMessage(request, RewindRequestSchema);
  const outcome = await account(env).rewind({
    userId: identity.userId,
    spaceId,
    rewindId: body.rewindId,
    restorePointMs: Number(body.restorePointMs),
    expectedHistoryId: body.expectedHistoryId,
  });
  if (!outcome.ok) return errorReply(request, outcome.code, outcome.message, REWIND_STATUS[outcome.code]);
  return reply(request, RewindResultSchema, create(RewindResultSchema, outcome.value));
}

/** The per-space ticket key, derived for each request from the cached server key, so a request keeps no per-space state. */
async function spaceKey(env: Env, spaceId: string): Promise<Uint8Array> {
  return deriveSpaceKey((await key(env)).secret, spaceId);
}

/**
 * Relay-mode chunk transfer (spec v0 sections 8 and 10). The ticket from PrepareUpload or GetDownloadTargets is the
 * only credential; it names the key incarnation to read or write (RFC 0003 section 5). R2 checks the SHA-256 of an
 * upload, and the receipt lets the Space DO accept it in a commit for that incarnation only.
 */
async function chunk(request: Request, env: Env, spaceId: string, hex: string): Promise<Response> {
  const params = new URL(request.url).searchParams;
  const ticket = params.get("ticket") ?? "";
  const incarnation = params.get("inc") ?? "";
  if (!INCARNATION.test(incarnation)) return errorReply(request, Error_Code.FORBIDDEN, "invalid or expired ticket");
  const object: ChunkObject = { spaceId, hashHex: hex, incarnation };
  const nowMs = Date.now();
  const key = chunkKey(spaceId, hex, incarnation);

  if (request.method === "GET") {
    if (!(await verifyChunkGrant(await spaceKey(env, spaceId), "get", object, 0, ticket, nowMs))) {
      return errorReply(request, Error_Code.FORBIDDEN, "invalid or expired download ticket");
    }
    const stored = await env.BUCKET.get(key);
    if (!stored) return errorReply(request, Error_Code.INVALID_ARGUMENT, "chunk not found", 404);
    return new Response(stored.body, {
      headers: { "Content-Type": "application/octet-stream", "Content-Length": String(stored.size), "Cache-Control": "private, max-age=3600" },
    });
  }

  if (request.method !== "PUT") return new Response("Method Not Allowed", { status: 405, headers: { Allow: "GET, PUT" } });
  const length = request.headers.get("Content-Length");
  if (length === null || !/^\d+$/.test(length)) return errorReply(request, Error_Code.INVALID_ARGUMENT, "Content-Length is required", 411);
  const size = Number(length);
  if (size < 1 || size > MAX_CHUNK_BYTES) return errorReply(request, Error_Code.LIMIT_EXCEEDED, "chunk size out of range");
  const secret = await spaceKey(env, spaceId);
  if (!(await verifyChunkGrant(secret, "put", object, size, ticket, nowMs))) {
    return errorReply(request, Error_Code.FORBIDDEN, "invalid or expired upload ticket, or a size the ticket does not cover");
  }
  try {
    await env.BUCKET.put(key, request.body, { sha256: hex, onlyIf: { etagDoesNotMatch: "*" } });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error("chunk upload failed", spaceId, hex, message);
    if (/checksum/i.test(message)) return errorReply(request, Error_Code.INVALID_ARGUMENT, "the body does not match the chunk hash");
    const throttled = /\b429\b|10058|too many|rate/i.test(message);
    return errorReply(request, Error_Code.RETRY_LATER, "chunk storage is unavailable; retry the upload", throttled ? 429 : 503);
  }
  const token = await signChunkGrant(secret, "receipt", object, size, nowMs + GRANT_MS);
  return reply(request, UploadReceiptSchema, create(UploadReceiptSchema, { hash: fromHex(hex), size, token }));
}

function space(env: Env, spaceId: string) {
  return env.SPACE.get(env.SPACE.idFromName(spaceId));
}

async function route(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const path = url.pathname;
  const method = request.method;

  if (method === "GET" && path === "/v1/server") {
    const info = create(ServerInfoSchema, {
      serverVersion: SERVER_VERSION,
      protoMin: PROTO_MIN,
      protoMax: PROTO_MAX,
      dataPath: ServerInfo_DataPath.RELAY,
      claimed: await account(env).isClaimed(),
    });
    return reply(request, ServerInfoSchema, info);
  }

  if (method === "GET" && path === "/setup") {
    const code = await account(env).issueClaimCode();
    if (code) console.log("Flarebox claim code (valid for 1 hour): " + code);
    const text = (await account(env).isClaimed())
      ? "This Flarebox server already has an owner.\n"
      : "A one-time claim code, valid for one hour, is in this Worker's logs. Find it under this Worker's Observability tab in the dashboard by searching for \"Flarebox claim code\", run `flarebox server claim`, or read it with `wrangler tail`.\n";
    return new Response(text + "\nFlarebox is not affiliated with Cloudflare, Inc.\n", { headers: { "Content-Type": "text/plain; charset=utf-8" } });
  }

  if (method === "POST" && path === "/v1/claim") {
    const body = await readMessage(request, ClaimRequestSchema);
    const recoveryKey = (env as Env & { FLAREBOX_RECOVERY_KEY?: string }).FLAREBOX_RECOVERY_KEY;
    const outcome = await account(env).claim(body, request.headers.get("CF-Connecting-IP") ?? "", recoveryKey);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    return reply(request, DeviceCredentialsSchema, create(DeviceCredentialsSchema, outcome.value));
  }

  if (method === "POST" && path === "/v1/pair/complete") {
    const body = await readMessage(request, PairCompleteRequestSchema);
    const outcome = await account(env).completePairing(body);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    return reply(request, DeviceCredentialsSchema, create(DeviceCredentialsSchema, outcome.value));
  }

  if (method === "POST" && path === "/v1/token") {
    const body = await readMessage(request, TokenRequestSchema);
    const outcome = await account(env).refreshAccess(body);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    return reply(request, AccessTokenSchema, create(AccessTokenSchema, outcome.value));
  }

  const chunkRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/chunks\/([0-9a-f]{64})$/.exec(path);
  if (chunkRoute && isUuidV7(chunkRoute[1]!)) return chunk(request, env, chunkRoute[1]!, chunkRoute[2]!);

  const rewindRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/rewind$/.exec(path);
  if (rewindRoute && method === "POST" && isUuidV7(rewindRoute[1]!)) return rewind(request, env, rewindRoute[1]!);

  const spaceRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/(ws|rpc)$/.exec(path);
  if (path !== "/v1/pair/start" && path !== "/v1/spaces" && !spaceRoute) return new Response("Not Found", { status: 404 });
  const identity = await authenticate(request, env, spaceRoute && isUuidV7(spaceRoute[1]!) ? spaceRoute[1]! : undefined);
  if (identity instanceof Response) return identity;

  if (method === "POST" && path === "/v1/pair/start") {
    const outcome = await account(env).startPairing(identity.deviceId);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    const started = create(PairStartResponseSchema, { code: outcome.value.code, expiresAtMs: BigInt(outcome.value.expiresAtMs) });
    return reply(request, PairStartResponseSchema, started);
  }

  if (path === "/v1/spaces") {
    if (method === "GET") {
      const spaces = await account(env).listSpaces(identity.userId);
      return reply(request, SpaceListSchema, create(SpaceListSchema, { spaces: spaces.map((s) => create(SpaceInfoSchema, s)) }));
    }
    if (method === "POST") {
      const body = await readMessage(request, CreateSpaceRequestSchema);
      const created = await account(env).createSpace({ userId: identity.userId, name: body.name });
      return reply(request, SpaceInfoSchema, create(SpaceInfoSchema, created), 201);
    }
  }

  if (spaceRoute && isUuidV7(spaceRoute[1]!)) {
    const connect: Connect = { identity, origin: url.origin };
    const stub = space(env, spaceRoute[1]!);
    if (spaceRoute[2] === "ws" && method === "GET") {
      const headers = new Headers(request.headers);
      headers.set(CONNECT_HEADER, JSON.stringify(connect));
      return stub.fetch(new Request(request.url, { headers }));
    }
    if (spaceRoute[2] === "rpc" && method === "POST") {
      const tooLarge = () => errorReply(request, Error_Code.LIMIT_EXCEEDED, "request body larger than 8 MiB", 413);
      if (Number(request.headers.get("Content-Length") ?? 0) > MAX_CLIENT_FRAME) return tooLarge();
      const body = new Uint8Array(await request.arrayBuffer());
      if (body.length > MAX_CLIENT_FRAME) return tooLarge();
      return new Response(await stub.httpRpc(connect, body), { headers: { "Content-Type": "application/x-protobuf" } });
    }
  }

  return new Response("Not Found", { status: 404 });
}

export default {
  async fetch(request, env, _ctx): Promise<Response> {
    try {
      return await route(request, env);
    } catch (error) {
      if (error instanceof BadRequest) return errorReply(request, Error_Code.INVALID_ARGUMENT, error.message);
      throw error;
    }
  },
} satisfies ExportedHandler<Env>;

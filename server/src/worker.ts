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
  ServerInfo_DataPath,
  ServerInfoSchema,
  SpaceInfoSchema,
  SpaceListSchema,
  TokenRequestSchema,
} from "./gen/flarebox/v1/auth_pb";
import { Error_Code } from "./gen/flarebox/v1/rpc_pb";
import { BadRequest, errorReply, readMessage, reply } from "./http";
import { isUuidV7 } from "./ids";
import { CONNECT_HEADER, type Connect } from "./space-do";
import { PROTO_MAX, PROTO_MIN, SERVER_VERSION } from "./version";

export { AccountDO } from "./account-do";
export { SpaceDO } from "./space-do";

let serverKey: Promise<ServerKey> | undefined;

function account(env: Env) {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

/** The server key, read once per isolate from the Account DO so that tokens verify without a DO call. */
function key(env: Env): Promise<ServerKey> {
  serverKey ??= account(env).serverKey().catch((error: unknown) => {
    serverKey = undefined;
    throw error;
  });
  return serverKey;
}

async function authenticate(request: Request, env: Env): Promise<Identity | Response> {
  const token = bearerToken(request);
  const identity = token ? await verifyAccessToken(await key(env), token, Date.now()) : undefined;
  return identity ?? errorReply(request, Error_Code.UNAUTHENTICATED, "missing or invalid access token");
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
      : "A one-time claim code is in this Worker's logs. Run `flarebox server claim`, or read it with `wrangler tail`.\n";
    return new Response(text + "\nFlarebox is not affiliated with Cloudflare, Inc.\n", { headers: { "Content-Type": "text/plain; charset=utf-8" } });
  }

  if (method === "POST" && path === "/v1/claim") {
    const body = await readMessage(request, ClaimRequestSchema);
    const outcome = await account(env).claim(body);
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

  const spaceRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/(ws|rpc)$/.exec(path);
  if (path !== "/v1/pair/start" && path !== "/v1/spaces" && !spaceRoute) return new Response("Not Found", { status: 404 });
  const identity = await authenticate(request, env);
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
      const body = new Uint8Array(await request.arrayBuffer());
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

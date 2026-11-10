// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import pkg from "../package.json";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  ServerInfoSchema,
  SpaceInfoSchema,
} from "../src/gen/flarebox/v1/auth_pb";
import { ClientMessageSchema, type ServerMessage, ServerMessageSchema } from "../src/gen/flarebox/v1/messages_pb";
import { Close } from "../src/space-do";
import { PROTO_MAX, PROTO_MIN, SERVER_VERSION } from "../src/version";

const BASE = "https://flarebox.test";

afterEach(() => {
  vi.restoreAllMocks();
});

async function post<S extends DescMessage>(path: string, body: Uint8Array, schema: S, token?: string): Promise<MessageShape<S>> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (token) headers.Authorization = "Bearer " + token;
  const response = await exports.default.fetch(BASE + path, { method: "POST", headers, body });
  expect(response.ok, path).toBe(true);
  return fromBinary(schema, new Uint8Array(await response.arrayBuffer()));
}

let setup: Promise<{ owner: DeviceCredentials; spaceId: string }> | undefined;

/** Storage is shared by the tests in this file, so the deployment is claimed and its space created once. */
function deployment() {
  setup ??= (async () => {
    const log = vi.spyOn(console, "log");
    await exports.default.fetch(BASE + "/setup");
    const code = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Flarebox claim code"))!.split(": ")[1]!;
    const claim = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop", platform: "macos" }));
    const owner = await post("/v1/claim", claim, DeviceCredentialsSchema);
    const request = toBinary(CreateSpaceRequestSchema, create(CreateSpaceRequestSchema, { name: "Home" }));
    const space = await post("/v1/spaces", request, SpaceInfoSchema, owner.access!.token);
    return { owner, spaceId: space.spaceId };
  })();
  return setup;
}

/** Opens a socket, sends Hello with the given range and returns the first server message and, after a Goodbye, the close. */
async function hello(protoMin: number, protoMax: number): Promise<{ msg: ServerMessage["msg"]; close?: { code: number; reason: string } }> {
  const { owner, spaceId } = await deployment();
  const response = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/ws", {
    headers: { Upgrade: "websocket", Authorization: "Bearer " + owner.access!.token },
  });
  const ws = response.webSocket!;
  ws.binaryType = "arraybuffer";
  ws.accept();
  const first = new Promise<ServerMessage>((resolve) =>
    ws.addEventListener("message", (event) => resolve(fromBinary(ServerMessageSchema, new Uint8Array(event.data as ArrayBuffer))), { once: true }),
  );
  const closed = new Promise<{ code: number; reason: string }>((resolve) =>
    ws.addEventListener("close", (event) => resolve({ code: event.code, reason: event.reason })),
  );
  const message = create(ClientMessageSchema, {
    msg: { case: "hello", value: { clientName: "test", clientVersion: "0", protoMin, protoMax, maxFormat: 1 } },
  });
  ws.send(toBinary(ClientMessageSchema, message));
  const { msg } = await first;
  if (msg.case !== "goodbye") {
    ws.close(1000, "done");
    return { msg };
  }
  return { msg, close: await closed };
}

describe("protocol version negotiation (spec v0 section 9.1)", () => {
  it("reports the server version and a protocol range that keeps the previous minor version (N-1)", async () => {
    const response = await exports.default.fetch(BASE + "/v1/server");
    const info = fromBinary(ServerInfoSchema, new Uint8Array(await response.arrayBuffer()));
    expect(info.serverVersion).toBe(SERVER_VERSION);
    expect(SERVER_VERSION).toBe(pkg.version);
    expect([info.protoMin, info.protoMax]).toEqual([PROTO_MIN, PROTO_MAX]);
    expect(PROTO_MIN).toBeGreaterThanOrEqual(1);
    expect(PROTO_MIN).toBeLessThanOrEqual(Math.max(1, PROTO_MAX - 1));
  });

  it("picks the highest version both sides speak", async () => {
    const { msg } = await hello(1, PROTO_MAX + 5);
    expect(msg.case).toBe("welcome");
    if (msg.case !== "welcome") return;
    expect(msg.value.proto).toBe(PROTO_MAX);
    expect(msg.value.serverVersion).toBe(SERVER_VERSION);
  });

  it("serves a client that speaks only the previous minor version", async () => {
    const previous = Math.max(PROTO_MIN, PROTO_MAX - 1);
    const { msg } = await hello(previous, previous);
    expect(msg.case).toBe("welcome");
    if (msg.case === "welcome") expect(msg.value.proto).toBe(previous);
  });

  for (const [what, protoMin, protoMax] of [
    ["newer than", PROTO_MAX + 1, PROTO_MAX + 3],
    ["older than", 0, PROTO_MIN - 1],
  ] as const) {
    it(`refuses a client ${what} every version the server speaks, naming the server's range`, async () => {
      const { msg, close } = await hello(protoMin, protoMax);
      expect(msg.case).toBe("goodbye");
      if (msg.case !== "goodbye") return;
      expect(msg.value.code).toBe(Close.UNSUPPORTED_VERSION);
      expect(msg.value.reason).toContain(`1.${PROTO_MIN} to 1.${PROTO_MAX}`);
      expect(close).toEqual({ code: Close.UNSUPPORTED_VERSION, reason: msg.value.reason });
    });
  }
});

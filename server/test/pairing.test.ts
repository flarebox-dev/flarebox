// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import { CLAIM_FAILURE_WINDOW_MS, PAIRING_FAILURES_PER_WINDOW } from "../src/account/claim";
import { ClaimRequestSchema, type DeviceCredentials, DeviceCredentialsSchema, PairCompleteRequestSchema, PairStartResponseSchema } from "../src/gen/koidrive/v1/auth_pb";
import { ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";

const BASE = "https://koidrive.test";
const OWNER_ADDRESS = "198.51.100.7";
const STRANGER_ADDRESS = "203.0.113.9";

function inAccount<R>(callback: (sql: SqlStorage) => R): Promise<R> {
  return runInDurableObject(env.ACCOUNT.get(env.ACCOUNT.idFromName("account")), (_instance: AccountDO, state) => callback(state.storage.sql));
}

async function call<S extends DescMessage>(
  path: string,
  init: { body?: Uint8Array; token?: string; address?: string },
  schema: S,
): Promise<{ status: number; message?: MessageShape<S>; error?: string }> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  if (init.address) headers["CF-Connecting-IP"] = init.address;
  const response = await exports.default.fetch(BASE + path, { method: "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (response.ok) return { status: response.status, message: fromBinary(schema, bytes) };
  return { status: response.status, error: fromBinary(ErrorSchema, bytes).message };
}

let owner: DeviceCredentials | undefined;

async function getOwner(): Promise<DeviceCredentials> {
  if (owner) return owner;
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  log.mockRestore();
  const body = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code: line!.split(": ")[1]!, userName: "Owner", deviceName: "Laptop", platform: "macos" }));
  owner = (await call("/v1/claim", { body, address: OWNER_ADDRESS }, DeviceCredentialsSchema)).message!;
  return owner;
}

async function startPairing(): Promise<string> {
  const started = await call("/v1/pair/start", { token: (await getOwner()).access!.token }, PairStartResponseSchema);
  return started.message!.code;
}

function completePairing(code: string, address: string) {
  const body = toBinary(PairCompleteRequestSchema, create(PairCompleteRequestSchema, { code, deviceName: "Phone", platform: "ios" }));
  return call("/v1/pair/complete", { body, address }, DeviceCredentialsSchema);
}

async function throttle(address: string): Promise<void> {
  for (let i = 0; i < PAIRING_FAILURES_PER_WINDOW; i++) {
    expect(await completePairing(`WRNG-${String(i).padStart(4, "0")}`, address)).toMatchObject({ status: 403, error: "invalid or expired pairing code" });
  }
  expect((await completePairing("WRNG-LAST", address)).status).toBe(429);
}

describe("pairing failures per client address", () => {
  it("redeems the correct code from another address and from the throttled one", async () => {
    const first = await startPairing();
    const second = await startPairing();
    await throttle(STRANGER_ADDRESS);
    expect((await completePairing(first, OWNER_ADDRESS)).status).toBe(200);
    expect((await completePairing(second, STRANGER_ADDRESS)).status).toBe(200);
  });

  it("does not throttle one address for another address's wrong codes", async () => {
    await throttle("192.0.2.10");
    expect(await completePairing("WRNG-OTHR", OWNER_ADDRESS)).toMatchObject({ status: 403, error: "invalid or expired pairing code" });
  });

  it("allows an address wrong codes again once its window has passed", async () => {
    await inAccount((sql) => sql.exec("UPDATE claim_failures SET window_start_ms = window_start_ms - ?", CLAIM_FAILURE_WINDOW_MS));
    await throttle(STRANGER_ADDRESS);
  });
});

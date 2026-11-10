// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import type { AccountDO } from "../src/account-do";
import { type DeviceCredentials, ServerInfoSchema, TokenRequestSchema } from "../src/gen/flarebox/v1/auth_pb";
import { Error_Code } from "../src/gen/flarebox/v1/rpc_pb";

const ADDRESS = "198.51.100.7";
const LOST_RESPONSES = 3;

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

/** A recovery-key claim with a fresh key, as the CLI makes after setting a new one-time secret. */
async function claim(setupAttempt: string, deviceName = "Laptop"): Promise<DeviceCredentials> {
  const key = crypto.randomUUID() + crypto.randomUUID();
  const outcome = await account().claim({ code: key, userName: "Owner", deviceName, platform: "macos", setupAttempt }, ADDRESS, key);
  if (!outcome.ok) throw new Error(outcome.message);
  return outcome.value;
}

async function refreshes(device: DeviceCredentials): Promise<boolean> {
  const body = toBinary(TokenRequestSchema, create(TokenRequestSchema, { deviceId: device.deviceId, refreshCredential: device.refreshCredential }));
  const response = await exports.default.fetch("https://flarebox.test/v1/token", { method: "POST", headers: { "Content-Type": "application/x-protobuf" }, body });
  await response.arrayBuffer();
  return response.ok;
}

function activeDevices(): Promise<string[]> {
  return runInDurableObject(account(), (_instance: AccountDO, state) =>
    state.storage.sql
      .exec("SELECT device_id FROM devices WHERE revoked_at_ms IS NULL ORDER BY device_id")
      .toArray()
      .map((row) => String(row.device_id)),
  );
}

/** Storage is shared by the tests in this file, and they run in order on one deployment. */
describe("recovery-key claims of one setup attempt", () => {
  let owner: DeviceCredentials;
  let lost: DeviceCredentials[] = [];

  it(`ends with one device after ${LOST_RESPONSES} lost responses, starting on an unclaimed deployment`, async () => {
    const attempt = "attempt-a";
    for (let i = 0; i < LOST_RESPONSES; i++) lost.push(await claim(attempt));
    owner = await claim(attempt);
    expect(new Set([...lost, owner].map((device) => device.userId)).size).toBe(1);
    expect(await activeDevices()).toEqual([owner.deviceId]);
    for (const device of lost) expect(await refreshes(device)).toBe(false);
    expect(await refreshes(owner)).toBe(true);
  });

  it("keeps a device of the attempt that has refreshed a token", async () => {
    const used = await claim("attempt-b", "Desktop");
    expect(await refreshes(used)).toBe(true);
    const next = await claim("attempt-b", "Desktop");
    expect(await activeDevices()).toEqual([owner.deviceId, used.deviceId, next.deviceId].sort());
  });

  it("leaves other attempts alone, and a claim without an attempt revokes nothing", async () => {
    const before = await activeDevices();
    const unused = await claim("attempt-c", "Phone");
    const plain = await claim("", "Phone");
    expect(await activeDevices()).toEqual([...before, unused.deviceId, plain.deviceId].sort());
  });

  it("reports in ServerInfo that the Account DO handles setup attempts", async () => {
    const response = await exports.default.fetch("https://flarebox.test/v1/server");
    expect(fromBinary(ServerInfoSchema, new Uint8Array(await response.arrayBuffer())).setupAttempts).toBe(true);
  });

  it("refuses an attempt id longer than 64 characters and adds no device", async () => {
    const before = await activeDevices();
    const key = crypto.randomUUID() + crypto.randomUUID();
    const outcome = await account().claim({ code: key, userName: "Owner", deviceName: "Laptop", platform: "macos", setupAttempt: "x".repeat(65) }, ADDRESS, key);
    expect(outcome).toMatchObject({ ok: false, code: Error_Code.INVALID_ARGUMENT });
    expect(await activeDevices()).toEqual(before);
  });
});

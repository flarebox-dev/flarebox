// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import { type DeviceCredentials, ServerInfoSchema, TokenRequestSchema } from "../src/gen/koidrive/v1/auth_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";

const ADDRESS = "198.51.100.7";
const LOST_RESPONSES = 3;

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

async function claim(setupAttempt: string, deviceName = "Laptop"): Promise<DeviceCredentials> {
  const key = crypto.randomUUID() + crypto.randomUUID();
  const outcome = await account().claim({ code: key, userName: "Owner", deviceName, platform: "macos", setupAttempt }, ADDRESS, key);
  if (!outcome.ok) throw new Error(outcome.message);
  return outcome.value;
}

async function refreshes(device: DeviceCredentials): Promise<boolean> {
  const body = toBinary(TokenRequestSchema, create(TokenRequestSchema, { deviceId: device.deviceId, refreshCredential: device.refreshCredential }));
  const response = await exports.default.fetch("https://koidrive.test/v1/token", { method: "POST", headers: { "Content-Type": "application/x-protobuf" }, body });
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
    const response = await exports.default.fetch("https://koidrive.test/v1/server");
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

describe("cleanup after a claim", () => {
  it("keeps a selected device that refreshes a token while an earlier revocation is still delivering", async () => {
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("refresh-during-cleanup"));
    const claimOn = async (setupAttempt: string) => {
      const key = crypto.randomUUID() + crypto.randomUUID();
      const outcome = await stub.claim({ code: key, userName: "Owner", deviceName: "Laptop", platform: "macos", setupAttempt }, ADDRESS, key);
      if (!outcome.ok) throw new Error(outcome.message);
      return outcome.value;
    };
    const first = await claimOn("first");
    const second = await claimOn("second");
    await stub.createSpace({ userId: first.userId, name: "Space" });
    await runInDurableObject(stub, async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET setup_attempt = 'interrupted'");
      const revoke = instance.revokeDevice.bind(instance);
      const revoked: string[] = [];
      const spy = vi.spyOn(instance, "revokeDevice").mockImplementation(async (deviceId) => {
        revoked.push(deviceId);
        await revoke(deviceId);
        const other = deviceId === first.deviceId ? second : first;
        expect((await instance.refreshAccess({ deviceId: other.deviceId, refreshCredential: other.refreshCredential })).ok).toBe(true);
      });
      try {
        const key = crypto.randomUUID() + crypto.randomUUID();
        expect((await instance.claim({ code: key, userName: "Owner", deviceName: "Laptop", platform: "macos", setupAttempt: "interrupted" }, ADDRESS, key)).ok).toBe(true);
        expect(revoked).toHaveLength(1);
        const refreshed = revoked[0] === first.deviceId ? second : first;
        expect(await instance.isDeviceActive(refreshed.deviceId)).toBe(true);
      } finally {
        spy.mockRestore();
      }
    });
  });

  it("returns the claim's credentials when a revocation fails, and still revokes the other selected devices", async () => {
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("revocation-fails"));
    const lostDevices: string[] = [];
    for (const setupAttempt of ["first", "second"]) {
      const key = crypto.randomUUID() + crypto.randomUUID();
      const outcome = await stub.claim({ code: key, userName: "Owner", deviceName: "Laptop", platform: "macos", setupAttempt }, ADDRESS, key);
      if (!outcome.ok) throw new Error(outcome.message);
      lostDevices.push(outcome.value.deviceId);
    }
    await runInDurableObject(stub, async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET setup_attempt = 'failing'");
      const spy = vi.spyOn(instance, "revokeDevice").mockRejectedValueOnce(new Error("injected revocation failure"));
      try {
        const key = crypto.randomUUID() + crypto.randomUUID();
        const outcome = await instance.claim({ code: key, userName: "Owner", deviceName: "Laptop", platform: "macos", setupAttempt: "failing" }, ADDRESS, key);
        if (!outcome.ok) throw new Error(outcome.message);
        expect(spy).toHaveBeenCalledTimes(2);
        expect((await instance.refreshAccess({ deviceId: outcome.value.deviceId, refreshCredential: outcome.value.refreshCredential })).ok).toBe(true);
        const active = await Promise.all(lostDevices.map((deviceId) => instance.isDeviceActive(deviceId)));
        expect(active.filter(Boolean)).toHaveLength(1);
      } finally {
        spy.mockRestore();
      }
    });
  });
  it("refuses a claim that a later claim of its attempt superseded instead of returning revoked credentials", async () => {
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("superseded"));
    const claimOn = (instance: AccountDO | DurableObjectStub<AccountDO>) => {
      const key = crypto.randomUUID() + crypto.randomUUID();
      return instance.claim({ code: key, userName: "Owner", deviceName: "Laptop", platform: "macos", setupAttempt: "overlap" }, ADDRESS, key);
    };
    const initial = await claimOn(stub);
    if (!initial.ok) throw new Error(initial.message);
    await stub.createSpace({ userId: initial.value.userId, name: "Space" });
    await runInDurableObject(stub, async (instance: AccountDO, state) => {
      let release = () => {};
      let entered = () => {};
      const hold = new Promise<void>((resolve) => (release = resolve));
      const paused = new Promise<void>((resolve) => (entered = resolve));
      const revoke = instance.revokeDevice.bind(instance);
      const spy = vi.spyOn(instance, "revokeDevice").mockImplementation(async (deviceId) => {
        await revoke(deviceId);
        if (deviceId === initial.value.deviceId) {
          entered();
          await hold;
        }
      });
      try {
        const earlier = claimOn(instance);
        await paused;
        const later = await claimOn(instance);
        if (!later.ok) throw new Error(later.message);
        release();
        expect(await earlier).toEqual({
          ok: false,
          code: Error_Code.FORBIDDEN,
          message: "a later claim of this setup attempt replaced this one; set a new recovery key and claim again",
        });
        expect((await instance.refreshAccess({ deviceId: later.value.deviceId, refreshCredential: later.value.refreshCredential })).ok).toBe(true);
        const active = state.storage.sql.exec("SELECT device_id FROM devices WHERE revoked_at_ms IS NULL").toArray();
        expect(active.map((row) => String(row.device_id))).toEqual([later.value.deviceId]);
      } finally {
        release();
        spy.mockRestore();
      }
    });
  });
});

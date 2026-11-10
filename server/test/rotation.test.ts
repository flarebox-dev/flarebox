// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import type { AccountDO } from "../src/account-do";
import { AccessTokenSchema, type DeviceCredentials, TokenRequestSchema } from "../src/gen/flarebox/v1/auth_pb";
import { Error_Code } from "../src/gen/flarebox/v1/rpc_pb";
import { base64url } from "../src/crypto";

// RFC 0011 section 1: refresh credential rotation on POST /v1/token.

const MINUTE = 60_000;

/** A credential as a device generates one: 32 random bytes, base64url without padding. */
function newCredential(): string {
  return base64url(crypto.getRandomValues(new Uint8Array(32)));
}

type Row = { refresh_hash: ArrayBuffer; last_seen_ms: number | null; rotation_clock_ms: number };

/** Runs `body` in a fresh Account DO with one claimed device and a clock the test sets. */
function withDevice(
  name: string,
  body: (instance: AccountDO, device: DeviceCredentials, row: () => Row, setClock: (ms: number) => void, sql: SqlStorage) => Promise<void>,
) {
  return runInDurableObject(env.ACCOUNT.get(env.ACCOUNT.idFromName(name)), async (instance: AccountDO, state) => {
    let now = 1_000_000;
    instance.clock = () => now;
    const code = (await instance.issueClaimCode())!;
    const claimed = await instance.claim({ code, userName: "Owner", deviceName: "Laptop", platform: "macos" });
    if (!claimed.ok) throw new Error(claimed.message);
    const row = () =>
      state.storage.sql
        .exec("SELECT refresh_hash, last_seen_ms, rotation_clock_ms FROM devices WHERE device_id = ?", claimed.value.deviceId)
        .one() as unknown as Row;
    await body(
      instance,
      claimed.value,
      row,
      (ms) => {
        now = ms;
      },
      state.storage.sql,
    );
  });
}

function hex(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

describe("refresh credential rotation (RFC 0011 section 1)", () => {
  it("answers a request that sets rotation with server_time_ms and nothing else, and a request without it as before", async () => {
    await withDevice("rotation-time", async (instance, device, row, setClock) => {
      const before = hex(row().refresh_hash);
      setClock(5_000_000);
      const answer = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true });
      if (!answer.ok) throw new Error(answer.message);
      expect(answer.value.serverTimeMs).toBe(5_000_000n);
      expect(answer.value.rotated).toBe(false);
      expect(hex(row().refresh_hash)).toBe(before);

      setClock(6_000_000);
      const plain = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential });
      if (!plain.ok) throw new Error(plain.message);
      expect(plain.value.serverTimeMs).toBe(0n);
      expect(plain.value.rotated).toBe(false);
      expect(row()).toMatchObject({ last_seen_ms: 6_000_000, rotation_clock_ms: 5_000_000 });
    });
  });

  it("stores a proposal that arrives by its deadline, after which the old credential is refused and the device stays active", async () => {
    await withDevice("rotation-stored", async (instance, device) => {
      const next = newCredential();
      const answer = await instance.refreshAccess({
        deviceId: device.deviceId,
        refreshCredential: device.refreshCredential,
        rotation: true,
        nextRefreshCredential: next,
        rotateBeforeMs: 1_000_000n + BigInt(MINUTE),
      });
      if (!answer.ok) throw new Error(answer.message);
      expect(answer.value).toMatchObject({ serverTimeMs: 1_000_000n, rotated: true });
      expect(await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential })).toMatchObject({
        ok: false,
        code: Error_Code.UNAUTHENTICATED,
      });
      expect(await instance.isDeviceActive(device.deviceId)).toBe(true);
      expect(await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: next })).toMatchObject({ ok: true });
    });
  });

  it("answers a proposal after its deadline without rotated and keeps the credential, while the clocks advance", async () => {
    await withDevice("rotation-late", async (instance, device, row, setClock) => {
      const before = hex(row().refresh_hash);
      setClock(2_000_000);
      const answer = await instance.refreshAccess({
        deviceId: device.deviceId,
        refreshCredential: device.refreshCredential,
        rotation: true,
        nextRefreshCredential: newCredential(),
        rotateBeforeMs: 2_000_000n - 1n,
      });
      if (!answer.ok) throw new Error(answer.message);
      expect(answer.value).toMatchObject({ serverTimeMs: 2_000_000n, rotated: false });
      expect(hex(row().refresh_hash)).toBe(before);
      expect(row()).toMatchObject({ last_seen_ms: 2_000_000, rotation_clock_ms: 2_000_000 });
      expect(await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential })).toMatchObject({ ok: true });
    });
  });

  it("refuses a malformed or incomplete proposal with INVALID_ARGUMENT and changes nothing", async () => {
    await withDevice("rotation-malformed", async (instance, device, row) => {
      const before = row();
      const deadline = 1_000_000n + BigInt(MINUTE);
      const proposals = [
        { rotation: true, nextRefreshCredential: "too-short", rotateBeforeMs: deadline },
        { rotation: true, nextRefreshCredential: newCredential().slice(0, 42) + "+", rotateBeforeMs: deadline },
        { rotation: true, nextRefreshCredential: newCredential() + "A", rotateBeforeMs: deadline },
        { rotation: false, nextRefreshCredential: newCredential(), rotateBeforeMs: deadline },
        { rotation: true, nextRefreshCredential: newCredential() },
      ];
      for (const [index, proposal] of proposals.entries()) {
        const answer = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, ...proposal });
        expect(answer, `proposal ${index}`).toMatchObject({ ok: false, code: Error_Code.INVALID_ARGUMENT });
      }
      expect(hex(row().refresh_hash)).toBe(hex(before.refresh_hash));
      expect(row()).toMatchObject({ last_seen_ms: before.last_seen_ms, rotation_clock_ms: before.rotation_clock_ms });
    });
  });

  it("refuses the proposal of a revoked or expired principal and changes nothing", async () => {
    for (const ending of ["revoked", "expired"] as const) {
      await withDevice(`rotation-${ending}`, async (instance, device, row, _setClock, sql) => {
        if (ending === "revoked") {
          expect(await instance.revokeUserDevice({ userId: device.userId, callerId: device.deviceId, deviceId: device.deviceId, confirmLastDevice: true })).toMatchObject({ ok: true });
        } else {
          sql.exec("UPDATE devices SET expires_at_ms = 1 WHERE device_id = ?", device.deviceId);
        }
        const before = row();
        const next = newCredential();
        const answer = await instance.refreshAccess({
          deviceId: device.deviceId,
          refreshCredential: device.refreshCredential,
          rotation: true,
          nextRefreshCredential: next,
          rotateBeforeMs: 1_000_000n + BigInt(MINUTE),
        });
        expect(answer, ending).toMatchObject({ ok: false, code: Error_Code.UNAUTHENTICATED });
        expect(hex(row().refresh_hash)).toBe(hex(before.refresh_hash));
        expect(row().rotation_clock_ms).toBe(before.rotation_clock_ms);
      });
    }
  });

  it("of two proposals from the same credential, stores only the first, and only its credential works", async () => {
    await withDevice("rotation-two", async (instance, device) => {
      const proposals = [newCredential(), newCredential()];
      const answers = await Promise.all(
        proposals.map((next) =>
          instance.refreshAccess({
            deviceId: device.deviceId,
            refreshCredential: device.refreshCredential,
            rotation: true,
            nextRefreshCredential: next,
            rotateBeforeMs: 1_000_000n + BigInt(MINUTE),
          }),
        ),
      );
      const stored = answers.map((answer) => answer.ok && answer.value.rotated);
      expect(stored.filter(Boolean)).toHaveLength(1);
      expect(answers.find((answer) => !answer.ok)).toMatchObject({ code: Error_Code.UNAUTHENTICATED });
      for (const [index, next] of proposals.entries()) {
        expect((await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: next })).ok).toBe(stored[index]);
      }
    });
  });

  it("never answers an earlier time after the clock moves back, so a delayed proposal is stored only if no later time was answered", async () => {
    // The client saw a time later than its deadline and cleared its staged credential; the delayed proposal then arrives
    // while the clock reads a time before the deadline.
    await withDevice("rotation-clock-cleared", async (instance, device, row, setClock) => {
      const start = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true });
      if (!start.ok) throw new Error(start.message);
      const deadline = start.value.serverTimeMs + BigInt(MINUTE);
      setClock(Number(deadline) + 1);
      const after = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true });
      if (!after.ok) throw new Error(after.message);
      expect(after.value.serverTimeMs).toBeGreaterThan(deadline);

      setClock(Number(start.value.serverTimeMs));
      const before = hex(row().refresh_hash);
      const delayed = await instance.refreshAccess({
        deviceId: device.deviceId,
        refreshCredential: device.refreshCredential,
        rotation: true,
        nextRefreshCredential: newCredential(),
        rotateBeforeMs: deadline,
      });
      if (!delayed.ok) throw new Error(delayed.message);
      expect(delayed.value).toMatchObject({ serverTimeMs: after.value.serverTimeMs, rotated: false });
      expect(hex(row().refresh_hash)).toBe(before);
    });

    // The proposal was dropped before it reached the server and the clock moved back: the answers carry the rotation
    // clock, so the same staged credential proposed again is stored, and a copy of the first proposal arriving later
    // presents a credential that is no longer stored.
    await withDevice("rotation-clock-retried", async (instance, device, _row, setClock) => {
      const start = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true });
      if (!start.ok) throw new Error(start.message);
      const staged = { nextRefreshCredential: newCredential(), rotateBeforeMs: start.value.serverTimeMs + BigInt(MINUTE) };
      setClock(Number(start.value.serverTimeMs) - 10 * MINUTE);
      const retried = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true, ...staged });
      if (!retried.ok) throw new Error(retried.message);
      expect(retried.value).toMatchObject({ serverTimeMs: start.value.serverTimeMs, rotated: true });
      const copy = await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true, ...staged });
      expect(copy).toMatchObject({ ok: false, code: Error_Code.UNAUTHENTICATED });
      expect(await instance.refreshAccess({ deviceId: device.deviceId, refreshCredential: staged.nextRefreshCredential })).toMatchObject({ ok: true });
    });
  });

  it("carries the new fields over binary and JSON only for a request that sets rotation", async () => {
    const device = await runInDurableObject(env.ACCOUNT.get(env.ACCOUNT.idFromName("account")), async (instance: AccountDO) => {
      const code = (await instance.issueClaimCode())!;
      const claimed = await instance.claim({ code, userName: "Owner", deviceName: "Laptop", platform: "macos" });
      if (!claimed.ok) throw new Error(claimed.message);
      return claimed.value;
    });
    const post = (body: BodyInit, contentType: string) =>
      exports.default.fetch("https://flarebox.test/v1/token", { method: "POST", headers: { "Content-Type": contentType, Accept: contentType }, body });

    const plainJson = await post(JSON.stringify({ deviceId: device.deviceId, refreshCredential: device.refreshCredential }), "application/json");
    expect(plainJson.status).toBe(200);
    expect(Object.keys((await plainJson.json()) as object).sort()).toEqual(["expiresAtMs", "token"]);

    const rotationJson = await post(JSON.stringify({ deviceId: device.deviceId, refreshCredential: device.refreshCredential, rotation: true }), "application/json");
    expect(rotationJson.status).toBe(200);
    expect(Object.keys((await rotationJson.json()) as object).sort()).toEqual(["expiresAtMs", "serverTimeMs", "token"]);

    const next = newCredential();
    const request = create(TokenRequestSchema, {
      deviceId: device.deviceId,
      refreshCredential: device.refreshCredential,
      rotation: true,
      nextRefreshCredential: next,
      rotateBeforeMs: BigInt(Date.now() + MINUTE),
    });
    const binary = await post(toBinary(TokenRequestSchema, request), "application/x-protobuf");
    expect(binary.status).toBe(200);
    const answer = fromBinary(AccessTokenSchema, new Uint8Array(await binary.arrayBuffer()));
    expect(answer.rotated).toBe(true);
    expect(answer.serverTimeMs).toBeGreaterThan(0n);
  });
});

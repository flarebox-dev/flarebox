// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import { AccessTokenSchema, ClaimRequestSchema, DeviceCredentialsSchema, ServerInfoSchema, TokenRequestSchema } from "../src/gen/koidrive/v1/auth_pb";
import { Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";

const BASE = "https://koidrive.test";
const ADDRESS = "198.51.100.7";
const RECOVERY_KEY = "test-recovery-key-for-local-workerd-only";
const request = { userName: "Owner", deviceName: "Laptop", platform: "macos" };

afterEach(() => {
  vi.restoreAllMocks();
});

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

function count(query: string): Promise<number> {
  return runInDurableObject(account(), (_instance: AccountDO, state) => Number(state.storage.sql.exec(query).one().n));
}

async function call<S extends DescMessage>(path: string, body: Uint8Array, schema: S): Promise<{ status: number; message?: MessageShape<S>; error?: string }> {
  const headers = { "Content-Type": "application/x-protobuf", "CF-Connecting-IP": ADDRESS };
  const response = await exports.default.fetch(BASE + path, { method: "POST", headers, body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (response.ok) return { status: response.status, message: fromBinary(schema, bytes) };
  return { status: response.status, error: fromBinary(ErrorSchema, bytes).message };
}

function claim(code: string) {
  return call("/v1/claim", toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, ...request })), DeviceCredentialsSchema);
}

async function claimed(): Promise<boolean> {
  const response = await exports.default.fetch(BASE + "/v1/server");
  return fromBinary(ServerInfoSchema, new Uint8Array(await response.arrayBuffer())).claimed;
}

async function setupCode(): Promise<string | undefined> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup").then((response) => response.text());
  const line = log.mock.calls.map((args) => String(args[0])).find((entry) => entry.startsWith("Koidrive claim code"));
  log.mockRestore();
  return line?.split(": ")[1];
}

describe("claim with the recovery key on a deployment without an owner", () => {
  it("ignores a key that is too short, and does not count it as a wrong code", async () => {
    const outcome = await account().claim({ ...request, code: "short-key" }, ADDRESS, "short-key");
    expect(outcome).toMatchObject({ ok: false, code: Error_Code.FORBIDDEN, message: "no valid claim code; open /setup to issue one" });
    expect(await count("SELECT COUNT(*) AS n FROM claim_failures")).toBe(0);
    expect(await claimed()).toBe(false);
  });

  it("leaves the key unused and the deployment unclaimed when storing the device fails", async () => {
    await runInDurableObject(account(), (_instance: AccountDO, state) =>
      state.storage.sql.exec("CREATE TRIGGER fail_device_insert BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT, 'injected device insert failure'); END"),
    );
    const failed = await account()
      .claim({ ...request, code: RECOVERY_KEY }, ADDRESS, RECOVERY_KEY)
      .then(
        () => undefined,
        (error: unknown) => String(error),
      );
    await runInDurableObject(account(), (_instance: AccountDO, state) => state.storage.sql.exec("DROP TRIGGER fail_device_insert"));
    expect(failed).toContain("injected device insert failure");
    expect(await count("SELECT COUNT(*) AS n FROM users")).toBe(0);
    expect(await count("SELECT COUNT(*) AS n FROM used_recovery_keys")).toBe(0);
    expect(await claimed()).toBe(false);
  });

  it("makes the sender the owner, once, and voids the code /setup issued", async () => {
    const code = await setupCode();
    expect(code).toBeDefined();

    const owner = await claim(RECOVERY_KEY);
    expect(owner.status).toBe(200);
    expect(await claimed()).toBe(true);
    expect(await count("SELECT COUNT(*) AS n FROM users")).toBe(1);
    expect(await count("SELECT COUNT(*) AS n FROM claim_codes")).toBe(0);
    const refresh = toBinary(TokenRequestSchema, create(TokenRequestSchema, { deviceId: owner.message!.deviceId, refreshCredential: owner.message!.refreshCredential }));
    expect((await call("/v1/token", refresh, AccessTokenSchema)).status).toBe(200);

    expect(await claim(code!)).toMatchObject({ status: 403, error: "this deployment already has an owner" });
    expect(await claim(RECOVERY_KEY)).toMatchObject({ status: 403, error: "this recovery key was already used; set a new one" });
    expect(await count("SELECT COUNT(*) AS n FROM users")).toBe(1);
  });
});

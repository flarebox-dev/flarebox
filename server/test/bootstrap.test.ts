// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import { CLAIM_FAILURE_WINDOW_MS, CLAIM_FAILURES_PER_WINDOW } from "../src/account/claim";
import { sha256 } from "../src/crypto";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  ServerInfoSchema,
  TokenRequestSchema,
} from "../src/gen/flarebox/v1/auth_pb";
import { Error_Code, ErrorSchema } from "../src/gen/flarebox/v1/rpc_pb";

const BASE = "https://flarebox.test";
const OWNER_ADDRESS = "198.51.100.7";
const STRANGER_ADDRESS = "203.0.113.9";
/** The key vitest.config.ts gives the Worker as FLAREBOX_RECOVERY_KEY. */
const RECOVERY_KEY = "test-recovery-key-for-local-workerd-only";
const WRONG_CODE = "AAAA-BBBB-CCCC";

afterEach(() => {
  vi.restoreAllMocks();
});

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

function inAccount<R>(callback: (sql: SqlStorage) => R): Promise<R> {
  return runInDurableObject(account(), (_instance: AccountDO, state) => callback(state.storage.sql));
}

async function call<S extends DescMessage>(
  path: string,
  body: Uint8Array,
  schema: S,
  address: string | undefined,
): Promise<{ status: number; message?: MessageShape<S>; error?: string }> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (address) headers["CF-Connecting-IP"] = address;
  const response = await exports.default.fetch(BASE + path, { method: "POST", headers, body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (response.ok) return { status: response.status, message: fromBinary(schema, bytes) };
  return { status: response.status, error: fromBinary(ErrorSchema, bytes).message };
}

function claim(code: string, address: string | undefined, deviceName = "Laptop", userName = "Owner") {
  const body = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, userName, deviceName, platform: "macos" }));
  return call("/v1/claim", body, DeviceCredentialsSchema, address);
}

/** Sends wrong codes from one address until it is throttled, and checks the responses on the way. */
async function throttle(address: string | undefined, wrongCode: string): Promise<void> {
  const left = CLAIM_FAILURES_PER_WINDOW - (await failuresOf(address ?? "(no address)"));
  for (let i = 0; i < left; i++) expect((await claim(wrongCode + i, address)).status).toBe(403);
  expect((await claim(wrongCode, address)).status).toBe(429);
}

function failuresOf(client: string): Promise<number> {
  return inAccount((sql) => Number(sql.exec("SELECT COALESCE(SUM(failures), 0) AS n FROM claim_failures WHERE client = ?", client).one().n));
}

/** Opens /setup and returns the claim code it wrote to the log, if it wrote one. */
async function setup(): Promise<{ text: string; code: string | undefined }> {
  const log = vi.spyOn(console, "log");
  const text = await exports.default.fetch(BASE + "/setup").then((response) => response.text());
  const line = log.mock.calls.map((args) => String(args[0])).find((entry) => entry.startsWith("Flarebox claim code"));
  log.mockRestore();
  return { text, code: line?.split(": ")[1] };
}

async function claimed(): Promise<boolean> {
  const response = await exports.default.fetch(BASE + "/v1/server");
  return fromBinary(ServerInfoSchema, new Uint8Array(await response.arrayBuffer())).claimed;
}

let owner: DeviceCredentials | undefined;
let firstCode = "";

/** Storage is shared by the tests in this file, and they run in order: unclaimed, claimed, then recovery. */
describe("first owner claim", () => {
  it("refuses a claim before /setup has issued a code", async () => {
    expect(await claimed()).toBe(false);
    expect(await claim(WRONG_CODE, OWNER_ADDRESS)).toMatchObject({ status: 403, error: "no valid claim code; open /setup to issue one" });
  });

  it("issues one code at a time, even to concurrent /setup requests", async () => {
    const opened = await Promise.all([setup(), setup(), setup()]);
    const codes = opened.flatMap(({ code }) => (code ? [code] : []));
    expect(codes).toHaveLength(1);
    firstCode = codes[0]!;
    expect(firstCode).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{4}$/);
    expect(opened[0]!.text).toContain("Observability");
    expect((await setup()).code).toBeUndefined();
  });

  it("throttles wrong codes per address, with requests that lack an address in a bucket of their own", async () => {
    await throttle(STRANGER_ADDRESS, "WRNG-CODE-");
    expect(await claim(WRONG_CODE, OWNER_ADDRESS)).toMatchObject({ status: 403, error: "wrong claim code" });
    await throttle(undefined, "NOAD-DRSS-");
    expect(await claim(WRONG_CODE, "192.0.2.10")).toMatchObject({ status: 403, error: "wrong claim code" });
    expect(await failuresOf("")).toBe(0);
    expect(await claimed()).toBe(false);
    expect((await setup()).code).toBeUndefined();
  });

  it("forgets wrong codes once their window has passed", async () => {
    await inAccount((sql) => sql.exec("UPDATE claim_failures SET window_start_ms = window_start_ms - ?", CLAIM_FAILURE_WINDOW_MS));
    expect(await claim(WRONG_CODE, STRANGER_ADDRESS)).toMatchObject({ status: 403, error: "wrong claim code" });
    expect(await failuresOf(STRANGER_ADDRESS)).toBe(1);
  });

  it("redeems the correct code even from a throttled address, and makes only the first of two concurrent claims the owner", async () => {
    await throttle(STRANGER_ADDRESS, "WRNG-CODE-");
    const results = await Promise.all([claim(firstCode, STRANGER_ADDRESS, "Laptop"), claim(firstCode, "192.0.2.44", "Phone")]);
    const winners = results.filter((result) => result.status === 200);
    expect(winners).toHaveLength(1);
    expect(results.find((result) => result.status !== 200)).toMatchObject({ status: 403 });
    owner = winners[0]!.message!;
    expect(owner.userName).toBe("Owner");
    expect(await claimed()).toBe(true);
  });

  it("refuses every later claim and issues no new code", async () => {
    const opened = await setup();
    expect(opened.code).toBeUndefined();
    expect(opened.text).toContain("already has an owner");
    for (const code of [firstCode, WRONG_CODE]) {
      expect(await claim(code, OWNER_ADDRESS)).toMatchObject({ status: 403, error: "this deployment already has an owner" });
    }
  });
});

describe("owner recovery", () => {
  const request = { userName: "Someone else", deviceName: "New laptop", platform: "linux" };

  it("is refused when the Worker has no recovery key, or one that is too short", async () => {
    for (const key of [undefined, "short-key"]) {
      const outcome = await account().claim({ ...request, code: key ?? RECOVERY_KEY }, OWNER_ADDRESS, key);
      expect(outcome).toMatchObject({ ok: false, code: Error_Code.FORBIDDEN, message: "this deployment already has an owner" });
    }
  });

  it("answers wrong recovery claims the same whether the Worker has a recovery key, a short one or none", async () => {
    async function answers(address: string, key: string | undefined) {
      const seen: string[] = [];
      for (let i = 0; i <= CLAIM_FAILURES_PER_WINDOW; i++) {
        const outcome = await account().claim({ ...request, code: `not-the-recovery-key-${i}` }, address, key);
        seen.push(outcome.ok ? "ok" : `${Error_Code[outcome.code]}: ${outcome.message}`);
      }
      return seen;
    }
    const withKey = await answers("198.51.100.31", RECOVERY_KEY);
    expect(withKey.at(-1)).toMatch(/^RETRY_LATER/);
    expect(await answers("198.51.100.32", undefined)).toEqual(withKey);
    expect(await answers("198.51.100.33", "short-key")).toEqual(withKey);
  });

  it("throttles wrong keys, but the right key still adds a device for the existing owner, once", async () => {
    await throttle(STRANGER_ADDRESS, "not-the-recovery-key-");
    const recovered = await claim(RECOVERY_KEY, STRANGER_ADDRESS, "New laptop", "Someone else");
    expect(recovered.status).toBe(200);
    expect(recovered.message!.userId).toBe(owner!.userId);
    expect(recovered.message!.userName).toBe("Owner");
    expect(recovered.message!.deviceId).not.toBe(owner!.deviceId);
    const body = toBinary(
      TokenRequestSchema,
      create(TokenRequestSchema, { deviceId: recovered.message!.deviceId, refreshCredential: recovered.message!.refreshCredential }),
    );
    expect((await call("/v1/token", body, AccessTokenSchema, OWNER_ADDRESS)).status).toBe(200);

    expect(await claim(RECOVERY_KEY, OWNER_ADDRESS)).toMatchObject({ status: 403, error: "this recovery key was already used; set a new one" });
  });

  it("leaves the key unused when storing the device fails", async () => {
    const key = "a-second-recovery-key-set-by-the-owner";
    const keyHash = (await sha256(new TextEncoder().encode(key))).slice().buffer;
    const devices = () => inAccount((sql) => Number(sql.exec("SELECT COUNT(*) AS n FROM devices").one().n));
    const before = await devices();
    await inAccount((sql) =>
      sql.exec("CREATE TRIGGER fail_device_insert BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT, 'injected device insert failure'); END"),
    );
    const failed = await account()
      .claim({ ...request, code: key }, OWNER_ADDRESS, key)
      .then(
        () => undefined,
        (error: unknown) => String(error),
      );
    expect(failed).toContain("injected device insert failure");
    await inAccount((sql) => sql.exec("DROP TRIGGER fail_device_insert"));
    expect(await devices()).toBe(before);
    expect(await inAccount((sql) => sql.exec("SELECT COUNT(*) AS n FROM used_recovery_keys WHERE key_hash = ?", keyHash).one().n)).toBe(0);

    const outcome = await account().claim({ ...request, code: key }, OWNER_ADDRESS, key);
    expect(outcome.ok && outcome.value.userId).toBe(owner!.userId);
    expect(await devices()).toBe(before + 1);
    expect(await inAccount((sql) => sql.exec("SELECT COUNT(*) AS n FROM users").one().n)).toBe(1);
  });
});

// SPDX-License-Identifier: AGPL-3.0-or-later

// The first-owner claim and owner recovery (docs/design/deploy-and-upgrade.md section 2), and the per-address count of
// wrong codes that pairing shares. Each claim function runs inside one transactionSync of the Account DO, so a claim
// either creates the owner and the device or changes nothing but a failure count.

import { Error_Code } from "../gen/koidrive/v1/rpc_pb";
import type { Sql } from "../host";

/** Wrong codes one client address may send in one window before further wrong codes are refused. */
export const CLAIM_FAILURES_PER_WINDOW = 10;
export const CLAIM_FAILURE_WINDOW_MS = 10 * 60_000;
/** Wrong pairing codes one client address may send in one window before further wrong codes are refused. */
export const PAIRING_FAILURES_PER_WINDOW = 20;
/** Failure rows kept at most; the oldest windows are dropped first. */
export const MAX_FAILURE_ROWS = 10_000;
/** The failure bucket of requests without CF-Connecting-IP, which no real address can share. */
export const NO_ADDRESS = "(no address)";
/** Recovery keys shorter than this are ignored, so a short secret never opens the deployment. */
export const MIN_RECOVERY_KEY_LENGTH = 32;
/** The longest ClaimRequest.setup_attempt the server stores. */
export const MAX_SETUP_ATTEMPT_LENGTH = 64;
/** Devices of one setup attempt and owner that never refreshed a token and are not revoked. */
const UNUSED_OF_ATTEMPT = "setup_attempt = ? AND user_id = ? AND last_seen_ms IS NULL AND revoked_at_ms IS NULL";

/** A successful claim lists the earlier devices of its setup attempt, for the caller to revoke once it has committed. */
export type ClaimOutcome = { ok: true; revoke: string[] } | { ok: false; code: Error_Code; message: string };

export interface ClaimAttempt {
  /** SHA-256 of the presented code after normalization, for claim codes. */
  codeHash: ArrayBuffer;
  /** SHA-256 of the presented code as sent, for the recovery key. */
  rawHash: ArrayBuffer;
  /** SHA-256 of the recovery key the Worker is configured with, if it has a long enough one. */
  recoveryKeyHash: ArrayBuffer | undefined;
  /** The client address the edge reported, or empty. */
  client: string;
  nowMs: number;
  /** The owner when the attempt was prepared, and the user the new device belongs to: that owner, or a new one. */
  ownerBefore: string | undefined;
  userId: string;
  userName: string;
  /** The prepared device, and the client's setup attempt id, or empty. */
  deviceId: string;
  setupAttempt: string;
  /** Stores the prepared device; called inside the transaction, after every check passed. */
  storeDevice(): void;
}

function refused(message: string): ClaimOutcome {
  return { ok: false, code: Error_Code.FORBIDDEN, message };
}

function sameDigest(a: ArrayBuffer, b: ArrayBuffer): boolean {
  return a.byteLength === b.byteLength && crypto.subtle.timingSafeEqual(a, b);
}

function bucket(client: string): string {
  return client || NO_ADDRESS;
}

/** Wrong codes one client address may send in one window, for each kind of code. */
const FAILURES_PER_WINDOW = { claim: CLAIM_FAILURES_PER_WINDOW, recovery: CLAIM_FAILURES_PER_WINDOW, pair: PAIRING_FAILURES_PER_WINDOW };

type FailureScope = keyof typeof FAILURES_PER_WINDOW;

function throttled(sql: Sql, scope: FailureScope, client: string, nowMs: number): boolean {
  const row = sql
    .exec("SELECT failures FROM claim_failures WHERE scope = ? AND client = ? AND window_start_ms > ?", scope, bucket(client), nowMs - CLAIM_FAILURE_WINDOW_MS)
    .toArray()[0];
  return Number(row?.failures ?? 0) >= FAILURES_PER_WINDOW[scope];
}

/** Counts a wrong code, dropping expired windows and keeping at most MAX_FAILURE_ROWS rows. */
function recordFailure(sql: Sql, scope: FailureScope, client: string, nowMs: number): void {
  sql.exec("DELETE FROM claim_failures WHERE window_start_ms <= ?", nowMs - CLAIM_FAILURE_WINDOW_MS);
  sql.exec(
    "INSERT INTO claim_failures (scope, client, failures, window_start_ms) VALUES (?, ?, 1, ?) ON CONFLICT (scope, client) DO UPDATE SET failures = failures + 1",
    scope,
    bucket(client),
    nowMs,
  );
  sql.exec(
    "DELETE FROM claim_failures WHERE rowid IN (SELECT rowid FROM claim_failures ORDER BY window_start_ms LIMIT max(0, (SELECT COUNT(*) FROM claim_failures) - ?))",
    MAX_FAILURE_ROWS,
  );
}

/** Refuses a wrong code and counts it against the client's address, or answers RETRY_LATER once the address sent too many. */
export function wrongCode(sql: Sql, scope: FailureScope, client: string, nowMs: number, message: string): { ok: false; code: Error_Code; message: string } {
  if (throttled(sql, scope, client, nowMs)) {
    return { ok: false, code: Error_Code.RETRY_LATER, message: "too many wrong codes from this address; try again in a few minutes" };
  }
  recordFailure(sql, scope, client, nowMs);
  return { ok: false, code: Error_Code.FORBIDDEN, message };
}

/** Stores a new claim code unless the deployment has an owner or a code is still valid. Returns whether it stored one. */
export function storeClaimCode(sql: Sql, codeHash: ArrayBuffer, nowMs: number, expiresAtMs: number): boolean {
  if (sql.exec("SELECT 1 FROM users LIMIT 1").toArray().length > 0) return false;
  sql.exec("DELETE FROM claim_codes WHERE expires_at_ms <= ?", nowMs);
  if (sql.exec("SELECT 1 FROM claim_codes LIMIT 1").toArray().length > 0) return false;
  sql.exec("INSERT INTO claim_codes (code_hash, expires_at_ms) VALUES (?, ?)", codeHash, expiresAtMs);
  return true;
}

/** The deployment's owner, if it has one. */
export function ownerOf(sql: Sql): string | undefined {
  const row = sql.exec("SELECT user_id FROM users ORDER BY created_at_ms, user_id LIMIT 1").toArray()[0];
  return row ? String(row.user_id) : undefined;
}

/**
 * Redeems a claim code or the recovery key on a deployment without an owner, creating the owner and the device, or
 * the recovery key on a deployment with one, adding the device for the existing owner. A correct code or key always
 * redeems; a wrong one counts against the client's address, and an address with too many recent wrong codes gets
 * RETRY_LATER for wrong codes.
 */
export function redeemClaim(sql: Sql, attempt: ClaimAttempt): ClaimOutcome {
  const owner = ownerOf(sql);
  if (owner !== attempt.ownerBefore) return { ok: false, code: Error_Code.RETRY_LATER, message: "the deployment changed during the claim; retry" };
  if (owner) return recover(sql, attempt);

  const key = presentedKey(attempt);
  if (key) {
    const spent = spendKey(sql, key, attempt.nowMs);
    if (spent) return spent;
  } else {
    const live = sql.exec("SELECT code_hash FROM claim_codes WHERE expires_at_ms > ?", attempt.nowMs).toArray();
    if (live.length === 0) return refused("no valid claim code; open /setup to issue one");
    if (!live.some((row) => sameDigest(row.code_hash as ArrayBuffer, attempt.codeHash))) return wrongCode(sql, "claim", attempt.client, attempt.nowMs, "wrong claim code");
  }
  sql.exec("DELETE FROM claim_codes");
  sql.exec("DELETE FROM claim_failures WHERE scope = 'claim'");
  sql.exec("INSERT INTO users (user_id, name, created_at_ms) VALUES (?, ?, ?)", attempt.userId, attempt.userName || "Owner", attempt.nowMs);
  attempt.storeDevice();
  return { ok: true, revoke: key ? joinAttempt(sql, attempt) : [] };
}

/** A wrong key counts and throttles the same with or without a configured key, so answers do not reveal whether one is. */
function recover(sql: Sql, attempt: ClaimAttempt): ClaimOutcome {
  const key = presentedKey(attempt);
  if (!key) return wrongCode(sql, "recovery", attempt.client, attempt.nowMs, "this deployment already has an owner");
  const spent = spendKey(sql, key, attempt.nowMs);
  if (spent) return spent;
  attempt.storeDevice();
  return { ok: true, revoke: joinAttempt(sql, attempt) };
}

/**
 * Records the new device's setup attempt and returns the attempt's earlier devices that never refreshed a token. Their
 * credentials never reached the client, so nobody can use them.
 */
function joinAttempt(sql: Sql, attempt: ClaimAttempt): string[] {
  if (!attempt.setupAttempt) return [];
  sql.exec("UPDATE devices SET setup_attempt = ? WHERE device_id = ?", attempt.setupAttempt, attempt.deviceId);
  return sql
    .exec(`SELECT device_id FROM devices WHERE ${UNUSED_OF_ATTEMPT} AND device_id != ?`, attempt.setupAttempt, attempt.userId, attempt.deviceId)
    .toArray()
    .map((row) => String(row.device_id));
}

/** Whether a device joinAttempt returned is still an unused device of that owner's attempt. */
export function stillUnused(sql: Sql, deviceId: string, setupAttempt: string, userId: string): boolean {
  return sql.exec(`SELECT 1 FROM devices WHERE ${UNUSED_OF_ATTEMPT} AND device_id = ?`, setupAttempt, userId, deviceId).toArray().length > 0;
}

/** The Worker's recovery key hash, if the attempt presents that key. */
function presentedKey(attempt: ClaimAttempt): ArrayBuffer | undefined {
  const key = attempt.recoveryKeyHash;
  return key && sameDigest(key, attempt.rawHash) ? key : undefined;
}

/** Records the key as used, or refuses it if it was used before. */
function spendKey(sql: Sql, key: ArrayBuffer, nowMs: number): ClaimOutcome | undefined {
  if (sql.exec("SELECT 1 FROM used_recovery_keys WHERE key_hash = ?", key).toArray().length > 0) {
    return refused("this recovery key was already used; set a new one");
  }
  sql.exec("INSERT INTO used_recovery_keys (key_hash, used_at_ms) VALUES (?, ?)", key, nowMs);
  return undefined;
}

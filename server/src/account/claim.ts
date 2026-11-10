// SPDX-License-Identifier: AGPL-3.0-or-later

// The first-owner claim and owner recovery (docs/design/deploy-and-upgrade.md section 2). Each function runs inside
// one transactionSync of the Account DO, so a claim either creates the owner and the device or changes nothing but
// a failure count.

import { Error_Code } from "../gen/flarebox/v1/rpc_pb";

/** Wrong codes one client address may send in one window before further wrong codes are refused. */
export const CLAIM_FAILURES_PER_WINDOW = 10;
export const CLAIM_FAILURE_WINDOW_MS = 10 * 60_000;
/** Failure rows kept at most; the oldest windows are dropped first. */
export const MAX_FAILURE_ROWS = 10_000;
/** The failure bucket of requests without CF-Connecting-IP, which no real address can share. */
export const NO_ADDRESS = "(no address)";
/** Recovery keys shorter than this are ignored, so a short secret never opens the deployment. */
export const MIN_RECOVERY_KEY_LENGTH = 32;

export type ClaimOutcome = { ok: true } | { ok: false; code: Error_Code; message: string };

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

function throttled(sql: SqlStorage, scope: string, attempt: ClaimAttempt): boolean {
  const row = sql
    .exec("SELECT failures FROM claim_failures WHERE scope = ? AND client = ? AND window_start_ms > ?", scope, bucket(attempt.client), attempt.nowMs - CLAIM_FAILURE_WINDOW_MS)
    .toArray()[0];
  return Number(row?.failures ?? 0) >= CLAIM_FAILURES_PER_WINDOW;
}

/** Counts a wrong code, dropping expired windows and keeping at most MAX_FAILURE_ROWS rows. */
function recordFailure(sql: SqlStorage, scope: string, attempt: ClaimAttempt): void {
  const expired = attempt.nowMs - CLAIM_FAILURE_WINDOW_MS;
  sql.exec("DELETE FROM claim_failures WHERE window_start_ms <= ?", expired);
  sql.exec(
    "INSERT INTO claim_failures (scope, client, failures, window_start_ms) VALUES (?, ?, 1, ?) ON CONFLICT (scope, client) DO UPDATE SET failures = failures + 1",
    scope,
    bucket(attempt.client),
    attempt.nowMs,
  );
  sql.exec(
    "DELETE FROM claim_failures WHERE rowid IN (SELECT rowid FROM claim_failures ORDER BY window_start_ms LIMIT max(0, (SELECT COUNT(*) FROM claim_failures) - ?))",
    MAX_FAILURE_ROWS,
  );
}

function wrong(sql: SqlStorage, scope: string, attempt: ClaimAttempt, message: string): ClaimOutcome {
  if (throttled(sql, scope, attempt)) {
    return { ok: false, code: Error_Code.RETRY_LATER, message: "too many wrong codes from this address; try again in a few minutes" };
  }
  recordFailure(sql, scope, attempt);
  return refused(message);
}

/** Stores a new claim code unless the deployment has an owner or a code is still valid. Returns whether it stored one. */
export function storeClaimCode(sql: SqlStorage, codeHash: ArrayBuffer, nowMs: number, expiresAtMs: number): boolean {
  if (sql.exec("SELECT 1 FROM users LIMIT 1").toArray().length > 0) return false;
  sql.exec("DELETE FROM claim_codes WHERE expires_at_ms <= ?", nowMs);
  if (sql.exec("SELECT 1 FROM claim_codes LIMIT 1").toArray().length > 0) return false;
  sql.exec("INSERT INTO claim_codes (code_hash, expires_at_ms) VALUES (?, ?)", codeHash, expiresAtMs);
  return true;
}

/** The deployment's owner, if it has one. */
export function ownerOf(sql: SqlStorage): string | undefined {
  const row = sql.exec("SELECT user_id FROM users ORDER BY created_at_ms, user_id LIMIT 1").toArray()[0];
  return row ? String(row.user_id) : undefined;
}

/**
 * Redeems a claim code on a deployment without an owner, creating the owner and the device, or the recovery key on a
 * deployment with one, adding the device for the existing owner. A correct code always redeems; a wrong one counts
 * against the client's address, and an address with too many recent wrong codes gets RETRY_LATER for wrong codes.
 */
export function redeemClaim(sql: SqlStorage, attempt: ClaimAttempt): ClaimOutcome {
  const owner = ownerOf(sql);
  if (owner !== attempt.ownerBefore) return { ok: false, code: Error_Code.RETRY_LATER, message: "the deployment changed during the claim; retry" };
  if (owner) return recover(sql, attempt);

  const key = attempt.recoveryKeyHash;
  if (key && sameDigest(key, attempt.rawHash)) {
    if (sql.exec("SELECT 1 FROM used_recovery_keys WHERE key_hash = ?", key).toArray().length > 0) {
      return refused("this recovery key was already used; set a new one");
    }
    sql.exec("INSERT INTO used_recovery_keys (key_hash, used_at_ms) VALUES (?, ?)", key, attempt.nowMs);
    sql.exec("DELETE FROM claim_codes");
    sql.exec("INSERT INTO users (user_id, name, created_at_ms) VALUES (?, ?, ?)", attempt.userId, attempt.userName || "Owner", attempt.nowMs);
    attempt.storeDevice();
    return { ok: true };
  }

  const live = sql.exec("SELECT code_hash FROM claim_codes WHERE expires_at_ms > ?", attempt.nowMs).toArray();
  if (live.length === 0) return refused("no valid claim code; open /setup to issue one");
  if (!live.some((row) => sameDigest(row.code_hash as ArrayBuffer, attempt.codeHash))) return wrong(sql, "claim", attempt, "wrong claim code");
  sql.exec("DELETE FROM claim_codes");
  sql.exec("DELETE FROM claim_failures WHERE scope = 'claim'");
  sql.exec("INSERT INTO users (user_id, name, created_at_ms) VALUES (?, ?, ?)", attempt.userId, attempt.userName || "Owner", attempt.nowMs);
  attempt.storeDevice();
  return { ok: true };
}

function recover(sql: SqlStorage, attempt: ClaimAttempt): ClaimOutcome {
  const key = attempt.recoveryKeyHash;
  if (!key) return refused("this deployment already has an owner");
  if (!sameDigest(key, attempt.rawHash)) return wrong(sql, "recovery", attempt, "this deployment already has an owner");
  if (sql.exec("SELECT 1 FROM used_recovery_keys WHERE key_hash = ?", key).toArray().length > 0) {
    return refused("this recovery key was already used; set a new one");
  }
  sql.exec("INSERT INTO used_recovery_keys (key_hash, used_at_ms) VALUES (?, ?)", key, attempt.nowMs);
  attempt.storeDevice();
  return { ok: true };
}

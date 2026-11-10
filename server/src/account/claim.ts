// SPDX-License-Identifier: AGPL-3.0-or-later

// The first-owner claim and owner recovery (docs/design/deploy-and-upgrade.md section 2). Each function runs inside
// one transactionSync of the Account DO, so a claim either creates the owner or changes nothing but its failure count.

import { Error_Code } from "../gen/flarebox/v1/rpc_pb";
import { toHex } from "../ids";

/** Wrong codes one client address may send against one claim code or recovery key before it is refused. */
export const CLAIM_FAILURES_PER_CLIENT = 10;
/** Recovery keys shorter than this are ignored, so a short secret never opens the deployment. */
export const MIN_RECOVERY_KEY_LENGTH = 32;

export type ClaimOutcome = { ok: true; userId: string } | { ok: false; code: Error_Code; message: string };

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
  newUserId: string;
  userName: string;
}

function refused(message: string): ClaimOutcome {
  return { ok: false, code: Error_Code.FORBIDDEN, message };
}

function sameBytes(a: ArrayBuffer, b: ArrayBuffer): boolean {
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  return x.length === y.length && x.every((byte, i) => byte === y[i]);
}

function failures(sql: SqlStorage, target: string, client: string): number {
  return Number(sql.exec("SELECT failures FROM claim_failures WHERE target = ? AND client = ?", target, client).toArray()[0]?.failures ?? 0);
}

function recordFailure(sql: SqlStorage, target: string, client: string): void {
  sql.exec(
    "INSERT INTO claim_failures (target, client, failures) VALUES (?, ?, 1) ON CONFLICT (target, client) DO UPDATE SET failures = failures + 1",
    target,
    client,
  );
}

/** Stores a new claim code unless the deployment has an owner or a code is still valid. Returns whether it stored one. */
export function storeClaimCode(sql: SqlStorage, codeHash: ArrayBuffer, nowMs: number, expiresAtMs: number): boolean {
  if (sql.exec("SELECT 1 FROM users LIMIT 1").toArray().length > 0) return false;
  sql.exec("DELETE FROM claim_codes WHERE expires_at_ms <= ?", nowMs);
  if (sql.exec("SELECT 1 FROM claim_codes LIMIT 1").toArray().length > 0) return false;
  sql.exec("DELETE FROM claim_failures WHERE target LIKE 'code:%'");
  sql.exec("INSERT INTO claim_codes (code_hash, expires_at_ms) VALUES (?, ?)", codeHash, expiresAtMs);
  return true;
}

/**
 * Redeems a claim code on a deployment without an owner, creating the owner, or the recovery key on a deployment with
 * one, returning the existing owner. Anything else is refused, and a wrong code counts against the client's address.
 */
export function redeemClaim(sql: SqlStorage, attempt: ClaimAttempt): ClaimOutcome {
  const owner = sql.exec("SELECT user_id FROM users ORDER BY created_at_ms, user_id LIMIT 1").toArray()[0];
  if (owner) return recover(sql, attempt, String(owner.user_id));

  const live = sql.exec("SELECT code_hash FROM claim_codes WHERE expires_at_ms > ? ORDER BY expires_at_ms DESC", attempt.nowMs).toArray();
  if (live.length === 0) return refused("no valid claim code; open /setup to issue one");
  const target = "code:" + toHex(new Uint8Array(live[0]!.code_hash as ArrayBuffer));
  if (failures(sql, target, attempt.client) >= CLAIM_FAILURES_PER_CLIENT) {
    return { ok: false, code: Error_Code.RETRY_LATER, message: "too many wrong claim codes from this address; wait for the next code" };
  }
  if (!live.some((row) => sameBytes(row.code_hash as ArrayBuffer, attempt.codeHash))) {
    recordFailure(sql, target, attempt.client);
    return refused("wrong claim code");
  }
  sql.exec("DELETE FROM claim_codes");
  sql.exec("DELETE FROM claim_failures WHERE target LIKE 'code:%'");
  sql.exec("INSERT INTO users (user_id, name, created_at_ms) VALUES (?, ?, ?)", attempt.newUserId, attempt.userName || "Owner", attempt.nowMs);
  return { ok: true, userId: attempt.newUserId };
}

function recover(sql: SqlStorage, attempt: ClaimAttempt, ownerId: string): ClaimOutcome {
  const key = attempt.recoveryKeyHash;
  if (!key) return refused("this deployment already has an owner");
  const target = "key:" + toHex(new Uint8Array(key));
  if (failures(sql, target, attempt.client) >= CLAIM_FAILURES_PER_CLIENT) {
    return { ok: false, code: Error_Code.RETRY_LATER, message: "too many wrong recovery keys from this address" };
  }
  if (!sameBytes(key, attempt.rawHash)) {
    recordFailure(sql, target, attempt.client);
    return refused("this deployment already has an owner");
  }
  if (sql.exec("SELECT 1 FROM used_recovery_keys WHERE key_hash = ?", key).toArray().length > 0) {
    return refused("this recovery key was already used; set a new one");
  }
  sql.exec("INSERT INTO used_recovery_keys (key_hash, used_at_ms) VALUES (?, ?)", key, attempt.nowMs);
  sql.exec("DELETE FROM claim_failures WHERE target = ?", target);
  return { ok: true, userId: ownerId };
}

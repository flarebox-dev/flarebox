// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { DurableObject } from "cloudflare:workers";
import { ACCESS_TOKEN_MS, type ServerKey, signAccessToken } from "./auth";
import { base64url, deriveSpaceKey, sha256 } from "./crypto";
import {
  type AccessToken,
  AccessTokenSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  type SpaceInfo,
  SpaceInfoSchema,
} from "./gen/flarebox/v1/auth_pb";
import { Error_Code } from "./gen/flarebox/v1/rpc_pb";
import { uuidv7 } from "./ids";
import { FORMAT_VERSION } from "./space-do";

const CLAIM_CODE_MS = 3_600_000;
const CLAIM_ATTEMPTS = 10;
const PAIRING_CODE_MS = 10 * 60_000;
const PAIRING_FAILURES_PER_WINDOW = 20;
/** Highest PBKDF2 iteration count workerd accepts; see cloudflare/workerd#1346. */
export const SHARE_PASSWORD_ITERATIONS = 100_000;
const SHARE_PASSWORD_KDF = "pbkdf2-sha256";
const SHARE_FAILURES_BEFORE_LOCK = 5;
const SHARE_LOCK_MS = 15 * 60_000;
const BASE32 = "ABCDEFGHJKMNPQRSTVWXYZ0123456789";
const encoder = new TextEncoder();

export type Outcome<T> = { ok: true; value: T } | { ok: false; code: Error_Code; message: string };

function ok<T>(value: T): Outcome<T> {
  return { ok: true, value };
}

function failure<T>(code: Error_Code, message: string): Outcome<T> {
  return { ok: false, code, message };
}

function randomCode(length: number): string {
  const bytes = crypto.getRandomValues(new Uint8Array(length));
  return Array.from(bytes, (byte) => BASE32[byte % BASE32.length]).join("");
}

/** Codes are shown to people, so case and separators do not matter. */
function normalizeCode(code: string): string {
  return code.toUpperCase().replace(/[^A-Z0-9]/g, "");
}

async function codeHash(code: string): Promise<ArrayBuffer> {
  return (await sha256(encoder.encode(normalizeCode(code)))).slice().buffer;
}

function sameBytes(a: ArrayBuffer, b: ArrayBuffer): boolean {
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  return x.length === y.length && x.every((byte, i) => byte === y[i]);
}

async function passwordHash(password: string, salt: Uint8Array, iterations: number): Promise<ArrayBuffer> {
  const key = await crypto.subtle.importKey("raw", encoder.encode(password), "PBKDF2", false, ["deriveBits"]);
  return crypto.subtle.deriveBits({ name: "PBKDF2", hash: "SHA-256", salt, iterations }, key, 256);
}

const SCHEMA = `
  CREATE TABLE IF NOT EXISTS users (user_id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at_ms INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS devices (device_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL,
    platform TEXT NOT NULL, created_at_ms INTEGER NOT NULL, last_seen_ms INTEGER, refresh_hash BLOB NOT NULL,
    revoked_at_ms INTEGER);
  CREATE TABLE IF NOT EXISTS spaces (space_id TEXT PRIMARY KEY, name TEXT NOT NULL, owner_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL, format_version INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS share_links (link_id TEXT PRIMARY KEY, space_id TEXT NOT NULL, node_id TEXT NOT NULL,
    created_by TEXT NOT NULL, created_at_ms INTEGER NOT NULL, expires_at_ms INTEGER, password_kdf TEXT,
    password_hash BLOB, password_salt BLOB, password_iterations INTEGER, revoked_at_ms INTEGER,
    failed_attempts INTEGER NOT NULL DEFAULT 0, locked_until_ms INTEGER);
  CREATE TABLE IF NOT EXISTS pairing_codes (code_hash BLOB PRIMARY KEY, device_id TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0);
  CREATE TABLE IF NOT EXISTS claim_codes (code_hash BLOB PRIMARY KEY, expires_at_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0);
  CREATE TABLE IF NOT EXISTS server_keys (kid TEXT PRIMARY KEY, secret BLOB NOT NULL, created_at_ms INTEGER NOT NULL,
    status TEXT NOT NULL);
  CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
`;

interface DeviceRow {
  device_id: string;
  user_id: string;
  name: string;
  refresh_hash: ArrayBuffer;
  revoked_at_ms: number | null;
}

interface SpaceRow {
  space_id: string;
  name: string;
  owner_id: string;
  created_at_ms: number;
  format_version: number;
}

interface ShareLinkRow {
  link_id: string;
  space_id: string;
  node_id: string;
  expires_at_ms: number | null;
  password_kdf: string | null;
  password_hash: ArrayBuffer | null;
  password_salt: ArrayBuffer | null;
  password_iterations: number | null;
  revoked_at_ms: number | null;
  failed_attempts: number;
  locked_until_ms: number | null;
}

function spaceInfo(row: SpaceRow): SpaceInfo {
  return create(SpaceInfoSchema, {
    spaceId: row.space_id,
    name: row.name,
    formatVersion: row.format_version,
    createdAtMs: BigInt(row.created_at_ms),
  });
}

/** One per deployment: users, devices, tokens, spaces and share links. */
export class AccountDO extends DurableObject<Env> {
  private readonly sql: SqlStorage;
  private keyCache: ServerKey | undefined;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(SCHEMA);
  }

  /** The signing key for tokens, tickets and receipts, created with crypto.getRandomValues on first use. */
  async serverKey(): Promise<ServerKey> {
    if (this.keyCache) return this.keyCache;
    let row = this.sql.exec("SELECT kid, secret FROM server_keys WHERE status = 'active' LIMIT 1").toArray()[0];
    if (!row) {
      const kid = base64url(crypto.getRandomValues(new Uint8Array(6)));
      const secret = crypto.getRandomValues(new Uint8Array(32));
      this.sql.exec("INSERT INTO server_keys (kid, secret, created_at_ms, status) VALUES (?, ?, ?, 'active')", kid, secret.buffer, Date.now());
      row = { kid, secret: secret.buffer };
    }
    this.keyCache = { kid: String(row.kid), secret: new Uint8Array(row.secret as ArrayBuffer) };
    return this.keyCache;
  }

  async isClaimed(): Promise<boolean> {
    return this.sql.exec("SELECT 1 FROM users LIMIT 1").toArray().length > 0;
  }

  /**
   * Starts an owner claim: returns a fresh one-time code for the Worker to write to its log, or undefined when
   * the deployment is claimed or a code is still valid.
   */
  async issueClaimCode(): Promise<string | undefined> {
    const nowMs = Date.now();
    if (await this.isClaimed()) return undefined;
    this.sql.exec("DELETE FROM claim_codes WHERE expires_at_ms <= ? OR attempts >= ?", nowMs, CLAIM_ATTEMPTS);
    if (this.sql.exec("SELECT 1 FROM claim_codes").toArray().length > 0) return undefined;
    const code = randomCode(12);
    this.sql.exec("INSERT INTO claim_codes (code_hash, expires_at_ms) VALUES (?, ?)", await codeHash(code), nowMs + CLAIM_CODE_MS);
    return `${code.slice(0, 4)}-${code.slice(4, 8)}-${code.slice(8)}`;
  }

  async claim(args: { code: string; userName: string; deviceName: string; platform: string }): Promise<Outcome<DeviceCredentials>> {
    const nowMs = Date.now();
    if (await this.isClaimed()) return failure(Error_Code.FORBIDDEN, "this deployment already has an owner");
    const hash = await codeHash(args.code);
    const row = this.sql.exec("SELECT code_hash FROM claim_codes WHERE expires_at_ms > ? AND attempts < ?", nowMs, CLAIM_ATTEMPTS).toArray()[0];
    if (!row) return failure(Error_Code.FORBIDDEN, "no valid claim code; open /setup to issue one");
    if (!sameBytes(row.code_hash as ArrayBuffer, hash)) {
      this.sql.exec("UPDATE claim_codes SET attempts = attempts + 1");
      return failure(Error_Code.FORBIDDEN, "wrong claim code");
    }
    const userId = uuidv7(nowMs);
    this.sql.exec("DELETE FROM claim_codes");
    this.sql.exec("INSERT INTO users (user_id, name, created_at_ms) VALUES (?, ?, ?)", userId, args.userName || "Owner", nowMs);
    return ok(await this.addDevice(userId, args.deviceName, args.platform, nowMs));
  }

  private async addDevice(userId: string, deviceName: string, platform: string, nowMs: number): Promise<DeviceCredentials> {
    const deviceId = uuidv7(nowMs);
    const refresh = base64url(crypto.getRandomValues(new Uint8Array(32)));
    this.sql.exec(
      "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash) VALUES (?, ?, ?, ?, ?, ?)",
      deviceId,
      userId,
      deviceName || "Unnamed device",
      platform,
      nowMs,
      (await sha256(encoder.encode(refresh))).slice().buffer,
    );
    return create(DeviceCredentialsSchema, {
      userId,
      deviceId,
      refreshCredential: refresh,
      access: await this.accessToken(userId, deviceId, deviceName || "Unnamed device", nowMs),
    });
  }

  private async accessToken(userId: string, deviceId: string, deviceName: string, nowMs: number) {
    const expiresAtMs = nowMs + ACCESS_TOKEN_MS;
    const token = await signAccessToken(await this.serverKey(), { userId, deviceId, deviceName, expiresAtMs });
    return create(AccessTokenSchema, { token, expiresAtMs: BigInt(expiresAtMs) });
  }

  private activeDevice(deviceId: string): DeviceRow | undefined {
    const row = this.sql.exec("SELECT * FROM devices WHERE device_id = ? AND revoked_at_ms IS NULL", deviceId).toArray()[0];
    return row as unknown as DeviceRow | undefined;
  }

  /** Called for an already paired device; returns an 8-character one-time code valid for 10 minutes. */
  async startPairing(deviceId: string): Promise<Outcome<{ code: string; expiresAtMs: number }>> {
    const nowMs = Date.now();
    if (!this.activeDevice(deviceId)) return failure(Error_Code.UNAUTHENTICATED, "device is not active");
    const code = randomCode(8);
    const expiresAtMs = nowMs + PAIRING_CODE_MS;
    this.sql.exec("DELETE FROM pairing_codes WHERE expires_at_ms <= ?", nowMs);
    this.sql.exec("INSERT INTO pairing_codes (code_hash, device_id, expires_at_ms) VALUES (?, ?, ?)", await codeHash(code), deviceId, expiresAtMs);
    return ok({ code: `${code.slice(0, 4)}-${code.slice(4)}`, expiresAtMs });
  }

  /** Redeems a pairing code for a new device of the same user. Failed attempts are counted per 10-minute window. */
  async completePairing(args: { code: string; deviceName: string; platform: string }): Promise<Outcome<DeviceCredentials>> {
    const nowMs = Date.now();
    const window = JSON.parse(
      String(this.sql.exec("SELECT value FROM settings WHERE key = 'pair_failures'").toArray()[0]?.value ?? '{"start":0,"count":0}'),
    ) as { start: number; count: number };
    if (nowMs - window.start > PAIRING_CODE_MS) Object.assign(window, { start: nowMs, count: 0 });
    if (window.count >= PAIRING_FAILURES_PER_WINDOW) return failure(Error_Code.RETRY_LATER, "too many failed pairing attempts");
    const row = this.sql
      .exec("DELETE FROM pairing_codes WHERE code_hash = ? AND expires_at_ms > ? RETURNING device_id", await codeHash(args.code), nowMs)
      .toArray()[0];
    const inviter = row && this.activeDevice(String(row.device_id));
    if (!inviter) {
      window.count++;
      this.sql.exec(
        "INSERT INTO settings (key, value) VALUES ('pair_failures', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        JSON.stringify(window),
      );
      return failure(Error_Code.FORBIDDEN, "invalid or expired pairing code");
    }
    return ok(await this.addDevice(inviter.user_id, args.deviceName, args.platform, nowMs));
  }

  async refreshAccess(args: { deviceId: string; refreshCredential: string }): Promise<Outcome<AccessToken>> {
    const nowMs = Date.now();
    const device = this.activeDevice(args.deviceId);
    const presented = (await sha256(encoder.encode(args.refreshCredential))).slice().buffer;
    if (!device || !sameBytes(device.refresh_hash, presented)) {
      return failure(Error_Code.UNAUTHENTICATED, "unknown device or refresh credential");
    }
    this.sql.exec("UPDATE devices SET last_seen_ms = ? WHERE device_id = ?", nowMs, device.device_id);
    return ok(await this.accessToken(device.user_id, device.device_id, device.name, nowMs));
  }

  /** Revokes a device: its refresh credential stops working and its open WebSockets close with 4003. */
  async revokeDevice(deviceId: string): Promise<void> {
    this.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ? AND revoked_at_ms IS NULL", Date.now(), deviceId);
    const spaces = this.sql.exec("SELECT space_id FROM spaces").toArray();
    await Promise.all(spaces.map(({ space_id }) => this.env.SPACE.get(this.env.SPACE.idFromName(String(space_id))).closeDevice(deviceId)));
  }

  async createSpace(args: { userId: string; name: string }): Promise<SpaceInfo> {
    const nowMs = Date.now();
    const spaceId = uuidv7(nowMs);
    const name = args.name.trim() || "Flarebox";
    const spaceKey = await deriveSpaceKey((await this.serverKey()).secret, spaceId);
    await this.env.SPACE.get(this.env.SPACE.idFromName(spaceId)).init({ spaceId, name, ownerUserId: args.userId, spaceKey });
    this.sql.exec(
      "INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version) VALUES (?, ?, ?, ?, ?)",
      spaceId,
      name,
      args.userId,
      nowMs,
      FORMAT_VERSION,
    );
    return spaceInfo({ space_id: spaceId, name, owner_id: args.userId, created_at_ms: nowMs, format_version: FORMAT_VERSION });
  }

  async listSpaces(userId: string): Promise<SpaceInfo[]> {
    const found = this.sql.exec("SELECT * FROM spaces WHERE owner_id = ? ORDER BY created_at_ms", userId).toArray();
    return (found as unknown as SpaceRow[]).map(spaceInfo);
  }

  /** Records a read-only share link with a 128-bit random id. */
  async createShareLink(args: {
    spaceId: string;
    nodeId: string;
    createdBy: string;
    expiresAtMs?: number;
    password?: string;
  }): Promise<string> {
    const linkId = base64url(crypto.getRandomValues(new Uint8Array(16)));
    const salt = args.password ? crypto.getRandomValues(new Uint8Array(16)) : undefined;
    this.sql.exec(
      `INSERT INTO share_links (link_id, space_id, node_id, created_by, created_at_ms, expires_at_ms, password_kdf,
         password_hash, password_salt, password_iterations) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      linkId,
      args.spaceId,
      args.nodeId,
      args.createdBy,
      Date.now(),
      args.expiresAtMs ?? null,
      salt ? SHARE_PASSWORD_KDF : null,
      salt ? await passwordHash(args.password!, salt, SHARE_PASSWORD_ITERATIONS) : null,
      salt ? salt.buffer : null,
      salt ? SHARE_PASSWORD_ITERATIONS : null,
    );
    return linkId;
  }

  async revokeShareLink(linkId: string): Promise<void> {
    this.sql.exec("UPDATE share_links SET revoked_at_ms = ? WHERE link_id = ? AND revoked_at_ms IS NULL", Date.now(), linkId);
  }

  /** Resolves a share link, checking expiry, revocation and the password, and locking it after repeated failures. */
  async openShareLink(linkId: string, password: string | undefined): Promise<Outcome<{ spaceId: string; nodeId: string }>> {
    const nowMs = Date.now();
    const read = () => this.sql.exec("SELECT * FROM share_links WHERE link_id = ?", linkId).toArray()[0] as unknown as ShareLinkRow | undefined;
    const found = read();
    if (!found || found.revoked_at_ms !== null || (found.expires_at_ms !== null && found.expires_at_ms <= nowMs)) {
      return failure(Error_Code.FORBIDDEN, "this link does not exist or has expired");
    }
    if (found.password_hash) {
      if (found.locked_until_ms !== null && found.locked_until_ms > nowMs) return failure(Error_Code.RETRY_LATER, "too many wrong passwords");
      if (found.password_kdf !== SHARE_PASSWORD_KDF) return failure(Error_Code.INTERNAL, "unsupported password scheme");
      const given = password === undefined ? undefined : await passwordHash(password, new Uint8Array(found.password_salt!), found.password_iterations!);
      const link = read()!;
      if (link.locked_until_ms !== null && link.locked_until_ms > nowMs) return failure(Error_Code.RETRY_LATER, "too many wrong passwords");
      if (!given || !sameBytes(given, link.password_hash!)) {
        const failed = link.failed_attempts + 1;
        const locked = failed >= SHARE_FAILURES_BEFORE_LOCK;
        this.sql.exec(
          "UPDATE share_links SET failed_attempts = ?, locked_until_ms = ? WHERE link_id = ?",
          locked ? 0 : failed,
          locked ? nowMs + SHARE_LOCK_MS : link.locked_until_ms,
          linkId,
        );
        return failure(Error_Code.UNAUTHENTICATED, "wrong password");
      }
      if (link.failed_attempts > 0) this.sql.exec("UPDATE share_links SET failed_attempts = 0 WHERE link_id = ?", linkId);
    }
    return ok({ spaceId: found.space_id, nodeId: found.node_id });
  }
}

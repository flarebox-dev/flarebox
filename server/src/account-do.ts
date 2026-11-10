// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { DurableObject } from "cloudflare:workers";
import { type ClaimAttempt, MIN_RECOVERY_KEY_LENGTH, ownerOf, redeemClaim, storeClaimCode } from "./account/claim";
import { migrate } from "./account/schema";
import { ACCESS_TOKEN_MS, type ServerKey, signAccessToken } from "./auth";
import { base64url, deriveSpaceKey, sha256 } from "./crypto";
import {
  type AccessToken,
  AccessTokenSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  DeviceInfo_Kind,
  DeviceInfoSchema,
  type DeviceList,
  DeviceListSchema,
  type RewindResult,
  RewindResultSchema,
  type SpaceInfo,
  SpaceInfoSchema,
} from "./gen/flarebox/v1/auth_pb";
import { Error_Code } from "./gen/flarebox/v1/rpc_pb";
import type { ShareLink, ShareLinkList } from "./gen/flarebox/v1/share_pb";
import { isUuidV7, uuidv7 } from "./ids";
import { knownPrincipal } from "./policy";
import { FORMAT_VERSION } from "./space-do";
import { type Outcome, type ResolvedLink, type ShareOutcome, ShareLinks } from "./share/links";

export type { Outcome } from "./share/links";

const CLAIM_CODE_MS = 3_600_000;
const PAIRING_CODE_MS = 10 * 60_000;
const PAIRING_FAILURES_PER_WINDOW = 20;
/** Principals per page of GET /v1/devices (RFC 0006 section 7). */
const DEVICE_PAGE = 100;
const BASE32 = "ABCDEFGHJKMNPQRSTVWXYZ0123456789";
const DAY_MS = 24 * 3_600_000;
/** How long a chunk retirement record is kept: PITR cannot restore a state older than 30 days (RFC 0003 section 5, rule 10). */
const RETIREMENT_KEEP_MS = 30 * DAY_MS;
/** PITR restores any point in the past 30 days. */
const PITR_WINDOW_MS = 30 * DAY_MS;
/** Admissible restore points lie from one minute to 29 days back (RFC 0003 section 2). */
const REWIND_MIN_AGE_MS = 60_000;
const REWIND_MAX_AGE_MS = 29 * DAY_MS;
/** The Account DO alarm retries a rewind phase with backoff from one second up to ten minutes (RFC 0003 section 3). */
const REWIND_RETRY_MIN_MS = 1000;
/** How long the alarm waits before it delivers a revocation notice again to a space that did not take it. */
const NOTICE_RETRY_MS = 30_000;
const REWIND_RETRY_MAX_MS = 10 * 60_000;

/** The phases of a rewind record (RFC 0003 section 3). */
type RewindPhase = "admitted" | "prepared" | "arming" | "completed" | "failed";

interface RewindRow {
  rewind_id: string;
  space_id: string;
  restore_point_ms: number;
  expected_history_id: string;
  bookmark: string;
  history_id: string;
  rewind_number: number;
  previous_history_id: string | null;
  previous_head_seq: number | null;
  attempt: number;
  phase: RewindPhase;
  result: ArrayBuffer | null;
  error_code: number | null;
  error: string | null;
  admitted_at_ms: number;
  ended_at_ms: number | null;
  tries: number;
}

export interface RewindArgs {
  userId: string;
  spaceId: string;
  rewindId: string;
  restorePointMs: number;
  expectedHistoryId: string;
}
const encoder = new TextEncoder();

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

interface DeviceRow {
  device_id: string;
  user_id: string;
  name: string;
  platform: string;
  created_at_ms: number;
  last_seen_ms: number | null;
  refresh_hash: ArrayBuffer;
  revoked_at_ms: number | null;
  kind: string;
  scope: string | null;
  expires_at_ms: number | null;
  issued_by: string | null;
}

/**
 * What checkSession answers: kind and scope come from the device's row, never from its token (RFC 0006 section 2).
 * revoked is set only for a device whose revocation is recorded, which cannot be undone; a device that is inactive
 * for another reason, such as an expired credential or a kind this server does not know, may become active again.
 */
export interface SessionCheck {
  active: boolean;
  revoked: boolean;
  closed: boolean;
  kind: string;
  scope: string | null;
}

const KINDS: Record<string, DeviceInfo_Kind> = { device: DeviceInfo_Kind.DEVICE, agent: DeviceInfo_Kind.AGENT };

interface SpaceRow {
  space_id: string;
  name: string;
  owner_id: string;
  created_at_ms: number;
  format_version: number;
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
  /** Rewinds this instance is driving now, so a retry and the alarm do not run the same phase at once. */
  private readonly driving = new Set<string>();
  /** The time rewinds are admitted, completed and timed out against. Tests replace it to move through days. */
  clock: () => number = () => Date.now();
  private readonly shares: ShareLinks;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    ctx.storage.transactionSync(() => migrate(this.sql));
    this.shares = new ShareLinks(this.sql, {
      clock: () => this.clock(),
      serverSecret: async () => (await this.serverKey()).secret,
      spaceClosed: (spaceId) => this.spaceClosed(spaceId),
    });
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
    const code = randomCode(12);
    const hash = await codeHash(code);
    const stored = this.ctx.storage.transactionSync(() => storeClaimCode(this.sql, hash, nowMs, nowMs + CLAIM_CODE_MS));
    return stored ? `${code.slice(0, 4)}-${code.slice(4, 8)}-${code.slice(8)}` : undefined;
  }

  /**
   * Redeems a claim code, which makes the first caller the owner, or on a claimed deployment the recovery key the
   * Worker is configured with, which adds a device for the owner. client is the caller's address, for failure counts.
   * Everything that can fail is prepared first, so the transaction stores the owner, the key's use and the device together.
   */
  async claim(
    args: { code: string; userName: string; deviceName: string; platform: string },
    client = "",
    recoveryKey?: string,
  ): Promise<Outcome<DeviceCredentials>> {
    const nowMs = Date.now();
    const ownerBefore = ownerOf(this.sql);
    const userId = ownerBefore ?? uuidv7(nowMs);
    const device = await this.prepareDevice(userId, args.deviceName, args.platform, nowMs, null);
    const attempt: ClaimAttempt = {
      codeHash: await codeHash(args.code),
      rawHash: (await sha256(encoder.encode(args.code))).slice().buffer,
      recoveryKeyHash:
        recoveryKey && recoveryKey.length >= MIN_RECOVERY_KEY_LENGTH ? (await sha256(encoder.encode(recoveryKey))).slice().buffer : undefined,
      client,
      nowMs,
      ownerBefore,
      userId,
      userName: args.userName,
      storeDevice: device.store,
    };
    const outcome = this.ctx.storage.transactionSync(() => redeemClaim(this.sql, attempt));
    if (!outcome.ok) return outcome;
    return ok(device.credentials);
  }

  private async addDevice(userId: string, deviceName: string, platform: string, nowMs: number, issuedBy: string): Promise<DeviceCredentials> {
    const device = await this.prepareDevice(userId, deviceName, platform, nowMs, issuedBy);
    device.store();
    return device.credentials;
  }

  /**
   * Creates the credentials and access token of a full-scope device that never expires, the only principal 0.1.0 issues,
   * without storing anything; store() inserts it. issuedBy is the principal whose request creates it (RFC 0006 section 6).
   */
  private async prepareDevice(userId: string, deviceName: string, platform: string, nowMs: number, issuedBy: string | null) {
    const deviceId = uuidv7(nowMs);
    const name = deviceName || "Unnamed device";
    const refresh = base64url(crypto.getRandomValues(new Uint8Array(32)));
    const refreshHash = (await sha256(encoder.encode(refresh))).slice().buffer;
    const credentials = create(DeviceCredentialsSchema, {
      userId,
      deviceId,
      refreshCredential: refresh,
      access: await this.accessToken({ user_id: userId, device_id: deviceId, name, kind: "device", scope: null, expires_at_ms: null }, nowMs),
      credentialExpiresAtMs: 0n,
    });
    const store = () => {
      this.sql.exec(
        "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, issued_by) VALUES (?, ?, ?, ?, ?, ?, ?)",
        deviceId,
        userId,
        name,
        platform,
        nowMs,
        refreshHash,
        issuedBy,
      );
    };
    return { credentials, store };
  }

  /** An access token for the principal, which never outlives its credential (RFC 0006 section 2). */
  private async accessToken(device: Pick<DeviceRow, "user_id" | "device_id" | "name" | "kind" | "scope" | "expires_at_ms">, nowMs: number) {
    const expiresAtMs = Math.min(nowMs + ACCESS_TOKEN_MS, device.expires_at_ms ?? Infinity);
    const token = await signAccessToken(await this.serverKey(), {
      userId: device.user_id,
      deviceId: device.device_id,
      deviceName: device.name,
      expiresAtMs,
      kind: device.kind,
      scope: device.scope,
    });
    return create(AccessTokenSchema, { token, expiresAtMs: BigInt(expiresAtMs) });
  }

  async isDeviceActive(deviceId: string): Promise<boolean> {
    return this.activeDevice(deviceId) !== undefined;
  }

  /**
   * The principal, if it may act at all: not revoked, not expired, and of a kind and scope this server knows. Every
   * check of a principal goes through here, so its current row always decides, whatever a token claims.
   */
  private activeDevice(deviceId: string): DeviceRow | undefined {
    const row = this.sql.exec("SELECT * FROM devices WHERE device_id = ? AND revoked_at_ms IS NULL", deviceId).toArray()[0] as unknown as
      | DeviceRow
      | undefined;
    if (!row || !knownPrincipal(row.kind, row.scope)) return undefined;
    if (row.expires_at_ms !== null && row.expires_at_ms <= Date.now()) return undefined;
    return row;
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
    return ok(await this.addDevice(inviter.user_id, args.deviceName, args.platform, nowMs, inviter.device_id));
  }

  async refreshAccess(args: { deviceId: string; refreshCredential: string }): Promise<Outcome<AccessToken>> {
    const nowMs = Date.now();
    const device = this.activeDevice(args.deviceId);
    const presented = (await sha256(encoder.encode(args.refreshCredential))).slice().buffer;
    if (!device || !sameBytes(device.refresh_hash, presented)) {
      return failure(Error_Code.UNAUTHENTICATED, "unknown device or refresh credential");
    }
    this.sql.exec("UPDATE devices SET last_seen_ms = ? WHERE device_id = ?", nowMs, device.device_id);
    return ok(await this.accessToken(device, nowMs));
  }

  /** GET /v1/devices: the user's principals in creation order, revoked ones included (RFC 0006 section 7). */
  async listDevices(args: { userId: string; callerId: string; cursor: string }): Promise<DeviceList> {
    const found = this.sql
      .exec("SELECT * FROM devices WHERE user_id = ? AND device_id > ? ORDER BY device_id LIMIT ?", args.userId, args.cursor, DEVICE_PAGE + 1)
      .toArray() as unknown as DeviceRow[];
    const page = found.slice(0, DEVICE_PAGE);
    return create(DeviceListSchema, {
      devices: page.map((row) =>
        create(DeviceInfoSchema, {
          deviceId: row.device_id,
          name: row.name,
          platform: row.platform,
          kind: KINDS[row.kind] ?? DeviceInfo_Kind.UNSPECIFIED,
          createdAtMs: BigInt(row.created_at_ms),
          lastSeenMs: BigInt(row.last_seen_ms ?? 0),
          revokedAtMs: BigInt(row.revoked_at_ms ?? 0),
          credentialExpiresAtMs: BigInt(row.expires_at_ms ?? 0),
          issuedBy: row.issued_by ?? "",
          current: row.device_id === args.callerId,
        }),
      ),
      nextCursor: found.length > DEVICE_PAGE ? page[page.length - 1]!.device_id : "",
    });
  }

  /**
   * DELETE /v1/devices/{device_id}: revokes one of the user's principals other than the caller's own, so the caller
   * always stays active and the user keeps at least one device (RFC 0009 section 5). Revoking a revoked one changes
   * nothing, but delivers the notices still pending for it.
   */
  async revokeUserDevice(args: { userId: string; callerId: string; deviceId: string }): Promise<Outcome<null>> {
    if (args.deviceId === args.callerId) return failure(Error_Code.FORBIDDEN, "a device cannot revoke itself; revoke it from another device");
    // The Worker checked the caller in an earlier event, and a concurrent revocation may have revoked it since. Checked
    // again here, with no await before revokeDevice's update, so of two devices revoking each other only one succeeds.
    if (!this.activeDevice(args.callerId)) return failure(Error_Code.UNAUTHENTICATED, "this device was revoked");
    const found = this.sql.exec("SELECT 1 FROM devices WHERE device_id = ? AND user_id = ?", args.deviceId, args.userId).toArray().length > 0;
    if (!found) return failure(Error_Code.NOT_FOUND, "no such device");
    await this.revokeDevice(args.deviceId);
    return ok(null);
  }

  /**
   * Revokes a device: its refresh credential stops working and its open WebSockets close with 4003. The revocation and
   * a notice to every space are stored together, and a notice stays until its space took it, so a space that cannot be
   * reached now gets it on a repeat or from the alarm. The alarm is armed before the first delivery, so the notices are
   * retried even if this object is evicted before it delivers them.
   */
  async revokeDevice(deviceId: string): Promise<void> {
    const nowMs = Date.now();
    this.ctx.storage.transactionSync(() => {
      const revoked = this.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ? AND revoked_at_ms IS NULL", nowMs, deviceId).rowsWritten;
      if (revoked === 0) return;
      this.sql.exec(
        "INSERT INTO revocation_notices (device_id, space_id, next_try_ms) SELECT ?, space_id, ? FROM spaces WHERE true ON CONFLICT DO NOTHING",
        deviceId,
        nowMs + NOTICE_RETRY_MS,
      );
    });
    await this.armAlarm();
    await this.deliverNotices(deviceId);
  }

  /** Delivers the pending revocation notices, of one device or all that are due, and arms the alarm for those that failed. */
  private async deliverNotices(deviceId?: string): Promise<void> {
    const pending = (
      deviceId === undefined
        ? this.sql.exec("SELECT device_id, space_id FROM revocation_notices WHERE next_try_ms <= ?", Date.now())
        : this.sql.exec("SELECT device_id, space_id FROM revocation_notices WHERE device_id = ?", deviceId)
    ).toArray();
    await Promise.all(
      pending.map(async ({ device_id, space_id }) => {
        try {
          await this.spaceStub(String(space_id)).closeDevice(String(device_id));
          this.sql.exec("DELETE FROM revocation_notices WHERE device_id = ? AND space_id = ?", device_id, space_id);
        } catch (error) {
          console.error("revocation notice failed", device_id, space_id, error);
          this.sql.exec("UPDATE revocation_notices SET next_try_ms = ? WHERE device_id = ? AND space_id = ?", Date.now() + NOTICE_RETRY_MS, device_id, space_id);
        }
      }),
    );
    await this.armAlarm();
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

  /**
   * Records chunk key incarnations as retired before the space deletes their objects (RFC 0003 section 5, rule 8).
   * The records live here, out of the Space DO's PITR, and are kept for at least RETIREMENT_KEEP_MS.
   */
  async retireChunks(spaceId: string, objects: { hashHex: string; incarnation: string }[]): Promise<Outcome<null>> {
    const nowMs = Date.now();
    if (this.spaceClosed(spaceId)) return failure(Error_Code.RETRY_LATER, "the space is being rewound");
    this.sql.exec("DELETE FROM chunk_retirements WHERE space_id = ? AND retired_at_ms < ?", spaceId, nowMs - RETIREMENT_KEEP_MS);
    for (const { hashHex, incarnation } of objects) {
      this.sql.exec(
        "INSERT INTO chunk_retirements (space_id, hash_hex, incarnation, retired_at_ms) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        spaceId,
        hashHex,
        incarnation,
        nowMs,
      );
    }
    return ok(null);
  }

  /** checkSession for several devices at once, keyed by device. */
  async checkSessions(deviceIds: string[], spaceId: string): Promise<Record<string, SessionCheck>> {
    return Object.fromEntries(await Promise.all(deviceIds.map(async (deviceId) => [deviceId, await this.checkSession(deviceId, spaceId)])));
  }

  /**
   * Whether the device is active, with the kind and scope of its current row, and whether a rewind holds the space, if
   * one is named, closed (RFC 0003 section 4).
   */
  async checkSession(deviceId: string, spaceId?: string): Promise<SessionCheck> {
    const device = this.activeDevice(deviceId);
    const revoked = this.sql.exec("SELECT 1 FROM devices WHERE device_id = ? AND revoked_at_ms IS NOT NULL", deviceId).toArray().length > 0;
    return {
      active: device !== undefined,
      revoked,
      closed: spaceId !== undefined && this.spaceClosed(spaceId),
      kind: device?.kind ?? "",
      scope: device?.scope ?? null,
    };
  }

  async isSpaceClosed(spaceId: string): Promise<boolean> {
    return this.spaceClosed(spaceId);
  }

  private spaceClosed(spaceId: string): boolean {
    return this.sql.exec("SELECT 1 FROM rewinds WHERE space_id = ? AND phase NOT IN ('completed', 'failed')", spaceId).toArray().length > 0;
  }

  private rewindRow(rewindId: string): RewindRow | undefined {
    return this.sql.exec("SELECT * FROM rewinds WHERE rewind_id = ?", rewindId).toArray()[0] as unknown as RewindRow | undefined;
  }

  private spaceStub(spaceId: string) {
    return this.env.SPACE.get(this.env.SPACE.idFromName(spaceId));
  }

  /**
   * POST /v1/spaces/{space_id}/rewind (RFC 0003 section 1). A known rewind_id answers with that rewind's state; a new one
   * is checked, admitted and driven as far as it goes now. The alarm drives the rest.
   */
  async rewind(args: RewindArgs): Promise<Outcome<RewindResult>> {
    if (!isUuidV7(args.rewindId)) return failure(Error_Code.INVALID_ARGUMENT, "rewind_id must be a UUIDv7");
    const known = this.rewindRow(args.rewindId);
    if (known) {
      if (known.space_id !== args.spaceId || known.restore_point_ms !== args.restorePointMs || known.expected_history_id !== args.expectedHistoryId) {
        return failure(Error_Code.INVALID_ARGUMENT, "this rewind_id was used with other fields");
      }
      await this.drive(args.rewindId);
      return this.rewindOutcome(args.rewindId);
    }

    const nowMs = this.clock();
    const space = this.sql.exec("SELECT owner_id, created_at_ms FROM spaces WHERE space_id = ?", args.spaceId).toArray()[0];
    if (!space || space.owner_id !== args.userId) return failure(Error_Code.FORBIDDEN, "only the space's owner can rewind it");
    const point = args.restorePointMs;
    if (point > nowMs - REWIND_MIN_AGE_MS || point < nowMs - REWIND_MAX_AGE_MS) {
      return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms must be one minute to 29 days in the past");
    }
    if (point < Number(space.created_at_ms)) return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms is before the space was created");
    const closedBefore = this.sql
      .exec("SELECT 1 FROM rewinds WHERE space_id = ? AND ended_at_ms IS NOT NULL AND admitted_at_ms <= ? AND ended_at_ms >= ?", args.spaceId, point, point)
      .toArray();
    if (closedBefore.length > 0) return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms lies inside an earlier rewind of this space");
    if (this.spaceClosed(args.spaceId)) return failure(Error_Code.RETRY_LATER, "another rewind of this space has not ended");

    let info;
    try {
      info = await this.spaceStub(args.spaceId).rewindInfo(point);
    } catch (error) {
      console.error("rewind admission failed", error);
      return failure(Error_Code.RETRY_LATER, "the space could not be read; retry the rewind");
    }
    if (info.historyId !== args.expectedHistoryId) return failure(Error_Code.HISTORY_MISMATCH, "expected_history_id is not the space's current history");
    if (!info.descriptorDurable) return failure(Error_Code.RETRY_LATER, "the space's current history is not exported to R2 yet");
    if (this.rewindRow(args.rewindId) || this.spaceClosed(args.spaceId)) return failure(Error_Code.RETRY_LATER, "another rewind of this space has not ended");

    const count = Number(this.sql.exec("SELECT rewind_count FROM space_rewinds WHERE space_id = ?", args.spaceId).toArray()[0]?.rewind_count ?? 0) + 1;
    this.sql.exec(
      "INSERT INTO space_rewinds (space_id, rewind_count) VALUES (?, ?) ON CONFLICT (space_id) DO UPDATE SET rewind_count = excluded.rewind_count",
      args.spaceId,
      count,
    );
    this.sql.exec(
      `INSERT INTO rewinds (rewind_id, space_id, restore_point_ms, expected_history_id, bookmark, history_id, rewind_number, attempt,
         phase, admitted_at_ms, next_try_ms) VALUES (?, ?, ?, ?, ?, ?, ?, 1, 'admitted', ?, ?)`,
      args.rewindId,
      args.spaceId,
      point,
      args.expectedHistoryId,
      info.bookmark,
      uuidv7(),
      count,
      nowMs,
      Date.now() + REWIND_RETRY_MIN_MS,
    );
    await this.armAlarm();
    await this.drive(args.rewindId);
    return this.rewindOutcome(args.rewindId);
  }

  private rewindOutcome(rewindId: string): Outcome<RewindResult> {
    const row = this.rewindRow(rewindId)!;
    if (row.phase === "completed") return ok(fromBinary(RewindResultSchema, new Uint8Array(row.result!)));
    if (row.phase === "failed") return failure(row.error_code ?? Error_Code.INTERNAL, row.error ?? "the rewind failed");
    return failure(Error_Code.RETRY_LATER, "the rewind is running" + (row.error ? "; last error: " + row.error : ""));
  }

  /**
   * Moves a rewind through its phases (RFC 0003 section 3). Each phase is recorded before the call it starts; an error
   * leaves the recorded phase and schedules the alarm with backoff.
   */
  private async drive(rewindId: string): Promise<void> {
    if (this.driving.has(rewindId)) return;
    this.driving.add(rewindId);
    try {
      for (;;) {
        const row = this.rewindRow(rewindId);
        if (!row || row.phase === "completed" || row.phase === "failed") return;
        if (row.phase === "admitted") {
          const prepared = await this.spaceStub(row.space_id).rewindPrepare({ rewindId, attempt: row.attempt, bookmark: row.bookmark });
          if (!prepared.earlier) return await this.failRewind(row, "the selected restore point is not earlier than the space's current state");
          this.sql.exec(
            `UPDATE rewinds SET phase = 'prepared', previous_history_id = COALESCE(previous_history_id, ?),
               previous_head_seq = COALESCE(previous_head_seq, ?) WHERE rewind_id = ?`,
            prepared.historyId,
            prepared.headSeq,
            rewindId,
          );
        } else if (row.phase === "prepared") {
          this.sql.exec("UPDATE rewinds SET phase = 'arming' WHERE rewind_id = ?", rewindId);
          try {
            await this.spaceStub(row.space_id).rewindArm(row.bookmark);
          } catch {
            // Arming restarts the Space DO, so the call ends in an error either way; settling decides what happened.
          }
        } else {
          try {
            await this.spaceStub(row.space_id).restart();
          } catch {
            // The restart ends the call; any pending restore has applied once the object starts again.
          }
          const nowMs = this.clock();
          const completion = await this.spaceStub(row.space_id).rewindComplete({
            rewindId,
            attempt: row.attempt,
            historyId: row.history_id,
            rewindNumber: row.rewind_number,
            restorePointMs: row.restore_point_ms,
            completedAtMs: nowMs,
            revokedDevices: this.sql.exec("SELECT device_id FROM devices WHERE revoked_at_ms IS NOT NULL").toArray().map((r) => String(r.device_id)),
            retired: this.sql
              .exec("SELECT hash_hex, incarnation FROM chunk_retirements WHERE space_id = ?", row.space_id)
              .toArray()
              .map((r) => ({ hashHex: String(r.hash_hex), incarnation: String(r.incarnation) })),
          });
          if (completion.state === "marker") {
            if (nowMs > row.restore_point_ms + PITR_WINDOW_MS) {
              return await this.failRewind(row, "the restore point left the PITR window before the restore ran");
            }
            this.sql.exec("UPDATE rewinds SET phase = 'admitted', attempt = attempt + 1 WHERE rewind_id = ?", rewindId);
            throw new Error("the restore did not run; preparing another attempt");
          }
          const result = create(RewindResultSchema, {
            historyId: row.history_id,
            restoreSeq: BigInt(completion.receipt.restoreSeq),
            restorePointMs: BigInt(row.restore_point_ms),
            previousHistoryId: row.previous_history_id ?? "",
            previousHeadSeq: BigInt(row.previous_head_seq ?? 0),
          });
          this.sql.exec(
            "UPDATE rewinds SET phase = 'completed', result = ?, error = NULL, ended_at_ms = ?, next_try_ms = NULL WHERE rewind_id = ?",
            toBinary(RewindResultSchema, result),
            nowMs,
            rewindId,
          );
          this.sql.exec("UPDATE space_rewinds SET completed_at_ms = ? WHERE space_id = ?", completion.receipt.completedAtMs, row.space_id);
          return;
        }
      }
    } catch (error) {
      const row = this.rewindRow(rewindId)!;
      const delay = Math.min(REWIND_RETRY_MAX_MS, REWIND_RETRY_MIN_MS * 2 ** Math.min(row.tries, 20));
      console.error("rewind phase failed", rewindId, row.phase, error);
      this.sql.exec(
        "UPDATE rewinds SET tries = tries + 1, error = ?, next_try_ms = ? WHERE rewind_id = ?",
        error instanceof Error ? error.message : String(error),
        Date.now() + delay,
        rewindId,
      );
    } finally {
      this.driving.delete(rewindId);
      await this.armAlarm();
    }
  }

  /** Step 6: only when no restore can have happened or be pending. Removes the markers and reopens the space unchanged. */
  private async failRewind(row: RewindRow, reason: string): Promise<void> {
    await this.spaceStub(row.space_id).rewindAbort(row.rewind_id);
    this.sql.exec(
      "UPDATE rewinds SET phase = 'failed', error_code = ?, error = ?, ended_at_ms = ?, next_try_ms = NULL WHERE rewind_id = ?",
      Error_Code.INVALID_ARGUMENT,
      reason,
      this.clock(),
      row.rewind_id,
    );
  }

  /**
   * Sets the alarm for the next due rewind or revocation notice, at least a second ahead so that work done right now is
   * not retried in a loop.
   */
  private async armAlarm(): Promise<void> {
    const rewind = this.sql.exec("SELECT MIN(next_try_ms) AS due FROM rewinds WHERE phase NOT IN ('completed', 'failed')").one().due;
    const notice = this.sql.exec("SELECT MIN(next_try_ms) AS due FROM revocation_notices").one().due;
    const due = [rewind, notice].filter((value) => value !== null).map(Number);
    if (due.length === 0) await this.ctx.storage.deleteAlarm();
    else await this.ctx.storage.setAlarm(Math.max(Math.min(...due), Date.now() + REWIND_RETRY_MIN_MS));
  }

  override async alarm(): Promise<void> {
    const due = this.sql
      .exec("SELECT rewind_id FROM rewinds WHERE phase NOT IN ('completed', 'failed') AND (next_try_ms IS NULL OR next_try_ms <= ?)", Date.now())
      .toArray();
    for (const { rewind_id } of due) await this.drive(String(rewind_id));
    await this.deliverNotices();
  }

  async listSpaces(userId: string): Promise<SpaceInfo[]> {
    const found = this.sql.exec("SELECT * FROM spaces WHERE owner_id = ? ORDER BY created_at_ms", userId).toArray();
    return (found as unknown as SpaceRow[]).map(spaceInfo);
  }

  /** Records a read-only share link with a 128-bit random id. */
  async createShareLink(args: { spaceId: string; nodeId: string; createdBy: string; expiresAtMs?: number; password?: string }): Promise<string> {
    return this.shares.create(args);
  }

  async revokeShareLink(linkId: string): Promise<void> {
    this.shares.revoke(linkId);
  }

  /** Resolves a share link, checking expiry, revocation and the password, and locking it after repeated failures. */
  async openShareLink(linkId: string, password: string | undefined): Promise<Outcome<{ spaceId: string; nodeId: string }>> {
    return this.shares.open(linkId, password);
  }

  /** The owner API and public API of RFC 0005; see ShareLinks. */
  async shareCreate(args: Parameters<ShareLinks["ownerCreate"]>[0]): Promise<ShareOutcome<ShareLink>> {
    return this.shares.ownerCreate(args, () => this.spaceStub(args.spaceId).shareCheckNode(args.nodeId));
  }

  async shareList(args: Parameters<ShareLinks["ownerList"]>[0]): Promise<ShareOutcome<ShareLinkList>> {
    return this.shares.ownerList(args);
  }

  async shareRevoke(args: Parameters<ShareLinks["ownerRevoke"]>[0]): Promise<ShareOutcome<null>> {
    return this.shares.ownerRevoke(args);
  }

  async shareResolve(args: Parameters<ShareLinks["resolve"]>[0]): Promise<ShareOutcome<ResolvedLink>> {
    return this.shares.resolve(args);
  }

  async shareUnlock(args: Parameters<ShareLinks["unlock"]>[0]): Promise<ShareOutcome<{ session?: string }>> {
    return this.shares.unlock(args);
  }
}

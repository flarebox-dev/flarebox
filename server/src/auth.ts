// SPDX-License-Identifier: AGPL-3.0-or-later

import { base64url, fromBase64url, hmac, hmacVerify } from "./crypto";

export const ACCESS_TOKEN_MS = 15 * 60_000;

export interface ServerKey {
  kid: string;
  secret: Uint8Array;
}

export interface Identity {
  userId: string;
  deviceId: string;
  deviceName: string;
  expiresAtMs: number;
  kind: string;
  scope: string | null;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export async function signAccessToken(key: ServerKey, identity: Identity): Promise<string> {
  const payload = base64url(
    encoder.encode(
      JSON.stringify({ u: identity.userId, d: identity.deviceId, n: identity.deviceName, e: identity.expiresAtMs, k: identity.kind, s: identity.scope }),
    ),
  );
  const signed = `v1.${key.kid}.${payload}`;
  return `${signed}.${base64url(await hmac(key.secret, signed))}`;
}

export async function verifyAccessToken(key: ServerKey, token: string, nowMs: number): Promise<Identity | undefined> {
  const parts = token.split(".");
  if (parts.length !== 4 || parts[0] !== "v1" || parts[1] !== key.kid) return undefined;
  const mac = fromBase64url(parts[3]!);
  const payload = fromBase64url(parts[2]!);
  if (!mac || !payload || !(await hmacVerify(key.secret, parts.slice(0, 3).join("."), mac))) return undefined;
  const claims = JSON.parse(decoder.decode(payload)) as { u: string; d: string; n: string; e: number; k?: string; s?: string | null };
  if (claims.e < nowMs) return undefined;
  return { userId: claims.u, deviceId: claims.d, deviceName: claims.n, expiresAtMs: claims.e, kind: claims.k ?? "device", scope: claims.s ?? null };
}

export function bearerToken(request: Request): string | undefined {
  const header = request.headers.get("Authorization");
  return header?.startsWith("Bearer ") ? header.slice(7).trim() : undefined;
}

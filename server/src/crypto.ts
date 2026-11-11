// SPDX-License-Identifier: AGPL-3.0-or-later

const encoder = new TextEncoder();

export async function sha256(data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", data));
}

export function base64url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

export function fromBase64url(text: string): Uint8Array | undefined {
  if (!/^[A-Za-z0-9_-]*$/.test(text)) return undefined;
  try {
    const binary = atob(text.replaceAll("-", "+").replaceAll("_", "/"));
    return Uint8Array.from(binary, (c) => c.charCodeAt(0));
  } catch {
    return undefined;
  }
}

function importHmacKey(secret: Uint8Array): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", secret, { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
}

export async function hmac(secret: Uint8Array, message: string): Promise<Uint8Array> {
  const key = await importHmacKey(secret);
  return new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(message)));
}

export async function hmacVerify(secret: Uint8Array, message: string, mac: Uint8Array): Promise<boolean> {
  const key = await importHmacKey(secret);
  return crypto.subtle.verify("HMAC", key, mac, encoder.encode(message));
}

/** The per-space secret for chunk tickets and upload receipts, derived from a server key. */
export function deriveSpaceKey(serverKey: Uint8Array, spaceId: string): Promise<Uint8Array> {
  return hmac(serverKey, `koidrive/space-key/v1|${spaceId}`);
}

export type ChunkGrant = "put" | "get" | "receipt";

/** The chunk a grant covers: one incarnation of one hash in one space (RFC 0003 section 5, rule 7). */
export interface ChunkObject {
  spaceId: string;
  hashHex: string;
  incarnation: string;
}

function grantMessage(grant: ChunkGrant, chunk: ChunkObject, size: number, expiresAtMs: number): string {
  return `${grant}|${chunk.spaceId}|${chunk.hashHex}|${chunk.incarnation}|${size}|${expiresAtMs}`;
}

/**
 * Signs a chunk ticket (put, get) or upload receipt as "v1.<expires_at_ms>.<mac>".
 * Get tickets pass size 0 because a download does not depend on it.
 */
export async function signChunkGrant(spaceKey: Uint8Array, grant: ChunkGrant, chunk: ChunkObject, size: number, expiresAtMs: number): Promise<string> {
  const mac = await hmac(spaceKey, grantMessage(grant, chunk, size, expiresAtMs));
  return `v1.${expiresAtMs}.${base64url(mac)}`;
}

export async function verifyChunkGrant(
  spaceKey: Uint8Array,
  grant: ChunkGrant,
  chunk: ChunkObject,
  size: number,
  token: string,
  nowMs: number,
): Promise<boolean> {
  const [version, expires, macText, ...rest] = token.split(".");
  if (version !== "v1" || rest.length > 0 || !expires || !/^\d+$/.test(expires) || macText === undefined) return false;
  const expiresAtMs = Number(expires);
  if (expiresAtMs < nowMs) return false;
  const mac = fromBase64url(macText);
  if (!mac) return false;
  return hmacVerify(spaceKey, grantMessage(grant, chunk, size, expiresAtMs), mac);
}

// SPDX-License-Identifier: AGPL-3.0-or-later

const DAY_MS = 86_400_000;

/**
 * The runtime reports these limits only in the error message. SQLITE_FULL is the text the Durable Objects limits FAQ
 * documents and workerd's SQLite wrapper builds. The free-tier text is not documented: Cloudflare's pricing page says
 * only that the operation fails with an error, and the four kinds come from user reports.
 */
const STORAGE_FULL = /\bdatabase or disk is full: SQLITE_FULL\b/;
const FREE_TIER_DAILY = /\bExceeded allowed (?:rows written|rows read|duration|volume of requests) in Durable Objects free tier\b/;

export interface QuotaRefusal {
  message: string;
  retryAfterMs: number;
}

/**
 * The QUOTA_EXCEEDED answer for an error that reports a platform limit, with a wait until the next 00:00 UTC (spec v0
 * section 13), or undefined for any other error. Free-plan daily limits reset then; a full storage needs the owner to
 * act, so a client tries again once a day instead of in a loop.
 */
export function quotaRefusal(error: unknown, nowMs: number): QuotaRefusal | undefined {
  const text = error instanceof Error ? error.message : String(error);
  const retryAfterMs = DAY_MS - (nowMs % DAY_MS);
  if (FREE_TIER_DAILY.test(text)) return { message: "the server reached a Workers Free daily limit; it resets at 00:00 UTC", retryAfterMs };
  if (STORAGE_FULL.test(text)) return { message: "the server's storage is full", retryAfterMs };
  return undefined;
}

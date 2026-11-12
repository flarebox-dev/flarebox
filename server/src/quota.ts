// SPDX-License-Identifier: AGPL-3.0-or-later

const DAY_MS = 86_400_000;
const RESET_LAG_MS = 30 * 60_000;
const RESET_LAG_RETRY_MS = 5 * 60_000;

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
 * The wait runs until the next 00:00 UTC, when free-plan daily limits reset, except in the first 30 minutes after it,
 * because the errors can outlast the reset.
 */
export function quotaRefusal(error: unknown, nowMs: number): QuotaRefusal | undefined {
  const text = error instanceof Error ? error.message : String(error);
  const sinceMidnightMs = nowMs % DAY_MS;
  const retryAfterMs = sinceMidnightMs < RESET_LAG_MS ? RESET_LAG_RETRY_MS : DAY_MS - sinceMidnightMs;
  if (FREE_TIER_DAILY.test(text)) return { message: "the server reached a Workers Free daily limit; it resets at 00:00 UTC", retryAfterMs };
  if (STORAGE_FULL.test(text)) return { message: "the server's storage is full", retryAfterMs };
  return undefined;
}

// SPDX-License-Identifier: AGPL-3.0-or-later

const MINUTE_MS = 60_000;
const HOUR_MS = 3_600_000;
/** Public API requests one client may make per minute (RFC 0005 section 5). */
export const REQUESTS_PER_MINUTE = 300;
/** Lookups of unknown, revoked or expired links one client may make per hour before every request is refused. */
export const MISSES_PER_HOUR = 30;
/** Clients tracked at once. */
export const MAX_CLIENTS = 10_000;

interface Counts {
  minute: number;
  requests: number;
  hour: number;
  misses: number;
}

/**
 * Fixed-window counts of public share-link requests per client, kept in memory by the Account DO. They are exact while
 * the object runs, because one object sees every request, and start over when it restarts.
 */
export class ClientLimiter {
  private readonly clients = new Map<string, Counts>();

  /** Counts one request. Returns how long to wait when the client is over a limit, or undefined when it may go on. */
  request(client: string, nowMs: number): number | undefined {
    const counts = this.counts(client, nowMs);
    if (counts.misses >= MISSES_PER_HOUR) return (counts.hour + 1) * HOUR_MS - nowMs;
    counts.requests++;
    if (counts.requests > REQUESTS_PER_MINUTE) return (counts.minute + 1) * MINUTE_MS - nowMs;
    return undefined;
  }

  /** Counts a lookup of a link that is unknown, revoked or expired. */
  miss(client: string, nowMs: number): void {
    this.counts(client, nowMs).misses++;
  }

  /** The client's counts, with windows that have ended reset, moved to the newest position in the table. */
  private counts(client: string, nowMs: number): Counts {
    const minute = Math.floor(nowMs / MINUTE_MS);
    const hour = Math.floor(nowMs / HOUR_MS);
    let counts = this.clients.get(client);
    this.clients.delete(client);
    if (!counts) {
      this.makeRoom(minute, hour);
      counts = { minute, requests: 0, hour, misses: 0 };
    }
    if (counts.minute !== minute) Object.assign(counts, { minute, requests: 0 });
    if (counts.hour !== hour) Object.assign(counts, { hour, misses: 0 });
    this.clients.set(client, counts);
    return counts;
  }

  /** Drops clients whose windows have all ended, then the oldest, until a new client fits. */
  private makeRoom(minute: number, hour: number): void {
    if (this.clients.size < MAX_CLIENTS) return;
    for (const [client, counts] of this.clients) {
      if (counts.minute < minute && counts.hour < hour) this.clients.delete(client);
    }
    for (const client of this.clients.keys()) {
      if (this.clients.size < MAX_CLIENTS) break;
      this.clients.delete(client);
    }
  }
}

/** The rate-limit key of a request: its client IP address, or the /64 prefix of an IPv6 address. */
export function clientKey(request: Request): string {
  const ip = request.headers.get("CF-Connecting-IP") ?? "";
  if (!ip.includes(":")) return ip;
  const [head = "", tail] = ip.toLowerCase().split("::");
  const groups = (part: string) => (part ? part.split(":").flatMap((g) => (g.includes(".") ? ["0", "0"] : [g])) : []);
  const front = groups(head);
  const back = tail === undefined ? [] : groups(tail);
  const all = [...front, ...Array(Math.max(0, 8 - front.length - back.length)).fill("0"), ...back];
  if (all.length !== 8 || !all.every((g) => /^[0-9a-f]{1,4}$/.test(g))) return ip;
  return all.slice(0, 4).map((g) => parseInt(g, 16).toString(16)).join(":") + "::/64";
}

// SPDX-License-Identifier: AGPL-3.0-or-later

const MINUTE_MS = 60_000;
const HOUR_MS = 3_600_000;
export const REQUESTS_PER_MINUTE = 300;
export const MISSES_PER_HOUR = 30;
export const MAX_CLIENTS = 10_000;

interface Counts {
  minute: number;
  requests: number;
  hour: number;
  misses: number;
}

export class ClientLimiter {
  private readonly clients = new Map<string, Counts>();

  request(client: string, nowMs: number): number | undefined {
    const counts = this.counts(client, nowMs);
    if (counts.misses >= MISSES_PER_HOUR) return (counts.hour + 1) * HOUR_MS - nowMs;
    counts.requests++;
    if (counts.requests > REQUESTS_PER_MINUTE) return (counts.minute + 1) * MINUTE_MS - nowMs;
    return undefined;
  }

  miss(client: string, nowMs: number): void {
    this.counts(client, nowMs).misses++;
  }

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

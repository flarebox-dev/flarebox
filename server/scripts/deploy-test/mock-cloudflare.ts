// SPDX-License-Identifier: AGPL-3.0-or-later

// An in-memory stand-in for the parts of the Cloudflare API that run.ts and Wrangler's tail use, plus the two Worker
// routes the claim calls, for run.unit.ts. Point CLOUDFLARE_API_BASE_URL at url + "/client/v4" and the state's server
// URL at url. The tail is a minimal WebSocket server that pushes one request event whose log line carries the claim
// code each time /setup is requested. POST /mock/workers/<name> creates a Worker from another process, as a concurrent
// creator or Wrangler would.

import { createHash } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { Socket } from "node:net";

export interface MockWorker {
  id: string;
}

export interface MockBucket {
  creationDate: string;
  objects: Map<string, number>;
}

export class MockCloudflare {
  readonly workers = new Map<string, MockWorker>();
  readonly buckets = new Map<string, MockBucket>();
  readonly namespaces: { id: string; class: string; script: string }[] = [];
  /** Every API request as "METHOD /path", without the query string. */
  readonly requests: string[] = [];
  /** The status of a bucket creation; 409 also creates the bucket, as a concurrent creator would. */
  createBucketStatus = 200;
  claimStatus = 200;
  readonly claimCode = "ZQ7K-M2XR-9WPD";
  url = "";
  private server: Server | undefined;
  private readonly tails = new Set<Socket>();

  async start(): Promise<string> {
    this.server = createServer((request, response) => this.handle(request, response));
    this.server.on("upgrade", (request: IncomingMessage, socket: Socket) => this.upgrade(request, socket));
    await new Promise<void>((resolve) => this.server!.listen(0, "127.0.0.1", resolve));
    const address = this.server.address();
    this.url = "http://127.0.0.1:" + (typeof address === "object" && address ? address.port : 0);
    return this.url;
  }

  async stop(): Promise<void> {
    for (const socket of this.tails) socket.destroy();
    this.server?.closeAllConnections();
    await new Promise<void>((resolve) => this.server?.close(() => resolve()) ?? resolve());
  }

  /** A Worker with its two Durable Object namespaces, as a deploy would leave it. */
  addWorker(name: string, id: string = crypto.randomUUID()): MockWorker {
    const worker = { id };
    this.workers.set(name, worker);
    this.namespaces.push({ id: id + "-account", class: "AccountDO", script: name }, { id: id + "-space", class: "SpaceDO", script: name });
    return worker;
  }

  addBucket(name: string, creationDate = new Date().toISOString(), keys: string[] = []): MockBucket {
    const bucket = { creationDate, objects: new Map(keys.map((key) => [key, 10])) };
    this.buckets.set(name, bucket);
    return bucket;
  }

  deletes(): string[] {
    return this.requests.filter((request) => request.startsWith("DELETE ") && !request.includes("/tails/"));
  }

  private upgrade(request: IncomingMessage, socket: Socket): void {
    const key = String(request.headers["sec-websocket-key"] ?? "");
    const accept = createHash("sha1").update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\nSec-WebSocket-Protocol: trace-v1\r\n\r\n",
    );
    socket.on("data", () => {});
    socket.on("error", () => {});
    socket.on("close", () => this.tails.delete(socket));
    this.tails.add(socket);
  }

  private pushClaimCode(url: string): void {
    const event = JSON.stringify({
      outcome: "ok",
      scriptName: "mock",
      eventTimestamp: Date.now(),
      event: { request: { url, method: "GET", headers: {} } },
      logs: [{ message: ["Flarebox claim code (valid for 1 hour): " + this.claimCode], level: "log", timestamp: Date.now() }],
      exceptions: [],
    });
    const payload = Buffer.from(event);
    const header =
      payload.length < 126
        ? Buffer.from([0x81, payload.length])
        : Buffer.from([0x81, 126, payload.length >> 8, payload.length & 0xff]);
    for (const socket of this.tails) socket.write(Buffer.concat([header, payload]));
  }

  private handle(request: IncomingMessage, response: ServerResponse): void {
    const path = (request.url ?? "/").split("?")[0]!;
    const method = request.method ?? "GET";
    let body = "";
    request.on("data", (chunk: Buffer) => (body += chunk));
    request.on("end", () => {
      const send = (status: number, value: unknown) => {
        response.writeHead(status, { "Content-Type": "application/json" });
        response.end(JSON.stringify(value));
      };
      const ok = (result: unknown) => send(200, { success: true, errors: [], messages: [], result });
      const missing = () => send(404, { success: false, errors: [{ code: 10007, message: "not found" }], messages: [], result: null });
      let match: RegExpExecArray | null;

      if (path === "/setup") {
        this.pushClaimCode(this.url + "/setup");
        response.writeHead(200, { "Content-Type": "text/plain" });
        response.end("A one-time claim code, valid for one hour, is in this Worker's logs.\n\nFlarebox is not affiliated with Cloudflare, Inc.\n");
        return;
      }
      if ((match = /^\/mock\/workers\/([^/]+)$/.exec(path)) && method === "POST") return send(200, this.addWorker(match[1]!));
      if (path === "/v1/claim") {
        const claimed = JSON.parse(body) as { code?: string };
        if (this.claimStatus !== 200 || claimed.code !== this.claimCode) return send(403, { code: "CODE_FORBIDDEN", message: "wrong claim code" });
        return send(200, { userId: "user-1", deviceId: "device-1", refreshCredential: "refresh-1", access: { token: "t", expiresAtMs: "0" } });
      }

      this.requests.push(method + " " + path);
      const api = /^\/client\/v4\/accounts\/[^/]+(\/.*)$/.exec(path)?.[1];
      if (!api) return missing();
      if ((match = /^\/workers\/workers\/([^/]+)$/.exec(api)) && method === "GET") {
        const worker = this.workers.get(match[1]!);
        return worker ? ok({ id: worker.id, name: match[1], created_on: new Date().toISOString() }) : missing();
      }
      if ((match = /^\/workers\/scripts\/([^/]+)$/.exec(api)) && method === "DELETE") {
        if (!this.workers.delete(match[1]!)) return missing();
        const name = match[1];
        for (let i = this.namespaces.length - 1; i >= 0; i--) if (this.namespaces[i]!.script === name) this.namespaces.splice(i, 1);
        return ok(null);
      }
      if ((match = /^\/workers\/scripts\/([^/]+)\/tails$/.exec(api)) && method === "POST") {
        return ok({ id: "tail-1", url: this.url.replace("http", "ws") + "/tail", expires_at: new Date(Date.now() + 3_600_000).toISOString() });
      }
      if (/^\/workers\/scripts\/[^/]+\/tails\/[^/]+$/.test(api) && method === "DELETE") return ok(null);
      if (api === "/workers/durable_objects/namespaces" && method === "GET") {
        return send(200, { success: true, errors: [], messages: [], result: this.namespaces, result_info: { page: 1, total_pages: 1 } });
      }
      if (api === "/r2/buckets" && method === "POST") {
        const { name } = JSON.parse(body) as { name: string };
        if (this.createBucketStatus === 409) {
          this.addBucket(name);
          return send(409, { success: false, errors: [{ code: 10004, message: "The bucket you tried to create already exists" }], messages: [], result: null });
        }
        this.addBucket(name);
        return ok({ name });
      }
      if ((match = /^\/r2\/buckets\/([^/]+)$/.exec(api))) {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket) return missing();
        if (method === "GET") return ok({ name: match[1], creation_date: bucket.creationDate });
        if (method === "DELETE") {
          if (bucket.objects.size > 0) return send(409, { success: false, errors: [{ code: 10008, message: "The bucket you tried to delete is not empty" }], messages: [], result: null });
          this.buckets.delete(match[1]!);
          return ok(null);
        }
      }
      if ((match = /^\/r2\/buckets\/([^/]+)\/objects$/.exec(api)) && method === "GET") {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket) return missing();
        return send(200, { success: true, errors: [], messages: [], result: [...bucket.objects].map(([key, size]) => ({ key, size })), result_info: { is_truncated: false } });
      }
      if ((match = /^\/r2\/buckets\/([^/]+)\/objects\/(.+)$/.exec(api)) && method === "DELETE") {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket?.objects.delete(decodeURIComponent(match[2]!))) return missing();
        return ok(null);
      }
      return missing();
    });
  }
}

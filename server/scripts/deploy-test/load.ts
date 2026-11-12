// SPDX-License-Identifier: AGPL-3.0-or-later

import { appendFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { monitorEventLoopDelay } from "node:perf_hooks";
import { create, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { afterAll, describe, expect, it } from "vitest";
import { SpaceInfoSchema, SpaceListSchema } from "../../src/gen/koidrive/v1/auth_pb";
import { type CommitResult, OpResult_Code } from "../../src/gen/koidrive/v1/commit_pb";
import { ClientMessageSchema, type RpcResponse, ServerMessageSchema } from "../../src/gen/koidrive/v1/messages_pb";
import type { Content } from "../../src/gen/koidrive/v1/model_pb";
import { Error_Code } from "../../src/gen/koidrive/v1/rpc_pb";
import { Device, fileData, op, randomBytes, Server, SpaceClient, sleep, stats, uuidv7 } from "./client";
import { type Account, accountFromEnv, graphql, type Namespace, namespacesOf } from "./cloudflare.ts";
import {
  commitBudget,
  commitsAt,
  emptyCounts,
  FALLBACK_ROWS_PER_COMMIT,
  layout,
  readConfig,
  saturated,
  type StepCounts,
  type StepSummary,
  summarize,
} from "./load-model.ts";
import { addStats, readState } from "./state.ts";

const COMMIT_TIMEOUT_MS = 60_000;
const TOKEN_CHECK_MS = 30_000;
const PROBE_EVERY_MS = 2_000;
const CALIBRATION_WRITERS = 5;
const CALIBRATION_MS = 120_000;
const ANALYTICS_LAG_MS = 3 * 60_000;
const CALIBRATION_WAIT_MS = ANALYTICS_LAG_MS + 10 * 60_000;
const CALIBRATION_READ_EVERY_MS = 60_000;
const STORAGE_WAIT_MS = 20 * 60_000;

const config = readConfig(process.env);
const state = readState();
const server = new Server(state.url ?? "http://unset.invalid");
const account = accountFromEnv();

let measuring: StepCounts | undefined;
let committed = 0;
let budget = Number.POSITIVE_INFINITY;
let stopping = false;
const timeline: [number, number][] = [];

function count(record: Record<string, number>, key: string): void {
  record[key] = (record[key] ?? 0) + 1;
}

class Timeout extends Error {}

type ClientMsg = MessageInitShape<typeof ClientMessageSchema>["msg"];
type Answer = (response: RpcResponse | Error) => void;

class Session {
  readonly device: Device;
  readonly spaceId: string;
  readonly live: boolean;
  historyId = "";
  headSeq = 0n;
  deviceSeq = 0n;
  private socket: WebSocket | undefined;
  private waiting = new Map<bigint, Answer>();
  private nextReq = 1n;
  private sentToken = "";
  private tokenCheckedAtMs = 0;
  private readonly closedHere = new WeakSet<WebSocket>();

  constructor(device: Device, spaceId: string, live: boolean) {
    this.device = device;
    this.spaceId = spaceId;
    this.live = live;
  }

  get connected(): boolean {
    return this.socket?.readyState === WebSocket.OPEN;
  }

  async connect(): Promise<void> {
    const token = await this.device.token();
    const socket = new WebSocket(server.base.replace(/^http/, "ws") + "/v1/spaces/" + this.spaceId + "/ws", { headers: { Authorization: "Bearer " + token } });
    stats.requests++;
    socket.binaryType = "arraybuffer";
    const waiting = new Map<bigint, Answer>();
    this.socket = socket;
    this.waiting = waiting;
    this.sentToken = token;
    this.tokenCheckedAtMs = Date.now();
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error("no Welcome within 30 s"));
        this.close();
      }, 30_000);
      const settle = (error?: Error) => {
        clearTimeout(timer);
        if (error) reject(error);
        else resolve();
      };
      socket.addEventListener("open", () => this.send(socket, { case: "hello", value: { clientName: "koidrive-load-test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } }));
      socket.addEventListener("message", (event) => {
        if (!(event.data instanceof ArrayBuffer)) return;
        stats.bytesDown += event.data.byteLength;
        const msg = fromBinary(ServerMessageSchema, new Uint8Array(event.data)).msg;
        if (msg.case === "rpc") {
          const answer = waiting.get(msg.value.reqId);
          waiting.delete(msg.value.reqId);
          answer?.(msg.value);
        } else if (msg.case === "changes") {
          const now = Date.now();
          for (const record of msg.value.commits) {
            if (record.seq > this.headSeq) this.headSeq = record.seq;
            measuring?.pushLagMs.push(now - Number(record.committedAtMs));
          }
        } else if (msg.case === "welcome") {
          this.historyId = msg.value.historyId;
          this.headSeq = msg.value.headSeq;
          this.deviceSeq = msg.value.deviceSeq;
          if (this.live) this.send(socket, { case: "resume", value: { historyId: this.historyId, sinceSeq: this.headSeq } });
          else settle();
        } else if (msg.case === "caughtUp") {
          settle();
        } else if (msg.case === "resyncRequired") {
          settle(new Error("the server asked for a resync (reason " + msg.value.reason + ")"));
          this.close();
        }
      });
      socket.addEventListener("close", (event) => {
        if (this.socket === socket) this.socket = undefined;
        if (measuring && !stopping && !this.closedHere.has(socket)) count(measuring.closes, String(event.code));
        for (const answer of waiting.values()) answer(new Error("the WebSocket closed with " + event.code));
        waiting.clear();
        settle(new Error("the WebSocket closed with " + event.code + " " + event.reason));
      });
      socket.addEventListener("error", () => settle(new Error("the WebSocket to " + this.spaceId + " failed")));
    });
  }

  async keepToken(): Promise<void> {
    if (Date.now() - this.tokenCheckedAtMs < TOKEN_CHECK_MS) return;
    this.tokenCheckedAtMs = Date.now();
    const token = await this.device.token();
    if (token === this.sentToken || !this.socket || !this.connected) return;
    this.send(this.socket, { case: "reauth", value: { accessToken: token } });
    this.sentToken = token;
  }

  commit(nodeId: string, content: Content): Promise<RpcResponse> {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return Promise.reject(new Error("not connected"));
    const waiting = this.waiting;
    const reqId = this.nextReq++;
    const answer = new Promise<RpcResponse>((resolve, reject) => {
      const timer = setTimeout(() => {
        waiting.delete(reqId);
        reject(new Timeout("no answer within " + COMMIT_TIMEOUT_MS / 1000 + " s"));
      }, COMMIT_TIMEOUT_MS);
      waiting.set(reqId, (response) => {
        clearTimeout(timer);
        if (response instanceof Error) reject(response);
        else resolve(response);
      });
    });
    const commit = { commitId: uuidv7(), baseSeq: this.headSeq, ops: [op.update(nodeId, content)], historyId: this.historyId, deviceSeq: this.deviceSeq + 1n };
    this.send(socket, { case: "rpc", value: { reqId, call: { case: "commit", value: commit } } });
    return answer;
  }

  close(): void {
    const socket = this.socket;
    this.socket = undefined;
    if (!socket) return;
    this.closedHere.add(socket);
    socket.close(1000);
  }

  private send(socket: WebSocket, msg: ClientMsg): void {
    const frame = toBinary(ClientMessageSchema, create(ClientMessageSchema, { msg }));
    stats.bytesUp += frame.byteLength;
    socket.send(frame);
  }
}

interface SpaceRun {
  spaceId: string;
  contents: [Content, Content];
  nodeIds: string[];
  setupCommits: number;
}

interface Writer {
  session: Session;
  nodeId: string;
  contents: [Content, Content];
  active: boolean;
  loop?: Promise<void>;
}

async function write(writer: Writer): Promise<void> {
  const { session } = writer;
  let next = 0;
  let backoffMs = 1000;
  while (writer.active && !stopping && committed < budget) {
    if (!session.connected) {
      try {
        await session.connect();
        backoffMs = 1000;
      } catch {
        if (measuring) count(measuring.closes, "connect failed");
        await sleep(backoffMs);
        backoffMs = Math.min(backoffMs * 2, 15_000);
      }
      continue;
    }
    try {
      await session.keepToken();
    } catch {
      if (measuring) count(measuring.closes, "token refresh failed");
      session.close();
      await sleep(backoffMs);
      backoffMs = Math.min(backoffMs * 2, 15_000);
      continue;
    }
    const started = performance.now();
    let response: RpcResponse;
    try {
      response = await session.commit(writer.nodeId, writer.contents[next]!);
    } catch (error) {
      if (measuring) {
        if (error instanceof Timeout) measuring.timeouts++;
        else count(measuring.refused, "no answer before the WebSocket closed");
      }
      session.close();
      continue;
    }
    const ms = performance.now() - started;
    const result = response.result;
    if (result.case === "commit") {
      committed++;
      next = 1 - next;
      session.deviceSeq += 1n;
      if (result.value.seq > session.headSeq) session.headSeq = result.value.seq;
      if (measuring) {
        measuring.accepted++;
        measuring.latenciesMs.push(ms);
        for (const outcome of result.value.results) if (outcome.code !== OpResult_Code.APPLIED) count(measuring.refused, "op " + OpResult_Code[outcome.code]);
      }
    } else if (result.case === "error") {
      const code = result.value.code;
      if (measuring) count(measuring.refused, Error_Code[code] ?? String(code));
      if (code === Error_Code.RETRY_LATER || code === Error_Code.QUOTA_EXCEEDED) await sleep(Math.min(Math.max(Number(result.value.retryAfterMs), 1000), 10_000));
      else session.close();
    } else {
      if (measuring) count(measuring.refused, "answer " + result.case);
      session.close();
    }
  }
}

async function subscribe(session: Session): Promise<void> {
  while (!stopping) {
    try {
      if (!session.connected) await session.connect();
      else await session.keepToken();
    } catch {
      if (measuring) count(measuring.closes, session.connected ? "token refresh failed" : "connect failed");
      session.close();
    }
    await sleep(1000);
  }
}

async function probe(owner: Device): Promise<void> {
  while (!stopping) {
    const started = performance.now();
    try {
      await server.json("GET", "/v1/spaces", SpaceListSchema, { token: await owner.token() });
      measuring?.probeMs.push(performance.now() - started);
    } catch {
      if (measuring) measuring.probeErrors++;
    }
    await sleep(PROBE_EVERY_MS);
  }
}

function applied(result: CommitResult): void {
  const codes = result.results.map((outcome) => OpResult_Code[outcome.code]);
  if (codes.some((code) => code !== "APPLIED")) throw new Error("a setup commit was not applied: " + codes.join(", "));
}

async function makeSpace(owner: Device, index: number, files: number): Promise<SpaceRun> {
  const created = await server.json("POST", "/v1/spaces", SpaceInfoSchema, { body: { name: "Load test " + (index + 1) + " " + uuidv7().slice(-8) }, token: await owner.token(), status: 201 });
  const client = new SpaceClient(owner, created.spaceId);
  await client.welcome();
  const a = await fileData([randomBytes(1024)]);
  const b = await fileData([randomBytes(1024)]);
  const { receipts } = await client.upload(a, b);
  applied(await client.commit([op.create(uuidv7(), created.spaceId, "seed-a.bin", a.content), op.create(uuidv7(), created.spaceId, "seed-b.bin", b.content)], receipts));
  const nodeIds = Array.from({ length: files }, () => uuidv7());
  let setupCommits = 1;
  for (let i = 0; i < files; i += 500) {
    applied(await client.commit(nodeIds.slice(i, i + 500).map((nodeId, j) => op.create(nodeId, created.spaceId, "writer-" + (i + j + 1) + ".bin", a.content))));
    setupCommits++;
  }
  return { spaceId: created.spaceId, contents: [b.content, a.content], nodeIds, setupCommits };
}

async function pairMany(owner: Device, n: number, label: string): Promise<Device[]> {
  const devices: Device[] = [];
  for (let i = 0; i < n; i += 8) {
    devices.push(...(await Promise.all(Array.from({ length: Math.min(8, n - i) }, (_, j) => owner.pair(label + " " + (i + j + 1))))));
  }
  return devices;
}

async function runFor(writers: Writer[], ms: number): Promise<number> {
  const started = Date.now();
  start(writers);
  while (Date.now() - started < ms && committed < budget) await sleep(500);
  await stop(writers);
  return Date.now() - started;
}

function start(writers: Writer[]): void {
  for (const writer of writers) {
    if (writer.loop) continue;
    writer.active = true;
    writer.loop = write(writer).finally(() => {
      writer.loop = undefined;
    });
  }
}

async function stop(writers: Writer[]): Promise<void> {
  for (const writer of writers) writer.active = false;
  await Promise.all(writers.map((writer) => writer.loop));
}

// Figures for a window are read after the run, because the Analytics API lags by minutes.

interface ClassUsage {
  requests: number;
  errors: number;
  wallTimeP50Ms?: number;
  wallTimeP99Ms?: number;
  cpuMs: number;
  busiestObjectCpuMs: number;
  objects: number;
  rowsRead: number;
  rowsWritten: number;
  inboundWebsocketMessages: number;
}

interface Window {
  label: string;
  startMs: number;
  endMs: number;
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

async function accountQuery<T>(account: Account, inner: string, vars: Record<string, unknown>): Promise<T> {
  const types: Record<string, string> = { account: "string!", ids: "[string!]", start: "Time!", end: "Time!" };
  const used = Object.keys(vars).filter((key) => inner.includes("$" + key));
  const args = ["$account: string!", ...used.map((key) => "$" + key + ": " + types[key])].join(", ");
  const query = "query (" + args + ") { viewer { accounts(filter: { accountTag: $account }) { " + inner + " } } }";
  const data = await graphql<{ viewer: { accounts: T[] } }>(account, query, { account: account.id, ...Object.fromEntries(used.map((key) => [key, vars[key]])) });
  return data.viewer.accounts[0]!;
}

async function usage(account: Account, namespaces: Namespace[], window: Window): Promise<Record<string, ClassUsage>> {
  const vars = { ids: namespaces.map((namespace) => namespace.id), start: iso(window.startMs), end: iso(window.endMs) };
  const filter = "filter: { namespaceId_in: $ids, datetime_geq: $start, datetime_leq: $end }";
  type Invocations = { dimensions: { namespaceId: string }; sum: { requests: number; errors: number }; quantiles: { wallTimeP50: number; wallTimeP99: number } };
  type Periodic = { dimensions: { namespaceId: string; objectId: string }; sum: { cpuTime: number; rowsRead: number; rowsWritten: number; inboundWebsocketMsgCount: number } };
  const invocations = await accountQuery<{ durableObjectsInvocationsAdaptiveGroups: Invocations[] }>(
    account,
    "durableObjectsInvocationsAdaptiveGroups(limit: 10000, " + filter + ") { dimensions { namespaceId } sum { requests errors } quantiles { wallTimeP50 wallTimeP99 } }",
    vars,
  );
  const periodic = await accountQuery<{ durableObjectsPeriodicGroups: Periodic[] }>(
    account,
    "durableObjectsPeriodicGroups(limit: 10000, " + filter + ") { dimensions { namespaceId objectId } sum { cpuTime rowsRead rowsWritten inboundWebsocketMsgCount } }",
    vars,
  );
  const byClass: Record<string, ClassUsage> = {};
  const of = (namespaceId: string) => {
    const name = namespaces.find((namespace) => namespace.id === namespaceId)?.class ?? namespaceId;
    return (byClass[name] ??= { requests: 0, errors: 0, cpuMs: 0, busiestObjectCpuMs: 0, objects: 0, rowsRead: 0, rowsWritten: 0, inboundWebsocketMessages: 0 });
  };
  for (const row of invocations.durableObjectsInvocationsAdaptiveGroups) {
    const entry = of(row.dimensions.namespaceId);
    entry.requests += row.sum.requests;
    entry.errors += row.sum.errors;
    entry.wallTimeP50Ms = row.quantiles.wallTimeP50 / 1000;
    entry.wallTimeP99Ms = row.quantiles.wallTimeP99 / 1000;
  }
  const perObject = new Map<string, number>();
  for (const row of periodic.durableObjectsPeriodicGroups) {
    const entry = of(row.dimensions.namespaceId);
    entry.cpuMs += row.sum.cpuTime / 1000;
    entry.rowsRead += row.sum.rowsRead;
    entry.rowsWritten += row.sum.rowsWritten;
    entry.inboundWebsocketMessages += row.sum.inboundWebsocketMsgCount;
    const key = row.dimensions.namespaceId + "/" + row.dimensions.objectId;
    perObject.set(key, (perObject.get(key) ?? 0) + row.sum.cpuTime / 1000);
  }
  for (const [key, cpuMs] of perObject) {
    const entry = of(key.split("/")[0]!);
    entry.objects++;
    entry.busiestObjectCpuMs = Math.max(entry.busiestObjectCpuMs, cpuMs);
  }
  return byClass;
}

function rowsWritten(byClass: Record<string, ClassUsage>): number {
  return Object.values(byClass).reduce((sum, entry) => sum + entry.rowsWritten, 0);
}

interface StorageSample {
  namespaceId: string;
  datetime: string;
  storedBytes: number;
}

async function storageSamples(account: Account, namespaces: Namespace[], startMs: number): Promise<StorageSample[]> {
  const data = await accountQuery<{ durableObjectsSqlStorageGroups: { dimensions: { namespaceId: string; datetime: string }; max: { storedBytes: number } }[] }>(
    account,
    "durableObjectsSqlStorageGroups(limit: 1000, filter: { namespaceId_in: $ids, datetime_geq: $start }, orderBy: [datetime_ASC]) { dimensions { namespaceId datetime } max { storedBytes } }",
    { ids: namespaces.map((namespace) => namespace.id), start: iso(startMs) },
  );
  return data.durableObjectsSqlStorageGroups.map((row) => ({ namespaceId: row.dimensions.namespaceId, datetime: row.dimensions.datetime, storedBytes: row.max.storedBytes }));
}

const runs: SpaceRun[] = [];
const writers: Writer[][] = [];
const subscribers: Session[][] = [];
const windows: Window[] = [];
const summaries: (StepSummary & { usage?: Record<string, ClassUsage> | string })[] = [];
const results: Record<string, unknown> = { config, startedAt: iso(Date.now()) };
let calibration: Window | undefined;
let loadEndMs = 0;
let stopReason = "";
let written = false;
const background: Promise<void>[] = [];
const subscribing = new Set<Session>();

function fixed(value: number | undefined, digits = 0): string {
  return value === undefined || !Number.isFinite(value) ? "-" : value.toFixed(digits);
}

function counts(record: Record<string, number>): string {
  return Object.entries(record).map(([key, n]) => key + " " + n).join(", ") || "none";
}

describe("load test " + config.scenario, () => {
  afterAll(async () => {
    stopping = true;
    for (const session of [...writers.flat().map((writer) => writer.session), ...subscribers.flat()]) session.close();
    await Promise.allSettled(background);
    addStats(stats);
    if (!written) writeResults("the suite stopped early, so these results are partial");
  }, 2 * 60_000);

  it("sets up the spaces and pairs a device for every writer and subscriber", async () => {
    if (!state.owner || !state.url) throw new Error("nothing is claimed yet; run run.ts deploy and claim first");
    const owner = new Device(server, state.owner);
    const { spaces, writersPerSpace } = layout(config, config.steps.at(-1)!);
    for (let s = 0; s < spaces; s++) runs.push(await makeSpace(owner, s, writersPerSpace));
    const devices = await pairMany(owner, spaces * writersPerSpace, "load-test writer");
    const watchers = await pairMany(owner, spaces * config.subscribersPerSpace, "load-test subscriber");
    runs.forEach((run, s) => {
      writers.push(run.nodeIds.map((nodeId, w) => ({ session: new Session(devices[s * writersPerSpace + w]!, run.spaceId, false), nodeId, contents: run.contents, active: false })));
      subscribers.push(Array.from({ length: config.subscribersPerSpace }, (_, i) => new Session(watchers[s * config.subscribersPerSpace + i]!, run.spaceId, true)));
    });
    background.push(probe(owner));
    background.push(
      (async () => {
        while (!stopping) {
          timeline.push([Date.now(), committed]);
          await sleep(5000);
        }
      })(),
    );
    results.setupCommits = runs.reduce((sum, run) => sum + run.setupCommits, 0);
    console.log("Set up " + spaces + " space(s) with " + writersPerSpace + " writer file(s) each, and paired " + devices.length + " writer and " + watchers.length + " subscriber device(s)");
  }, 30 * 60_000);

  it("measures the rows a commit writes and sets the commit budget", async () => {
    if (!account) {
      budget = commitBudget(config.maxRowsWritten, FALLBACK_ROWS_PER_COMMIT);
      results.calibration = { rowsPerCommit: null, assumed: FALLBACK_ROWS_PER_COMMIT, budget };
      console.log("No Cloudflare account in the environment: assuming " + FALLBACK_ROWS_PER_COMMIT + " rows per commit, so the budget is " + budget + " commits");
      return;
    }
    const startMs = Date.now();
    const before = committed;
    const ranMs = await runFor(writers[0]!.slice(0, CALIBRATION_WRITERS), CALIBRATION_MS);
    const commits = committed - before;
    calibration = { label: "calibration", startMs, endMs: startMs + ranMs + 60_000 };
    expect(commits, "calibration commits").toBeGreaterThan(0);
    const namespaces = await namespacesOf(account, state.name);
    const deadline = Date.now() + CALIBRATION_WAIT_MS;
    await sleep(ANALYTICS_LAG_MS);
    const readings: number[] = [];
    while (Date.now() < deadline) {
      try {
        readings.push(rowsWritten(await usage(account, namespaces, calibration)));
      } catch (error) {
        console.log("Analytics API: " + (error instanceof Error ? error.message : String(error)).slice(0, 200));
      }
      const last = readings.slice(-3);
      if (last.length === 3 && last[0]! > 0 && last.every((reading) => reading === last[0])) break;
      await sleep(CALIBRATION_READ_EVERY_MS);
    }
    const settled = readings.slice(-3);
    const rows = settled.length === 3 && settled[0]! > 0 && settled.every((reading) => reading === settled[0]) ? settled[0]! : 0;
    if (rows === 0 && readings.length > 0) console.log("Rows written did not settle at a positive count in three readings a minute apart: " + readings.join(", "));
    const rowsPerCommit = rows > 0 ? rows / commits : FALLBACK_ROWS_PER_COMMIT;
    budget = commitBudget(config.maxRowsWritten, rowsPerCommit);
    results.calibration = { commits, ranMs, rowsWritten: rows, rowsPerCommit: rows > 0 ? rowsPerCommit : null, assumed: rows > 0 ? null : FALLBACK_ROWS_PER_COMMIT, budget };
    console.log(
      "Calibration: " + commits + " commits from " + CALIBRATION_WRITERS + " writers in " + Math.round(ranMs / 1000) + " s wrote " + (rows > 0 ? rows + " rows, " + rowsPerCommit.toFixed(1) + " per commit" : "rows the Analytics API did not report, so " + FALLBACK_ROWS_PER_COMMIT + " per commit is assumed") + ". " +
        "The run stops at " + budget + " commits to stay under " + config.maxRowsWritten + " rows written",
    );
  }, CALIBRATION_MS + CALIBRATION_WAIT_MS + 5 * 60_000);

  it("runs the steps", async () => {
    const delay = monitorEventLoopDelay({ resolution: 10 });
    delay.enable();
    for (const step of config.steps) {
      const { spaces, writersPerSpace } = layout(config, step);
      const active = writers.slice(0, spaces).flatMap((space) => space.slice(0, writersPerSpace));
      for (const session of subscribers.slice(0, spaces).flat()) {
        if (subscribing.has(session)) continue;
        subscribing.add(session);
        background.push(subscribe(session));
      }
      start(active);
      const warmupEnd = Date.now() + config.warmupMs;
      while (Date.now() < warmupEnd && committed < budget) await sleep(500);
      const counted = emptyCounts(step, spaces, active.length);
      const cpu = process.cpuUsage();
      delay.reset();
      const startMs = Date.now();
      measuring = counted;
      while (Date.now() - startMs < config.measureMs && committed < budget) await sleep(500);
      measuring = undefined;
      counted.measuredMs = Date.now() - startMs;
      const used = process.cpuUsage(cpu);
      counted.generatorCpuMs = (used.user + used.system) / 1000;
      counted.eventLoopDelayP99Ms = delay.percentile(99) / 1e6;
      windows.push({ label: String(step), startMs, endMs: startMs + counted.measuredMs });
      const summary = summarize(counted);
      summaries.push(summary);
      console.log(
        "Step " + step + " (" + spaces + " space(s) x " + writersPerSpace + " writers): " + summary.commitsPerSecond.toFixed(1) + " commits/s, commit p50/p95/p99 " + fixed(summary.commitMs.p50) + "/" + fixed(summary.commitMs.p95) + "/" + fixed(summary.commitMs.p99) + " ms, " +
          "failed " + summary.failed + " (" + counts(summary.refused) + ", timeouts " + summary.timeouts + "), closes " + counts(summary.closes) + ", push lag p50/p95 " + fixed(summary.pushLagMs.p50) + "/" + fixed(summary.pushLagMs.p95) + " ms, " +
          "Account probe p50/p95 " + fixed(summary.probeMs.p50) + "/" + fixed(summary.probeMs.p95) + " ms, generator CPU " + (summary.generatorCpuShare * 100).toFixed(0) + "%, event loop delay p99 " + summary.eventLoopDelayP99Ms.toFixed(1) + " ms",
      );
      if (committed >= budget) {
        stopReason = "the commit budget of " + budget + " ran out during step " + step;
        break;
      }
      const why = saturated(summary);
      if (why && step !== config.steps.at(-1)) {
        stopReason = "stopped after step " + step + ": " + why;
        break;
      }
    }
    stopping = true;
    await stop(writers.flat());
    loadEndMs = Date.now();
    timeline.push([loadEndMs, committed]);
    delay.disable();
    if (stopReason) console.log(stopReason);
    results.stopReason = stopReason || null;
    results.writerCommits = committed;
  }, config.steps.length * (config.warmupMs + config.measureMs) + 10 * 60_000);

  it("reads each step's Durable Object usage and the stored bytes per commit", async () => {
    if (!account) {
      console.log("No Cloudflare account in the environment, so no analytics");
      return;
    }
    const namespaces = await namespacesOf(account, state.name);
    console.log("Waiting " + ANALYTICS_LAG_MS / 60_000 + " minutes for the Analytics API to catch up");
    await sleep(ANALYTICS_LAG_MS);
    for (const [i, window] of windows.entries()) {
      try {
        summaries[i]!.usage = await usage(account, namespaces, window);
      } catch (error) {
        summaries[i]!.usage = "unavailable: " + (error instanceof Error ? error.message : String(error)).slice(0, 200);
      }
    }
    const space = namespaces.find((namespace) => namespace.class === "SpaceDO");
    const firstMs = calibration?.startMs ?? windows[0]?.startMs ?? loadEndMs;
    let samples: StorageSample[] = [];
    const deadline = Date.now() + STORAGE_WAIT_MS;
    while (space) {
      try {
        samples = (await storageSamples(account, namespaces, firstMs - 3_600_000)).filter((sample) => sample.namespaceId === space.id);
      } catch (error) {
        console.log("Analytics API: " + (error instanceof Error ? error.message : String(error)).slice(0, 200));
      }
      if (samples.some((sample) => Date.parse(sample.datetime) >= loadEndMs) || Date.now() > deadline) break;
      await sleep(60_000);
    }
    const last = samples.at(-1);
    const during = samples.filter((sample) => Date.parse(sample.datetime) >= firstMs);
    const first = during[0];
    const setupCommits = Number(results.setupCommits ?? 0);
    const total = committed + setupCommits;
    results.storage = {
      samples,
      afterLoad: last ? Date.parse(last.datetime) >= loadEndMs : false,
      bytesPerCommitOverall: last ? last.storedBytes / total : null,
      bytesPerCommitBetweenSamples:
        first && last && first !== last ? (last.storedBytes - first.storedBytes) / Math.max(commitsAt(timeline, Date.parse(last.datetime)) - commitsAt(timeline, Date.parse(first.datetime)), 1) : null,
    };
    console.log("SpaceDO storage samples: " + (samples.map((sample) => sample.datetime + " " + sample.storedBytes).join(", ") || "none") + "; " + total + " commits in all, " + setupCommits + " of them setup");
  }, ANALYTICS_LAG_MS + STORAGE_WAIT_MS + 10 * 60_000);

  it("writes the results", () => {
    writeResults("");
  });
});

function writeResults(partial: string): void {
  written = true;
  results.partial = partial || null;
  results.steps = summaries;
  results.windows = windows;
  results.calibrationWindow = calibration ?? null;
  results.timeline = timeline;
  results.finishedAt = iso(Date.now());
  const path = join(process.env.RUNNER_TEMP || tmpdir(), "load-results.json");
  writeFileSync(path, JSON.stringify(results, null, 2));
  const share = (ms: number | undefined, window: number) => (ms === undefined ? "-" : ((ms / window) * 100).toFixed(0) + "%");
  const rows = summaries.map((summary) => {
    const used = typeof summary.usage === "object" ? summary.usage : undefined;
    const accountDo = used?.AccountDO;
    const spaceDo = used?.SpaceDO;
    return [
      String(summary.step),
      summary.spaces + " x " + summary.writers / summary.spaces,
      summary.commitsPerSecond.toFixed(1),
      fixed(summary.commitMs.p50) + " / " + fixed(summary.commitMs.p95) + " / " + fixed(summary.commitMs.p99),
      summary.failed + (summary.failed ? " (" + counts(summary.refused) + ")" : ""),
      counts(summary.closes),
      fixed(summary.pushLagMs.p50) + " / " + fixed(summary.pushLagMs.p95),
      fixed(summary.probeMs.p50) + " / " + fixed(summary.probeMs.p95),
      (summary.generatorCpuShare * 100).toFixed(0) + "%",
      share(accountDo?.cpuMs, summary.measuredMs) + (accountDo ? ", " + (accountDo.requests / (summary.measuredMs / 1000)).toFixed(0) + " req/s" : ""),
      share(spaceDo?.busiestObjectCpuMs, summary.measuredMs),
      used && summary.accepted > 0 ? (rowsWritten(used) / summary.accepted).toFixed(1) : "-",
    ];
  });
  const header = ["Step", "Spaces x writers", "Commits/s", "Commit ms p50 / p95 / p99", "Failed", "Closes", "Push lag ms p50 / p95", "Account probe ms p50 / p95", "Generator CPU", "Account DO CPU", "Busiest SpaceDO CPU", "Rows written per commit"];
  const table = [header, header.map(() => "---"), ...rows].map((row) => "| " + row.map((cell) => cell.replaceAll("|", "\\|")).join(" | ") + " |").join("\n");
  const storage = results.storage as { bytesPerCommitOverall: number | null; bytesPerCommitBetweenSamples: number | null; afterLoad: boolean } | undefined;
  const notes = [
    ...(partial ? ["Partial: " + partial] : []),
    "Stop: " + (stopReason || "every step ran"),
    "Writer commits: " + committed + ", budget " + (Number.isFinite(budget) ? budget : "none"),
    "SpaceDO stored bytes per commit: " + (storage ? fixed(storage.bytesPerCommitOverall ?? undefined) + " overall, " + fixed(storage.bytesPerCommitBetweenSamples ?? undefined) + " between samples" + (storage.afterLoad ? "" : " (no sample after the load yet)") : "not read"),
  ];
  console.log("\n" + table + "\n\n" + notes.join("\n"));
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, "### Load test " + config.scenario + "\n\n" + table + "\n\n" + notes.map((note) => "- " + note).join("\n") + "\n\n");
  console.log("Wrote " + path);
}

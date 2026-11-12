// SPDX-License-Identifier: AGPL-3.0-or-later

// Runs run.ts against MockCloudflare: teardown deletes only resources whose identity this run recorded, a name that is
// already taken stops the deploy before it creates anything, a Worker is recorded only when this run's own deploy
// created it, and the claim code read through the pinned Wrangler's tail ends up in no file and no output, whether the
// claim succeeds or fails.

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { emptyBucket } from "./cloudflare.ts";
import { MockCloudflare } from "./mock-cloudflare.ts";
import type { State } from "./state.ts";

const here = dirname(fileURLToPath(import.meta.url));
const server = join(here, "..", "..");
const NAME = "koidrive-test-1-1";

let mock: MockCloudflare;
let work: string;

beforeEach(async () => {
  mock = new MockCloudflare();
  await mock.start();
  work = mkdtempSync(join(tmpdir(), "koidrive-run-unit-"));
  mkdirSync(join(work, "home"));
  mkdirSync(join(work, "tmp"));
  mkdirSync(join(work, "bin"));
});

afterEach(async () => {
  await mock.stop();
  rmSync(work, { recursive: true, force: true });
});

function writeState(state: Partial<State>): void {
  const full: State = { name: NAME, owned: {}, deployments: [], stats: { requests: 0, bytesUp: 0, bytesDown: 0 }, ...state };
  writeFileSync(join(work, "state.json"), JSON.stringify(full));
}

function readState(): State {
  return JSON.parse(readFileSync(join(work, "state.json"), "utf8")) as State;
}

/** Runs a command to completion without blocking this process, which serves the mock API. */
function exec(command: string, args: string[], cwd: string, env: NodeJS.ProcessEnv): Promise<{ status: number | null; output: string }> {
  return new Promise((resolve) => {
    const child = spawn(command, args, { cwd, env });
    let output = "";
    child.stdout.on("data", (chunk: Buffer) => (output += chunk));
    child.stderr.on("data", (chunk: Buffer) => (output += chunk));
    const timer = setTimeout(() => child.kill("SIGKILL"), 120_000);
    child.on("close", (status) => {
      clearTimeout(timer);
      resolve({ status, output });
    });
  });
}

function runTs(...args: string[]): Promise<{ status: number | null; output: string }> {
  const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
  return exec(process.execPath, [...flags, join(here, "run.ts"), ...args], server, {
      PATH: join(work, "bin") + delimiter + process.env.PATH,
      HOME: join(work, "home"),
      TMPDIR: join(work, "tmp"),
      MOCK_URL: mock.url,
      FAKE_PNPM_LOG: join(work, "pnpm.log"),
      CLOUDFLARE_API_BASE_URL: mock.url + "/client/v4",
      CLOUDFLARE_API_TOKEN: "dummy-token",
      CLOUDFLARE_ACCOUNT_ID: "0123456789abcdef0123456789abcdef",
      KOIDRIVE_TEST_NAME: NAME,
      KOIDRIVE_DEPLOY_TEST_STATE: join(work, "state.json"),
      WRANGLER_SEND_METRICS: "false",
  });
}

interface Step {
  /** Uploads a version of the run's Worker through the mock, creating the Worker if needed. */
  createsWorker: boolean;
  /** Whether that upload carries the deploy's --tag, as Wrangler's does; a concurrent creator's never does. */
  tagged?: boolean;
  status: number;
}

/**
 * Puts a pnpm on run.ts's PATH that logs its arguments without --tag, uploads the run's Worker through the mock when a
 * step says so (as a concurrent creator during install, or as Wrangler during deploy), and exits with the step's status.
 * A deploy that exits 0 prints what wrangler deploy prints.
 */
function fakePnpm(steps: { install: Step; deploy: Step }): void {
  const script = `#!${process.execPath}
const { appendFileSync } = require("node:fs");
const steps = ${JSON.stringify(steps)};
const step = process.argv[2] === "install" ? steps.install : steps.deploy;
const args = process.argv.slice(2);
const at = args.indexOf("--tag");
const tag = at >= 0 ? args.splice(at, 2)[1] : undefined;
appendFileSync(process.env.FAKE_PNPM_LOG, args.join(" ") + "\\n");
const body = JSON.stringify(process.argv[2] !== "install" && step.tagged !== false ? { tag } : {});
const created = step.createsWorker ? fetch(process.env.MOCK_URL + "/mock/workers/${NAME}", { method: "POST", body }) : Promise.resolve();
created.then(() => {
  if (process.argv[2] !== "install" && step.status === 0) {
    console.log("Deployed ${NAME} triggers\\n  https://${NAME}.mock.workers.dev\\nCurrent Version ID: 00000000-0000-4000-8000-000000000001");
  }
  process.exit(step.status);
});
`;
  writeFileSync(join(work, "bin", "pnpm"), script, { mode: 0o755 });
}

function pnpmCalls(): string[] {
  return existsSync(join(work, "pnpm.log")) ? readFileSync(join(work, "pnpm.log"), "utf8").trim().split("\n") : [];
}

/** Files under root whose bytes contain text. */
function filesContaining(root: string, text: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(root, { withFileTypes: true, recursive: true })) {
    const path = join(entry.parentPath, entry.name);
    if (entry.isFile() && statSync(path).size < 64 * 1024 * 1024 && readFileSync(path).includes(text)) found.push(path);
  }
  return found;
}

describe("teardown", () => {
  it("empties 3072 chunk objects across four listing pages with one prefix-delete job and one status poll", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 3072 }, (_, i) => "spaces/bench/chunks/aa/bb/" + i));
    for (const key of bucket.objects.keys()) bucket.objects.set(key, 4 * 1024 * 1024);
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketJobStatuses = ["RUNNING", "COMPLETED"];
    const started = performance.now();
    const { status, output } = await runTs("teardown");
    expect({ status, errors: output.includes("::error::") }).toEqual({ status: 0, errors: false });
    expect(mock.buckets.has(NAME)).toBe(false);
    expect(mock.requests.filter((request) => request.startsWith("GET ") && request.endsWith("/objects"))).toHaveLength(4);
    expect(mock.deletes().filter((request) => request.includes("/objects/"))).toEqual([]);
    expect(mock.bucketJobBodies).toEqual([{ jobType: "prefixDelete", prefix: "" }]);
    expect(mock.requests.filter((request) => request.includes("/jobs/"))).toHaveLength(1);
    expect(mock.requests).toHaveLength(9);
    expect(output).toContain("3072 objects");
    console.info("3072-object teardown mock: " + (performance.now() - started).toFixed(0) + " ms, " + mock.requests.length + " API requests, four listing pages, one job and one poll, no network retries or analytics wait");
  });

  it("accepts a prefix-delete job that completes synchronously", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    expect((await runTs("teardown")).status).toBe(0);
    expect(mock.bucketJobBodies).toHaveLength(1);
    expect(mock.requests.filter((request) => request.includes("/jobs/"))).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it.each(["FAILED", "CANCELLED", "unexpected"])("leaves the bucket and reports a prefix-delete job with status %s", async (jobStatus) => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketJobStatuses = ["RUNNING", jobStatus];
    const { status, output } = await runTs("teardown");
    expect(status).toBe(1);
    expect(output).toContain("bucket-emptying job");
    expect(mock.bucketJobBodies).toHaveLength(1);
    expect(mock.buckets.get(NAME)?.objects.size).toBe(1001);
    expect(mock.deletes()).toEqual([]);
  });

  it("does not retry an ambiguous prefix-delete creation failure", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketJobCreateStatus = 503;
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.bucketJobBodies).toHaveLength(1);
    expect(mock.buckets.get(NAME)?.objects.size).toBe(1001);
    expect(mock.deletes()).toEqual([]);
  });

  it.each([undefined, "2020-01-01T00:00:00.000Z"])("does not start a bulk job for a large bucket with unproven ownership %s", async (recordedDate) => {
    mock.addBucket(NAME, "2026-01-01T00:00:00.000Z", Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: recordedDate } });
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.bucketJobBodies).toEqual([]);
    expect(mock.buckets.get(NAME)?.objects.size).toBe(1001);
    expect(mock.deletes()).toEqual([]);
  });

  it("stops polling a running bucket-emptying job at six minutes", async () => {
    vi.useFakeTimers();
    const fetchJob = vi.fn(async () => Response.json({ result: { id: "empty-1", status: "RUNNING" } }));
    vi.stubGlobal("fetch", fetchJob);
    try {
      const finished = expect(emptyBucket({ id: "dummy-account", token: "dummy-token" }, NAME)).rejects.toThrow("six-minute deadline");
      await vi.advanceTimersByTimeAsync(6 * 60_000);
      await finished;
      expect(fetchJob).toHaveBeenCalledTimes(72);
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("leaves a Worker and a bucket alone that this run did not record creating", async () => {
    mock.addWorker(NAME);
    mock.addBucket(NAME, "2026-01-01T00:00:00.000Z", ["preexisting-user-data.txt"]);
    const { status, output } = await runTs("teardown");
    expect(status).toBe(1);
    expect(mock.deletes()).toEqual([]);
    expect(output).toContain("did not record creating it, so it was left alone");
    expect(mock.buckets.get(NAME)?.objects.has("preexisting-user-data.txt")).toBe(true);
  });

  it("deletes the Worker, its Durable Object namespaces, the objects and the bucket whose identity this run recorded", async () => {
    mock.addWorker(NAME, undefined, "this-runs-tag");
    const bucket = mock.addBucket(NAME, undefined, ["spaces/a/chunks/aa/aa/1", "spaces/a/meta/h/space.json"]);
    writeState({ owned: { workerTag: "this-runs-tag", bucketCreationDate: bucket.creationDate } });
    const { status, output } = await runTs("teardown");
    expect(output).not.toContain("::error::");
    expect(status).toBe(0);
    expect(mock.workers.has(NAME)).toBe(false);
    expect(mock.namespaces).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it("leaves a Worker and a bucket alone that replaced the ones this run created", async () => {
    mock.addWorker(NAME, "someone-elses-worker");
    mock.addBucket(NAME, "2026-02-02T00:00:00.000Z", ["theirs.txt"]);
    writeState({ owned: { workerTag: "this-runs-tag", bucketCreationDate: "2026-01-01T00:00:00.000Z" } });
    const { status } = await runTs("teardown");
    expect(status).toBe(1);
    expect(mock.deletes()).toEqual([]);
  });
});

describe("deploy", () => {
  it("stops before creating anything when a bucket already has the run's name, and teardown then deletes nothing", async () => {
    mock.addBucket(NAME, "2026-01-01T00:00:00.000Z", ["preexisting-user-data.txt"]);
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(deployed.output).toContain("already exists");
    expect(mock.requests.filter((request) => request.startsWith("POST "))).toEqual([]);
    expect(existsSync(join(work, "state.json")) ? readState().owned : {}).toEqual({});
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes()).toEqual([]);
  });

  it("stops before creating anything when a Worker already has the run's name", async () => {
    mock.addWorker(NAME);
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(mock.requests.filter((request) => request.startsWith("POST "))).toEqual([]);
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes()).toEqual([]);
  });

  it("records nothing when creating the bucket fails, so teardown leaves the bucket that won the name alone", async () => {
    mock.createBucketStatus = 409;
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(existsSync(join(work, "state.json")) ? readState().owned : {}).toEqual({});
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes()).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(true);
  });

  it("records no Worker when the install fails while another creator takes the name, so teardown deletes nothing", async () => {
    fakePnpm({ install: { createsWorker: true, status: 1 }, deploy: { createsWorker: true, status: 0 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile"]);
    expect(readState().owned.workerTag).toBeUndefined();
    const id = mock.workers.get(NAME)?.id;
    expect(id).toBeDefined();
    const tornDown = await runTs("teardown");
    expect(tornDown.status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(mock.workers.get(NAME)?.id).toBe(id);
    expect(mock.namespaces.filter((namespace) => namespace.script === NAME)).toHaveLength(2);
  });

  it("does not deploy over a Worker that another creator made while dependencies were installing", async () => {
    fakePnpm({ install: { createsWorker: true, status: 0 }, deploy: { createsWorker: true, status: 0 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(deployed.output).toContain("appeared before this run deployed");
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile"]);
    expect(readState().owned.workerTag).toBeUndefined();
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(mock.workers.has(NAME)).toBe(true);
  });

  it("records the Worker its own deploy created, even when a later step fails, so teardown deletes it", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 0 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile", "run deploy"]);
    expect(deployed.status).toBe(1);
    expect(mock.workers.get(NAME)?.versions[0]?.tag).toBe(readState().owned.workerTag);
    expect(readState().owned.workerTag).toMatch(/^[0-9a-f]{16}$/);
    const tornDown = await runTs("teardown");
    expect(tornDown.output).not.toContain("::error::");
    expect(tornDown.status).toBe(0);
    expect(mock.workers.has(NAME)).toBe(false);
    expect(mock.buckets.has(NAME)).toBe(false);
  });
});

describe("a later deploy of the same run", () => {
  it("refuses to deploy over a Worker that carries no version of this run's deploy", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 0 } });
    mock.addWorker(NAME, "someone-elses-worker");
    writeState({ owned: { workerTag: "this-runs-tag" }, deployments: [{ ref: "HEAD", sha: "0".repeat(40), dir: server, versionId: "v", atMs: Date.now() }] });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(deployed.output).toContain("is no longer the one this run created");
    expect(pnpmCalls()).toEqual([]);
  });
});

describe("a deploy that fails after creating the Worker", () => {
  it("leaves proof that this run created the Worker, so teardown deletes it, its Durable Objects and the bucket", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 1 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile", "run deploy"]);
    expect(mock.workers.has(NAME)).toBe(true);
    const tornDown = await runTs("teardown");
    expect(tornDown.output).not.toContain("::error::");
    expect(tornDown.status).toBe(0);
    expect(mock.workers.has(NAME)).toBe(false);
    expect(mock.namespaces.filter((namespace) => namespace.script === NAME)).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it("leaves the Worker alone and reports only the error when its versions cannot be read", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 1 } });
    expect((await runTs("deploy", "HEAD")).status).toBe(1);
    mock.versionsStatus = 403;
    const tornDown = await runTs("teardown");
    expect(tornDown.status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(tornDown.output).toContain("::error::teardown: reading the Worker's version tags: GET");
    expect(tornDown.output).not.toContain("none of its versions carries");
    expect(tornDown.output).not.toContain("did not record creating it");
  });

  it("fails teardown loudly and names the Worker to delete by hand when no upload of this run's deploy is on it", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, tagged: false, status: 1 } });
    expect((await runTs("deploy", "HEAD")).status).toBe(1);
    const tornDown = await runTs("teardown");
    expect(tornDown.status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(mock.workers.has(NAME)).toBe(true);
    expect(tornDown.output).toContain("::error::teardown: a Worker named " + NAME);
    expect(tornDown.output).toContain("delete the Worker " + NAME + " by hand");
  });
});

describe("claim", () => {
  function claimState(): void {
    writeState({ url: mock.url, deployments: [{ ref: "HEAD", sha: "0".repeat(40), dir: server, versionId: "v", atMs: Date.now() }] });
  }

  it("shows that the pinned Wrangler writes tail messages to its log file unless told not to", async () => {
    const logs = join(work, "wrangler-logs");
    const tail = spawn(join(server, "node_modules", ".bin", "wrangler"), ["tail", NAME, "--format", "pretty"], {
      cwd: server,
      env: {
        PATH: process.env.PATH,
        HOME: join(work, "home"),
        CLOUDFLARE_API_BASE_URL: mock.url + "/client/v4",
        CLOUDFLARE_API_TOKEN: "dummy-token",
        CLOUDFLARE_ACCOUNT_ID: "0123456789abcdef0123456789abcdef",
        WRANGLER_SEND_METRICS: "false",
        WRANGLER_LOG_PATH: logs,
      },
    });
    let output = "";
    tail.stdout.on("data", (chunk: Buffer) => (output += chunk));
    tail.stderr.on("data", (chunk: Buffer) => (output += chunk));
    const closed = new Promise((resolve) => tail.on("close", resolve));
    try {
      await vi.waitUntil(() => output.includes("Connected to"), { timeout: 30_000, interval: 200 });
      await fetch(mock.url + "/setup");
      await vi.waitUntil(() => output.includes(mock.claimCode), { timeout: 30_000, interval: 200 });
    } finally {
      tail.kill("SIGTERM");
      await closed;
    }
    expect(filesContaining(logs, mock.claimCode).length).toBeGreaterThan(0);
  });

  it("claims with the code from the Worker's tail and leaves the code in no file and no output", async () => {
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(output).toContain("No file in Wrangler's directories or the deployed copy holds the claim code");
    expect(status).toBe(0);
    expect(readState().owner?.userId).toBe("user-1");
    expect(output).not.toContain(mock.claimCode);
    expect(filesContaining(work, mock.claimCode)).toEqual([]);
  });

  it("leaves the code in no file and no output when the claim fails", async () => {
    mock.claimStatus = 403;
    claimState();
    const { status, output } = await runTs("claim");
    expect(status).toBe(1);
    expect(output).toContain("the claim failed with HTTP 403");
    expect(output).toContain("<claim code>");
    expect(output).not.toContain(mock.claimCode);
    expect(filesContaining(work, mock.claimCode)).toEqual([]);
  });
});

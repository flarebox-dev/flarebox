// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { appendFileSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  type Account,
  LISTING_MS,
  newestDeployment,
  createBucket,
  deleteBucket,
  deleteObject,
  deleteWorker,
  emptyBucket,
  getBucket,
  getWorker,
  graphql,
  listObjects,
  type Namespace,
  namespacesOf,
  requireAccount,
  serves,
  workerTag,
} from "./cloudflare.ts";
import { readState, type State, testName, writeState } from "./state.ts";

const INSTALL_MS = 5 * 60_000;
const DEPLOY_MS = 5 * 60_000;
const SERVING_MS = 3 * 60_000;
const TAIL_MS = 60_000;
const PROPAGATION_MS = 15_000;
/** The first commit whose server/ deploys on its own (#38); an upgrade cannot start earlier. */
const FIRST_DEPLOYABLE = "8c9bf4a";
const CLAIM_LOG_LINE = "Koidrive claim code";
const CLAIM_CODE = /Koidrive claim code[^:]*: ([A-Z0-9-]+)/;

const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

class Failed extends Error {}

function fail(message: string): never {
  throw new Failed(message);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function git(args: string[]): string {
  return execFileSync("git", args, { cwd: repo, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function tryGit(args: string[]): string | undefined {
  try {
    return git(args);
  } catch {
    return undefined;
  }
}

function withoutSecrets(): NodeJS.ProcessEnv {
  const env = { ...process.env };
  delete env.CLOUDFLARE_API_TOKEN;
  delete env.CLOUDFLARE_ACCOUNT_ID;
  return env;
}

function run(command: string, args: string[], cwd: string, timeoutMs: number, env: NodeJS.ProcessEnv): Promise<string> {
  console.log("$ " + [command, ...args].join(" "));
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd, env: { ...env, CI: "true", WRANGLER_SEND_METRICS: "false" }, detached: true });
    let output = "";
    const take = (chunk: Buffer) => {
      output += chunk;
      process.stdout.write(chunk);
    };
    child.stdout.on("data", take);
    child.stderr.on("data", take);
    const timer = setTimeout(() => {
      try {
        process.kill(-child.pid!, "SIGKILL");
      } catch {}
      reject(new Failed(command + " " + args.join(" ") + " was killed after " + timeoutMs / 1000 + " s"));
    }, timeoutMs);
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(new Failed("could not start " + command + ": " + error.message));
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code === 0) resolve(output);
      else reject(new Failed(command + " " + args.join(" ") + " exited with " + code));
    });
  });
}

function output(name: string, value: string): void {
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, name + "=" + value + "\n");
}

function upgradeFrom(input: string): void {
  const head = git(["rev-parse", "HEAD"]);
  let ref = input.trim();
  if (!ref) {
    ref = tryGit(["describe", "--tags", "--abbrev=0", "HEAD^"]) ?? "";
    if (!ref) {
      fail(
        "there is no release tag before " + head.slice(0, 12) + ", so there is no previous release to upgrade from. " +
          "Run the upgrade mode with from_ref set to an earlier commit, at or after " + FIRST_DEPLOYABLE + " (#38), the first whose server/ deploys on its own.",
      );
    }
    console.log("No from_ref given: upgrading from the newest tag before HEAD, " + ref);
  }
  const sha = tryGit(["rev-parse", "--verify", "--quiet", ref + "^{commit}"]) ?? fail("from_ref " + JSON.stringify(ref) + " is not a commit in this repository");
  if (sha === head) fail("from_ref " + ref + " is the commit under test; pick an earlier one");
  if (tryGit(["merge-base", "--is-ancestor", sha, head]) === undefined) {
    fail("from_ref " + ref + " (" + sha.slice(0, 12) + ") is not an ancestor of " + head.slice(0, 12) + "; an upgrade only moves forward");
  }
  const pkg = tryGit(["show", sha + ":server/package.json"]);
  const deployScript = pkg ? (JSON.parse(pkg) as { scripts?: Record<string, string> }).scripts?.deploy : undefined;
  if (tryGit(["cat-file", "-e", sha + ":server/pnpm-lock.yaml"]) === undefined || deployScript !== "wrangler deploy") {
    fail("server/ at " + sha.slice(0, 12) + " does not deploy on its own (no lockfile or no deploy script); pick a commit at or after " + FIRST_DEPLOYABLE);
  }
  console.log("Upgrade from " + ref + " (" + sha + ") to " + head);
  output("sha", sha);
}

function rename(dir: string, name: string): void {
  const path = join(dir, "wrangler.jsonc");
  let config: { name?: string; r2_buckets?: { binding: string; bucket_name?: string }[] };
  try {
    config = JSON.parse(readFileSync(path, "utf8").replace(/^\s*\/\/.*$/gm, ""));
  } catch (error) {
    fail("cannot read " + path + " as JSON with line comments: " + (error instanceof Error ? error.message : String(error)));
  }
  const bucket = config.r2_buckets?.find((entry) => entry.binding === "BUCKET") ?? fail("wrangler.jsonc has no R2 binding named BUCKET");
  config.name = name;
  bucket.bucket_name = name;
  writeFileSync(path, JSON.stringify(config, null, 2) + "\n");
}

async function serving(url: string): Promise<{ serverVersion: string; claimed: boolean }> {
  const deadline = Date.now() + SERVING_MS;
  let last = "no response";
  for (;;) {
    try {
      const response = await fetch(url + "/v1/server", { headers: { Accept: "application/json" }, signal: AbortSignal.timeout(10_000) });
      if (response.ok) return (await response.json()) as { serverVersion: string; claimed: boolean };
      last = "HTTP " + response.status;
      await response.body?.cancel();
    } catch (error) {
      last = error instanceof Error ? error.message : String(error);
    }
    if (Date.now() > deadline) fail(url + "/v1/server did not answer within " + SERVING_MS / 1000 + " s (last: " + last + ")");
    await sleep(3000);
  }
}

/**
 * Wrangler sends the tag with the upload that creates the Worker, so the Worker carries it even if the deploy fails
 * afterwards.
 */
async function deploy(account: Account, ref: string): Promise<void> {
  const state = readState();
  const sha = tryGit(["rev-parse", "--verify", "--quiet", ref + "^{commit}"]) ?? fail(JSON.stringify(ref) + " is not a commit in this repository");
  const first = state.deployments.length === 0;
  if (first) {
    if (state.owned.workerTag || state.owned.bucketCreationDate) fail("the state file already records resources but no deployment; start a new run");
    const worker = await getWorker(account, state.name);
    if (worker) fail("a Worker named " + state.name + " already exists (id " + worker.id + "); this run did not create it and leaves it alone");
    if (await getBucket(account, state.name)) fail("an R2 bucket named " + state.name + " already exists; this run did not create it and leaves it alone");
    try {
      await createBucket(account, state.name);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("10042")) fail("R2 is not activated for this account. Activate it in the dashboard first (docs/deploy.md, What you need): " + message);
      throw error;
    }
    state.owned.bucketCreationDate = (await getBucket(account, state.name) ?? fail("the bucket " + state.name + " was created but cannot be read back")).creationDate;
    writeState(state);
    console.log("Created the R2 bucket " + state.name + " (created " + state.owned.bucketCreationDate + ")");
  } else if (!state.owned.workerTag || (await workerTag(account, state.name, state.owned.workerTag)) !== "tagged") {
    fail("the Worker " + state.name + " is no longer the one this run created; refusing to deploy over it");
  }

  const dir = mkdtempSync(join(tmpdir(), "koidrive-deploy-test-"));
  execFileSync("sh", ["-c", 'git archive --format=tar "$1:server" | tar -x -C "$2"', "sh", sha, dir], { cwd: repo, stdio: "inherit" });
  rename(dir, state.name);
  console.log("Copied server/ at " + sha + " to " + dir + " as Worker and R2 bucket " + state.name);

  await run("pnpm", ["install", "--frozen-lockfile"], dir, INSTALL_MS, withoutSecrets());
  if (first) {
    const worker = await getWorker(account, state.name);
    if (worker) fail("a Worker named " + state.name + " appeared before this run deployed (id " + worker.id + "); this run did not create it and leaves it alone");
    state.owned.workerTag = randomBytes(8).toString("hex");
    writeState(state);
  }
  const deployed = await run("pnpm", ["run", "deploy", "--tag", state.owned.workerTag!], dir, DEPLOY_MS, process.env);
  const url = /https:\/\/[a-z0-9.-]+\.workers\.dev/.exec(deployed)?.[0];
  if (!url) fail("wrangler deploy printed no workers.dev address; the account needs a workers.dev subdomain (docs/deploy-test.md)");
  const versionId = /Current Version ID: ([0-9a-f-]{36})/.exec(deployed)?.[1] ?? fail("wrangler deploy printed no version ID");
  const newest = await newestDeployment(account, state.name, versionId);
  if (!serves(newest, versionId)) {
    const listedAs = newest ? "sends " + newest.versions.map((version) => version.percentage + "% to " + version.version_id).join(" and ") : "is not listed";
    fail("after " + LISTING_MS / 1000 + " s the newest deployment of " + state.name + " " + listedAs + ", not 100% to " + versionId);
  }
  console.log("Deployment of " + state.name + " sends all traffic to version " + versionId);
  state.url = url;
  state.deployments.push({ ref, sha, dir, versionId, atMs: Date.now() });
  writeState(state);

  if (state.deployments.length > 1) {
    console.log("Waiting " + PROPAGATION_MS / 1000 + " s for the new version to reach the edge");
    await sleep(PROPAGATION_MS);
  }
  const info = await serving(url);
  console.log(url + " serves Koidrive " + info.serverVersion + (info.claimed ? ", claimed" : ", not claimed yet"));
}

async function until<T>(check: () => T | undefined, what: string, timeoutMs: number): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = check();
    if (value !== undefined) return value;
    if (Date.now() > deadline) fail("timed out after " + timeoutMs / 1000 + " s waiting for " + what);
    await sleep(500);
  }
}

function filesWithClaimCode(roots: string[]): string[] {
  const found: string[] = [];
  const visit = (path: string) => {
    let entries;
    try {
      entries = readdirSync(path, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const child = join(path, entry.name);
      if (entry.isDirectory() && entry.name !== "node_modules") visit(child);
      else if (entry.isFile() && statSync(child).size < 64 * 1024 * 1024 && readFileSync(child).includes(CLAIM_LOG_LINE)) found.push(child);
    }
  };
  for (const root of roots) visit(root);
  return found;
}

function wranglerDirs(): string[] {
  const home = homedir();
  return [join(home, ".wrangler"), join(process.env.XDG_CONFIG_HOME || join(home, ".config"), ".wrangler"), join(home, "Library", "Preferences", ".wrangler")];
}

/** A tail can connect before it receives the Worker's events. */
async function claim(): Promise<void> {
  const state = readState();
  const deployment = state.deployments.at(-1) ?? fail("nothing is deployed yet");
  const url = state.url!;
  const logDir = mkdtempSync(join(tmpdir(), "koidrive-wrangler-logs-"));
  const tail = spawn(join(deployment.dir, "node_modules", ".bin", "wrangler"), ["tail", state.name, "--format", "pretty"], {
    cwd: deployment.dir,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", WRANGLER_WRITE_LOGS: "false", WRANGLER_LOG_PATH: logDir },
    detached: true,
  });
  let log = "";
  let code = "";
  let exited: number | null | undefined;
  const closed = new Promise<void>((resolve) => tail.on("close", () => resolve()));
  tail.stdout.on("data", (chunk: Buffer) => (log += chunk));
  tail.stderr.on("data", (chunk: Buffer) => (log += chunk));
  tail.on("close", (status) => (exited = status));
  const redacted = (text: string) => (code ? text.replaceAll(code, "<claim code>") : text).replace(/\b[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{4}\b/g, "<claim code>");
  try {
    await until(() => (log.includes("Connected to") ? true : exited !== undefined ? fail("wrangler tail exited with " + exited) : undefined), "wrangler tail to connect", TAIL_MS);
    const deadline = Date.now() + TAIL_MS;
    while (!log.includes("/v1/server")) {
      if (exited !== undefined) fail("wrangler tail exited with " + exited);
      if (Date.now() > deadline) {
        fail("wrangler tail showed none of this Worker's requests to /v1/server within " + TAIL_MS / 1000 + " s, so /setup was not opened and no claim code was spent; run the claim again");
      }
      await fetch(url + "/v1/server", { signal: AbortSignal.timeout(10_000) }).then((response) => response.body?.cancel(), () => undefined);
      await sleep(2000);
    }
    console.log("wrangler tail shows this Worker's requests, so it will see the claim code");
    const setup = await fetch(url + "/setup", { signal: AbortSignal.timeout(15_000) });
    const page = await setup.text();
    if (!setup.ok || !page.includes("claim code")) fail("/setup did not issue a claim code (HTTP " + setup.status + "): " + page.slice(0, 200));
    code = await until(() => CLAIM_CODE.exec(log)?.[1], "the claim code in the Worker's log", TAIL_MS);
    console.log("Read the claim code from the Worker's log through wrangler tail");
    const response = await fetch(url + "/v1/claim", {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify({ code, userName: "Deploy test owner", deviceName: "deploy-test owner", platform: "linux" }),
      signal: AbortSignal.timeout(15_000),
    });
    const body = (await response.json()) as { userId?: string; deviceId?: string; refreshCredential?: string; message?: string };
    if (!response.ok || !body.userId || !body.deviceId || !body.refreshCredential) fail("the claim failed with HTTP " + response.status + ": " + redacted(body.message ?? "no message"));
    state.owner = { userId: body.userId, deviceId: body.deviceId, refreshCredential: body.refreshCredential };
    writeState(state);
    console.log("Claimed " + url + ": owner " + body.userId + ", first device " + body.deviceId);
  } catch (error) {
    console.error("wrangler tail output (claim codes redacted):\n" + redacted(log));
    throw error;
  } finally {
    try {
      process.kill(-tail.pid!, "SIGTERM");
    } catch {}
    await Promise.race([closed, sleep(10_000)]);
    const holders = filesWithClaimCode([logDir, join(deployment.dir, ".wrangler"), ...wranglerDirs()]);
    for (const holder of holders) rmSync(holder, { force: true });
    rmSync(logDir, { recursive: true, force: true });
    if (holders.length > 0) fail("Wrangler wrote the claim code's log line to " + holders.join(", ") + "; those files were deleted");
    console.log("No file in Wrangler's directories or the deployed copy holds the claim code");
  }
}

interface Storage {
  objects: number;
  bytes: number;
  chunkObjects: number;
  chunkBytes: number;
  exportObjects: number;
  exportBytes: number;
}

async function emptyAndDeleteBucket(account: Account, name: string): Promise<Storage | undefined> {
  const first = await listObjects(account, name);
  if (!first) return undefined;
  const storage: Storage = { objects: 0, bytes: 0, chunkObjects: 0, chunkBytes: 0, exportObjects: 0, exportBytes: 0 };
  for (const object of first) {
    storage.objects++;
    storage.bytes += object.size;
    if (/^spaces\/[^/]+\/chunks\//.test(object.key)) {
      storage.chunkObjects++;
      storage.chunkBytes += object.size;
    } else if (/^spaces\/[^/]+\/meta\//.test(object.key)) {
      storage.exportObjects++;
      storage.exportBytes += object.size;
    }
  }
  let objects = first;
  if (first.length > 1000) {
    await emptyBucket(account, name);
    objects = (await listObjects(account, name)) ?? [];
  }
  for (let attempt = 1; ; attempt++) {
    for (let i = 0; i < objects.length; i += 8) await Promise.all(objects.slice(i, i + 8).map((object) => deleteObject(account, name, object.key)));
    try {
      await deleteBucket(account, name);
      return storage;
    } catch (error) {
      if (attempt >= 5) throw error;
      await sleep(5000 * attempt);
      objects = (await listObjects(account, name)) ?? [];
    }
  }
}

function bytes(n: number): string {
  return n < 1024 ? n + " B" : n < 1024 * 1024 ? (n / 1024).toFixed(1) + " KiB" : (n / 1024 / 1024).toFixed(2) + " MiB";
}

async function analytics(account: Account, state: State | undefined, namespaces: Namespace[]): Promise<[string, string][]> {
  const start = new Date((state?.deployments[0]?.atMs ?? Date.now() - 3_600_000) - 60_000).toISOString();
  const end = new Date().toISOString();
  const vars = { account: account.id, script: state?.name ?? testName(), start, end, day: start.slice(0, 10), ids: namespaces.map((namespace) => namespace.id) };
  const accounts = (inner: string, args: string) =>
    `query (${args}) { viewer { accounts(filter: { accountTag: $account }) { ${inner} } } }`;
  const queries: [string, string, string, (data: unknown) => string][] = [
    [
      "Worker requests, subrequests, errors",
      "$account: string!, $script: string!, $start: Time!, $end: Time!",
      "workersInvocationsAdaptive(limit: 10000, filter: { scriptName: $script, datetime_geq: $start, datetime_leq: $end }) { sum { requests subrequests errors } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { workersInvocationsAdaptive: { sum: { requests: number; subrequests: number; errors: number } }[] }[] } }).viewer.accounts[0]!.workersInvocationsAdaptive;
        const sum = rows.reduce((a, row) => ({ r: a.r + row.sum.requests, s: a.s + row.sum.subrequests, e: a.e + row.sum.errors }), { r: 0, s: 0, e: 0 });
        return sum.r + " requests, " + sum.s + " subrequests, " + sum.e + " errors";
      },
    ],
    [
      "Durable Object requests",
      "$account: string!, $ids: [string!], $start: Time!, $end: Time!",
      "durableObjectsInvocationsAdaptiveGroups(limit: 10000, filter: { namespaceId_in: $ids, datetime_geq: $start, datetime_leq: $end }) { sum { requests } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { durableObjectsInvocationsAdaptiveGroups: { sum: { requests: number } }[] }[] } }).viewer.accounts[0]!.durableObjectsInvocationsAdaptiveGroups;
        return rows.reduce((a, row) => a + row.sum.requests, 0) + " requests";
      },
    ],
    [
      "Durable Object storage of the whole account (largest sample)",
      "$account: string!, $day: Date!",
      "durableObjectsStorageGroups(limit: 1000, filter: { date_geq: $day }) { max { storedBytes } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { durableObjectsStorageGroups: { max: { storedBytes: number } }[] }[] } }).viewer.accounts[0]!.durableObjectsStorageGroups;
        return rows.length === 0 ? "no sample yet" : bytes(Math.max(...rows.map((row) => row.max.storedBytes)));
      },
    ],
    [
      "R2 operations",
      "$account: string!, $script: string!, $start: Time!, $end: Time!",
      "r2OperationsAdaptiveGroups(limit: 10000, filter: { bucketName: $script, datetime_geq: $start, datetime_leq: $end }) { sum { requests } dimensions { actionType } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { r2OperationsAdaptiveGroups: { sum: { requests: number }; dimensions: { actionType: string } }[] }[] } }).viewer.accounts[0]!.r2OperationsAdaptiveGroups;
        return rows.length === 0 ? "none recorded yet" : rows.map((row) => row.dimensions.actionType + " " + row.sum.requests).join(", ");
      },
    ],
  ];
  const rows: [string, string][] = [];
  for (const [label, args, inner, read] of queries) {
    try {
      const query = accounts(inner, args);
      const used = Object.fromEntries(Object.entries(vars).filter(([key]) => args.includes("$" + key + ":")));
      rows.push([label + " (Analytics API)", read(await graphql(account, query, used))]);
    } catch (error) {
      rows.push([label + " (Analytics API)", "unavailable: " + (error instanceof Error ? error.message : String(error)).slice(0, 200)]);
    }
  }
  return rows;
}

async function teardown(account: Account): Promise<void> {
  const name = testName();
  const state = readState();
  const problems: string[] = [];
  const attempt = async <T>(what: string, step: () => Promise<T>): Promise<T | undefined> => {
    try {
      return await step();
    } catch (error) {
      problems.push(what + ": " + (error instanceof Error ? error.message : String(error)));
      return undefined;
    }
  };

  let namespaces: Namespace[] = [];
  const worker = await attempt("reading the Worker", () => getWorker(account, name));
  const tag = state.owned.workerTag;
  const found = worker && tag ? await attempt("reading the Worker's version tags", () => workerTag(account, name, tag)) : undefined;
  if (worker && found === "tagged") {
    namespaces = (await attempt("listing the Durable Object namespaces", () => namespacesOf(account, name))) ?? [];
    const deleted = await attempt("deleting the Worker", () => deleteWorker(account, name));
    if (deleted) console.log("Deleted the Worker " + name + " (id " + worker.id + "), which carries the tag of this run's deploy");
    const left = await attempt("checking the Durable Object namespaces", () => namespacesOf(account, name));
    if (left?.length) problems.push("Durable Object namespaces of " + name + " remain: " + left.map((namespace) => namespace.class + " " + namespace.id).join(", "));
    else if (left) console.log("No Durable Object namespace of " + name + " remains (" + (namespaces.map((namespace) => namespace.class).join(", ") || "none") + " deleted with the Worker)");
  } else if (worker && (found === "untagged" || found === "unlisted")) {
    problems.push(
      "a Worker named " + name + " (id " + worker.id + ") exists, but " + (found === "untagged" ? "none of its versions carries the tag of this run's deploy" : "it listed no versions within " + LISTING_MS / 1000 + " s") +
        ", so this run cannot prove it created the Worker and left it alone. If this run's deploy created it, delete the Worker " + name + " by hand under Workers & Pages, which also deletes its Durable Objects",
    );
  } else if (worker && !tag) {
    problems.push("a Worker named " + name + " (id " + worker.id + ") exists, but this run did not record creating it, so it was left alone");
  } else if (!worker && tag) {
    console.log("No Worker named " + name + " exists, so this run's deploy left none to delete");
  }

  let storage: Storage | undefined;
  const bucket = await attempt("reading the R2 bucket", () => getBucket(account, name));
  if (bucket && bucket.creationDate === state.owned.bucketCreationDate) {
    storage = await attempt("emptying and deleting the R2 bucket", () => emptyAndDeleteBucket(account, name));
    if (storage) console.log("Deleted the R2 bucket " + name + " and its " + storage.objects + " objects");
  } else if (bucket) {
    problems.push("an R2 bucket named " + name + " (created " + bucket.creationDate + ") exists, but this run did not record creating it, so it was left alone");
  } else if (state.owned.bucketCreationDate) {
    console.log("The R2 bucket " + name + " this run created is already gone");
  }
  if (!state.owned.workerTag && !state.owned.bucketCreationDate) console.log("This run created nothing, so teardown deleted nothing");

  const deployments = state.deployments;
  const rows: [string, string][] = [
    ["Worker and bucket", name],
    ["Deployed", deployments.map((deployment) => deployment.ref + " (" + deployment.sha.slice(0, 12) + ", version " + deployment.versionId + ")").join(", then ") || "nothing"],
    ["Test client requests", state.stats.requests + " HTTPS and WebSocket requests, " + bytes(state.stats.bytesUp) + " sent, " + bytes(state.stats.bytesDown) + " received"],
    ["R2 storage at teardown", storage ? storage.objects + " objects, " + bytes(storage.bytes) : "no bucket deleted"],
    ["  chunks", storage ? storage.chunkObjects + " objects, " + bytes(storage.chunkBytes) : "-"],
    ["  metadata export", storage ? storage.exportObjects + " objects, " + bytes(storage.exportBytes) : "-"],
    ["Durable Object namespaces", namespaces.map((namespace) => namespace.class).join(", ") || "none deleted"],
  ];
  if (deployments.length > 0) {
    console.log("Waiting 60 s for the Analytics API to catch up");
    await sleep(60_000);
    rows.push(...(await analytics(account, state, namespaces)));
  }
  const width = Math.max(...rows.map(([label]) => label.length));
  console.log("\nCost-relevant usage of this run (Analytics API figures can lag by several minutes):");
  for (const [label, value] of rows) console.log("  " + label.padEnd(width) + "  " + value);
  if (process.env.GITHUB_STEP_SUMMARY) {
    const table = ["| Usage | |", "|---|---|", ...rows.map(([label, value]) => "| " + label.trim() + " | " + value.replaceAll("|", "\\|") + " |")];
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, "### Deploy test usage\n\nAnalytics API figures can lag by several minutes.\n\n" + table.join("\n") + "\n");
  }

  if (problems.length > 0) {
    for (const problem of problems) console.error("::error::teardown: " + problem);
    fail("teardown could not delete everything this run created, or found resources it did not create; check the Worker and R2 bucket named " + name + " in the dashboard");
  }
}

const [command, arg = ""] = process.argv.slice(2);
try {
  if (command === "upgrade-from") upgradeFrom(arg);
  else if (command === "deploy") await deploy(requireAccount(), arg || "HEAD");
  else if (command === "claim") await claim();
  else if (command === "teardown") await teardown(requireAccount());
  else fail("usage: run.ts upgrade-from [<ref>] | deploy <ref> | claim | teardown");
} catch (error) {
  console.error("::error::deploy test " + command + ": " + (error instanceof Error ? error.message : String(error)));
  process.exitCode = 1;
}

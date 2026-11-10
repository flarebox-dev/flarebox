// SPDX-License-Identifier: AGPL-3.0-or-later

// Checks that server/ deploys on its own, the way the deploy button sees it (docs/design/deploy-and-upgrade.md
// section 1): copies the tracked files of server/ into an empty directory, installs from the lockfile, checks the
// Wrangler configuration, bundles the Worker with a dry-run deploy, then serves the bundle locally and claims it.
// Needs no account or credentials. Usage: node scripts/check-deploy.mjs

import { execFileSync, spawn } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

/** Durable Object migration tags a deployment may already have applied. Append only (spec v0 section 9.4). */
const MIGRATION_TAGS = ["v1"];
const DURABLE_OBJECTS = { ACCOUNT: "AccountDO", SPACE: "SpaceDO" };

const server = join(dirname(fileURLToPath(import.meta.url)), "..");
const root = mkdtempSync(join(tmpdir(), "flarebox-deploy-"));
const env = { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "true" };

function fail(message) {
  console.error("::error::check-deploy: " + message);
  process.exit(1);
}

function run(command, args) {
  console.log("$ " + [command, ...args].join(" "));
  try {
    execFileSync(command, args, { cwd: root, env, stdio: "inherit" });
  } catch (error) {
    fail(command + " " + args.join(" ") + " failed with " + (error.status ?? error.message) + " in the isolated copy " + root);
  }
}

function copyTrackedFiles() {
  const files = execFileSync("git", ["ls-files", "-z"], { cwd: server, encoding: "utf8" }).split("\0").filter(Boolean);
  for (const file of files) {
    mkdirSync(dirname(join(root, file)), { recursive: true });
    cpSync(join(server, file), join(root, file));
  }
  console.log("Copied " + files.length + " tracked files of server/ to " + root);
}

async function checkConfig() {
  for (const file of [".dev.vars.example", ".env.example"]) {
    if (existsSync(join(root, file))) fail(file + " exists, so the deploy button would ask owners for secrets; Flarebox needs none");
  }
  const pkg = createRequire(join(root, "package.json"))("./package.json");
  if (pkg.scripts?.deploy !== "wrangler deploy") fail("package.json needs the script \"deploy\": \"wrangler deploy\" for the deploy button");
  const wranglerEntry = createRequire(join(root, "package.json")).resolve("wrangler");
  const { unstable_readConfig } = await import(pathToFileURL(wranglerEntry).href);
  const config = unstable_readConfig({ config: join(root, "wrangler.jsonc") });
  if (!config.observability?.enabled) fail("observability.enabled must be true, or the claim code is lost unless someone watched live logs");
  const tags = config.migrations.map((migration) => migration.tag);
  if (MIGRATION_TAGS.some((tag, i) => tags[i] !== tag)) {
    fail("Durable Object migration tags must start with " + JSON.stringify(MIGRATION_TAGS) + " and only grow, but are " + JSON.stringify(tags));
  }
  const sqliteClasses = config.migrations.flatMap((migration) => migration.new_sqlite_classes ?? []);
  for (const [name, className] of Object.entries(DURABLE_OBJECTS)) {
    const binding = config.durable_objects.bindings.find((entry) => entry.name === name);
    if (binding?.class_name !== className) fail("binding " + name + " must name the Durable Object class " + className);
    if (!sqliteClasses.includes(className)) fail("no migration creates " + className + " as a SQLite class");
  }
  const bucket = config.r2_buckets.find((entry) => entry.binding === "BUCKET");
  if (!bucket?.bucket_name) fail("the R2 binding BUCKET needs a default bucket_name for the deploy button to provision");
  for (const name of [...Object.keys(DURABLE_OBJECTS), "BUCKET"]) {
    if (!pkg.cloudflare?.bindings?.[name]?.description) fail("package.json cloudflare.bindings." + name + ".description is missing");
  }
  console.log("Wrangler configuration: migration tags " + JSON.stringify(tags) + ", bindings ACCOUNT, SPACE and BUCKET, logs persisted");
}

async function waitFor(check, what, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check().catch(() => undefined);
    if (value) return value;
    if (Date.now() > deadline) fail("timed out after " + timeoutMs + " ms waiting for " + what);
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
}

async function claimLocally() {
  const port = String(20000 + Math.floor(Math.random() * 20000));
  const base = "http://127.0.0.1:" + port;
  const local = join(root, "node_modules", ".bin", process.platform === "win32" ? "wrangler.cmd" : "wrangler");
  const dev = spawn(local, ["dev", "--ip", "127.0.0.1", "--port", port, "--persist-to", join(root, ".state")], { cwd: root, env });
  let output = "";
  dev.stdout.on("data", (chunk) => (output += chunk));
  dev.stderr.on("data", (chunk) => (output += chunk));
  const json = { "Content-Type": "application/json", Accept: "application/json" };
  try {
    const before = await waitFor(
      () => fetch(base + "/v1/server", { headers: json }).then((response) => (response.ok ? response.json() : undefined)),
      "wrangler dev to serve /v1/server",
      60_000,
    );
    if (before.claimed) fail("a fresh deployment reports that it already has an owner");
    const page = await fetch(base + "/setup").then((response) => response.text());
    if (!page.includes("not affiliated with Cloudflare, Inc.")) fail("/setup lost the notice that Flarebox is not affiliated with Cloudflare, Inc.");
    const code = await waitFor(async () => /Flarebox claim code[^:]*: ([A-Z0-9-]+)/.exec(output)?.[1], "the claim code in the Worker log", 10_000);
    const response = await fetch(base + "/v1/claim", {
      method: "POST",
      headers: json,
      body: JSON.stringify({ code, userName: "Owner", deviceName: "check-deploy", platform: "linux" }),
    });
    const body = await response.json();
    if (!response.ok || !body.deviceId) fail("claiming the local deployment failed with " + response.status + ": " + JSON.stringify(body));
    const after = await fetch(base + "/v1/server", { headers: json }).then((r) => r.json());
    if (!after.claimed) fail("the deployment does not report an owner after the claim");
    console.log("Served the bundle with wrangler dev, read the claim code from its log and claimed it (server " + after.serverVersion + ")");
  } catch (error) {
    console.error(output);
    throw error;
  } finally {
    dev.kill("SIGTERM");
  }
}

try {
  copyTrackedFiles();
  run("pnpm", ["install", "--frozen-lockfile"]);
  await checkConfig();
  run("pnpm", ["exec", "wrangler", "deploy", "--dry-run", "--outdir", join(root, ".dist")]);
  await claimLocally();
  console.log("check-deploy: server/ deploys on its own");
} finally {
  rmSync(root, { recursive: true, force: true });
}

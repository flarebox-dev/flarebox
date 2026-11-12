// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { State } from "./state.ts";

const here = dirname(fileURLToPath(import.meta.url));
const owner = { userId: "owner-id", deviceId: "device-id", refreshCredential: "private-refresh-credential" };
let work: string;

beforeEach(() => {
  work = mkdtempSync(join(tmpdir(), "koidrive-bench-unit-"));
  mkdirSync(join(work, "bin"));
  writeState({ url: "https://koidrive-test.example.workers.dev", owner });
});

afterEach(() => rmSync(work, { recursive: true, force: true }));

function writeState(extra: Partial<State>): void {
  const state: State = { name: "koidrive-test-1-1", owned: {}, deployments: [], stats: { requests: 2, bytesUp: 3, bytesDown: 4 }, ...extra };
  writeFileSync(join(work, "state.json"), JSON.stringify(state));
}

function fakeCargo(status = 0): void {
  writeFileSync(join(work, "bin", "cargo"), `#!${process.execPath}
const { writeFileSync } = require("node:fs");
const { join } = require("node:path");
writeFileSync(join(process.env.HOME, "cargo.json"), JSON.stringify({ args: process.argv.slice(2), env: process.env, cwd: process.cwd() }));
console.log("| Benchmark | MiB/s | tailed by Rust |");
process.exit(${status});
`, { mode: 0o755 });
}

function run({ command, env = {} }: { command?: string; env?: NodeJS.ProcessEnv } = {}): Promise<{ status: number | null; output: string }> {
  const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [...flags, join(here, "bench.ts"), ...(command ? [command] : [])], {
      cwd: join(here, "..", ".."),
      env: {
        PATH: join(work, "bin") + delimiter + process.env.PATH,
        HOME: work,
        TMPDIR: work,
        KOIDRIVE_DEPLOY_TEST_STATE: join(work, "state.json"),
        CLOUDFLARE_API_TOKEN: "private-cloudflare-token",
        CLOUDFLARE_ACCOUNT_ID: "private-cloudflare-account",
        GH_TOKEN: "private-github-token",
        KOIDRIVE_BENCH_OWNER: "must-not-use-this-owner",
        ...env,
      },
    });
    let output = "";
    child.stdout.on("data", (chunk: Buffer) => (output += chunk));
    child.stderr.on("data", (chunk: Buffer) => (output += chunk));
    child.on("error", reject);
    child.on("close", (status) => resolve({ status, output }));
  });
}

describe("benchmark input validation before deployment", () => {
  it.each(["1"])("accepts the supported workflow list %s without state or cargo", async (ns) => {
    rmSync(join(work, "state.json"));
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_NS: ns, KOIDRIVE_BENCH_REPETITIONS: "3" } });
    expect(result).toEqual({ status: 0, output: "" });
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it.each(["2", "4", "8", "1,2,4,8", "8,1"])("refuses unsupported list %s before deployment without state or cargo", async (ns) => {
    rmSync(join(work, "state.json"));
    fakeCargo();
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_NS: ns } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("supports only 1 on this build");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it.each(["", "0", "3", "16", "1,1", "1,2,4,8,1", "1, 2", "1,", "1;echo injected"])("rejects invalid list %s before cargo", async (ns) => {
    fakeCargo();
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_NS: ns } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("KOIDRIVE_BENCH_NS");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it.each(["", "0", "4", "1.5", "01", "2e0", "2;echo injected"])("rejects invalid repetitions %s", async (repetitions) => {
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_REPETITIONS: repetitions } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("KOIDRIVE_BENCH_REPETITIONS");
  });
});

describe("Rust benchmark child", () => {
  it("runs the exact release test with private state credentials and an allowlisted environment", async () => {
    fakeCargo();
    const result = await run({ env: { GITHUB_STEP_SUMMARY: join(work, "summary.md"), RUNNER_OS: "Linux", CARGO_TARGET_DIR: join(work, "target") } });
    expect(result.status).toBe(0);
    expect(result.output).toContain("| Benchmark |");
    const child = JSON.parse(readFileSync(join(work, "cargo.json"), "utf8"));
    expect(child.args).toEqual(["test", "-p", "koidrive-net", "--release", "--locked", "--test", "upload_bench", "upload_benchmark", "--", "--ignored", "--exact", "--nocapture"]);
    expect(child.cwd).toBe(join(here, "..", "..", ".."));
    expect(child.env).toMatchObject({ KOIDRIVE_BENCH_URL: "https://koidrive-test.example.workers.dev", KOIDRIVE_BENCH_OWNER: JSON.stringify(owner), KOIDRIVE_BENCH_NS: "1", KOIDRIVE_BENCH_REPETITIONS: "3", RUNNER_OS: "Linux", GITHUB_STEP_SUMMARY: join(work, "summary.md"), CARGO_TARGET_DIR: join(work, "target") });
    expect(child.env).not.toHaveProperty("CLOUDFLARE_API_TOKEN");
    expect(child.env).not.toHaveProperty("CLOUDFLARE_ACCOUNT_ID");
    expect(child.env).not.toHaveProperty("GH_TOKEN");
    expect(child.env).not.toHaveProperty("KOIDRIVE_DEPLOY_TEST_STATE");
    expect(result.output).not.toContain(owner.refreshCredential);
    expect(JSON.parse(readFileSync(join(work, "state.json"), "utf8")).stats).toEqual({ requests: 2, bytesUp: 3, bytesDown: 4 });
  });

  it("refuses unsupported concurrency without reading state or launching cargo", async () => {
    fakeCargo();
    writeFileSync(join(work, "state.json"), "invalid private state");
    const result = await run({ env: { KOIDRIVE_BENCH_NS: "1,2,4,8", KOIDRIVE_BENCH_REPETITIONS: "1" } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("supports only 1 on this build");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it("reports cargo failure without credentials", async () => {
    fakeCargo(7);
    const result = await run();
    expect(result.status).toBe(1);
    expect(result.output).toContain("cargo exited with status 7");
    expect(result.output).not.toContain(owner.refreshCredential);
  });

  it.each([{ name: "URL", state: { url: undefined } }, { name: "owner", state: { owner: undefined } }])("refuses state missing $name before launching cargo", async ({ state }) => {
    writeState({ url: "https://koidrive-test.example.workers.dev", owner, ...state });
    fakeCargo();
    const result = await run();
    expect(result.status).toBe(1);
    expect(result.output).toContain("claimed deployment");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it("does not expose malformed credential-bearing state in exceptions", async () => {
    writeFileSync(join(work, "state.json"), '{"refreshCredential":"private-refresh-credential"');
    const result = await run();
    expect(result.status).toBe(1);
    expect(result.output).toContain("could not read the private deploy-test state");
    expect(result.output).not.toContain(owner.refreshCredential);
    expect(result.output).not.toContain("SyntaxError");
  });

  it("sanitizes spawn errors even when PATH contains a credential", async () => {
    const result = await run({ env: { PATH: join(work, owner.refreshCredential) } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("could not start cargo");
    expect(result.output).not.toContain(owner.refreshCredential);
  });

  it.each([undefined, "true"])("allows a smaller payload only outside GitHub Actions, GITHUB_ACTIONS=%s", async (actions) => {
    fakeCargo();
    expect((await run({ env: { GITHUB_ACTIONS: actions, KOIDRIVE_BENCH_MIB: "8" } })).status).toBe(0);
    expect(JSON.parse(readFileSync(join(work, "cargo.json"), "utf8")).env.KOIDRIVE_BENCH_MIB).toBe(actions ? undefined : "8");
  });
});

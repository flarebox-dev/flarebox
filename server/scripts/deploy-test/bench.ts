// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { readState } from "./state.ts";

class Failed extends Error {}

const SUPPORTED_NS = ["1"];

function inputs(): { ns: string; repetitions: string } {
  const ns = process.env.KOIDRIVE_BENCH_NS ?? "1";
  const values = ns.split(",");
  if (values.length > 4 || new Set(values).size !== values.length || values.some((n) => !SUPPORTED_NS.includes(n))) {
    throw new Failed("KOIDRIVE_BENCH_NS supports only " + SUPPORTED_NS.join(",") + " on this build; provide distinct comma-separated values");
  }
  const repetitions = process.env.KOIDRIVE_BENCH_REPETITIONS ?? "3";
  if (!/^[1-3]$/.test(repetitions)) throw new Failed("KOIDRIVE_BENCH_REPETITIONS must be an integer from 1 to 3");
  return { ns, repetitions };
}

async function bench({ ns, repetitions }: ReturnType<typeof inputs>): Promise<void> {
  let state: ReturnType<typeof readState>;
  try {
    state = readState();
  } catch {
    throw new Failed("could not read the private deploy-test state");
  }
  if (!state?.url || !state.owner) throw new Failed("benchmark requires a claimed deployment in the deploy-test state");
  const env: NodeJS.ProcessEnv = {};
  for (const key of ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "SSL_CERT_FILE", "SSL_CERT_DIR", "TMPDIR", "GITHUB_STEP_SUMMARY", "GITHUB_SHA", "RUNNER_ENVIRONMENT", "RUNNER_NAME", "RUNNER_OS", "RUNNER_ARCH", "RUNNER_REGION", "AZURE_REGION"]) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  env.KOIDRIVE_BENCH_URL = state.url;
  env.KOIDRIVE_BENCH_OWNER = JSON.stringify(state.owner);
  env.KOIDRIVE_BENCH_NS = ns;
  env.KOIDRIVE_BENCH_REPETITIONS = repetitions;
  if (!process.env.GITHUB_ACTIONS && process.env.KOIDRIVE_BENCH_MIB !== undefined) env.KOIDRIVE_BENCH_MIB = process.env.KOIDRIVE_BENCH_MIB;
  await new Promise<void>((resolve, reject) => {
    const child = spawn("cargo", ["test", "-p", "koidrive-net", "--release", "--locked", "--test", "upload_bench", "upload_benchmark", "--", "--ignored", "--exact", "--nocapture"], {
      cwd: fileURLToPath(new URL("../../../", import.meta.url)),
      env,
      stdio: ["ignore", "inherit", "inherit"],
    });
    child.on("error", () => reject(new Failed("could not start cargo")));
    child.on("close", (status) => {
      if (status === 0) resolve();
      else reject(new Failed(status === null ? "cargo terminated without an exit status" : "cargo exited with status " + status));
    });
  });
}

try {
  const config = inputs();
  const command = process.argv[2];
  if (process.argv.length > 3 || (command !== undefined && command !== "validate")) throw new Failed("usage: bench.ts [validate]");
  if (command !== "validate") await bench(config);
} catch (error) {
  console.error("::error::deploy benchmark: " + (error instanceof Failed ? error.message : "benchmark runner failed; private error details suppressed"));
  process.exitCode = 1;
}

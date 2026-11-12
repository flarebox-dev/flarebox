// SPDX-License-Identifier: AGPL-3.0-or-later

export type ScenarioName = "s1" | "s4";

export interface Config {
  scenario: ScenarioName;
  steps: number[];
  warmupMs: number;
  measureMs: number;
  maxRowsWritten: number;
  subscribersPerSpace: number;
}

export const S4_WRITERS_PER_SPACE = 25;
export const MAX_WRITERS = 400;
export const MAX_STEPS_MS = 180 * 60_000;
export const FALLBACK_ROWS_PER_COMMIT = 60;
const BUDGET_MARGIN = 1.25;

const DEFAULT_STEPS: Record<ScenarioName, number[]> = { s1: [1, 5, 10, 25, 50, 100, 200], s4: [1, 4, 16] };

function whole(env: Record<string, string | undefined>, name: string, fallback: number, min: number, max = Number.MAX_SAFE_INTEGER): number {
  const text = env[name];
  if (text === undefined || text === "") return fallback;
  const value = Number(text);
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new Error(name + " must be a whole number of at least " + min + (max < Number.MAX_SAFE_INTEGER ? " and at most " + max : "") + ", not " + JSON.stringify(text));
  }
  return value;
}

export function readConfig(env: Record<string, string | undefined>): Config {
  const scenario = env.LOAD_SCENARIO ?? "";
  if (scenario !== "s1" && scenario !== "s4") throw new Error("LOAD_SCENARIO must be s1 or s4, not " + JSON.stringify(scenario));
  let steps = DEFAULT_STEPS[scenario];
  if (env.LOAD_STEPS) {
    steps = env.LOAD_STEPS.split(",").map((part) => Number(part.trim()));
    if (steps.some((step, i) => !Number.isSafeInteger(step) || step < 1 || (i > 0 && step <= steps[i - 1]!))) {
      throw new Error("LOAD_STEPS must be rising whole numbers of at least 1 separated by commas, not " + JSON.stringify(env.LOAD_STEPS));
    }
  }
  const config: Config = {
    scenario,
    steps,
    warmupMs: whole(env, "LOAD_WARMUP_SECONDS", 120, 0, 600) * 1000,
    measureMs: whole(env, "LOAD_MEASURE_SECONDS", 480, 10, 1800) * 1000,
    maxRowsWritten: whole(env, "LOAD_MAX_ROWS_WRITTEN", 40_000_000, 1),
    subscribersPerSpace: whole(env, "LOAD_SUBSCRIBERS", 1, 0),
  };
  const stepsMs = steps.length * (config.warmupMs + config.measureMs);
  if (stepsMs > MAX_STEPS_MS) {
    throw new Error(steps.length + " steps of " + (config.warmupMs + config.measureMs) / 1000 + " s take " + stepsMs / 60_000 + " minutes; at most " + MAX_STEPS_MS / 60_000 + " fit the run step with setup, calibration and analytics");
  }
  const last = layout(config, steps.at(-1)!);
  if (last.spaces * last.writersPerSpace > MAX_WRITERS) {
    throw new Error("the last step needs " + last.spaces * last.writersPerSpace + " writers; one runner holds at most " + MAX_WRITERS);
  }
  return config;
}

export function layout(config: Pick<Config, "scenario">, step: number): { spaces: number; writersPerSpace: number } {
  return config.scenario === "s1" ? { spaces: 1, writersPerSpace: step } : { spaces: step, writersPerSpace: S4_WRITERS_PER_SPACE };
}

export function percentile(sorted: number[], p: number): number | undefined {
  if (sorted.length === 0) return undefined;
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1))];
}

export interface StepCounts {
  step: number;
  spaces: number;
  writers: number;
  measuredMs: number;
  latenciesMs: number[];
  accepted: number;
  refused: Record<string, number>;
  timeouts: number;
  closes: Record<string, number>;
  pushLagMs: number[];
  probeMs: number[];
  probeErrors: number;
  generatorCpuMs: number;
  eventLoopDelayP99Ms: number;
}

export function emptyCounts(step: number, spaces: number, writers: number): StepCounts {
  return { step, spaces, writers, measuredMs: 0, latenciesMs: [], accepted: 0, refused: {}, timeouts: 0, closes: {}, pushLagMs: [], probeMs: [], probeErrors: 0, generatorCpuMs: 0, eventLoopDelayP99Ms: 0 };
}

export interface StepSummary {
  step: number;
  spaces: number;
  writers: number;
  measuredMs: number;
  accepted: number;
  commitsPerSecond: number;
  commitMs: { p50?: number; p95?: number; p99?: number };
  failed: number;
  failedShare: number;
  refused: Record<string, number>;
  timeouts: number;
  closes: Record<string, number>;
  pushLagMs: { p50?: number; p95?: number; samples: number };
  probeMs: { p50?: number; p95?: number; samples: number; errors: number };
  generatorCpuShare: number;
  eventLoopDelayP99Ms: number;
}

export function summarize(counts: StepCounts): StepSummary {
  const ascending = (values: number[]) => [...values].sort((a, b) => a - b);
  const latencies = ascending(counts.latenciesMs);
  const lag = ascending(counts.pushLagMs);
  const probe = ascending(counts.probeMs);
  const refusals = Object.entries(counts.refused).filter(([code]) => !code.startsWith("op ")).reduce((sum, [, n]) => sum + n, 0);
  const failed = refusals + counts.timeouts;
  return {
    step: counts.step,
    spaces: counts.spaces,
    writers: counts.writers,
    measuredMs: counts.measuredMs,
    accepted: counts.accepted,
    commitsPerSecond: counts.measuredMs > 0 ? counts.accepted / (counts.measuredMs / 1000) : 0,
    commitMs: { p50: percentile(latencies, 50), p95: percentile(latencies, 95), p99: percentile(latencies, 99) },
    failed,
    failedShare: failed + counts.accepted > 0 ? failed / (failed + counts.accepted) : 0,
    refused: counts.refused,
    timeouts: counts.timeouts,
    closes: counts.closes,
    pushLagMs: { p50: percentile(lag, 50), p95: percentile(lag, 95), samples: lag.length },
    probeMs: { p50: percentile(probe, 50), p95: percentile(probe, 95), samples: probe.length, errors: counts.probeErrors },
    generatorCpuShare: counts.measuredMs > 0 ? counts.generatorCpuMs / counts.measuredMs : 0,
    eventLoopDelayP99Ms: counts.eventLoopDelayP99Ms,
  };
}

export function saturated(summary: StepSummary): string | undefined {
  if (summary.accepted === 0) return "no commit was accepted";
  if (summary.failedShare > 0.1) return (summary.failedShare * 100).toFixed(1) + "% of commits failed";
  if ((summary.commitMs.p50 ?? 0) > 10_000) return "the median commit took " + Math.round(summary.commitMs.p50!) + " ms";
  return undefined;
}

export function commitBudget(maxRowsWritten: number, rowsPerCommit: number): number {
  return Math.floor(maxRowsWritten / (Math.max(rowsPerCommit, 1) * BUDGET_MARGIN));
}

export function commitsAt(timeline: [number, number][], atMs: number): number {
  if (timeline.length === 0) return 0;
  if (atMs <= timeline[0]![0]) return timeline[0]![1];
  for (let i = 1; i < timeline.length; i++) {
    const [t1, c1] = timeline[i]!;
    if (atMs <= t1) {
      const [t0, c0] = timeline[i - 1]!;
      return c0 + ((c1 - c0) * (atMs - t0)) / Math.max(t1 - t0, 1);
    }
  }
  return timeline.at(-1)![1];
}

/** Subscription states that no longer pay for the plan (Cloudflare's List Subscriptions reference). */
const ENDED_SUBSCRIPTION = /^(cancelled|failed|expired)$/i;

export function workersPlan(listed: { name: string; price: number; state: string }[] | undefined, unreadable: string, confirmed: boolean): { ok: boolean; message: string } {
  const paid = (listed ?? []).filter((subscription) => /workers/i.test(subscription.name) && subscription.price > 0 && !ENDED_SUBSCRIPTION.test(subscription.state));
  if (paid.length > 0) return { ok: true, message: "The account subscribes to " + paid.map((subscription) => subscription.name).join(", ") + ", so it is on Workers Paid." };
  const seen = listed
    ? "The account's subscriptions list no active paid Workers plan (" + (listed.map((subscription) => subscription.name + (subscription.state ? " " + subscription.state : "")).join(", ") || "none") + ")." : "The account's subscriptions cannot be read: " + unreadable + ".";
  if (confirmed) return { ok: true, message: seen + " Going on because the run was started with workers_paid=confirmed." };
  return { ok: false, message: seen + " The load test needs Workers Paid, because Workers Free allows 100,000 rows written a day. Nothing was deployed. Once the account is on Workers Paid, start the run again with workers_paid=confirmed." };
}

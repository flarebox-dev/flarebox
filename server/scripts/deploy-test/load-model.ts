// SPDX-License-Identifier: AGPL-3.0-or-later

// The parts of the load test (load.ts, docs/load-test.md) that need no server: its configuration, the statistics of one
// step, when to stop, and the commit budget that keeps a run's rows written under a limit. load.unit.ts tests them.

export type ScenarioName = "s1" | "s4";

export interface Config {
  scenario: ScenarioName;
  /** S1: the writers in the one space at each step. S4: the spaces at each step, each with S4_WRITERS_PER_SPACE writers. */
  steps: number[];
  warmupMs: number;
  measureMs: number;
  /** The run stops committing before its commits would write more rows than this, by the calibrated rows per commit. */
  maxRowsWritten: number;
  /** Live connections per space that receive every commit, so a commit pays for one push as well as for itself. */
  subscribersPerSpace: number;
}

export const S4_WRITERS_PER_SPACE = 25;
/** One runner holds every connection, so a run may not ask for more. */
export const MAX_WRITERS = 400;
/** Used when the Analytics API cannot measure the rows a commit writes. Above every estimate in the plan (20 to 40). */
export const FALLBACK_ROWS_PER_COMMIT = 60;
/** Headroom on the calibrated rows per commit, for commits that write more than the calibration's did. */
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

/** Reads the LOAD_* variables the workflow sets from its inputs. */
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
  const last = layout(config, steps.at(-1)!);
  if (last.spaces * last.writersPerSpace > MAX_WRITERS) {
    throw new Error("the last step needs " + last.spaces * last.writersPerSpace + " writers; one runner holds at most " + MAX_WRITERS);
  }
  return config;
}

/** The spaces and the writers in each space at one step. */
export function layout(config: Pick<Config, "scenario">, step: number): { spaces: number; writersPerSpace: number } {
  return config.scenario === "s1" ? { spaces: 1, writersPerSpace: step } : { spaces: step, writersPerSpace: S4_WRITERS_PER_SPACE };
}

/** Nearest-rank percentile of ascending values; undefined when there are none. */
export function percentile(sorted: number[], p: number): number | undefined {
  if (sorted.length === 0) return undefined;
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1))];
}

/** What a step records while it measures. */
export interface StepCounts {
  step: number;
  spaces: number;
  writers: number;
  /** How long the step measured; shorter than configured when the commit budget ran out. */
  measuredMs: number;
  latenciesMs: number[];
  accepted: number;
  /** Refused commits by error code, and accepted commits whose op was not applied as "op <result>". */
  refused: Record<string, number>;
  /** Commits without an answer within the timeout. */
  timeouts: number;
  /** WebSocket closes by code, and failed connection attempts as "connect". */
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
  /** Failed commits as a share of every commit that got an answer or timed out. */
  failedShare: number;
  refused: Record<string, number>;
  timeouts: number;
  closes: Record<string, number>;
  pushLagMs: { p50?: number; p95?: number; samples: number };
  probeMs: { p50?: number; p95?: number; samples: number; errors: number };
  /** The load generator's CPU time over the step's time: near 1 means its one thread was busy all the time. */
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

/** Why the steps after this one would only measure an overloaded server, or undefined to go on. */
export function saturated(summary: StepSummary): string | undefined {
  if (summary.accepted === 0) return "no commit was accepted";
  if (summary.failedShare > 0.1) return (summary.failedShare * 100).toFixed(1) + "% of commits failed";
  if ((summary.commitMs.p50 ?? 0) > 10_000) return "the median commit took " + Math.round(summary.commitMs.p50!) + " ms";
  return undefined;
}

/** The commits a run may make so that they write at most maxRowsWritten rows. */
export function commitBudget(maxRowsWritten: number, rowsPerCommit: number): number {
  return Math.floor(maxRowsWritten / (Math.max(rowsPerCommit, 1) * BUDGET_MARGIN));
}

/** Commits made by a given time, interpolated from a rising [time, commits] timeline. */
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

/** What the account's subscriptions say about its Workers plan, from the API or, failing that, the dispatcher. */
export function workersPlan(listed: { name: string; price: number; state: string }[] | undefined, unreadable: string, confirmed: boolean): { ok: boolean; message: string } {
  const paid = (listed ?? []).filter((subscription) => /workers/i.test(subscription.name) && subscription.price > 0 && !ENDED_SUBSCRIPTION.test(subscription.state));
  if (paid.length > 0) return { ok: true, message: "The account subscribes to " + paid.map((subscription) => subscription.name).join(", ") + ", so it is on Workers Paid." };
  const seen = listed
    ? "The account's subscriptions list no active paid Workers plan (" + (listed.map((subscription) => subscription.name + (subscription.state ? " " + subscription.state : "")).join(", ") || "none") + ")." : "The account's subscriptions cannot be read: " + unreadable + ".";
  if (confirmed) return { ok: true, message: seen + " Going on because the run was started with workers_paid=confirmed." };
  return { ok: false, message: seen + " The load test needs Workers Paid, because Workers Free allows 100,000 rows written a day. Nothing was deployed. Once the account is on Workers Paid, start the run again with workers_paid=confirmed." };
}

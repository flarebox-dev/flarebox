// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { commitBudget, commitsAt, emptyCounts, layout, percentile, readConfig, saturated, summarize, workersPlan } from "./load-model.ts";

describe("the load test's configuration", () => {
  it("runs the plan's steps by default", () => {
    const s1 = readConfig({ LOAD_SCENARIO: "s1" });
    expect(s1.steps).toEqual([1, 5, 10, 25, 50, 100, 200]);
    expect(s1.warmupMs).toBe(120_000);
    expect(s1.measureMs).toBe(480_000);
    expect(s1.maxRowsWritten).toBe(40_000_000);
    expect(s1.subscribersPerSpace).toBe(1);
    expect(readConfig({ LOAD_SCENARIO: "s4" }).steps).toEqual([1, 4, 16]);
    expect(layout(s1, 50)).toEqual({ spaces: 1, writersPerSpace: 50 });
    expect(layout({ scenario: "s4" }, 4)).toEqual({ spaces: 4, writersPerSpace: 25 });
  });

  it("takes steps and times from the workflow's inputs", () => {
    const config = readConfig({ LOAD_SCENARIO: "s1", LOAD_STEPS: "1, 5,10", LOAD_WARMUP_SECONDS: "0", LOAD_MEASURE_SECONDS: "30", LOAD_MAX_ROWS_WRITTEN: "1000", LOAD_SUBSCRIBERS: "0" });
    expect(config).toMatchObject({ steps: [1, 5, 10], warmupMs: 0, measureMs: 30_000, maxRowsWritten: 1000, subscribersPerSpace: 0 });
  });

  it("refuses an unknown scenario, steps that do not rise, and more writers than one runner holds", () => {
    expect(() => readConfig({ LOAD_SCENARIO: "s2" })).toThrow(/s1 or s4/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_STEPS: "5,5" })).toThrow(/rising/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_STEPS: "0,1" })).toThrow(/rising/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_STEPS: "401" })).toThrow(/at most 400/);
    expect(() => readConfig({ LOAD_SCENARIO: "s4", LOAD_STEPS: "1,17" })).toThrow(/425 writers/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_MEASURE_SECONDS: "5" })).toThrow(/at least 10/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_MEASURE_SECONDS: "1801" })).toThrow(/at most 1800/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_WARMUP_SECONDS: "601" })).toThrow(/at most 600/);
    expect(() => readConfig({ LOAD_SCENARIO: "s1", LOAD_WARMUP_SECONDS: "600", LOAD_MEASURE_SECONDS: "1800" })).toThrow(/7 steps of 2400 s take 280 minutes; at most 180/);
    expect(readConfig({ LOAD_SCENARIO: "s4", LOAD_WARMUP_SECONDS: "600", LOAD_MEASURE_SECONDS: "1800" }).steps).toEqual([1, 4, 16]);
  });
});

describe("a step's statistics", () => {
  it("takes nearest-rank percentiles", () => {
    const values = Array.from({ length: 100 }, (_, i) => i + 1);
    expect(percentile(values, 50)).toBe(50);
    expect(percentile(values, 99)).toBe(99);
    expect(percentile([7], 99)).toBe(7);
    expect(percentile([], 50)).toBeUndefined();
  });

  it("counts refusals and timeouts as failures, and not an op that was applied otherwise", () => {
    const counts = emptyCounts(10, 1, 10);
    counts.measuredMs = 10_000;
    counts.accepted = 900;
    counts.latenciesMs = [30, 10, 20];
    counts.refused = { RETRY_LATER: 60, "op CONFLICT_COPY": 5 };
    counts.timeouts = 40;
    counts.generatorCpuMs = 2500;
    const summary = summarize(counts);
    expect(summary.commitsPerSecond).toBe(90);
    expect(summary.commitMs).toEqual({ p50: 20, p95: 30, p99: 30 });
    expect(summary.failed).toBe(100);
    expect(summary.failedShare).toBeCloseTo(0.1);
    expect(summary.generatorCpuShare).toBe(0.25);
    expect(saturated(summary)).toBeUndefined();
  });

  it("stops the steps when commits fail, crawl, or stop", () => {
    const counts = emptyCounts(50, 1, 50);
    counts.measuredMs = 10_000;
    counts.accepted = 100;
    counts.timeouts = 12;
    expect(saturated(summarize(counts))).toMatch(/10.7% of commits failed/);
    counts.timeouts = 0;
    counts.latenciesMs = [12_000];
    expect(saturated(summarize(counts))).toMatch(/median commit took 12000 ms/);
    counts.accepted = 0;
    expect(saturated(summarize(counts))).toBe("no commit was accepted");
  });
});

describe("the run's limits", () => {
  it("leaves headroom in the commit budget", () => {
    expect(commitBudget(40_000_000, 32)).toBe(1_000_000);
    expect(commitBudget(1000, 0)).toBe(800);
  });

  it("interpolates commits from the timeline", () => {
    const timeline: [number, number][] = [
      [1000, 0],
      [2000, 100],
      [4000, 300],
    ];
    expect(commitsAt(timeline, 500)).toBe(0);
    expect(commitsAt(timeline, 1500)).toBe(50);
    expect(commitsAt(timeline, 3000)).toBe(200);
    expect(commitsAt(timeline, 9000)).toBe(300);
    expect(commitsAt([], 1)).toBe(0);
  });

  it("deploys only on Workers Paid, by the API or by the dispatcher's word", () => {
    expect(workersPlan([{ name: "Workers Paid", price: 5, state: "Paid" }], "", false).ok).toBe(true);
    expect(workersPlan([{ name: "Workers Free", price: 0, state: "Paid" }], "", false)).toMatchObject({ ok: false, message: expect.stringMatching(/no active paid Workers plan \(Workers Free Paid\)/) });
    expect(workersPlan([{ name: "Workers Paid", price: 5, state: "Cancelled" }], "", false)).toMatchObject({ ok: false, message: expect.stringMatching(/\(Workers Paid Cancelled\)/) });
    expect(workersPlan([{ name: "Workers Paid", price: 5, state: "Cancelled" }], "", true).ok).toBe(true);
    expect(workersPlan(undefined, "GET /accounts/x/subscriptions returned HTTP 403", false)).toMatchObject({ ok: false, message: expect.stringMatching(/cannot be read.*Nothing was deployed/) });
    expect(workersPlan(undefined, "HTTP 403", true)).toMatchObject({ ok: true, message: expect.stringMatching(/workers_paid=confirmed/) });
  });
});

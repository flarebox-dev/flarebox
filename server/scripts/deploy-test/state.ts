// SPDX-License-Identifier: AGPL-3.0-or-later

// What one deploy-test run knows, kept in a file on the runner between its steps. It holds the test owner's refresh
// credential, so it is never printed and lives only in the runner's temporary directory.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Test Workers and buckets carry this prefix, and teardown refuses any other name. */
export const NAME_PREFIX = "flarebox-test-";

export interface Deployment {
  ref: string;
  sha: string;
  /** The copy of server/ that was deployed. */
  dir: string;
  versionId: string;
  atMs: number;
}

export interface Credentials {
  userId: string;
  deviceId: string;
  refreshCredential: string;
}

export interface Stats {
  requests: number;
  bytesUp: number;
  bytesDown: number;
}

/** What the upgrade's write phase stored, for the verify phase to find again. */
export interface Written {
  spaceId: string;
  files: { nodeId: string; name: string; sha256: string; size: number }[];
  linkId: string;
  headSeq: string;
  second: Credentials;
}

export interface State {
  name: string;
  url?: string;
  deployments: Deployment[];
  owner?: Credentials;
  stats: Stats;
  written?: Written;
}

export function statePath(): string {
  return process.env.FLAREBOX_DEPLOY_TEST_STATE || join(process.env.RUNNER_TEMP || tmpdir(), "flarebox-deploy-test.json");
}

/** The Worker and bucket name of this run: FLAREBOX_TEST_NAME, which must carry the test prefix. */
export function testName(): string {
  const name = process.env.FLAREBOX_TEST_NAME ?? "";
  if (!name.startsWith(NAME_PREFIX) || !/^[a-z0-9-]{3,63}$/.test(name)) {
    throw new Error("FLAREBOX_TEST_NAME must be a lowercase name of at most 63 characters starting with " + JSON.stringify(NAME_PREFIX));
  }
  return name;
}

export function readState(): State {
  const path = statePath();
  if (!existsSync(path)) return { name: testName(), deployments: [], stats: { requests: 0, bytesUp: 0, bytesDown: 0 } };
  return JSON.parse(readFileSync(path, "utf8")) as State;
}

export function writeState(state: State): void {
  writeFileSync(statePath(), JSON.stringify(state, null, 2), { mode: 0o600 });
}

/** Adds one phase's request counts to the stored totals. */
export function addStats(stats: Stats): void {
  const state = readState();
  state.stats.requests += stats.requests;
  state.stats.bytesUp += stats.bytesUp;
  state.stats.bytesDown += stats.bytesDown;
  writeState(state);
}

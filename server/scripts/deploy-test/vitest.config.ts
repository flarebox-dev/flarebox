// SPDX-License-Identifier: AGPL-3.0-or-later

import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

// The deploy test's suites run in Node against a deployed server, one file at a time and in order, and stop at the
// first failure because each test builds on the one before. Waiting for R2 export segments takes minutes.
export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  test: {
    include: ["smoke.ts", "upgrade-write.ts", "upgrade-verify.ts", "run.unit.ts", "e2e.unit.ts", "load.ts", "load.unit.ts", "bench.unit.ts"],
    environment: "node",
    testTimeout: 20 * 60_000,
    fileParallelism: false,
    bail: 1,
  },
});

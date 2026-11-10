// SPDX-License-Identifier: AGPL-3.0-or-later

import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: { bindings: { FLAREBOX_RECOVERY_KEY: "test-recovery-key-for-local-workerd-only" } },
    }),
  ],
});

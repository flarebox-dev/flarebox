// SPDX-License-Identifier: AGPL-3.0-or-later

import { resolve } from "node:path";
import { defineConfig } from "vite";

export default defineConfig({
  root: resolve(import.meta.dirname, "src"),
  publicDir: resolve(import.meta.dirname, "public"),
  base: "/",
  build: {
    outDir: resolve(import.meta.dirname, "dist"),
    emptyOutDir: true,
    target: "es2023",
    modulePreload: { polyfill: false },
    assetsInlineLimit: 0,
    rolldownOptions: {
      input: { viewer: resolve(import.meta.dirname, "src/viewer/index.html") },
      output: {
        entryFileNames: "[name]/[name].js",
        chunkFileNames: "shared/[name]-[hash].js",
        assetFileNames: "[name]/[name][extname]",
      },
    },
  },
});

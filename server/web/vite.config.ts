// SPDX-License-Identifier: AGPL-3.0-or-later

import { resolve } from "node:path";
import { defineConfig } from "vite";

// A multi-page build: each page is an entry under src/, built to dist/ where Workers Static Assets serves it (RFC 0005
// section 8). File names keep no hash so each page's URLs stay fixed. The CSP allows no inline script or style, so
// nothing may be inlined: no module preload polyfill and no data: URLs for assets.
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

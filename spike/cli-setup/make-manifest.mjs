// Throwaway: writes manifest.json for a release bundle from server/wrangler.jsonc, the way a release job would.
// Usage: node make-manifest.mjs <server dir> <bundle dir>
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
const [server, bundle] = process.argv.slice(2);
const config = JSON.parse(readFileSync(join(server, "wrangler.jsonc"), "utf8").replace(/^\s*\/\/.*$/gm, ""));
const r2 = config.r2_buckets[0];
const manifest = {
  version: JSON.parse(readFileSync(join(server, "package.json"), "utf8")).version,
  main_module: "worker.js",
  compatibility_date: config.compatibility_date,
  durable_objects: config.durable_objects.bindings,
  r2_binding: r2.binding,
  default_bucket: r2.bucket_name,
  migrations: config.migrations,
  ...(config.assets ? { assets: { binding: config.assets.binding } } : {}),
  ...(config.observability ? { observability: config.observability } : {}),
};
writeFileSync(join(bundle, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
console.log(JSON.stringify(manifest));

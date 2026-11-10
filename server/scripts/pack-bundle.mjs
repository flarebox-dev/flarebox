// SPDX-License-Identifier: AGPL-3.0-or-later

// Packs the server the way `flarebox setup` uploads it (docs/design/cli-setup.md section 4.2): the Worker module from
// a dry-run deploy, the static assets, and manifest.json with what an API upload needs from wrangler.jsonc. The CLI
// embeds the result, so its deploy needs neither Node nor Wrangler. Needs no account or credentials.
// Usage: node scripts/pack-bundle.mjs <output directory>

import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const DRY_RUN_MS = 2 * 60_000;

const server = join(dirname(fileURLToPath(import.meta.url)), "..");
const output = process.argv[2] && resolve(process.argv[2]);
if (!output || process.argv.length !== 3) {
  console.error("usage: node scripts/pack-bundle.mjs <output directory>");
  process.exit(2);
}

function fail(message) {
  console.error("pack-bundle: " + message);
  process.exit(1);
}

/** Drops undefined fields, which Wrangler's normalized config keeps for options that are not set. */
function defined(object) {
  return Object.fromEntries(Object.entries(object).filter(([, value]) => value !== undefined));
}

const require = createRequire(join(server, "package.json"));
const { unstable_readConfig } = await import(pathToFileURL(require.resolve("wrangler")).href);
const config = unstable_readConfig({ config: join(server, "wrangler.jsonc") });
const version = /SERVER_VERSION = "([^"]+)"/.exec(readFileSync(join(server, "src", "version.ts"), "utf8"))?.[1];
if (!version) fail("src/version.ts has no SERVER_VERSION");
if (config.r2_buckets.length !== 1) fail("wrangler.jsonc must bind exactly one R2 bucket, found " + config.r2_buckets.length);
if (!config.assets?.directory || !config.assets.binding) fail("wrangler.jsonc must set assets.directory and assets.binding");

const built = mkdtempSync(join(tmpdir(), "flarebox-bundle-"));
try {
  console.log("$ wrangler deploy --dry-run --outdir " + built);
  execFileSync("pnpm", ["exec", "wrangler", "deploy", "--dry-run", "--outdir", built], {
    cwd: server,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false" },
    stdio: "inherit",
    timeout: DRY_RUN_MS,
    killSignal: "SIGKILL",
  });
  const mainModule = basename(config.main, extname(config.main)) + ".js";
  if (!existsSync(join(built, mainModule))) fail("the dry run wrote no " + mainModule + "; it wrote " + readdirSync(built).join(", "));
  const assets = resolve(server, config.assets.directory);
  if (!existsSync(assets) || readdirSync(assets).length === 0) fail(config.assets.directory + " is empty after the build");

  rmSync(output, { recursive: true, force: true });
  mkdirSync(join(output, "worker"), { recursive: true });
  cpSync(join(built, mainModule), join(output, "worker", mainModule));
  cpSync(assets, join(output, "assets"), { recursive: true });
  const r2 = config.r2_buckets[0];
  const manifest = defined({
    version,
    main_module: mainModule,
    compatibility_date: config.compatibility_date,
    durable_objects: config.durable_objects.bindings.map(({ name, class_name }) => ({ name, class_name })),
    r2_binding: r2.binding,
    default_bucket: r2.bucket_name,
    migrations: config.migrations.map(defined),
    assets: { binding: config.assets.binding },
    observability: config.observability,
    code_update_strategy: config.durable_objects.code_update_strategy,
  });
  writeFileSync(join(output, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
  console.log(JSON.stringify(manifest));
} finally {
  rmSync(built, { recursive: true, force: true });
}

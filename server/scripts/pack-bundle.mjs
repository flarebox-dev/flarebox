// SPDX-License-Identifier: AGPL-3.0-or-later

// Packs the server the way `flarebox setup` uploads it (docs/design/cli-setup.md section 4.2): the Worker module from
// a dry-run deploy, the static assets, and manifest.json with what an API upload needs from wrangler.jsonc. The CLI
// embeds the result, so its deploy needs neither Node nor Wrangler. Needs no account or credentials.
// Usage: node scripts/pack-bundle.mjs <output directory>

import { execFileSync } from "node:child_process";
import {
  constants,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { basename, dirname, extname, isAbsolute, join, relative, resolve, sep } from "node:path";
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

/** Copies a file or a directory tree to a path that must not exist yet, so that nothing is ever replaced. */
function copyNew(from, to) {
  if (!statSync(from).isDirectory()) return copyFileSync(from, to, constants.COPYFILE_EXCL);
  mkdirSync(to);
  for (const entry of readdirSync(from)) copyNew(join(from, entry), join(to, entry));
}

/** The path with every symlink in its existing part resolved; the rest need not exist. */
function realPath(path) {
  try {
    return realpathSync(path);
  } catch {
    return join(realPath(dirname(path)), basename(path));
  }
}

/** Drops undefined fields, which Wrangler's normalized config keeps for options that are not set. */
function defined(object) {
  return Object.fromEntries(Object.entries(object).filter(([, value]) => value !== undefined));
}

// The script never deletes anything at the output, so it writes only into a new or empty directory.
if (existsSync(output) && (!statSync(output).isDirectory() || readdirSync(output).length > 0)) {
  fail(output + " exists and is not an empty directory; name a new or empty one");
}

const require = createRequire(join(server, "package.json"));
const { unstable_readConfig } = await import(pathToFileURL(require.resolve("wrangler")).href);
const config = unstable_readConfig({ config: join(server, "wrangler.jsonc") });
const version = /SERVER_VERSION = "([^"]+)"/.exec(readFileSync(join(server, "src", "version.ts"), "utf8"))?.[1];
if (!version) fail("src/version.ts has no SERVER_VERSION");
if (config.r2_buckets.length !== 1) fail("wrangler.jsonc must bind exactly one R2 bucket, found " + config.r2_buckets.length);
if (!config.assets?.directory || !config.assets.binding) fail("wrangler.jsonc must set assets.directory and assets.binding");
const fromAssets = relative(realPath(resolve(server, config.assets.directory)), realPath(output));
if (fromAssets === "" || (fromAssets.split(sep)[0] !== ".." && !isAbsolute(fromAssets))) {
  fail(output + " is inside " + config.assets.directory + ", which the packer copies into it; name a directory outside it");
}

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
  // Every write creates its path exclusively, so a file that appeared in the output during the build stops the packer.
  try {
    mkdirSync(output, { recursive: true });
    mkdirSync(join(output, "worker"));
    copyFileSync(join(built, mainModule), join(output, "worker", mainModule), constants.COPYFILE_EXCL);
    copyNew(assets, join(output, "assets"));
    writeFileSync(join(output, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n", { flag: "wx" });
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
    fail(error.path + " appeared in the output during the build; nothing was replaced");
  }
  console.log(JSON.stringify(manifest));
} finally {
  rmSync(built, { recursive: true, force: true });
}

// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { releaseVersion } from "./release-version.mjs";

test("the committed standalone export matches the Cargo release version", () => {
  const server = join(dirname(fileURLToPath(import.meta.url)), "..");
  const cargo = readFileSync(join(server, "../Cargo.toml"), "utf8");
  const version = cargo.split("[workspace.package]")[1].match(/^version = "([^"]+)"/m)[1];
  assert.equal(JSON.parse(readFileSync(join(server, "release.json"), "utf8")).version, version);
});

test("release version exports Cargo's workspace version and refreshes a stale standalone export", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  const server = join(root, "server");
  mkdirSync(server);
  try {
    writeFileSync(join(root, "Cargo.toml"), '[package]\nversion = "9.9.9"\n[workspace.package]\nversion = "0.2.0-rc.3" # release\n[workspace.dependencies]\nother = "1"\n');
    writeFileSync(join(server, "release.json"), '{"version":"0.0.0"}');
    assert.equal(releaseVersion(server), "0.2.0-rc.3");
    assert.deepEqual(JSON.parse(readFileSync(join(server, "release.json"), "utf8")), { version: "0.2.0-rc.3" });
    rmSync(join(root, "Cargo.toml"));
    assert.equal(releaseVersion(server), "0.2.0-rc.3");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a missing or invalid workspace version cannot fall back to a stale export", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  const server = join(root, "server");
  mkdirSync(server);
  try {
    writeFileSync(join(server, "release.json"), '{"version":"0.0.0"}');
    for (const cargo of ['[package]\nversion = "9.9.9"\n', '[workspace.package]\nversion = "oops"\n']) {
      writeFileSync(join(root, "Cargo.toml"), cargo);
      assert.throws(() => releaseVersion(server), /needs a release version/);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// SPDX-License-Identifier: AGPL-3.0-or-later

// The parts of e2e.ts that need no server: matching a folder against a scenario's tree, and sealing the macOS
// device's pairing code.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { type Content, contents, format, keypair, listing, matches, seal, trees, unseal } from "./e2e.ts";

const roots: string[] = [];

function folder(files: Record<string, Content>): string {
  const root = mkdtempSync(join(tmpdir(), "koidrive-e2e-unit-"));
  roots.push(root);
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), contents[content]);
  }
  return root;
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

const conflicted: Record<string, Content> = {
  "docs/from-mac-renamed.txt": "fromMacEdited",
  "notes/random-moved.bin": "randomMac",
  "notes/random-from-linux.bin": "randomLinux",
  "shared.txt": "sharedMac",
  "shared (conflicted copy from linux).txt": "sharedLinux",
};

describe("a folder against a scenario's tree", () => {
  it("matches exactly the tree, finds a conflict copy by its pattern and skips the .koidrive folder", () => {
    const root = folder({ ...conflicted, ".koidrive/store.db": "fromMac" });
    expect(matches(listing(root), trees.conflicted)).toBe(true);
    expect(matches(listing(root), trees.deleted)).toBe(false);
  });

  it("takes either content under either name of a conflict", () => {
    const root = folder({ ...conflicted, "shared.txt": "sharedLinux", "shared (conflicted copy from linux).txt": "sharedMac" });
    expect(matches(listing(root), trees.conflicted)).toBe(true);
  });

  it("refuses a missing file, an extra file, other content and a file at another path", () => {
    const { "shared.txt": _, ...missing } = conflicted;
    expect(matches(listing(folder(missing)), trees.conflicted)).toBe(false);
    expect(matches(listing(folder({ ...conflicted, "notes/extra.txt": "fromMac" })), trees.conflicted)).toBe(false);
    expect(matches(listing(folder({ ...conflicted, "notes/random-moved.bin": "randomLinux" })), trees.conflicted)).toBe(false);
    const { "notes/random-moved.bin": moved, ...rest } = conflicted;
    expect(matches(listing(folder({ ...rest, "random-moved.bin": moved! })), trees.conflicted)).toBe(false);
  });

  it("lists files by path with / separators in byte order, as sha256sum prints them", () => {
    const text = format(listing(folder({ "b.txt": "fromMac", "a/c.txt": "fromLinux" })));
    expect(text.split("\n").map((line) => line.slice(66))).toEqual(["a/c.txt", "b.txt", ""]);
    expect(text).toMatch(/^[0-9a-f]{64} {2}a\/c\.txt\n/);
  });
});

describe("the sealed pairing code", () => {
  it("opens with the private key that belongs to the public key it was sealed to, and with no other", () => {
    const keys = keypair();
    const sealed = seal(keys.publicKey, "K7QM-2XPD@koidrive-test-1-1.example.workers.dev");
    expect(sealed.toString()).not.toContain("K7QM");
    expect(unseal(keys.privateKey, sealed)).toBe("K7QM-2XPD@koidrive-test-1-1.example.workers.dev");
    expect(() => unseal(keypair().privateKey, sealed)).toThrow();
  });
});

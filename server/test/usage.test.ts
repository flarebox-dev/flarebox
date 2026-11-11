// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { exports } from "cloudflare:workers";
import { describe, expect, it, vi } from "vitest";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  SpaceInfoSchema,
  SpaceUsageSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { uuidv7 } from "../src/ids";
import type { SpaceDO } from "../src/space-do";
import { client, content, newSpace, OWNER, op, type TestClient } from "./helpers";

const BASE = "https://koidrive.test";
const DAY = 86_400_000;

async function setup() {
  const space = await newSpace();
  const a = await client(space, "A");
  const root = [...(await a.tree()).values()].find((node) => node.parentId === "")!.nodeId;
  return { ...space, a, root };
}

/** Commits a new file whose chunks are the parts, one chunk each, and returns its node ID. */
async function createFile(a: TestClient, parentId: string, name: string, ...parts: string[]): Promise<string> {
  const nodeId = uuidv7();
  const file = await content(...parts);
  await a.commit([op.create(nodeId, parentId, name, file.content)], { receipts: await a.uploadParts(file) });
  return nodeId;
}

async function usage(space: Awaited<ReturnType<typeof setup>>) {
  const outcome = await space.stub.usage(OWNER);
  if (!outcome.ok) throw new Error(outcome.message);
  return outcome.value;
}

const LIVE = "chunk of a live file";
const SHARED = "chunk of a live file and a trashed file";
const OLD = "chunk only an old version holds";
const TRASH = "chunk only a trashed file holds";
const TRASH_AND_OLD = "chunk of a trashed file and an old version";
const PENDING = "uploaded and never committed";

/**
 * Two live files, one of them edited once, and one trashed file. LIVE appears twice in one file and in two files;
 * SHARED is in a live file and a trashed one; TRASH_AND_OLD is in the trashed file and the old version.
 */
async function populated() {
  const space = await setup();
  const { a, root } = space;
  await createFile(a, root, "kept.txt", LIVE, SHARED, LIVE);
  const edited = await createFile(a, root, "edited.txt", OLD, TRASH_AND_OLD);
  const edit = await content(LIVE);
  await a.commit([op.update(edited, edit.content)], { receipts: await a.uploadParts(edit) });
  const trashed = await createFile(a, root, "trashed.txt", TRASH, SHARED, TRASH_AND_OLD);
  await a.commit([op.delete(trashed)]);
  await a.uploadParts(await content(PENDING));
  return space;
}

describe("space usage", () => {
  it("is zero for a new space", async () => {
    expect(await usage(await setup())).toMatchObject({
      liveFiles: 0n,
      liveBytes: 0n,
      trashedFiles: 0n,
      trashBytes: 0n,
      oldVersions: 0n,
      oldVersionBytes: 0n,
      unreferencedBytes: 0n,
    });
  });

  it("counts each chunk once, under live before trash before old versions", async () => {
    expect(await usage(await populated())).toMatchObject({
      liveFiles: 2n,
      liveBytes: BigInt(LIVE.length + SHARED.length),
      trashedFiles: 1n,
      trashBytes: BigInt(TRASH.length + TRASH_AND_OLD.length),
      oldVersions: 1n,
      oldVersionBytes: BigInt(OLD.length),
      unreferencedBytes: BigInt(PENDING.length),
    });
  });

  it("follows restores, expiry, purges and the chunk grace period", async () => {
    const space = await populated();
    const restoredId = [...(await space.a.call({ case: "listTrash", value: { cursor: "", limit: 10 } }).then((result) =>
      result.case === "trash" ? result.value.nodes : [],
    ))][0]!.nodeId;
    await space.a.commit([op.restore(restoredId)]);
    expect(await usage(space)).toMatchObject({
      liveFiles: 3n,
      liveBytes: BigInt(LIVE.length + SHARED.length + TRASH.length + TRASH_AND_OLD.length),
      trashedFiles: 0n,
      trashBytes: 0n,
      oldVersions: 1n,
      oldVersionBytes: BigInt(OLD.length),
    });

    await space.a.commit([op.delete(restoredId)]);
    const gc = (atMs: number) => runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(atMs));
    const now = Date.now();
    await gc(now + 31 * DAY);
    expect(await usage(space)).toMatchObject({
      liveFiles: 2n,
      liveBytes: BigInt(LIVE.length + SHARED.length),
      trashedFiles: 0n,
      trashBytes: 0n,
      oldVersions: 0n,
      oldVersionBytes: 0n,
      unreferencedBytes: BigInt(OLD.length + TRASH.length + TRASH_AND_OLD.length),
    });

    await gc(now + 62 * DAY);
    await gc(now + 62 * DAY);
    expect(await usage(space)).toMatchObject({ liveBytes: BigInt(LIVE.length + SHARED.length), unreferencedBytes: 0n });
  });

  it("is for the space's owner only", async () => {
    const space = await setup();
    expect(await space.stub.usage(uuidv7())).toMatchObject({ ok: false, code: Error_Code.FORBIDDEN });
  });
});

describe("GET /v1/spaces/{space_id}/usage", () => {
  async function get(path: string, token?: string) {
    const headers: Record<string, string> = token ? { Authorization: "Bearer " + token } : {};
    const response = await exports.default.fetch(BASE + path, { headers });
    return { status: response.status, bytes: new Uint8Array(await response.arrayBuffer()) };
  }

  async function owner(): Promise<DeviceCredentials> {
    const log = vi.spyOn(console, "log");
    await exports.default.fetch(BASE + "/setup");
    const code = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"))!.split(": ")[1]!;
    log.mockRestore();
    const claim = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop", platform: "macos" }));
    const response = await exports.default.fetch(BASE + "/v1/claim", { method: "POST", body: claim });
    return fromBinary(DeviceCredentialsSchema, new Uint8Array(await response.arrayBuffer()));
  }

  it("answers the owner, and refuses a missing token and a space the caller does not own", async () => {
    const creds = await owner();
    const token = creds.access!.token;
    const created = await exports.default.fetch(BASE + "/v1/spaces", {
      method: "POST",
      headers: { Authorization: "Bearer " + token },
      body: toBinary(CreateSpaceRequestSchema, create(CreateSpaceRequestSchema, { name: "Home" })),
    });
    const { spaceId } = fromBinary(SpaceInfoSchema, new Uint8Array(await created.arrayBuffer()));

    const answered = await get(`/v1/spaces/${spaceId}/usage`, token);
    expect(answered.status).toBe(200);
    expect(fromBinary(SpaceUsageSchema, answered.bytes)).toMatchObject({ liveFiles: 0n, liveBytes: 0n, unreferencedBytes: 0n });

    const anonymous = await get(`/v1/spaces/${spaceId}/usage`);
    expect(anonymous.status).toBe(401);
    expect(fromBinary(ErrorSchema, anonymous.bytes).code).toBe(Error_Code.UNAUTHENTICATED);

    const elsewhere = await get(`/v1/spaces/${uuidv7()}/usage`, token);
    expect(elsewhere.status).toBe(403);
    expect(fromBinary(ErrorSchema, elsewhere.bytes).code).toBe(Error_Code.FORBIDDEN);
  });
});

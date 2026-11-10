// SPDX-License-Identifier: AGPL-3.0-or-later

// The first half of the upgrade test (docs/deploy-test.md): on the release deployed before the upgrade, pairs a second
// device, writes a space with files and creates a share link, and records what it wrote for upgrade-verify.ts.

import { afterAll, expect, it } from "vitest";
import { SpaceInfoSchema } from "../../src/gen/flarebox/v1/auth_pb";
import { OpResult_Code } from "../../src/gen/flarebox/v1/commit_pb";
import { ShareLinkSchema } from "../../src/gen/flarebox/v1/share_pb";
import { Device, fileData, op, randomBytes, Server, SpaceClient, stats, uuidv7 } from "./client";
import { addStats, readState, writeState } from "./state.ts";

const state = readState();
if (!state.url || !state.owner) throw new Error("run.ts deploy and run.ts claim must run first: the state file has no server or owner");
const server = new Server(state.url);
const owner = new Device(server, state.owner);

afterAll(() => addStats(stats));

it("writes a paired device, a space with files and a share link on the release before the upgrade", async () => {
  const second = await owner.pair("deploy-test second device");
  await second.token();
  const space = await server.json("POST", "/v1/spaces", SpaceInfoSchema, { body: { name: "Upgrade test" }, token: await owner.token(), status: 201 });
  const client = new SpaceClient(owner, space.spaceId);
  await client.welcome();
  const docs = uuidv7();
  const files = [
    { nodeId: uuidv7(), parentId: space.spaceId, name: "readme.txt", data: await fileData([new TextEncoder().encode("Written before the upgrade\n")]) },
    { nodeId: uuidv7(), parentId: docs, name: "archive.bin", data: await fileData([randomBytes(512 * 1024), randomBytes(256 * 1024)]) },
  ];
  const { receipts } = await client.upload(...files.map((file) => file.data));
  const result = await client.commit([op.mkdir(docs, space.spaceId, "docs"), ...files.map((file) => op.create(file.nodeId, file.parentId, file.name, file.data.content))], receipts);
  expect(result.results.every((r) => r.code === OpResult_Code.APPLIED)).toBe(true);
  const link = await server.json("POST", "/v1/spaces/" + space.spaceId + "/links", ShareLinkSchema, { body: { nodeId: files[1]!.nodeId }, token: await owner.token(), status: 201 });

  const updated = readState();
  updated.written = {
    spaceId: space.spaceId,
    files: files.map((file) => ({ nodeId: file.nodeId, name: file.name, sha256: file.data.sha256, size: file.data.size })),
    linkId: link.linkId,
    headSeq: String(result.seq),
    second: second.credentials,
  };
  writeState(updated);
  console.log("Wrote space " + space.spaceId + " up to seq " + result.seq + ", " + files.length + " files and a share link");
});

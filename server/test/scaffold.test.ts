// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { createExecutionContext, runInDurableObject, waitOnExecutionContext } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { CommitSchema } from "../src/gen/flarebox/v1/commit_pb";
import worker, { AccountDO, SpaceDO } from "../src/worker";

describe("server scaffold", () => {
  it("answers 404 for unknown routes", async () => {
    const ctx = createExecutionContext();
    const response = await worker.fetch(new Request("https://flarebox.test/"), env, ctx);
    await waitOnExecutionContext(ctx);
    expect(response.status).toBe(404);
  });

  it("binds the Space DO with SQLite storage", async () => {
    const stub = env.SPACE.get(env.SPACE.idFromName("test-space"));
    await runInDurableObject(stub, (instance, state) => {
      expect(instance).toBeInstanceOf(SpaceDO);
      expect(state.storage.sql.exec("SELECT 1 AS one").one().one).toBe(1);
    });
  });

  it("binds the Account DO with SQLite storage", async () => {
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
    await runInDurableObject(stub, (instance, state) => {
      expect(instance).toBeInstanceOf(AccountDO);
      expect(state.storage.sql.exec("SELECT 1 AS one").one().one).toBe(1);
    });
  });

  it("round-trips a generated protocol message", () => {
    const commit = create(CommitSchema, {
      commitId: "0190f3a0-0000-7000-8000-000000000001",
      baseSeq: 7n,
      ops: [{ op: { case: "delete", value: { nodeId: "0190f3a0-0000-7000-8000-000000000002" } } }],
    });
    expect(fromBinary(CommitSchema, toBinary(CommitSchema, commit))).toEqual(commit);
  });
});

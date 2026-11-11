// SPDX-License-Identifier: AGPL-3.0-or-later

import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { OpResult_Code } from "../src/gen/koidrive/v1/commit_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { toHex, uuidv7 } from "../src/ids";
import { MAX_HEAD_CHECKS, type SpaceDO } from "../src/space-do";
import { client, content, newSpace, op, type TestClient } from "./helpers";
import { dump, load } from "./pitr";

async function setup() {
  const space = await newSpace();
  const a = await client(space, "Laptop A");
  const b = await client(space, "Desktop B");
  return { ...space, a, b, root: space.spaceId };
}

function code(result: Awaited<ReturnType<TestClient["commitRaw"]>>) {
  return result.case === "error" ? result.value.code : result.case;
}

async function heads(c: TestClient) {
  return runInDurableObject(c.stub, (_instance, state) => ({
    head: Number(state.storage.sql.exec("SELECT value FROM meta WHERE key = 'head_seq'").one().value),
    devices: state.storage.sql.exec("SELECT device_id, device_seq FROM device_commits").toArray(),
  }));
}

describe("RFC 0002 commit checks", () => {
  it("fences a commit built in another history before anything else, a stored commit_id included", async () => {
    const { a, root } = await setup();
    const commitId = uuidv7();
    await a.commit([op.mkdir(uuidv7(), root, "Docs")], { commitId });
    for (const historyId of [uuidv7(), ""]) {
      expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "Other")], { historyId }))).toBe(Error_Code.HISTORY_MISMATCH);
      expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "Docs")], { commitId, historyId }))).toBe(Error_Code.HISTORY_MISMATCH);
    }
    expect((await heads(a)).head).toBe(1);
  });

  it("fences another history before the write checks and the commit_id format, for old clients, viewers and malformed ids alike", async () => {
    const space = await newSpace();
    const root = space.spaceId;
    const old = await client(space, "Old", { maxFormat: 0 });
    const viewer = await client(space, "Viewer", { role: "viewer" });
    const historyId = uuidv7();
    expect(code(await old.commitRaw([op.mkdir(uuidv7(), root, "x")], { historyId }))).toBe(Error_Code.HISTORY_MISMATCH);
    expect(code(await viewer.commitRaw([op.mkdir(uuidv7(), root, "x")], { historyId }))).toBe(Error_Code.HISTORY_MISMATCH);
    const a = await client(space, "Laptop A");
    expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "x")], { historyId, commitId: "not-a-uuid" }))).toBe(Error_Code.HISTORY_MISMATCH);
    expect(code(await old.commitRaw([op.mkdir(uuidv7(), root, "x")]))).toBe(Error_Code.READ_ONLY);
    expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "x")], { commitId: "not-a-uuid" }))).toBe(Error_Code.INVALID_ARGUMENT);
    expect((await heads(a)).head).toBe(0);
  });

  it("returns a stored result before the counter and base checks, also once the log no longer holds the commit", async () => {
    const { a, b, root } = await setup();
    const first = uuidv7();
    const second = uuidv7();
    const oneOps = [op.mkdir(uuidv7(), root, "One")];
    const twoOps = [op.mkdir(uuidv7(), root, "Two")];
    const one = await a.commit(oneOps, { commitId: first });
    const two = await a.commit(twoOps, { commitId: second });
    b.baseSeq = 2n;
    await b.commit([op.mkdir(uuidv7(), root, "Three")]);
    expect(await a.commit(oneOps, { commitId: first })).toEqual(one);
    await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("DELETE FROM commits WHERE commit_id = ?", second));
    expect(await a.commit(twoOps, { commitId: second })).toEqual(two);
    expect((await heads(a)).head).toBe(3);
  });

  it("checks the device counter: 0 and gaps are invalid, a used slot is stale, and only an accepted commit uses a slot", async () => {
    const { a, root } = await setup();
    expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "X")], { deviceSeq: 0n }))).toBe(Error_Code.INVALID_ARGUMENT);
    expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "X")], { deviceSeq: 2n }))).toBe(Error_Code.INVALID_ARGUMENT);
    const parts = await content("not uploaded");
    expect(code(await a.commitRaw([op.create(uuidv7(), root, "f", parts.content)]))).toBe(Error_Code.MISSING_CHUNKS);
    await a.commit([op.mkdir(uuidv7(), root, "Replanned")], { deviceSeq: 1n });
    await a.commit([op.mkdir(uuidv7(), root, "Next")]);
    for (const deviceSeq of [1n, 2n]) {
      expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "From a restored store")], { deviceSeq }))).toBe(Error_Code.STALE_DEVICE_STATE);
    }
    const { devices } = await heads(a);
    expect(devices).toEqual([{ device_id: a.session.deviceId, device_seq: 2 }]);
  });

  it("refuses a base ahead of the head with AHEAD_OF_SERVER and a moved head under require_head with HEAD_MOVED, both before op checks", async () => {
    const { a, b, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "One")]);
    expect(code(await b.commitRaw([], { baseSeq: 2n }))).toBe(Error_Code.AHEAD_OF_SERVER);
    expect(code(await b.commitRaw([], { baseSeq: 0n, requireHead: true }))).toBe(Error_Code.HEAD_MOVED);
    const answer = await b.commit([op.mkdir(uuidv7(), root, "Answer")], { baseSeq: 1n, requireHead: true });
    expect(answer.seq).toBe(2n);
    expect(b.acceptedSeq).toBe(1n);
  });
});

describe("RFC 0013 commit tokens", () => {
  type Token = { commitId: string; committedAtMs: bigint };

  async function tokenAt(c: TestClient, seq: number): Promise<Token> {
    return runInDurableObject(c.stub, (_instance, state) => {
      const row = state.storage.sql.exec("SELECT commit_id, committed_at_ms FROM commits WHERE seq = ?", seq).one();
      return { commitId: String(row.commit_id), committedAtMs: BigInt(Number(row.committed_at_ms)) };
    });
  }

  function base(token: Token) {
    return { baseCommitId: token.commitId, baseCommittedAtMs: token.committedAtMs };
  }

  it("applies a commit whose base token names the commit at base_seq, one with no token, and one based on seq 0", async () => {
    const { a, b, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "One")]);
    await a.commit([op.mkdir(uuidv7(), root, "Two")]);
    expect((await b.commit([op.mkdir(uuidv7(), root, "At the head")], { baseSeq: 2n, ...base(await tokenAt(a, 2)) })).seq).toBe(3n);
    expect((await b.commit([op.mkdir(uuidv7(), root, "Below the head")], { baseSeq: 1n, ...base(await tokenAt(a, 1)) })).seq).toBe(4n);
    expect((await b.commit([op.mkdir(uuidv7(), root, "No token")], { baseSeq: 4n })).seq).toBe(5n);
    expect((await b.commit([op.mkdir(uuidv7(), root, "From seq 0")], { baseSeq: 0n, baseCommitId: uuidv7(), baseCommittedAtMs: 1n })).seq).toBe(6n);
  });

  it("refuses a base token that names another commit than the one at base_seq with AHEAD_OF_SERVER and applies nothing", async () => {
    const { a, b, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "One")]);
    await a.commit([op.mkdir(uuidv7(), root, "Two")]);
    const one = await tokenAt(a, 1);
    const two = await tokenAt(a, 2);
    const forks = [
      { baseSeq: 1n, ...base({ ...one, commitId: uuidv7() }) },
      { baseSeq: 1n, ...base({ ...one, committedAtMs: one.committedAtMs + 1n }) },
      { baseSeq: 2n, ...base({ ...two, commitId: one.commitId }) },
      { baseSeq: 2n, ...base({ ...two, committedAtMs: two.committedAtMs - 1n }) },
    ];
    for (const init of forks) expect(code(await b.commitRaw([op.mkdir(uuidv7(), root, "Forked")], init))).toBe(Error_Code.AHEAD_OF_SERVER);
    expect(await heads(b)).toEqual({ head: 2, devices: [{ device_id: a.session.deviceId, device_seq: 2 }] });
    expect([...(await b.tree()).values()].map((node) => node.name)).not.toContain("Forked");
  });

  it("refuses a base token at a seq whose commit the log does not hold", async () => {
    const { a, b, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "One")]);
    await a.commit([op.mkdir(uuidv7(), root, "Two")]);
    const one = await tokenAt(a, 1);
    await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("DELETE FROM commits WHERE seq = 1"));
    expect(code(await b.commitRaw([op.mkdir(uuidv7(), root, "Forked")], { baseSeq: 1n, ...base(one) }))).toBe(Error_Code.AHEAD_OF_SERVER);
    expect((await heads(b)).head).toBe(2);
  });

  it("returns the stored result to a retried commit_id whose base token no longer matches", async () => {
    const { a, b, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "One")]);
    const ops = [op.mkdir(uuidv7(), root, "Two")];
    const init = { commitId: uuidv7(), baseSeq: 1n, ...base(await tokenAt(a, 1)) };
    const first = await b.commit(ops, init);
    await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("UPDATE commits SET committed_at_ms = committed_at_ms + 1 WHERE seq = 1"));
    expect(await b.commit(ops, init)).toEqual(first);
    expect((await heads(a)).head).toBe(2);
  });

  it("gives a CommitResult its commit's committed_at_ms, also when stored, and a TreePage the token at its head", async () => {
    const { a, root } = await setup();
    const listed = async () => {
      const page = await a.call({ case: "listTree", value: { cursor: "", limit: 10 } });
      if (page.case !== "tree") throw new Error("listTree failed");
      return { headSeq: page.value.headSeq, commitId: page.value.headCommitId, committedAtMs: page.value.headCommittedAtMs };
    };
    expect(await listed()).toEqual({ headSeq: 0n, commitId: "", committedAtMs: 0n });
    const commitId = uuidv7();
    const ops = [op.mkdir(uuidv7(), root, "One")];
    const result = await a.commit(ops, { commitId });
    const one = await tokenAt(a, 1);
    expect(one.commitId).toBe(commitId);
    expect(result.committedAtMs).toBe(one.committedAtMs);
    expect((await a.commit(ops, { commitId })).committedAtMs).toBe(one.committedAtMs);
    expect(await listed()).toEqual({ headSeq: 1n, ...one });
  });

  it("refuses a resent Delete whose base token names a commit a restore lost, so it cannot trash a folder moved in at the reused seq (GUN-454)", async () => {
    const space = await setup();
    const { a, b, root } = space;
    const c = await client(space, "Tablet C");
    const x = uuidv7();
    const z = uuidv7();
    await a.commit([op.mkdir(x, root, "X")]);
    c.baseSeq = 1n;
    await c.commit([op.mkdir(z, root, "Z")]);
    const atTwo = await runInDurableObject(a.stub, (_instance, state) => dump(state.storage.sql));
    b.baseSeq = 2n;
    await b.commit([op.mkdir(uuidv7(), root, "Notes")]);
    const deleteX = { commitId: uuidv7(), baseSeq: 3n, ...base(await tokenAt(a, 3)) };
    expect((await a.commit([op.delete(x)], deleteX)).seq).toBe(4n);

    await runInDurableObject(a.stub, (_instance, state) => state.storage.transactionSync(() => load(state.storage.sql, atTwo)));
    expect((await c.commit([op.move(z, x, "Z")])).seq).toBe(3n);
    expect(code(await a.commitRaw([op.delete(x)], deleteX))).toBe(Error_Code.AHEAD_OF_SERVER);
    expect((await heads(a)).head).toBe(3);
    const tree = await a.tree();
    expect(tree.has(x)).toBe(true);
    expect(tree.get(z)?.parentId).toBe(x);
  });
});

describe("RFC 0002 section 10", () => {
  async function trashedAndTaken(a: TestClient, root: string, edit: boolean) {
    const file = uuidv7();
    const v1 = await content("v1");
    await a.commit([op.create(file, root, "a.txt", v1.content)], { receipts: await a.uploadParts(v1) });
    if (edit) {
      const v2 = await content("v2 from A");
      await a.commit([op.update(file, v2.content)], { receipts: await a.uploadParts(v2) });
    }
    await a.commit([op.delete(file)]);
    const other = await content("another a.txt");
    await a.commit([op.create(uuidv7(), root, "A.TXT", other.content)], { receipts: await a.uploadParts(other) });
    return file;
  }

  it("answers APPLIED_RENAMED when an update revives its target under a conflict name", async () => {
    const { a, b, root } = await setup();
    const file = await trashedAndTaken(a, root, false);
    b.baseSeq = 1n;
    const edit = await content("B's edit");
    const result = await b.commit([op.update(file, edit.content)], { receipts: await b.uploadParts(edit) });
    expect(result.results[0]).toMatchObject({ code: OpResult_Code.APPLIED_RENAMED });
    expect(result.results[0]!.name).toMatch(/^a \(conflict from Desktop B \d{4}-\d{2}-\d{2}\)\.txt$/);
    const node = (await b.tree()).get(file)!;
    expect(node.name).toBe(result.results[0]!.name);
    expect(node.contentRev).toBe(result.seq);
  });

  it("keeps CONFLICT_COPY when the revived target also has an unseen content change", async () => {
    const { a, b, root } = await setup();
    const file = await trashedAndTaken(a, root, true);
    b.baseSeq = 1n;
    const edit = await content("B's edit");
    const result = await b.commit([op.update(file, edit.content)], { receipts: await b.uploadParts(edit) });
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
  });
});

describe("RFC 0002 section 11", () => {
  it("checks at most MAX_HEAD_CHECKS pending chunks without a receipt per call, records them and extends their holds", async () => {
    const { a, root } = await setup();
    const parts = await content(...Array.from({ length: MAX_HEAD_CHECKS + 10 }, (_, i) => "chunk " + i));
    await a.uploadParts(parts);
    const holds = () =>
      runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("SELECT hold_until_ms, head_checked FROM chunks").toArray());
    const before = Math.max(...(await holds()).map((row) => Number(row.hold_until_ms)));
    const file = [op.create(uuidv7(), root, "many", parts.content)];
    const commitId = uuidv7();
    const first = await a.commitRaw(file, { commitId });
    expect(first.case === "error" && first.value.code).toBe(Error_Code.MISSING_CHUNKS);
    expect(first.case === "error" && first.value.missingChunks.map(toHex)).toEqual(
      parts.content.chunks.slice(MAX_HEAD_CHECKS).map((c) => toHex(c.hash)),
    );
    const checked = (await holds()).filter((row) => row.head_checked === 1);
    expect(checked).toHaveLength(MAX_HEAD_CHECKS);
    expect(checked.every((row) => Number(row.hold_until_ms) > before)).toBe(true);
    expect((await a.commit(file, { commitId })).seq).toBe(1n);
  });

  it("lists a head-checked chunk as missing when its row changed before the commit transaction", async () => {
    const { a, root } = await setup();
    const parts = await content("checked, then retired");
    await a.uploadParts(parts);
    const swap = (patch: boolean) =>
      runInDurableObject(a.stub, (instance: SpaceDO, state) => {
        const holder = instance as unknown as { env: Env; original?: Env };
        if (!patch) {
          holder.env = holder.original!;
          return;
        }
        holder.original = holder.env;
        const head = async (key: string) => {
          const found = await env.BUCKET.head(key);
          state.storage.sql.exec("UPDATE chunks SET state = 3");
          return found;
        };
        holder.env = { ...holder.original, BUCKET: { head } as unknown as R2Bucket };
      });
    await swap(true);
    try {
      const result = await a.commitRaw([op.create(uuidv7(), root, "f", parts.content)]);
      expect(result.case === "error" && result.value.code).toBe(Error_Code.MISSING_CHUNKS);
      expect(result.case === "error" && result.value.missingChunks.map(toHex)).toEqual([toHex(parts.content.chunks[0]!.hash)]);
    } finally {
      await swap(false);
    }
  });
});

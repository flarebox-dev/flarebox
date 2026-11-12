// SPDX-License-Identifier: AGPL-3.0-or-later

// The R2 metadata export of spec v0 section 11, written per history (RFC 0003 section 7) in the format of RFC 0004.
// Each step records what it is about to write before it writes, so a crash or an unknown outcome is safe to repeat.

import { create, toBinary } from "@bufbuild/protobuf";
import { BinaryWriter } from "@bufbuild/protobuf/wire";
import { ExportHeaderSchema, NodeSnapshotSchema, RetainedVersionSchema } from "../gen/koidrive/v1/export_pb";
import type { CommitRecord } from "../gen/koidrive/v1/model_pb";
import { sha256 } from "../crypto";
import type { Database, ObjectStore, Sql } from "../host";
import { toHex, uuidv7Ms } from "../ids";
import { getMeta, INLINE_CHUNKS, type NodeRow, nodeState, rows, setMeta, type VersionRow, versionInfo } from "./store";

const DAY_MS = 24 * 3_600_000;
/** Spec v0 section 11: a log segment is written once this many commits wait, or this long after the first of them. */
const SEGMENT_COMMITS = 1000;
const SEGMENT_DELAY_MS = 5 * 60_000;
/** A segment also ends once its records reach this size, so that one segment fits in memory. */
const SEGMENT_BYTES = 16 * 1024 * 1024;
/** Spec v0 section 11: a snapshot is due a day after the last one if the space changed, or after 50,000 commits. */
const SNAPSHOT_COMMITS = 50_000;
/** A snapshot page ends once its records reach this size. */
const PAGE_BYTES = 8 * 1024 * 1024;
/** RFC 0004 section 7: the two newest completed snapshots of the current history are kept. */
const KEPT_SNAPSHOTS = 2;
/** Steps one run takes before it leaves the rest to the next alarm. */
const STEPS_PER_RUN = 25;
const LIST_LIMIT = 1000;

interface PageEntry {
  name: string;
  records: number;
  bytes: number;
  sha256: string;
}

interface Snapshot {
  startSeq: number;
  endSeq: number;
  createdAtMs: number;
}

interface RunningSnapshot {
  startSeq: number;
  createdAtMs: number;
  /** The last node_id written. */
  cursor: string;
  /** The next page number; a number is taken before its page's write is issued. */
  nextPage: number;
  pages: PageEntry[];
  /** head_seq when the last page was read. */
  endSeq?: number;
  /** The manifest's bytes, recorded before its first write so every retry writes the same. */
  manifest?: string;
}

interface ExportState {
  /** The descriptor waits for the first snapshot: the history was started by a rewind. */
  baseline: boolean;
  /** The current history's descriptor is durable. */
  descriptor: boolean;
  /** A snapshot that starts at or after this seq is due at once. */
  dueSeq: number | null;
  /** The snapshot cadence counts from here: the last completed snapshot, or the history's start. */
  since: { seq: number; atMs: number };
  /** A log segment whose range was recorded before its write. */
  segment?: { first: number; last: number };
  running?: RunningSnapshot;
  /** Completed snapshots of this history, oldest first. */
  done: Snapshot[];
  /** start_seq of an older snapshot being deleted. */
  pruning?: number;
}

type Step = "segment" | "prune" | "cut" | "page" | "manifest" | "descriptor" | "start";

function seqName(seq: number): string {
  return String(seq).padStart(20, "0");
}

function readState(sql: Sql): ExportState {
  const stored = getMeta(sql, "export_state");
  if (stored === undefined) return { baseline: false, descriptor: false, dueSeq: null, since: { seq: 0, atMs: 0 }, done: [] };
  return JSON.parse(String(stored)) as ExportState;
}

function exportedSeq(sql: Sql): number {
  return Number(getMeta(sql, "exported_seq") ?? 0);
}

function scheduleExport(sql: Sql, dueMs: number, lowerOnly: boolean): void {
  sql.exec(
    `INSERT INTO schedule (task, due_ms) VALUES ('export', ?)
     ON CONFLICT (task) DO UPDATE SET due_ms = ${lowerOnly ? "MIN(due_ms, excluded.due_ms)" : "excluded.due_ms"}`,
    dueMs,
  );
}

/**
 * Starts the export of the current history, inside the transaction that creates the space or completes a rewind. A
 * space's first history exports from seq 0 and writes its descriptor at once. A history that a rewind started exports
 * from restore_seq: a full snapshot is due at once, and the descriptor waits for it (RFC 0003 section 7).
 */
export function startExport(sql: Sql, fromSeq: number, atMs: number, baseline: boolean): void {
  setMeta(sql, "exported_seq", fromSeq);
  const state: ExportState = { baseline, descriptor: false, dueSeq: baseline ? fromSeq : null, since: { seq: fromSeq, atMs }, done: [] };
  setMeta(sql, "export_state", JSON.stringify(state));
  scheduleExport(sql, atMs, false);
}

/**
 * Starts the export of a space that a server without the export created, when its object opens. Its current history
 * exports from restore_seq if a rewind started it, else from seq 0, and a full snapshot is due at once, because the log
 * of earlier commits omits the chunk lists of large versions.
 */
export function bootstrapExport(sql: Sql, atMs: number): void {
  if (getMeta(sql, "space_id") === undefined || getMeta(sql, "export_state") !== undefined) return;
  const restoreSeq = getMeta(sql, "restore_seq");
  const fromSeq = Number(restoreSeq ?? 0);
  startExport(sql, fromSeq, atMs, restoreSeq !== undefined);
  const state = readState(sql);
  state.dueSeq ??= Number(getMeta(sql, "head_seq"));
  setMeta(sql, "export_state", JSON.stringify(state));
}

/** Whether the current history's descriptor is durable in R2, which rewind admission requires (RFC 0003 section 1). */
export function descriptorDurable(sql: Sql): boolean {
  return readState(sql).descriptor;
}

/**
 * Schedules the export a new commit makes due, inside the commit's transaction: a segment five minutes after the first
 * waiting commit or at once after 1,000, and a snapshot at once after a version whose chunk list the log omits
 * (RFC 0004 section 3). Returns whether the schedule changed.
 */
export function noteCommitted(sql: Sql, record: CommitRecord): boolean {
  const seq = Number(record.seq);
  const atMs = Number(record.committedAtMs);
  const waiting = seq - exportedSeq(sql);
  let dueMs = waiting >= SEGMENT_COMMITS ? atMs : waiting === 1 ? atMs + SEGMENT_DELAY_MS : undefined;
  const large = record.changes.some(
    ({ change }) => change.case === "node" && change.value.current?.seq === record.seq && (change.value.current.content?.chunkCount ?? 0) > INLINE_CHUNKS,
  );
  if (large) {
    const state = readState(sql);
    state.dueSeq = Math.max(state.dueSeq ?? seq, seq);
    setMeta(sql, "export_state", JSON.stringify(state));
    dueMs = atMs;
  }
  if (dueMs === undefined) return false;
  scheduleExport(sql, dueMs, true);
  return true;
}

async function gzip(data: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([data]).stream().pipeThrough(new CompressionStream("gzip"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

/** Writes the export of one Space DO to R2, one recorded step at a time. */
export class Exporter {
  constructor(
    private readonly storage: Database,
    private readonly bucket: ObjectStore,
    private readonly pageNodes: number,
  ) {}

  private get sql(): Sql {
    return this.storage.sql;
  }

  /** Takes steps until nothing is due or the run's budget is spent, then schedules the next run. */
  async run(nowMs: number): Promise<void> {
    for (let i = 0; i < STEPS_PER_RUN; i++) {
      const step = this.next(nowMs);
      if (!step) break;
      await this.take(step, nowMs);
    }
    const dueMs = this.next(nowMs) ? nowMs : this.nextDue();
    if (dueMs === undefined) this.sql.exec("DELETE FROM schedule WHERE task = 'export'");
    else scheduleExport(this.sql, dueMs, false);
  }

  private get headSeq(): number {
    return Number(getMeta(this.sql, "head_seq"));
  }

  private get spaceId(): string {
    return String(getMeta(this.sql, "space_id"));
  }

  private get historyId(): string {
    return String(getMeta(this.sql, "history_id"));
  }

  private key(path: string): string {
    return `spaces/${this.spaceId}/meta/${this.historyId}/${path}`;
  }

  private update(change: (state: ExportState) => void): void {
    this.storage.transactionSync(() => {
      const state = readState(this.sql);
      change(state);
      setMeta(this.sql, "export_state", JSON.stringify(state));
    });
  }

  private firstWaitingMs(): number {
    return Number(this.sql.exec("SELECT committed_at_ms FROM commits WHERE seq = ?", exportedSeq(this.sql) + 1).one().committed_at_ms);
  }

  /** The step that is due now, if any. */
  private next(nowMs: number): Step | undefined {
    const state = readState(this.sql);
    const head = this.headSeq;
    const exported = exportedSeq(this.sql);
    const running = state.running;
    if (state.segment) return "segment";
    if (state.pruning !== undefined) return "prune";
    if (
      head > exported &&
      (head - exported >= SEGMENT_COMMITS ||
        this.firstWaitingMs() + SEGMENT_DELAY_MS <= nowMs ||
        (running?.endSeq !== undefined && exported < running.endSeq))
    ) {
      return "cut";
    }
    if (running) return running.endSeq === undefined ? "page" : "manifest";
    if (!state.descriptor && (!state.baseline || state.done.length > 0)) return "descriptor";
    const { since } = state;
    if (state.dueSeq !== null || (head > since.seq && (nowMs - since.atMs >= DAY_MS || head - since.seq >= SNAPSHOT_COMMITS))) return "start";
    return undefined;
  }

  /** When the next step falls due with no further commit: a segment's delay or a snapshot's day. */
  private nextDue(): number | undefined {
    const state = readState(this.sql);
    const head = this.headSeq;
    const due: number[] = [];
    if (head > exportedSeq(this.sql)) due.push(this.firstWaitingMs() + SEGMENT_DELAY_MS);
    if (head > state.since.seq) due.push(state.since.atMs + DAY_MS);
    return due.length > 0 ? Math.min(...due) : undefined;
  }

  private async take(step: Step, nowMs: number): Promise<void> {
    switch (step) {
      case "segment":
        return this.writeSegment();
      case "prune":
        return this.prune();
      case "cut":
        return this.cutSegment();
      case "page":
        return this.writePage();
      case "manifest":
        return this.writeManifest();
      case "descriptor":
        return this.writeDescriptor();
      case "start": {
        const head = this.headSeq;
        return this.update((state) => {
          state.running = { startSeq: head, createdAtMs: nowMs, cursor: "", nextPage: 0, pages: [] };
        });
      }
    }
  }

  private header(format: string): Uint8Array {
    return toBinary(ExportHeaderSchema, create(ExportHeaderSchema, { format, formatVersion: 1, spaceId: this.spaceId, historyId: this.historyId }));
  }

  /** Records the range of the next segment: the waiting commits, at most 1,000 of them and SEGMENT_BYTES of records. */
  private cutSegment(): void {
    const first = exportedSeq(this.sql) + 1;
    const found = this.sql.exec("SELECT seq, length(record) AS bytes FROM commits WHERE seq >= ? ORDER BY seq LIMIT ?", first, SEGMENT_COMMITS).toArray();
    if (Number(found[0]?.seq) !== first) throw new Error(`commit ${first} is missing from the log`);
    let last = first;
    let bytes = 0;
    for (const row of found) {
      bytes += Number(row.bytes);
      if (Number(row.seq) > first && bytes > SEGMENT_BYTES) break;
      last = Number(row.seq);
    }
    this.update((state) => {
      state.segment = { first, last };
    });
  }

  /** Writes the recorded segment. A retry writes the same range with the same records. */
  private async writeSegment(): Promise<void> {
    const { first, last } = readState(this.sql).segment!;
    const writer = new BinaryWriter();
    writer.bytes(this.header("koidrive.log"));
    let expected = first;
    for (const row of this.sql.exec("SELECT seq, record FROM commits WHERE seq BETWEEN ? AND ? ORDER BY seq", first, last)) {
      if (Number(row.seq) !== expected++) throw new Error(`commit ${expected - 1} is missing from the log`);
      writer.bytes(new Uint8Array(row.record as ArrayBuffer));
    }
    if (expected !== last + 1) throw new Error(`commit ${expected} is missing from the log`);
    await this.bucket.put(this.key(`log/${seqName(first)}-${seqName(last)}.fblog`), await gzip(writer.finish()));
    this.update((state) => {
      state.segment = undefined;
      setMeta(this.sql, "exported_seq", last);
    });
  }

  /** One node of a snapshot page, with every chunk list complete (RFC 0004 section 3). */
  private nodeSnapshot(node: NodeRow): Uint8Array {
    const previous = rows<VersionRow>(
      this.sql.exec("SELECT * FROM versions WHERE node_id = ? AND version_id IS NOT ? ORDER BY seq, version_id", node.node_id, node.cur_version_id),
    ).map((version) => create(RetainedVersionSchema, { version: versionInfo(this.sql, version, true), supersededAtMs: BigInt(version.superseded_at_ms ?? 0) }));
    return toBinary(
      NodeSnapshotSchema,
      create(NodeSnapshotSchema, { node: nodeState(this.sql, node, true), previous, trashedAtMs: BigInt(node.trashed_at_ms ?? 0) }),
    );
  }

  /**
   * Reads the next node range and writes it as a page. The page number is taken before the write is issued and never
   * written again, so a write whose outcome is unknown cannot replace a later page's bytes (RFC 0004 section 4).
   */
  private async writePage(): Promise<void> {
    const running = readState(this.sql).running!;
    const head = this.headSeq;
    const found = rows<NodeRow>(this.sql.exec("SELECT * FROM nodes WHERE node_id > ? ORDER BY node_id LIMIT ?", running.cursor, this.pageNodes));
    const writer = new BinaryWriter();
    writer.bytes(this.header("koidrive.snapshot"));
    let records = 0;
    let bytes = 0;
    for (const node of found) {
      const record = this.nodeSnapshot(node);
      if (records > 0 && bytes + record.length > PAGE_BYTES) break;
      writer.bytes(record);
      records++;
      bytes += record.length;
    }
    const last = records === found.length && found.length < this.pageNodes;
    if (records === 0) {
      return this.update((state) => {
        state.running!.endSeq = head;
      });
    }
    const page = running.nextPage;
    this.update((state) => {
      state.running!.nextPage = page + 1;
    });
    const file = await gzip(writer.finish());
    await this.bucket.put(this.key(`snap/${seqName(running.startSeq)}/pages/${page}.fbsnap`), file);
    const entry: PageEntry = { name: `${page}.fbsnap`, records, bytes: file.length, sha256: toHex(await sha256(file)) };
    this.update((state) => {
      const current = state.running!;
      current.pages.push(entry);
      current.cursor = found[records - 1]!.node_id;
      if (last) current.endSeq = head;
    });
  }

  /**
   * Writes the manifest once its pages and the log through end_seq are durable. Its bytes are recorded before the first
   * write, so a late write can only store the same manifest (RFC 0004 section 4).
   */
  private async writeManifest(): Promise<void> {
    let running = readState(this.sql).running!;
    if (running.manifest === undefined) {
      const manifest = JSON.stringify({
        format: "koidrive.snapshot",
        format_version: 1,
        space_id: this.spaceId,
        history_id: this.historyId,
        start_seq: running.startSeq,
        end_seq: running.endSeq,
        created_at: new Date(running.createdAtMs).toISOString(),
        pages: running.pages,
      });
      this.update((state) => {
        state.running!.manifest = manifest;
      });
      running = readState(this.sql).running!;
    }
    await this.bucket.put(this.key(`snap/${seqName(running.startSeq)}/manifest.json`), running.manifest!);
    this.update((state) => {
      const { startSeq, endSeq, createdAtMs } = state.running!;
      state.running = undefined;
      state.done.push({ startSeq, endSeq: endSeq!, createdAtMs });
      state.since = { seq: endSeq!, atMs: createdAtMs };
      if (state.dueSeq !== null && startSeq >= state.dueSeq) state.dueSeq = null;
      if (state.done.length > KEPT_SNAPSHOTS) state.pruning = state.done.shift()!.startSeq;
    });
  }

  /** Deletes an older snapshot: its manifest first, then everything under its pages/ prefix, orphan pages included. */
  private async prune(): Promise<void> {
    const dir = this.key(`snap/${seqName(readState(this.sql).pruning!)}/`);
    await this.bucket.delete(dir + "manifest.json");
    const listed = await this.bucket.list({ prefix: dir + "pages/", limit: LIST_LIMIT });
    if (listed.objects.length > 0) await this.bucket.delete(listed.objects.map((object) => object.key));
    if (listed.truncated) return;
    this.update((state) => {
      state.pruning = undefined;
    });
  }

  /** Writes the current history's descriptor (RFC 0004 section 2). Its content is fixed for the history. */
  private async writeDescriptor(): Promise<void> {
    const spaceId = this.spaceId;
    const descriptor = {
      format: "koidrive.space",
      format_version: 1,
      space_id: spaceId,
      name: String(getMeta(this.sql, "name")),
      created_at: new Date(uuidv7Ms(spaceId)).toISOString(),
      hash_alg: "sha256",
      chunk_key: "spaces/{space_id}/chunks/{hex2}/{hex}/{incarnation}",
      chunk_encoding: "raw",
      history_id: this.historyId,
      rewind_number: Number(getMeta(this.sql, "rewind_number") ?? 0),
    };
    await this.bucket.put(this.key("space.json"), JSON.stringify(descriptor));
    this.update((state) => {
      state.descriptor = true;
    });
  }
}

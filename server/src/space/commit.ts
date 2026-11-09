// SPDX-License-Identifier: AGPL-3.0-or-later

// Applies the ops of one commit inside the Space DO transaction, following the rules in spec v0 section 6.

import { create } from "@bufbuild/protobuf";
import {
  type CreateDir,
  type CreateFile,
  type Delete,
  type Move,
  type Op,
  type OpResult,
  OpResult_Code,
  OpResultSchema,
  type Restore,
  type UpdateContent,
} from "../gen/flarebox/v1/commit_pb";
import { type ChangeRecord, ChangeRecordSchema, type ChunkRef, type Content } from "../gen/flarebox/v1/model_pb";
import { Error_Code } from "../gen/flarebox/v1/rpc_pb";
import { uuidv7 } from "../ids";
import { conflictName, isReservedAtRoot, nameKey, normalizeName } from "./names";
import { NodeKindValue, NodeStatusValue } from "./schema";
import {
  encodeChunkList,
  getNode,
  getVersion,
  type NodeRow,
  nodeState,
  PAGE_CHUNKS,
  rows,
  versionChunks,
  versionContent,
} from "./store";

export const MAX_DEPTH = 256;

/** Rejects a whole call; the transaction rolls back. */
export class CallFailure extends Error {
  constructor(
    readonly code: Error_Code,
    message: string,
    readonly missingChunks: Uint8Array[] = [],
    readonly retryAfterMs = 0,
  ) {
    super(message);
  }
}

export interface Committer {
  deviceId: string;
  userId: string;
  deviceName: string;
}

export interface ApplyContext {
  seq: number;
  baseSeq: number;
  nowMs: number;
  committer: Committer;
}

type Reject = "CYCLE" | "NOT_FOUND" | "INVALID_NAME" | "PARENT_NOT_DIR" | "ID_EXISTS" | "ROOT" | "NOT_FILE";

function result(code: OpResult_Code, nodeId = "", name = ""): OpResult {
  return create(OpResultSchema, { code, nodeId, name });
}

function rejected(reason: Reject): OpResult {
  return create(OpResultSchema, { code: OpResult_Code.REJECTED, rejectReason: reason });
}

export class CommitApplier {
  private readonly remap = new Map<string, string>();
  private readonly touched = new Set<string>();

  constructor(
    private readonly sql: SqlStorage,
    private readonly ctx: ApplyContext,
  ) {}

  apply(ops: readonly Op[]): { results: OpResult[]; changes: ChangeRecord[] } {
    const results = ops.map((op, index) => this.applyOp(op, index));
    const changes = [...this.touched].map((nodeId) =>
      create(ChangeRecordSchema, { change: { case: "node", value: nodeState(this.sql, getNode(this.sql, nodeId)!) } }),
    );
    return { results, changes };
  }

  private applyOp(op: Op, index: number): OpResult {
    switch (op.op.case) {
      case "createDir":
        return this.createDir(op.op.value);
      case "createFile":
        return this.createFile(op.op.value);
      case "updateContent":
        return this.updateContent(op.op.value.nodeId, op.op.value.content!, op.op.value.content!.chunks);
      case "move":
        return this.move(op.op.value);
      case "delete":
        return this.delete(op.op.value, index);
      case "restore":
        return this.restore(op.op.value);
      default:
        throw new CallFailure(Error_Code.INVALID_ARGUMENT, `op ${index} is empty`);
    }
  }

  private id(nodeId: string): string {
    return this.remap.get(nodeId) ?? nodeId;
  }

  private node(nodeId: string): NodeRow | undefined {
    return getNode(this.sql, this.id(nodeId));
  }

  private get dev(): string {
    return this.ctx.committer.deviceId;
  }

  private unseenContent(node: NodeRow): boolean {
    return node.content_rev > this.ctx.baseSeq && node.content_dev !== this.dev;
  }

  private unseenMeta(node: NodeRow): boolean {
    return node.meta_rev > this.ctx.baseSeq && node.meta_dev !== this.dev;
  }

  private depth(node: NodeRow): number {
    let depth = 0;
    for (let cur: NodeRow | undefined = node; cur?.parent_id; cur = getNode(this.sql, cur.parent_id)) {
      if (++depth > MAX_DEPTH) break;
    }
    return depth;
  }

  private checkDepth(parent: NodeRow): void {
    if (this.depth(parent) + 1 > MAX_DEPTH) {
      throw new CallFailure(Error_Code.LIMIT_EXCEEDED, `path depth would exceed ${MAX_DEPTH}`);
    }
  }

  /**
   * Rejects the commit when placing node under parent would put it or a descendant, trashed ones included, deeper
   * than MAX_DEPTH. A destination no deeper than the current position cannot, because the tree already satisfies
   * the limit.
   */
  private checkSubtreeDepth(node: NodeRow, parent: NodeRow): void {
    const newDepth = this.depth(parent) + 1;
    if (newDepth <= this.depth(node)) return;
    const height = this.sql
      .exec(
        `WITH RECURSIVE sub(id, d) AS (
           SELECT ?, 0 UNION ALL SELECT n.node_id, sub.d + 1 FROM nodes n JOIN sub ON n.parent_id = sub.id WHERE sub.d <= ?)
         SELECT MAX(d) AS height FROM sub`,
        node.node_id,
        MAX_DEPTH - newDepth,
      )
      .one().height;
    if (newDepth + Number(height) > MAX_DEPTH) {
      throw new CallFailure(Error_Code.LIMIT_EXCEEDED, `path depth would exceed ${MAX_DEPTH}`);
    }
  }

  private liveWithName(parentId: string, key: string, excludeId: string): NodeRow | undefined {
    return rows<NodeRow>(
      this.sql.exec(
        "SELECT * FROM nodes WHERE parent_id = ? AND name_key = ? AND status = 1 AND node_id != ? LIMIT 1",
        parentId,
        key,
        excludeId,
      ),
    )[0];
  }

  /** R1: the desired name, or the first free conflict name. A name reserved at the root counts as taken there. */
  private pickName(parentId: string, desired: string, excludeId: string): { name: string; renamed: boolean } {
    const atRoot = !getNode(this.sql, parentId)?.parent_id;
    let name = desired;
    for (let attempt = 1; this.liveWithName(parentId, nameKey(name), excludeId) || (atRoot && isReservedAtRoot(name)); attempt++) {
      name = conflictName(desired, this.ctx.committer.deviceName, this.ctx.nowMs, attempt);
    }
    return { name, renamed: name !== desired };
  }

  private pickConflictCopyName(parentId: string, original: string): string {
    for (let attempt = 1; ; attempt++) {
      const name = conflictName(original, this.ctx.committer.deviceName, this.ctx.nowMs, attempt);
      if (!this.liveWithName(parentId, nameKey(name), "")) return name;
    }
  }

  /** Brings one trashed node back under parentId, renaming it on a name clash. */
  private reviveOne(node: NodeRow, parentId: string, desiredName: string): { name: string; renamed: boolean } {
    const picked = this.pickName(parentId, desiredName, node.node_id);
    this.sql.exec(
      `UPDATE nodes SET status = 1, parent_id = ?, name = ?, name_key = ?, trash_group = NULL, trashed_at_ms = NULL,
         meta_rev = ?, meta_dev = ? WHERE node_id = ?`,
      parentId,
      picked.name,
      nameKey(picked.name),
      this.ctx.seq,
      this.dev,
      node.node_id,
    );
    this.touched.add(node.node_id);
    return picked;
  }

  /** R8: revives the trashed ancestor chain ending at nodeId, top-down. */
  private reviveChain(nodeId: string): void {
    const chain: NodeRow[] = [];
    for (let cur = getNode(this.sql, nodeId); cur && cur.status === NodeStatusValue.TRASHED; cur = getNode(this.sql, cur.parent_id ?? "")) {
      chain.push(cur);
    }
    for (const node of chain.reverse()) this.reviveOne(node, node.parent_id!, node.name);
  }

  /** Revives the trashed descendants of parentId that went to the trash with it. */
  private reviveGroup(parentId: string, group: string): void {
    const children = rows<NodeRow>(
      this.sql.exec("SELECT * FROM nodes WHERE parent_id = ? AND status = 2 AND trash_group = ?", parentId, group),
    );
    for (const child of children) {
      this.reviveOne(child, parentId, child.name);
      this.reviveGroup(child.node_id, group);
    }
  }

  /** R8 for the target of an edit or move: its ancestors, itself and its trash group come back. renamed tells whether the target took a conflict name. */
  private reviveTarget(node: NodeRow): { node: NodeRow; renamed: boolean } {
    if (node.status !== NodeStatusValue.TRASHED) return { node, renamed: false };
    const group = node.trash_group!;
    this.reviveChain(node.parent_id!);
    const { renamed } = this.reviveOne(node, node.parent_id!, node.name);
    this.reviveGroup(node.node_id, group);
    return { node: getNode(this.sql, node.node_id)!, renamed };
  }

  /** Resolves a parent for a create or move, reviving it when trashed. */
  private parentFor(parentId: string): NodeRow | Reject {
    const parent = this.node(parentId);
    if (!parent) return "NOT_FOUND";
    if (parent.kind !== NodeKindValue.DIR) return "PARENT_NOT_DIR";
    this.checkDepth(parent);
    if (parent.status === NodeStatusValue.TRASHED) this.reviveChain(parent.node_id);
    return parent;
  }

  private insertNode(nodeId: string, parentId: string, name: string, kind: number, versionId: string | null): void {
    this.sql.exec(
      `INSERT INTO nodes (node_id, parent_id, name, name_key, kind, status, content_rev, content_dev, meta_rev, meta_dev, cur_version_id)
       VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)`,
      nodeId,
      parentId,
      name,
      nameKey(name),
      kind,
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      versionId,
    );
    this.touched.add(nodeId);
  }

  private insertVersion(nodeId: string, content: Content, chunks: readonly ChunkRef[]): string {
    const versionId = uuidv7(this.ctx.nowMs);
    const paged = chunks.length > PAGE_CHUNKS;
    this.sql.exec(
      `INSERT INTO versions (version_id, node_id, seq, size, mtime_ns, executable, chunk_count, manifest_digest, chunker,
         chunks, device_id, user_id, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      versionId,
      nodeId,
      this.ctx.seq,
      Number(content.size),
      String(content.mtimeNs),
      content.executable ? 1 : 0,
      chunks.length,
      content.manifestDigest,
      content.chunker,
      paged ? null : encodeChunkList(chunks),
      this.dev,
      this.ctx.committer.userId,
      this.ctx.nowMs,
    );
    for (let page = 0; paged && page * PAGE_CHUNKS < chunks.length; page++) {
      this.sql.exec(
        "INSERT INTO version_chunk_pages (version_id, page, chunks) VALUES (?, ?, ?)",
        versionId,
        page,
        encodeChunkList(chunks.slice(page * PAGE_CHUNKS, (page + 1) * PAGE_CHUNKS)),
      );
    }
    const distinct = new Map(chunks.map((chunk) => [chunk.hash.join(","), chunk.hash]));
    for (const hash of distinct.values()) {
      this.sql.exec("UPDATE chunks SET refcount = refcount + 1, state = 2, zero_since_ms = NULL WHERE hash = ?", hash);
    }
    return versionId;
  }

  private createDir(op: CreateDir): OpResult {
    const name = normalizeName(op.name);
    if (!name) return rejected("INVALID_NAME");
    if (getNode(this.sql, op.nodeId)) return rejected("ID_EXISTS");
    const parent = this.parentFor(op.parentId);
    if (typeof parent === "string") return rejected(parent);
    if (!parent.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const existing = this.liveWithName(parent.node_id, nameKey(name), "");
    if (existing?.kind === NodeKindValue.DIR) {
      this.remap.set(op.nodeId, existing.node_id);
      return result(OpResult_Code.MERGED_INTO, existing.node_id, existing.name);
    }
    const picked = this.pickName(parent.node_id, name, "");
    this.insertNode(op.nodeId, parent.node_id, picked.name, NodeKindValue.DIR, null);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private createFile(op: CreateFile): OpResult {
    const name = normalizeName(op.name);
    if (!name) return rejected("INVALID_NAME");
    if (getNode(this.sql, op.nodeId)) return rejected("ID_EXISTS");
    const parent = this.parentFor(op.parentId);
    if (typeof parent === "string") return rejected(parent);
    if (!parent.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const picked = this.pickName(parent.node_id, name, "");
    const versionId = this.insertVersion(op.nodeId, op.content!, op.content!.chunks);
    this.insertNode(op.nodeId, parent.node_id, picked.name, NodeKindValue.FILE, versionId);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private updateContent(nodeId: string, content: Content, chunks: readonly ChunkRef[]): OpResult {
    const found = this.node(nodeId);
    if (!found) return rejected("NOT_FOUND");
    if (found.kind !== NodeKindValue.FILE) return rejected("NOT_FILE");
    const { node, renamed } = this.reviveTarget(found);
    if (this.unseenContent(node)) {
      const copyId = uuidv7(this.ctx.nowMs);
      const name = this.pickConflictCopyName(node.parent_id!, node.name);
      const versionId = this.insertVersion(copyId, content, chunks);
      this.insertNode(copyId, node.parent_id!, name, NodeKindValue.FILE, versionId);
      return result(OpResult_Code.CONFLICT_COPY, copyId, name);
    }
    this.sql.exec("UPDATE versions SET superseded_at_ms = ? WHERE version_id = ?", this.ctx.nowMs, node.cur_version_id);
    const versionId = this.insertVersion(node.node_id, content, chunks);
    this.sql.exec(
      "UPDATE nodes SET cur_version_id = ?, content_rev = ?, content_dev = ? WHERE node_id = ?",
      versionId,
      this.ctx.seq,
      this.dev,
      node.node_id,
    );
    this.touched.add(node.node_id);
    return result(renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", node.name);
  }

  private isSelfOrDescendant(candidate: NodeRow, ancestorId: string): boolean {
    for (let cur: NodeRow | undefined = candidate; cur; cur = cur.parent_id ? getNode(this.sql, cur.parent_id) : undefined) {
      if (cur.node_id === ancestorId) return true;
    }
    return false;
  }

  private move(op: Move): OpResult {
    const found = this.node(op.nodeId);
    if (!found) return rejected("NOT_FOUND");
    if (!found.parent_id) return rejected("ROOT");
    const name = normalizeName(op.newName);
    if (!name) return rejected("INVALID_NAME");
    const target = this.node(op.newParentId);
    if (!target) return rejected("NOT_FOUND");
    if (target.kind !== NodeKindValue.DIR) return rejected("PARENT_NOT_DIR");
    if (this.isSelfOrDescendant(target, found.node_id)) return rejected("CYCLE");
    if (!target.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    if (found.status === NodeStatusValue.LIVE && found.parent_id === target.node_id && found.name === name) {
      return result(OpResult_Code.APPLIED, "", name);
    }
    const parent = this.parentFor(target.node_id) as NodeRow;
    this.checkSubtreeDepth(found, parent);
    if (found.status === NodeStatusValue.TRASHED) {
      const group = found.trash_group!;
      const picked = this.reviveOne(found, parent.node_id, name);
      this.reviveGroup(found.node_id, group);
      return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
    }
    const picked = this.pickName(parent.node_id, name, found.node_id);
    this.sql.exec(
      "UPDATE nodes SET parent_id = ?, name = ?, name_key = ?, meta_rev = ?, meta_dev = ? WHERE node_id = ?",
      parent.node_id,
      picked.name,
      nameKey(picked.name),
      this.ctx.seq,
      this.dev,
      found.node_id,
    );
    this.touched.add(found.node_id);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  /** R7: trashes the subtree except nodes with unseen changes and their ancestors. Returns true when node stays. */
  private trashSubtree(node: NodeRow, group: string): boolean {
    let keep = this.unseenContent(node) || this.unseenMeta(node);
    const children = rows<NodeRow>(this.sql.exec("SELECT * FROM nodes WHERE parent_id = ? AND status = 1", node.node_id));
    for (const child of children) keep = this.trashSubtree(child, group) || keep;
    if (keep) return true;
    this.sql.exec(
      "UPDATE nodes SET status = 2, trash_group = ?, trashed_at_ms = ?, meta_rev = ?, meta_dev = ? WHERE node_id = ?",
      group,
      this.ctx.nowMs,
      this.ctx.seq,
      this.dev,
      node.node_id,
    );
    this.touched.add(node.node_id);
    return false;
  }

  private delete(op: Delete, index: number): OpResult {
    const node = this.node(op.nodeId);
    if (!node) return rejected("NOT_FOUND");
    if (!node.parent_id) return rejected("ROOT");
    if (node.status === NodeStatusValue.TRASHED) return result(OpResult_Code.APPLIED);
    const kept = this.trashSubtree(node, `${this.ctx.seq}:${index}`);
    return result(kept ? OpResult_Code.SUPERSEDED : OpResult_Code.APPLIED);
  }

  private restore(op: Restore): OpResult {
    const node = this.node(op.nodeId);
    if (!node) return rejected("NOT_FOUND");
    if (op.versionId) {
      const version = getVersion(this.sql, op.versionId);
      if (!version || version.node_id !== node.node_id) return rejected("NOT_FOUND");
      return this.updateContent(node.node_id, versionContent(this.sql, version, false), versionChunks(this.sql, version));
    }
    if (!node.parent_id) return rejected("ROOT");
    if (node.status !== NodeStatusValue.TRASHED) return result(OpResult_Code.APPLIED, "", node.name);
    const name = op.name ? normalizeName(op.name) : node.name;
    if (!name) return rejected("INVALID_NAME");
    const target = this.node(op.targetParentId || node.parent_id);
    if (!target) return rejected("NOT_FOUND");
    if (target.kind !== NodeKindValue.DIR) return rejected("PARENT_NOT_DIR");
    if (this.isSelfOrDescendant(target, node.node_id)) return rejected("CYCLE");
    if (!target.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const parent = this.parentFor(target.node_id) as NodeRow;
    this.checkSubtreeDepth(node, parent);
    const group = node.trash_group!;
    const picked = this.reviveOne(node, parent.node_id, name);
    this.reviveGroup(node.node_id, group);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }
}

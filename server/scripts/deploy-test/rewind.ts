// SPDX-License-Identifier: AGPL-3.0-or-later

import { type CommitRecord, NodeKind, type NodeState, NodeStatus } from "../../src/gen/koidrive/v1/model_pb";

export function replay(log: CommitRecord[], seq: bigint): Map<string, NodeState> {
  const nodes = new Map<string, NodeState>();
  for (const commit of log) {
    if (commit.seq > seq) break;
    for (const { change } of commit.changes) {
      if (change.case === "node") nodes.set(change.value.nodeId, change.value);
      else if (change.case === "purged") nodes.delete(change.value.nodeId);
    }
  }
  return nodes;
}

function liveNodes(nodes: Map<string, NodeState>): string {
  return [...nodes.values()]
    .filter((node) => node.status === NodeStatus.LIVE && node.parentId !== "")
    .map((node) => [node.nodeId, node.parentId, node.name, node.kind, node.current?.versionId ?? ""].join(" ") + "\n")
    .sort()
    .join("");
}

export function rewindMismatch(before: CommitRecord[], after: CommitRecord[], tree: Map<string, NodeState>, restoreSeq: bigint): string | undefined {
  const head = (log: CommitRecord[]) => log.at(-1)?.seq ?? 0n;
  if (head(before) < restoreSeq) return "the rewind reports restore_seq " + restoreSeq + ", but the log before it ends at seq " + head(before);
  if (head(after) !== restoreSeq) return "the rewind reports restore_seq " + restoreSeq + ", but the log after it ends at seq " + head(after);
  const commits = (log: CommitRecord[]) => log.map((commit) => commit.seq + " " + commit.commitId).join(", ");
  const kept = commits(before.filter((commit) => commit.seq <= restoreSeq));
  if (commits(after) !== kept) return "the log after the rewind is [" + commits(after) + "], but the log before it up to seq " + restoreSeq + " is [" + kept + "]";
  const want = liveNodes(replay(before, restoreSeq));
  const got = liveNodes(tree);
  if (got !== want) return "the server's tree after the rewind is not the tree the log before it holds at seq " + restoreSeq + ".\nLog:\n" + want + "Server:\n" + got;
  return undefined;
}

export function filePaths(tree: Map<string, NodeState>): [string, NodeState][] {
  const path = (node: NodeState): string => {
    const parent = tree.get(node.parentId);
    return parent && parent.parentId !== "" ? path(parent) + "/" + node.name : node.name;
  };
  return [...tree.values()]
    .filter((node) => node.status === NodeStatus.LIVE && node.kind === NodeKind.FILE)
    .map((node) => [path(node), node] as [string, NodeState])
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
}

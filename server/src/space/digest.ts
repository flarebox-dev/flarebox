// SPDX-License-Identifier: AGPL-3.0-or-later

// Digest v1 of a commit request (RFC 0006 section 8). Its coverage is frozen: the projection copies exactly the fields
// that section lists, so a field added to the schema later never changes a digest a Space DO has stored.

import { create, toBinary } from "@bufbuild/protobuf";
import { type Commit, CommitSchema, type Op, OpSchema } from "../gen/koidrive/v1/commit_pb";
import { type Content, ContentSchema } from "../gen/koidrive/v1/model_pb";
import { sha256 } from "../crypto";

function content(value: Content | undefined): Content | undefined {
  if (!value) return undefined;
  return create(ContentSchema, {
    size: value.size,
    mtimeNs: value.mtimeNs,
    executable: value.executable,
    chunks: value.chunks.map(({ hash, size, encoding }) => ({ hash, size, encoding })),
    chunkCount: value.chunkCount,
    manifestDigest: value.manifestDigest,
    chunker: value.chunker,
  });
}

function op({ op }: Op): Op {
  switch (op.case) {
    case "createDir":
      return create(OpSchema, { op: { case: op.case, value: { nodeId: op.value.nodeId, parentId: op.value.parentId, name: op.value.name } } });
    case "createFile": {
      const { nodeId, parentId, name } = op.value;
      return create(OpSchema, { op: { case: op.case, value: { nodeId, parentId, name, content: content(op.value.content) } } });
    }
    case "updateContent":
      return create(OpSchema, { op: { case: op.case, value: { nodeId: op.value.nodeId, content: content(op.value.content) } } });
    case "move": {
      const { nodeId, newParentId, newName } = op.value;
      return create(OpSchema, { op: { case: op.case, value: { nodeId, newParentId, newName } } });
    }
    case "delete":
      return create(OpSchema, { op: { case: op.case, value: { nodeId: op.value.nodeId } } });
    case "restore": {
      const { nodeId, versionId, targetParentId, name } = op.value;
      return create(OpSchema, { op: { case: op.case, value: { nodeId, versionId, targetParentId, name } } });
    }
    default:
      return create(OpSchema);
  }
}

/** SHA-256 of the v1 projection of the request: base_seq, ops, device_seq and require_head, with no unknown fields. */
export async function commitDigest(commit: Commit): Promise<Uint8Array> {
  const projected = create(CommitSchema, {
    baseSeq: commit.baseSeq,
    ops: commit.ops.map(op),
    deviceSeq: commit.deviceSeq,
    requireHead: commit.requireHead,
  });
  return sha256(toBinary(CommitSchema, projected, { writeUnknownFields: false }));
}

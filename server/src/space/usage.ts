// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { type SpaceUsage, SpaceUsageSchema } from "../gen/flarebox/v1/auth_pb";
import { toHex } from "../ids";
import { NodeStatusValue } from "./schema";
import { type VersionRow, versionChunks } from "./store";

/** Where a version's chunks count. A lower value wins for a chunk that several versions hold (RFC 0010 section 2). */
const LIVE = 0;
const TRASH = 1;
const OLD = 2;
type Holder = typeof LIVE | typeof TRASH | typeof OLD;

/**
 * Reads what the space holds (RFC 0010 section 2). One pass over the versions keeps, for each chunk, the first holder
 * that holds it, so a chunk shared by files or versions counts once. The cost grows with the versions and their chunks.
 */
export function readUsage(sql: SqlStorage): SpaceUsage {
  const held = new Map<string, { size: number; holder: Holder }>();
  const versions = [0, 0, 0];
  const cursor = sql.exec(
    "SELECT v.*, n.status AS node_status FROM versions v LEFT JOIN nodes n ON n.node_id = v.node_id AND n.cur_version_id = v.version_id",
  );
  for (const row of cursor) {
    const version = row as unknown as VersionRow & { node_status: number | null };
    const holder: Holder = version.node_status === NodeStatusValue.LIVE ? LIVE : version.node_status === NodeStatusValue.TRASHED ? TRASH : OLD;
    versions[holder]!++;
    for (const chunk of versionChunks(sql, version)) {
      const key = toHex(chunk.hash);
      const known = held.get(key);
      if (!known) held.set(key, { size: chunk.size, holder });
      else if (holder < known.holder) known.holder = holder;
    }
  }
  const bytes = [0, 0, 0];
  for (const { size, holder } of held.values()) bytes[holder]! += size;
  const unreferenced = Number(sql.exec("SELECT COALESCE(SUM(size), 0) AS bytes FROM chunks WHERE refcount = 0").one().bytes);
  return create(SpaceUsageSchema, {
    liveFiles: BigInt(versions[LIVE]!),
    liveBytes: BigInt(bytes[LIVE]!),
    trashedFiles: BigInt(versions[TRASH]!),
    trashBytes: BigInt(bytes[TRASH]!),
    oldVersions: BigInt(versions[OLD]!),
    oldVersionBytes: BigInt(bytes[OLD]!),
    unreferencedBytes: BigInt(unreferenced),
  });
}

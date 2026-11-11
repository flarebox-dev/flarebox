# RFC 0004: R2 export format

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-271.

Spec v0 §11 describes the R2 metadata export but leaves its file formats open, and names two records, `ExportHeader` and `NodeSnapshot`, that the schema lacks. RFC 0003 §7 moves the export under per-history prefixes. This RFC fixes the formats and the write order, so that an R2-only restore can read what any server wrote. Section numbers such as §11 refer to spec v0, and this RFC refers to its own sections as "section N".

The schema change is additive: `export.proto` adds `ExportHeader`, `NodeSnapshot` and `RetainedVersion`.

## 1. Layout

Each history writes under its own prefix (RFC 0003 §7):

```
spaces/<space_id>/meta/<history_id>/
  space.json                          descriptor (section 2)
  log/<first_seq>-<last_seq>.fblog    log segment (section 3)
  snap/<start_seq>/manifest.json      snapshot manifest (section 4)
  snap/<start_seq>/pages/<n>.fbsnap   snapshot page (section 3)
```

Seqs are 20-digit zero-padded decimals, as in §11. A page number `n` is a decimal without padding.

## 2. Descriptor

`space.json` is a JSON object with the fields of §11 and RFC 0003 §7:

| Field | Value |
|---|---|
| `format` | `"koidrive.space"` |
| `format_version` | `1` |
| `space_id` | the space's id |
| `name` | the space's name when the descriptor was written |
| `created_at` | when the space was created, an RFC 3339 UTC timestamp |
| `hash_alg` | `"sha256"` |
| `chunk_key` | `"spaces/{space_id}/chunks/{hex2}/{hex}/{incarnation}"` |
| `chunk_encoding` | `"raw"` |
| `history_id` | the history this descriptor belongs to |
| `rewind_number` | the history's rewind number (RFC 0003 §5, rule 6), a JSON number |

`chunk_key` names the layout of RFC 0003 §5, rule 5. An object is stored under the incarnation of the chunk row that holds it, and incarnations are internal to the server. A client never derives a key itself: it addresses chunks by hash and uses the targets the server issues (§10). An R2-only restore finds a chunk by listing `chunks/<hex2>/<hex>/` and using any incarnation there, as RFC 0003 §7 says.

## 3. Log segments and snapshot pages

Both kinds of file are a gzip stream (RFC 1952) of size-delimited protobuf messages: each message is preceded by its length as a varint. The first message is an `ExportHeader`:

| File | `format` | Then |
|---|---|---|
| `.fblog` | `"koidrive.log"` | the `CommitRecord`s from `first_seq` through `last_seq`, in seq order with no gap |
| `.fbsnap` | `"koidrive.snapshot"` | `NodeSnapshot` records |

The header's `format_version` is 1 and its `space_id` and `history_id` name the file's space and history. A reader refuses a file whose header has another `format`, a `format_version` other than 1, or another space or history. The same holds for the `format` and `format_version` of `space.json` and `manifest.json`.

**Log segments.** Segments of one history are contiguous and never overlap. The first starts at seq 1 in a space's first history, and at `restore_seq` + 1 in a history that a rewind started. A segment holds at most 1,000 commits. Segments are written as §11 says: once 1,000 commits are waiting, or 5 minutes after the first commit not yet exported. The writer may also end a segment earlier, to bound its size or to reach a snapshot's `end_seq` (section 4). The writer records a segment's range before it writes the file, and a retry writes the same range again with the same records. So a segment, once its write may have been issued, is never replaced by another range.

**Snapshot pages.** Each page holds the nodes of one `node_id` range, in ascending order, read at one moment. Consecutive pages cover consecutive ranges, so a node appears in at most one page, and the root is included. A node created or purged after its range was read is restored from the log. The page size is the writer's choice. A `NodeSnapshot` holds:
- `node`: the node's state when its page was read;
- `previous`: every retained version of the node other than `node.current`, each with the time it was superseded;
- `trashed_at_ms`: when the node was trashed, or 0 when it is not trashed.

Every chunk list in a snapshot is complete: `content.chunks` lists all `chunk_count` chunks, without the 16,384-chunk cap of §13, because the snapshot may be the only copy of the list.

**Large versions.** A log record carries a chunk list only when the version has at most 16,384 chunks (§13). So when a commit records a version with more chunks, a snapshot that starts after that commit is due at once. If a snapshot that started earlier is still being written, the new one starts as soon as it completes. Known limitation: until that snapshot's manifest is written, an R2-only restore cannot recover the version's chunk list.

## 4. Snapshot manifest

`manifest.json` is a JSON object:

| Field | Value |
|---|---|
| `format` | `"koidrive.snapshot"` |
| `format_version` | `1` |
| `space_id`, `history_id` | the snapshot's space and history |
| `start_seq` | `head_seq` when the first page was read, a JSON number |
| `end_seq` | `head_seq` when the last page was read, a JSON number |
| `created_at` | when the snapshot started, an RFC 3339 UTC timestamp |
| `pages` | one entry per page, in node order: `name` (the file name under `pages/`, for example `"0.fbsnap"`), `records` (the number of `NodeSnapshot` records), `bytes` (the size of the stored file) and `sha256` (lowercase hex SHA-256 of the stored file) |

The manifest is the only authority for a snapshot. A reader uses only its page list, never lists pages by prefix, and checks each page's size and hash.

Every write here may have an unknown outcome and land later, even after a read found nothing ([R2 consistency](https://developers.cloudflare.com/r2/reference/consistency/), accessed 2026-11-10). The writer therefore keeps these rules:
- **Pages.** Before it issues a page's write, the writer records the page number as used. It never writes that number again: after an unknown outcome it moves to the next number, because a late write could replace the retry's bytes after the manifest recorded their hash. So page numbers in a manifest can skip, and a page that no manifest lists is an orphan.
- **Manifest.** The writer writes the manifest only after every page it lists is durable and the log segments through `end_seq` are durable. Before the first write, it records the manifest's bytes, and every retry writes exactly those bytes until R2 confirms one. A late write can therefore only store the same manifest again.
- **Names.** A snapshot's `start_seq` is above the previous snapshot's `start_seq`, so no manifest name is used twice. It may equal the previous snapshot's `end_seq`: a snapshot that a large version makes due (section 3) starts at once, even if no commit follows.

A deleted manifest (section 7) can still reappear through a late write, together with some of its pages. Readers never treat it as current, because they take the newest manifest by `start_seq` (section 5), and retention always keeps two newer ones.

Snapshots follow the cadence of §11. A snapshot is also due at once for the first snapshot of a history that a rewind started (RFC 0003 §7), and after a large version (section 3).

## 5. Restore

An R2-only restore of a space:
1. Takes the descriptor with the highest `rewind_number` (RFC 0003 §7) and reads only that history's files.
2. Lists `snap/` and takes the manifest with the highest `start_seq`. If the history has none and its `rewind_number` is 0, it starts from a space that holds only its root, whose node id is the space id, with `start_seq` and `end_seq` 0. A history with a higher `rewind_number` always has a manifest, because its descriptor follows its first one (section 6), so a missing manifest there means the export is damaged.
3. Loads every page the manifest lists.
4. Replays the log from `start_seq` + 1 onward, whole commits in seq order (RFC 0002 §5.2), with the rules below. Records at or below `start_seq` are skipped. From the segment that holds `start_seq` + 1 on, each segment must start right after the previous one ends, and its records must carry consecutive seqs from its `first_seq` to its `last_seq`. The log must reach at least the manifest's `end_seq`. A gap, an overlap or a short log means the export is damaged, and the reader reports it rather than restoring partial state.

If the selected manifest is gone when the reader fetches it, or a page it lists is missing, the reader lists `snap/` again. If a newer manifest exists, retention has removed the snapshot during the read, and the reader starts again at step 2. Otherwise the export is damaged, and the reader reports it rather than falling back to an older manifest. Retention deletes a snapshot only after two newer ones are complete (section 7), so a fresh selection always finds a complete snapshot. It promises no minimum time: a reader that new snapshots keep overtaking may have to restart more than once.

Replay applies each change record to the restored node, version and trash state:
- **`NodeState` whose revision is not newer.** If the record's `max(content_rev, meta_rev)` is not above the restored node's, the record is skipped. The page that holds the node was read after that commit, so it already includes the record's effects.
- **`NodeState` that is newer**, or for a node not yet restored. The node takes the record's fields. If `current.version_id` differs from the restored current version, the restored current version joins the previous versions, with the commit's `committed_at_ms` as its superseded time. Versions are matched by `version_id`: when the record's version omits its chunk list (more than 16,384 chunks, §13) and the restore already holds that version with a complete list, the complete list stays.
- **Trash state.** A record that shows a node trashed, when the restore had it live or not at all, sets its `trashed_at_ms` to the commit's `committed_at_ms`. A node that becomes live again gets 0. A node that stays trashed keeps its time.
- **`NodePurged`.** The node and all its versions are removed.

The server does not log version expiry (§12). So a restored previous version may already have expired on the server, and its chunks may be gone after the GC grace. A restore treats previous versions as best effort.

## 6. Write order

Within one history:
1. Log segments are written in seq order.
2. A snapshot's manifest follows its pages and the segments through its `end_seq` (section 4).
3. The descriptor follows the history's baseline. In a space's first history there is none, so the descriptor is written when the space is created. In a history that a rewind started, it follows the first snapshot's manifest, which already implies the log segments that RFC 0003 §7 requires.

A file is durable once R2 has confirmed its write. Rewind admission refuses with `RETRY_LATER` until the current history's descriptor is durable (RFC 0003 §1).

## 7. Retention

The writer keeps the two newest completed snapshots of the current history. When a third completes, it deletes the oldest: first its manifest, then every object under its `pages/` prefix, orphan pages included. Deleting the manifest first stops new readers from choosing that snapshot. A reader that fetched the manifest earlier restarts as section 5 says. Log segments are never deleted, although §11 would allow deleting those before the older remaining snapshot. Files of earlier histories are never changed or deleted.

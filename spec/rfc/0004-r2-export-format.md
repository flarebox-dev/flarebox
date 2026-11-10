# RFC 0004: R2 export format

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-271.

Spec v0 §11 describes the R2 metadata export but leaves its file formats open, and names two records, `ExportHeader` and `NodeSnapshot`, that the schema lacks. RFC 0003 §7 moves the export under per-history prefixes. This RFC fixes the formats and the write order, so that an R2-only restore can read what any server wrote. Section numbers refer to spec v0.

The schema change is additive: `export.proto` adds `ExportHeader`, `NodeSnapshot` and `RetainedVersion`.

## 1. Layout

Each history writes under its own prefix (RFC 0003 §7):

```
spaces/<space_id>/meta/<history_id>/
  space.json                          descriptor (§2)
  log/<first_seq>-<last_seq>.fblog    log segment (§3)
  snap/<start_seq>/manifest.json      snapshot manifest (§4)
  snap/<start_seq>/pages/<n>.fbsnap   snapshot page (§3)
```

Seqs are 20-digit zero-padded decimals, as in §11. A page number `n` is a decimal without padding.

## 2. Descriptor

`space.json` is a JSON object with the fields of §11 and RFC 0003 §7:

| Field | Value |
|---|---|
| `format` | `"flarebox.space"` |
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
| `.fblog` | `"flarebox.log"` | the `CommitRecord`s from `first_seq` through `last_seq`, in seq order with no gap |
| `.fbsnap` | `"flarebox.snapshot"` | `NodeSnapshot` records |

The header's `format_version` is 1 and its `space_id` and `history_id` name the file's space and history. A reader refuses a file whose header has another `format`, a higher `format_version`, or another space or history.

**Log segments.** Segments of one history are contiguous and never overlap. The first starts at seq 1 in a space's first history, and at `restore_seq` + 1 in a history that a rewind started. A segment holds at most 1,000 commits. Segments are written as §11 says: once 1,000 commits are waiting, or 5 minutes after the first commit not yet exported. The writer may also end a segment earlier, to bound its size or to reach a snapshot's `end_seq` (§4). The writer records a segment's range before it writes the file, and a retry writes the same range again with the same records. So a segment, once its write may have been issued, is never replaced by another range.

**Snapshot pages.** Each page holds the nodes of one `node_id` range, in ascending order, read at one moment. Consecutive pages cover consecutive ranges, so a node appears in at most one page, and the root is included. A node created or purged after its range was read is restored from the log. The page size is the writer's choice. A `NodeSnapshot` holds:
- `node`: the node's state when its page was read;
- `previous`: every retained version of the node other than `node.current`, each with the time it was superseded;
- `trashed_at_ms`: when the node was trashed, or 0 when it is not trashed.

Every chunk list in a snapshot is complete: `content.chunks` lists all `chunk_count` chunks, without the 16,384-chunk cap of §13, because the snapshot may be the only copy of the list.

Known limitation: a log record carries a chunk list only when the version has at most 16,384 chunks (§13). Until a snapshot holds a larger version, an R2-only restore cannot recover that version's chunk list.

## 4. Snapshot manifest

`manifest.json` is a JSON object:

| Field | Value |
|---|---|
| `format` | `"flarebox.snapshot"` |
| `format_version` | `1` |
| `space_id`, `history_id` | the snapshot's space and history |
| `start_seq` | `head_seq` when the first page was read, a JSON number |
| `end_seq` | `head_seq` when the last page was read, a JSON number |
| `created_at` | when the snapshot started, an RFC 3339 UTC timestamp |
| `pages` | one entry per page, in node order: `name` (the file name under `pages/`, for example `"0.fbsnap"`), `records` (the number of `NodeSnapshot` records), `bytes` (the size of the stored file) and `sha256` (lowercase hex SHA-256 of the stored file) |

The manifest is the only authority for a snapshot:
- The writer writes it last, only after every page it lists is durable and the log segments through `end_seq` are durable. So the newest manifest of a history can always be restored with that history's log.
- A reader uses only the manifest's page list and never lists pages by prefix. It checks each page's size and hash.
- A page write whose outcome is unknown is never repeated under the same number, because a late write could replace the retry's bytes after the manifest recorded their hash. The writer moves to the next number instead, so page numbers in a manifest can skip. A page that no manifest lists is an orphan.

Snapshots follow the cadence of §11. The first snapshot of a history that a rewind started is due at once (RFC 0003 §7).

## 5. Write order

Within one history:
1. Log segments are written in seq order.
2. A snapshot's manifest follows its pages and the segments through its `end_seq` (§4).
3. The descriptor follows the history's baseline. In a space's first history there is none, so the descriptor is written when the space is created. In a history that a rewind started, it follows the first snapshot's manifest, which already implies the log segments that RFC 0003 §7 requires.

A file is durable once R2 has confirmed its write. Rewind admission refuses with `RETRY_LATER` until the current history's descriptor is durable (RFC 0003 §1).

## 6. Retention

The writer keeps the two newest completed snapshots of the current history. It deletes an older snapshot's manifest first and then every object under its `pages/` prefix, orphan pages included, so a reader never finds a manifest whose pages are gone. Log segments are never deleted, although §11 would allow deleting those before the older remaining snapshot. Files of earlier histories are never changed or deleted.

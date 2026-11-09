# RFC 0003: Space rewind

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-268.

The owner of a space can rewind it to an earlier point in time. The rewind restores the Space DO's SQLite database with point-in-time recovery (PITR) and starts a new history (§7.4). Devices then recover as RFC 0002 §6 describes. Section numbers refer to spec v0.

The schema changes add messages and fields only:

| Message | Fields | Section |
|---|---|---|
| `RewindRequest` (new, `auth.proto`) | `restore_point_ms = 1`, `expected_history_id = 2` | 1 |
| `RewindResult` (new, `auth.proto`) | `history_id = 1`, `restore_seq = 2`, `restore_point_ms = 3`, `previous_history_id = 4`, `previous_head_seq = 5` | 1 |

## 1. API (§15.1)

`POST /v1/spaces/{space_id}/rewind` takes a `RewindRequest` and returns a `RewindResult`. The body is protobuf, like the other endpoints of §15.1. The CLI command `flarebox space rewind` calls this endpoint and nothing else, so this request and response are the CLI's stable interface.

The request is refused, and nothing changes, in these cases:

| Condition | Error code | HTTP status |
|---|---|---|
| The caller is not the space's owner, or the device is revoked | `FORBIDDEN` or `UNAUTHENTICATED` | 403 or 401 |
| `restore_point_ms` is not inside the window (§2) | `INVALID_ARGUMENT` | 400 |
| `expected_history_id` is not the space's current history | `HISTORY_MISMATCH` | 409 |
| Another rewind of the space has not completed | `RETRY_LATER` | 503 |

`expected_history_id` ties the request to the state the owner looked at. Two rewinds sent from the same view therefore cannot both apply.

## 2. Window

`restore_point_ms` must be in the past and no earlier than the space's creation. It must also be no more than `min(chunk_grace_days, 29)` days before the request.

- **29 days.** PITR restores to any point in the past 30 days ([SQLite-backed Durable Object Storage, PITR API](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/#pitr-point-in-time-recovery-api), accessed 2026-11-10). The one-day margin leaves time for the rewind to finish inside that limit.
- **`chunk_grace_days`.** This bound keeps every chunk that the restored state references in R2. A version that existed at the restore point P either still exists now, or it was removed at some time T ≥ P. Removing it lowers refcounts, and GC deletes a chunk no earlier than `chunk_grace_days` after its refcount reaches 0 (§12). So the earliest such deletion is at T + `chunk_grace_days` ≥ P + `chunk_grace_days`, which is after now.

## 3. Procedure (§7.4)

The Account DO directs the rewind, because the Space DO's own storage is what gets replaced:

1. **Record.** The Account DO records a pending rewind for the space: the restore point and a new `history_id`. From this moment until step 4 completes, the space serves nothing. Every HTTP RPC and WebSocket upgrade gets `RETRY_LATER`, and the Space DO accepts no socket message.
2. **Restore.** The Space DO checks the window (§2) and `expected_history_id`, and records its current `history_id` and `head_seq` for the result. It sends `Goodbye` with close code 4004 (space rewound) on every WebSocket and closes it. It then gets the bookmark for the restore point, calls `onNextSessionRestoreBookmark` with it, and restarts with `ctx.abort()`.
3. **Check.** On the next call, the Space DO confirms that the restore happened: no commit in its log is later than the restore point. Otherwise the rewind fails and the pending record stays, so the Account DO retries step 2.
4. **Complete**, in one Space DO transaction:
   - set `history_id` to the new value, and `restore_seq` (the current `head_seq`) and `restore_point_ms` in `meta` (RFC 0002 §8);
   - add every device that the Account DO has revoked to `revoked_devices`, because the restore may have removed later revocations;
   - mark every chunk row with refcount 0, `pending` or `live`, as `deleting`. GC may have deleted such a chunk after the restore point, and a `PRESENT` answer for it would let a commit reference bytes that are gone. Uploads after the rewind store these chunks again;
   - schedule GC and the orphan sweep to run now, and re-arm the alarm from the restored `schedule` table. The orphan sweep removes the objects uploaded after the restore point, which have no row in the restored state.

   The Account DO then clears the pending record and returns the `RewindResult`. If step 2, 3 or 4 fails after the restore began, the Account DO keeps the record and retries until the rewind completes, and the space stays closed meanwhile (step 1).

## 4. What a rewind keeps and discards

- **Restored** to the restore point: everything in the Space DO. That includes the tree, versions, trash, commit log, chunk rows, members, the space's name and settings, and `device_commits` (RFC 0002 §2).
- **Kept**: the Account DO's state, including devices, revocations, spaces and share links. A share link that points to a node absent after the rewind finds nothing.
- **Discarded**: commits after `restore_seq`, and the chunks that only they referenced. A rewind cannot be undone with another rewind, because step 4 deletes those chunks. The bytes of discarded changes survive on devices, which hold them during recovery (RFC 0002 §6) until the user decides.

## 5. Devices

A connected device sees close code 4004 and reconnects. Its `Welcome` then carries the new `history_id`, and the device enters recovery (RFC 0002 §6). A device that sends `Resume` with its stored `history_id` gets `ResyncRequired` with `HISTORY_CHANGED`, `restore_seq` and `restore_point_ms` (RFC 0002 §8). Its commits from the old history are refused with `HISTORY_MISMATCH` (RFC 0002 §1).

## 6. R2 metadata export (§11)

The export layout changes so that each history keeps its own files:

- Log segments are written under `meta/<history_id>/log/`, and snapshots under `meta/<history_id>/snap/`.
- `space.json` gains `history_id`, the current history. It is rewritten when a history begins.
- A restore from R2 reads only the files of the history that `space.json` names.

The files of earlier histories are never changed. Without this change, the new history would write segments with the same seq ranges, under the same names, as the old history's segments, and the rule that a written segment is never changed would break.

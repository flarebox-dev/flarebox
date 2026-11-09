# RFC 0003: Space rewind

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-268.

The owner of a space can rewind it to an earlier point in time. The rewind restores the Space DO's SQLite database with point-in-time recovery (PITR) and starts a new history (§7.4). Devices then recover as RFC 0002 §6 describes. Section numbers refer to spec v0.

The schema changes add messages and fields only:

| Message | Fields | Section |
|---|---|---|
| `RewindRequest` (new, `auth.proto`) | `restore_point_ms = 1`, `expected_history_id = 2`, `rewind_id = 3` | 1 |
| `RewindResult` (new, `auth.proto`) | `history_id = 1`, `restore_seq = 2`, `restore_point_ms = 3`, `previous_history_id = 4`, `previous_head_seq = 5` | 1 |

PITR behaves as [the SQLite storage API documentation](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/#pitr-point-in-time-recovery-api) describes (accessed 2026-11-10):
- it restores to any point in the past 30 days;
- `getBookmarkForTime` returns a bookmark for approximately the given time;
- bookmarks compare lexically in time order;
- `onNextSessionRestoreBookmark` restores the storage when the object next starts.

## 1. API (§15.1)

`POST /v1/spaces/{space_id}/rewind` takes a `RewindRequest` and returns a `RewindResult`. The body is protobuf, like the other endpoints of §15.1. The CLI command `flarebox space rewind` calls this endpoint and nothing else, so this request and response are the CLI's stable interface.

`rewind_id` is a UUIDv7 that the caller chooses once per intended rewind and reuses on every retry. The Account DO looks it up before checking anything else:

- **Same `rewind_id`, same fields.** The request returns that rewind's state:
  - the stored `RewindResult` once the rewind is complete, even though the space's history has since changed;
  - `RETRY_LATER` while it runs;
  - its stored error if it failed.
- **Same `rewind_id`, different `restore_point_ms` or `expected_history_id`.** `INVALID_ARGUMENT`.
- **New `rewind_id`.** The checks below apply. A new request built on the old `expected_history_id` fails the history check, so it cannot start a second rewind.

The Account DO keeps every rewind record for at least 30 days after it ends.

A new `rewind_id` is refused, with nothing recorded and nothing changed, in these cases:

| Condition | Error code | HTTP status |
|---|---|---|
| The device is revoked, or the caller is not the space's owner | `UNAUTHENTICATED` or `FORBIDDEN` | 401 or 403 |
| `restore_point_ms` is not admissible (§2) | `INVALID_ARGUMENT` | 400 |
| `expected_history_id` is not the space's current history | `HISTORY_MISMATCH` | 409 |
| Another rewind of the space has not ended | `RETRY_LATER` | 503 |

## 2. Admissible restore points

At admission, `restore_point_ms` (P) must meet all of these:
- it is at least one minute and at most 29 days before the request;
- it is not before the space was created;
- it is not inside the interval during which an earlier rewind of the space held the space closed (§3).

The 29-day bound keeps P inside the 30-day PITR range, with a day for the rewind to finish. The window is checked once, at admission. A rewind already admitted is never refused later because time has passed.

P is the requested point. The restored state is the one at the bookmark that PITR selects for P, which is approximately P. `RewindResult.restore_seq` and `ResyncRequired.restore_seq` give the exact `head_seq` of the restored state, and `restore_point_ms` is always the requested P.

## 3. Phases

The Account DO directs the rewind and keeps a record of it in its own storage, which PITR on the Space DO does not touch. The record holds:
- the request;
- the selected bookmark B;
- the new `history_id`;
- the previous history and head;
- the attempt number;
- the phase;
- the result or the error.

An Account DO alarm drives the phases with backoff from one second up to ten minutes. Each phase is recorded before the call it starts, so a crash or an unknown call outcome resumes from the recorded phase.

1. **Admission.** The Account DO asks the Space DO, through a call that changes nothing, for:
   - its current `history_id` and `head_seq`;
   - the bookmark B for P.

   It then checks §1 and §2. Any failure refuses the request and records nothing. Otherwise it records the rewind as `admitted`, with B and the new `history_id`. From then on the space is closed (§4).
2. **Preparing attempt k.** The Space DO:
   - closes every WebSocket after sending `Goodbye` with close code 4004 (space rewound);
   - drains destructive work (§5, rule 5);
   - writes a marker row for the rewind and attempt k, and reads its current bookmark M;
   - returns its `history_id` and `head_seq`, and whether B is earlier than M.

   If B is not earlier than M, the restored state could contain the marker, and the rewind fails (step 5). Otherwise the Account DO records the phase `prepared`, and, on the first attempt, the returned history and head as the previous history and head. All of this happens before any restore can be armed. Preparing is idempotent: repeating it for the same attempt leaves one marker.
3. **Arming attempt k.** The Account DO records the phase `arming`. Then the Space DO calls `onNextSessionRestoreBookmark(B)` and restarts with `ctx.abort()`.
4. **Settling.** After an arming call, whether it answered, failed or has an unknown outcome, the Account DO first restarts the Space DO once more. A restore that is pending applies when the object starts, so after this restart none can still be pending. The Account DO then asks the Space DO to complete the rewind:
   - **Marker of attempt k present.** No restore happened and none is pending. The Account DO goes back to step 2 with attempt k + 1, or ends in step 6 if B can no longer be restored.
   - **No marker, but a completion receipt for this rewind in `meta`.** Completion already ran, and the Space DO returns that receipt again.
   - **Neither.** The storage was restored to B, which predates every marker of this rewind. The Space DO then completes the rewind in one transaction:
     - sets the new `history_id`, plus `restore_seq` (its `head_seq`) and `restore_point_ms` in `meta` (RFC 0002 §8);
     - adds every device that the Account DO has revoked to `revoked_devices`;
     - applies the chunk rules of §5;
     - resets the export state (§7);
     - stores the completion receipt.

   Every completion call, a replay included, then sets the Space DO's alarm from its `schedule` table. A repeated call therefore returns the same receipt and leaves the alarm set.

   An Account DO that restarts in the phase `arming` resumes at this step. So it never arms again before it has settled the earlier attempt.
5. **Reopening.** The Account DO stores the `RewindResult` and records the phase `completed`, which reopens the space.
6. **Failure.** A rewind ends as `failed` only when no restore can have happened or be pending. That is the case when B is not earlier than M in step 2, or after step 4 has proved that the marker is still present and B can no longer be restored. The Account DO then removes the markers, records the error, and reopens the space unchanged.

   Once step 4 has found the marker gone, the rewind can only complete. If completion keeps failing, the space stays closed: reopening a restored database under its old history would let devices treat it as that history. This can only happen through a server bug. A retried request with the same `rewind_id` reports `RETRY_LATER` and the last error.

## 4. Closed space

From admission until the record is `completed` or `failed`, the space is closed:
- every HTTP RPC and WebSocket upgrade gets `RETRY_LATER`;
- the Space DO answers every socket message by closing the socket with 4004;
- the Space DO's alarm runs no work and leaves the `schedule` table as it is.

A closed alarm sets the alarm again for at most ten minutes later. Work that comes due during the closure, such as GC, the orphan sweep or export, therefore runs within ten minutes of reopening, even with no client activity.

The Account DO decides whether the space is closed. The Worker and the Space DO already ask it about every request and message, and the alarm handler also asks it before it runs any work. So the restored database cannot run GC before completion has applied §5, and no client reaches the restored state before it carries its new history.

## 5. Chunk retention

Invariant: no chunk object is deleted earlier than `chunk_grace_days` after the last moment at which any history referenced it. These rules keep it:

1. GC deletes a chunk no earlier than `chunk_grace_days` after its refcount reaches 0, as in §12.
2. At completion, every chunk row whose refcount is 0, in any state, becomes `pending` with `hold_until` set to the completion time plus `chunk_grace_days`. The discarded history may have referenced the chunk until the rewind, and GC may already have deleted the object. As `pending`, `PrepareUpload` answers `UPLOAD`, so a commit that needs the chunk stores it again; a conditional PUT that finds the object present counts as stored. If no commit needs it, it ages out through the normal grace.
3. The orphan sweep deletes an object that has no chunk row only once both its upload time and the space's latest rewind completion are more than `chunk_grace_days` in the past. The Account DO keeps the latest completion time per space, and every completion copies it into `meta`, so a later rewind cannot lose it.
4. `chunk_grace_days` cannot be set below 30.
5. Before it writes a marker (§3 step 2), the Space DO waits for any GC run in progress to end. It then deletes the object of every chunk row in state `deleting` (orphan-sweep rows included), awaits each R2 response, and removes those rows. So every R2 delete the space issued before admission has completed before any restored row can become `pending` and uploadable again. [R2 lets overlapping writes to one key finish in any order](https://developers.cloudflare.com/r2/reference/consistency/), so a delete still outstanding could otherwise remove a fresh upload. A `deleting` row at admission had a refcount of 0 for at least `chunk_grace_days`, so no admissible target references it, and finishing its delete keeps the invariant. This drain assumes that R2 does not apply a request after its sender has received the response, or after the sending instance has ended and the object has restarted. The R2 documentation does not state this (unverified).

Any admissible target T satisfies T ≥ now − 29 days, and a chunk referenced at T was referenced until some time no earlier than T. By the invariant it is kept until at least T + 30 days, which is after now. The bound holds across repeated rewinds, including a target in an interval that an earlier rewind discarded. It does not depend on how `chunk_grace_days` changed over time, because rule 4 makes 30 days the floor. Rewinds therefore do not narrow the window. The cost is that the chunks only a discarded history used stay in R2 for 30 days after the rewind.

## 6. What a rewind restores, keeps and loses

- **Restored to the restore point:** everything in the Space DO. That includes:
  - the tree, every node's versions and the trash;
  - the commit log, chunk rows, members, the space's name and settings;
  - `device_commits` (RFC 0002 §2).
- **Kept:** the Account DO's state, including devices, revocations, spaces, share links and rewind records. A share link that points to a node absent after the rewind finds nothing.
- **Removed from the current history:** everything committed after `restore_seq`. That includes:
  - every intermediate version written after the restore point, not only the latest one;
  - nodes trashed after the restore point, and their trash state;
  - changes that no device downloaded before the rewind.

  For a while, PITR and the earlier history's export files still hold their metadata, and §5 keeps their chunks for 30 days. Within the window, a later rewind to a point before this one can bring them back. Nothing guarantees that, and once the window and the grace have passed they are gone.
- **What devices keep.** Devices keep only what is on their disks, through recovery (RFC 0002 §6):
  - A local file the device changed is held for the user's answer.
  - A local file the device did not change follows the restored server: the newer local copy moves to the device's local trash, which keeps it only as long as that trash's own retention allows.
  - No device is guaranteed to have a copy of a version it never had on disk, such as an intermediate version B in A → B → C after the restore point. Such a version exists only where the previous bullet allows, until it expires.
- **Undo.** There is no undo operation, and this RFC does not guarantee one. A later rewind to a point before this one began restores the state at that point like any other admissible target (§2 and §5), as yet another history. It does not reverse what devices did during their recovery.

## 7. R2 metadata export (§11)

Each history keeps its own export files, and a new history starts from a full baseline:

- Log segments are written under `meta/<history_id>/log/`, and snapshots under `meta/<history_id>/snap/`. The files of earlier histories are never changed.
- `space.json` gains `history_id`, the history that an R2 restore reads. A restore from R2 reads only that history's files.
- Completion resets the export state for the new history:
  - the exported log starts after `restore_seq`;
  - a full snapshot is due at once;
  - no snapshot of the new history exists yet.
- The exporter writes the new history's first snapshot and the log segments from that snapshot's `start_seq + 1` through its `end_seq`. Only once all of them are durable does it rewrite `space.json` to name the new history. Until then, `space.json` still names the previous history, and an R2-only restore yields the state before the rewind.

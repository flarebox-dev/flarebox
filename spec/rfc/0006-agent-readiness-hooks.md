# RFC 0006: Agent-readiness hooks

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-279.

Flarebox 0.1.0 does not support AI agents or other automated clients. This RFC adds the hooks that let agent support come later as an additive change: without a format bump, without a migration that rewrites existing rows, and without old clients misbehaving against a newer server. On the server, every hook keeps what 0.1.0 clients see unchanged, except the device routes of section 7 and the refusals of section 8. Section 1 is a rule for clients, which 0.1.0 clients follow. A bare section number such as section 3 refers to this RFC; sections of spec v0 are written as spec v0 §N.

The schema changes add fields, messages and one error code:

| Message | Field or value | Section |
|---|---|---|
| `Error.Code` | `CODE_CONFIRMATION_REQUIRED = 16` | 10 |
| `Commit` | `run_id = 8` (reserved, unused in 0.1.0) | 4 |
| `CommitRecord` | `run_id = 7` (reserved, unused in 0.1.0) | 4 |
| `DeviceCredentials` | `credential_expires_at_ms = 5` | 7 |
| `DeviceInfo` (new, `auth.proto`) | `device_id = 1`, `name = 2`, `platform = 3`, `kind = 4`, `created_at_ms = 5`, `last_seen_ms = 6`, `revoked_at_ms = 7`, `credential_expires_at_ms = 8`, `issued_by = 9`, `current = 10` | 7 |
| `DeviceInfo.Kind` (new) | `KIND_UNSPECIFIED = 0`, `KIND_DEVICE = 1`, `KIND_AGENT = 2` | 2, 7 |
| `DeviceList` (new, `auth.proto`) | `devices = 1`, `next_cursor = 2` | 7 |

Not covered here: commits over HTTP alone, which need `history_id` and `device_seq` outside the WebSocket `Welcome`; agent credentials, scopes and their policy; scoped change feeds; per-principal quotas; the server-side mass-delete thresholds. Each comes later in its own RFC, on top of these hooks.

## 1. How clients handle refusals (spec v0 §6.3, §8, §13; RFC 0002 §2, §3)

This section says what a client does with each `Error.Code` it gets for a request, and adds a new outcome, the policy halt, so that a server can refuse a request in a new way without old clients retrying it forever.

**Policy halt.** A policy halt means the client keeps the refused request and does not send it again by itself. For a commit:

1. The client keeps the commit in its store with its `commit_id` and slot, unchanged, and records that it was refused and with which code. It does not drop the commit, and it does not re-plan its changes into a new commit.
2. It sends nothing to that space: not the refused commit, and no new commit, because a device has at most one commit in flight (spec v0 §8). Downloads continue.
3. It reports the refusal to the user, with the code's name or number and the error's `message`.
4. The halt survives a reconnect and a restart. A `Welcome`, whatever it says, does not lift it. This overrides the automatic resend of RFC 0002 §2: a commit under a policy halt is not resent when the device connects, even when `Welcome.device_seq` would allow it.
5. Only a user action, or a client update that knows what the code asks for, lifts the halt. This RFC does not define those actions. A later RFC that makes a server send a policy refusal also defines how its halt is lifted.

A policy halt is not the read-only state of spec v0 §9.2. Read-only follows from the device's format or role, and a `Welcome` that allows writes ends it. A policy halt follows from a refusal of one commit, and only the user ends it.

**Codes for a commit.** For each code of `Error.Code`, a client does this with a refused `Commit`:

| Code | What the client does |
|---|---|
| `UNAUTHENTICATED` | Gets a new access token and sends the same commit again. If `POST /v1/token` refuses, the client is signed out (section 7) and keeps the commit |
| `FORBIDDEN` | Policy halt |
| `INVALID_ARGUMENT` | Policy halt. A client that follows the spec does not get this code for a commit, so it signals a bug or a reused `commit_id` (section 8), and the commit is kept as evidence |
| `MISSING_CHUNKS` | Uploads the listed chunks and sends the same commit again, with new receipts (spec v0 §8 step 4) |
| `LIMIT_EXCEEDED` | Forgets the commit and plans its changes again in smaller commits |
| `QUOTA_EXCEEDED` | With `retry_after_ms` greater than 0, waits that long and sends the same commit again. With 0, policy halt |
| `READ_ONLY` | Forgets the commit and stops committing until a `Welcome` allows writes (spec v0 §9.2) |
| `RETRY_LATER` | Waits `retry_after_ms`, or a backoff when it is 0, and sends the same commit again |
| `UNSUPPORTED_VERSION` | Forgets the commit: nothing was applied, its slot stays free, and its changes are planned again with a new `commit_id` |
| `INTERNAL` | Sends the same commit again after a backoff, because its outcome is unknown |
| `HISTORY_MISMATCH`, `STALE_DEVICE_STATE` | Keeps the commit as evidence and enters recovery (RFC 0002 §6) |
| `HEAD_MOVED` | Forgets the commit (RFC 0002 §4) |
| `AHEAD_OF_SERVER` | Forgets the commit and bootstraps again (RFC 0002 §3) |
| `NOT_FOUND` | Not sent for a commit. If it is, policy halt |
| `CONFIRMATION_REQUIRED` | Policy halt (section 10) |
| `UNSPECIFIED`, or any code the client does not know | Policy halt |

"Sends the same commit again" means the bytes of RFC 0002 §2: the same `commit_id`, `history_id`, `device_seq`, `base_seq`, `require_head` and ops. Only the receipts may be new.

**Other requests.** For a request that is not a commit, `RETRY_LATER` is retried as before: after `retry_after_ms` when it is greater than 0, otherwise after a backoff. `QUOTA_EXCEEDED` is retried only when `retry_after_ms` is greater than 0, after that wait. `UNAUTHENTICATED` is retried after a new access token, and `INTERNAL` after a backoff. Any other code, known or not, ends that request: the client reports it and does not send the same request again by itself.

The platform daily limit of spec v0 §13 always sets `retry_after_ms`, so it is retried as before. A later per-principal quota with no reset time is a policy halt.

## 2. Principals (spec v0 §2, §4.2, §15.2)

A row of the Account DO's `devices` table is a principal: one credential that the server can authenticate, list and revoke. A person's computer is one kind of principal. A later RFC adds others. The table gains four columns:

| Column | Type | Meaning |
|---|---|---|
| `kind` | TEXT NOT NULL, default `'device'` | `device`, or `agent` for the credentials of a later RFC |
| `scope` | TEXT, NULL | NULL means full scope: everything the user's role allows. A later RFC defines non-NULL values |
| `expires_at_ms` | INTEGER, NULL | When the refresh credential stops working. NULL means never |
| `issued_by` | TEXT, NULL | The `device_id` of the principal whose request created this one (section 6). NULL for the device created by claim or by owner recovery |

The migration adds the columns with these defaults, so every existing row becomes a full-scope device that never expires.

0.1.0 issues only `kind = 'device'` with full scope and no expiry. A server that finds a `kind` or a non-NULL `scope` it does not know denies that principal everything. It fails closed, so a credential written by a newer server is never treated as having full access by an older one. The server applies this wherever it reads the principal: when it refreshes a token, accepts a request or a WebSocket, takes a `Reauth`, and starts or completes a pairing. A token's claims never override the principal's current row.

The access token (spec v0 §15.2) carries the principal's kind, scope and expiry as claims, beside the user, device and expiry it carries today. The token stays opaque to clients, and only the server reads it, so adding claims changes no message. A token that lacks the new claims means a full-scope device. An access token never outlives its principal: its expiry is the earlier of 15 minutes after issue and `expires_at_ms`.

## 3. Authorization points (spec v0 §6.3, §15.1)

The server decides what a principal may do at two points, and only there:

- **`authorize(principal, op, effect)`** runs in the Space DO's commit transaction, once for each op of a commit, in op order, after the rules of spec v0 §6 have applied that op. `effect` describes the transition the op actually made, taken from the tree just before and just after that op, not from the commit's change records:
  - the op's effective node IDs, after R2's re-keying (`MERGED_INTO`) and including any node the op created, such as a conflict copy (R1, R3);
  - every node whose parent, name, status or content the op changes, with its ancestry from the root both before and after the op. This includes the side effects of R7 (the subtree a delete sends to the trash or leaves in place), R8 (revived ancestors and descendants) and R9 (a restored trash group);
  - for an op that moves, trashes or restores a folder, every node in its subtree, because their ancestry changes even though they get no change record.

  Change records hold only post-images, so they cannot tell a move inside an allowed folder from a move into it from outside. That is why `effect` is built from the tree itself. A refusal of any op refuses the whole commit with `FORBIDDEN`: the transaction is rolled back, nothing is stored, and the device's slot stays free (RFC 0002 §2).
- **`can(principal, capability)`** runs before an action that is not a commit. The capabilities are:

| Capability | Action |
|---|---|
| `space.create` | `POST /v1/spaces` |
| `device.pair` | `POST /v1/pair/start` (section 6) |
| `device.manage` | The device routes of section 7 |
| `share.manage` | The owner API of RFC 0005 §2 |
| `space.rewind` | `POST /v1/spaces/{space_id}/rewind` (RFC 0003) |

A refused capability is `FORBIDDEN`.

Both points allow everything for a full-scope principal, so 0.1.0 decides exactly as today. They are checked in addition to the role checks that exist: a viewer still cannot commit, and only the owner manages links or rewinds. A later RFC that defines scopes changes what the two points decide, not where they are called.

## 4. Attribution fields (spec v0 §2, §5.2, §6, §6.1)

`device_id` means the principal ID: the credential that made the request. This is its meaning wherever it appears: `CommitRecord.device_id`, `VersionInfo.device_id`, `DeviceCredentials.device_id`, `TokenRequest.device_id`, the `content_dev` and `meta_dev` columns of the unseen-change rule (spec v0 §6), and the device name in conflict names (spec v0 §6.1). No field is renamed. A rename would break proto3 JSON, which the server accepts (spec v0 §15.1) and the breaking-change check guards (spec v0 §14).

`Commit.run_id` (8) and `CommitRecord.run_id` (7) are reserved for a later RFC that labels the commits of one automated run. In 0.1.0 a client leaves `run_id` empty, and the server ignores it on `Commit` and never sets it on `CommitRecord`.

## 5. New change records behind a feature (spec v0 §9.1)

Spec v0 §9.1 allows a new change-record kind only with a raised `format_version`. This RFC adds a second way: a new kind may be introduced behind a `Hello` feature.

1. The RFC that adds the kind names a feature for it.
2. The server sends records of that kind only on connections whose `Hello.features` list that feature. This covers `Changes` pushes, the replay after `Resume`, and the `CommitResult` of the connection that committed. Requests over HTTP announce no features, so their results never contain such records.
3. On every other connection the server leaves those records out of each commit. A commit keeps its `seq` even when nothing in it remains: the client gets a `CommitRecord` with no changes, so the seqs it applies stay contiguous (RFC 0002 §5).
4. The kind must be one that a client without the feature can do without: leaving its records out must leave that client's tree correct. Records that every client needs to keep its tree correct still require a format bump.
5. The RFC that adds the kind states whether its records are stored in the log and exported, and how a reader of the R2 export (RFC 0004) treats them. A kind that an R2-only restore needs raises the export format version (spec v0 §9.3).

A client that receives a record of a kind it does not know still stops applying and asks for an update, as spec v0 §9.1 requires.

## 6. Writers and pairing (spec v0 §6, §8, §15.2)

**One credential per writer.** Every process or store that commits uses its own principal. Two processes that commit must never share a `device_id`: the unseen-change rule never counts a principal's own earlier change as unseen (spec v0 §6), so two writers with one credential silently overwrite each other's changes, and the strict `device_seq` of RFC 0002 §2 refuses one of them. A client that runs as several processes, such as an app and a background daemon that both write, pairs each writer separately. A credential copied to another store, without the store it belongs to, puts that client into recovery (RFC 0002 §2 and §6).

**Pairing is a capability, and it records its issuer.** `POST /v1/pair/start` requires `can(principal, "device.pair")`. The pairing code records the principal that started it, and `pair/complete` writes that principal's `device_id` into the new row's `issued_by`. Revoking a principal does not revoke the principals it issued in 0.1.0. `issued_by` lets a later RFC revoke a whole chain.

## 7. Device list, revocation and credential expiry (spec v0 §7.1, §15)

Two routes, both authenticated with an access token like the other endpoints of spec v0 §15.1, and both requiring `can(principal, "device.manage")`:

| Method and path | Body | Reply |
|---|---|---|
| `GET /v1/devices?cursor=` | none | `DeviceList`, 200 |
| `DELETE /v1/devices/{device_id}` | none | empty, 204 |

**List.** `GET /v1/devices` returns the principals of the caller's user, revoked ones included, in the order the server stored them. A page holds at most 100. `next_cursor` is empty on the last page, and the client passes it back as `cursor` to get the next. The cursor is opaque, and the order comes from a sequence number the server assigns when it stores a principal, not from `device_id`, so a principal stored after a listing passed newer ones appears on a later page of that listing (as for spaces, RFC 0007 section 2). A malformed cursor is refused with `INVALID_ARGUMENT`, and so is a cursor from a listing that a server version before this order served, so a client that gets `INVALID_ARGUMENT` for a cursor starts the listing again. `current` is true for the caller's own principal. `credential_expires_at_ms` is 0 for a credential that never expires.

**Revoke.** `DELETE /v1/devices/{device_id}` revokes a principal of the caller's user. It sets `revoked_at`, and every Space DO closes that principal's WebSockets with 4003 (spec v0 §7.1). A principal may not revoke itself: the request is refused with `FORBIDDEN` and changes nothing (RFC 0009 §5). Revoking a principal that is already revoked succeeds and changes nothing. A `device_id` that is unknown, or that belongs to another user, is `NOT_FOUND`.

**Credential expiry.** A refresh credential stops working when its principal is revoked or its `expires_at_ms` passes. `POST /v1/token` then answers `UNAUTHENTICATED`, the same answer as for an unknown `device_id` or a wrong credential. A client that gets `UNAUTHENTICATED` from `POST /v1/token` stops syncing, tells the user that this computer has been signed out, and waits to be paired again. It does not retry the request. `DeviceCredentials.credential_expires_at_ms` tells a newly paired client when its credential expires, and it is 0 for a credential that never expires. A revoked or expired principal is refused at once. An access token it still holds may carry a valid signature until the token expires, but the server checks the principal's current row on every request and every WebSocket message, so the signature alone authorizes nothing. Its WebSockets close with 4003.

## 8. Stored results belong to their principal and their request (spec v0 §8 step 4, RFC 0002 §3)

The Space DO stores a digest of each commit request beside its result, in `commits` and in `device_commits`. The digest is internal to the server and never sent.

**Digest v1.** The digest is the SHA-256 of a fixed projection of the request, encoded as proto3 binary. Its coverage is frozen for format v1: later schema changes do not change it, so every digest a server stores stays comparable for as long as the row exists. The projection keeps exactly these fields and drops every other field, known or unknown, at every level:

| Message | Fields kept |
|---|---|
| `Commit` | `base_seq = 2`, `ops = 3`, `device_seq = 6`, `require_head = 7` |
| `Op` | the oneof `op`, fields 1 to 6 |
| `CreateDir` | 1, 2, 3 |
| `CreateFile` | 1, 2, 3, 4 |
| `UpdateContent` | 1, 2 |
| `Move` | 1, 2, 3 |
| `Delete` | 1 |
| `Restore` | 1, 2, 3, 4 |
| `Content` | 1 to 7 |
| `ChunkRef` | 1, 2, 3 |

The encoding writes fields in field-number order. A scalar field that holds its proto3 default is omitted. A message field, and the set member of a oneof, is written when it is present, even when it is empty. Repeated fields keep the request's order: `ops` and `chunks` are never sorted. The projection has no maps, floating-point fields, optional scalars or packed repeated scalars, so it needs no rule for them.

A later field added inside these messages is not covered, so a reuse that differs only in that field is not detected. A later RFC that needs more coverage defines digest v2 and stores the version with each digest. A server then keeps computing v1 for rows stored with v1, so it never loses the check for an older row.

What the digest covers:

- **`base_seq` is included.** It changes what the commit means: R3 and R7 use it to decide which changes the commit has not seen (spec v0 §6), and with `require_head` it is the head the commit expects (RFC 0002 §4). A correct retry never changes it. RFC 0002 §2 resends a commit unchanged, with the same `commit_id` and slot, and a commit whose changes are planned again gets a new `commit_id`. The 0.1.0 engine sets `base_seq` once, when it builds the commit, and resends that stored commit as it is. After `MISSING_CHUNKS` (spec v0 §8 step 4) the client sends the same commit again with new receipts. That retry finds no stored result, because a refused commit stores nothing.
- **`receipts` are left out.** They prove that chunks were uploaded and may be renewed between attempts, so they do not change what the commit asks for.
- **`commit_id` is the key**, and `history_id` is checked by the history fence before any lookup (RFC 0002 §3 step 1). The principal is checked against the row's `device_id` (step 1 below). `run_id` is left out while it is reserved (section 4).

RFC 0002 §3 step 2 becomes: if a result is stored for the `commit_id`, in `commits` or in `device_commits`:

1. If the row's `device_id` is not the caller's principal, refuse with `INVALID_ARGUMENT`. A stored result is returned only to the principal that made the commit. Every row has a `device_id`, rows stored before this RFC included, so this check needs no digest. A commit the server made itself, such as a trash purge, has an empty `device_id` and is never returned.
2. If the row has a digest and it differs from this request's digest, refuse with `INVALID_ARGUMENT`.
3. Otherwise return the stored result. A row stored before this RFC has no digest. Its result is returned to its own principal without the body check, because its original body cannot be known.

Each refusal changes nothing. A client that follows RFC 0002 §2 never gets either refusal: it resends a commit unchanged, and it never reuses a `commit_id`, its own or another principal's. A client that does get one has a bug or a store copied between principals (section 6). It cannot tell this `INVALID_ARGUMENT` from a malformed request, and it does not need to, because `INVALID_ARGUMENT` on a commit puts the commit under a policy halt (section 1). The commit stays in the store, unchanged, as the evidence that recovery needs (RFC 0002 §6), and nothing is planned or sent until the user or a client update acts.

## 9. Log retention and the commit_id window (spec v0 §4.1, §7.3, §12)

The Space DO keeps every commit in `commits` for at least the longer of `version_retention_days` and `trash_retention_days` after its `committed_at_ms`, and never less than 30 days. `log_floor_seq` advances only past commits older than that. A `Resume` from any seq inside the window is therefore always served from the log.

The same window is the `commit_id` deduplication window: a retry within it gets the stored result back, or the refusal of section 8. After the window, a retry of the device's latest commit still finds its result in `device_commits` (RFC 0002 §2). Any older retry is refused by the counter check with `STALE_DEVICE_STATE` (RFC 0002 §3 step 3), so no commit is applied twice.

The server truncates no log in 0.1.0, so this costs nothing now.

## 10. Server-side delete count (spec v0 §8)

In the commit transaction, after the ops are applied, the Space DO counts the live files that the commit sends to the trash. It passes the count, the principal and the space to one policy point. The policy either allows the commit or refuses it with `CONFIRMATION_REQUIRED`. A refused commit stores nothing, and its slot stays free (RFC 0002 §2).

In 0.1.0 the policy allows every commit, so the count changes no outcome. A later RFC defines thresholds and how the owner confirms. Until then no server sends `CONFIRMATION_REQUIRED`. Over HTTP it maps to 409. A client that gets it puts the commit under a policy halt (section 1) and reports that the deletion waits for the owner's confirmation. The later RFC defines how the confirmation lifts the halt.

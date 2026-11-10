# RFC 0006: Agent-readiness hooks

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-279.

Flarebox 0.1.0 does not support AI agents or other automated clients. This RFC adds the hooks that let agent support come later as an additive change: without a format bump, without a migration that rewrites existing rows, and without old clients misbehaving against a newer server. Every hook keeps the behaviour of 0.1.0 clients unchanged, except the device routes of section 7 and the refusal of section 8. A bare section number such as section 3 refers to this RFC; sections of spec v0 are written as spec v0 §N.

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

## 1. Error codes a client does not know (spec v0 §6.3, §13)

A client treats an `Error.Code` it does not know as a refusal. `CODE_UNSPECIFIED` counts as unknown. A refusal means:

1. The client halts the operation that got the error. It does not resend the same request, either at once or after a backoff.
2. The client reports the refusal to the user, with the code's number and the error's `message`.
3. The client keeps its local state. A commit that was refused stays pending in the client's store and is not dropped. It waits for the user, or for a client update that knows the code.

A server may therefore add a new code for a new kind of refusal, and a client released before that code exists stops instead of retrying forever.

`QUOTA_EXCEEDED` is retried only when `retry_after_ms` is greater than 0, and only after that wait. With `retry_after_ms` 0, it is a refusal as above. The platform daily limit of spec v0 §13 always sets `retry_after_ms`, so it is retried as before. A later per-principal quota with no reset time is a refusal.

`RETRY_LATER` keeps its current behaviour. The rule of this section changes the handling of no other known code.

## 2. Principals (spec v0 §2, §4.2, §15.2)

A row of the Account DO's `devices` table is a principal: one credential that the server can authenticate, list and revoke. A person's computer is one kind of principal. A later RFC adds others. The table gains four columns:

| Column | Type | Meaning |
|---|---|---|
| `kind` | TEXT NOT NULL, default `'device'` | `device`, or `agent` for the credentials of a later RFC |
| `scope` | TEXT, NULL | NULL means full scope: everything the user's role allows. A later RFC defines non-NULL values |
| `expires_at_ms` | INTEGER, NULL | When the refresh credential stops working. NULL means never |
| `issued_by` | TEXT, NULL | The `device_id` of the principal whose request created this one (section 6). NULL for the device created by claim or by owner recovery |

The migration adds the columns with these defaults, so every existing row becomes a full-scope device that never expires.

0.1.0 issues only `kind = 'device'` with full scope and no expiry. A server that finds a `kind` or a non-NULL `scope` it does not know denies that principal everything. It fails closed, so a credential written by a newer server is never treated as having full access by an older one.

The access token (spec v0 §15.2) carries the principal's kind, scope and expiry as claims, beside the user, device and expiry it carries today. The token stays opaque to clients, and only the server reads it, so adding claims changes no message. A token that lacks the new claims means a full-scope device. An access token never outlives its principal: its expiry is the earlier of 15 minutes after issue and `expires_at_ms`.

## 3. Authorization points (spec v0 §6.3, §15.1)

The server decides what a principal may do at two points, and only there:

- **`authorize(principal, op, nodes)`** runs in the Space DO's commit transaction, once for each op of a commit, after the op is applied by the rules of spec v0 §6. `nodes` is every node whose state the op changes, including the side effects of R7 (the subtree a delete sends to the trash), R8 (revived ancestors and descendants) and R9 (a restored trash group): the nodes that the op's change records cover. A refusal refuses the whole commit with `FORBIDDEN`, and nothing is stored.
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

**List.** `GET /v1/devices` returns the principals of the caller's user, revoked ones included, in creation order. A page holds at most 100. `next_cursor` is empty on the last page, and the client passes it back as `cursor` to get the next. `current` is true for the caller's own principal. `credential_expires_at_ms` is 0 for a credential that never expires.

**Revoke.** `DELETE /v1/devices/{device_id}` revokes a principal of the caller's user. It sets `revoked_at`, and every Space DO closes that principal's WebSockets with 4003 (spec v0 §7.1). A principal may revoke itself. Revoking a principal that is already revoked succeeds and changes nothing. A `device_id` that is unknown, or that belongs to another user, is `NOT_FOUND`.

**Credential expiry.** A refresh credential stops working when its principal is revoked or its `expires_at_ms` passes. `POST /v1/token` then answers `UNAUTHENTICATED`, the same answer as for an unknown `device_id` or a wrong credential. A client that gets `UNAUTHENTICATED` from `POST /v1/token` stops syncing, tells the user that this computer has been signed out, and waits to be paired again. It does not retry the request. `DeviceCredentials.credential_expires_at_ms` tells a newly paired client when its credential expires, and it is 0 for a credential that never expires. An access token already issued stays valid until it expires (at most 15 minutes, section 2), but a revoked principal's WebSockets close at once.

## 8. commit_id reused with another body (spec v0 §8 step 4, RFC 0002 §3)

The Space DO stores a digest of each commit request beside its result, in `commits` and in `device_commits`. The digest is the SHA-256 of the committing principal's `device_id` in UTF-8, one zero byte, and the binary encoding of a `Commit` that holds only the request's `ops`, `device_seq` and `require_head`. The encoding writes fields in field-number order. The digest leaves out `commit_id`, which is the key itself, and `base_seq` and `receipts`, which do not change what the commit asks for. The digest is internal to the server and never sent.

RFC 0002 §3 step 2 becomes: if a result is stored for the `commit_id` and its digest differs from the digest of this request, refuse with `INVALID_ARGUMENT` and change nothing. Otherwise return the stored result. A result stored before this RFC has no digest and is returned as before.

The history fence of RFC 0002 §3 step 1 still comes first.

## 9. Log retention and the commit_id window (spec v0 §4.1, §7.3, §12)

The Space DO keeps every commit in `commits` for at least the longer of `version_retention_days` and `trash_retention_days` after its `committed_at_ms`, and never less than 30 days. `log_floor_seq` advances only past commits older than that. A `Resume` from any seq inside the window is therefore always served from the log.

The same window is the `commit_id` deduplication window: a retry within it gets the stored result back, or the refusal of section 8. After the window, a retry of the device's latest commit still finds its result in `device_commits` (RFC 0002 §2). Any older retry is refused by the counter check with `STALE_DEVICE_STATE` (RFC 0002 §3 step 3), so no commit is applied twice.

The server truncates no log in 0.1.0, so this costs nothing now.

## 10. Server-side delete count (spec v0 §8)

In the commit transaction, after the ops are applied, the Space DO counts the live files that the commit sends to the trash. It passes the count, the principal and the space to one policy point. The policy either allows the commit or refuses it with `CONFIRMATION_REQUIRED`. A refused commit stores nothing, and its slot stays free (RFC 0002 §2).

In 0.1.0 the policy allows every commit, so the count changes no outcome. A later RFC defines thresholds and how the owner confirms. Until then no server sends `CONFIRMATION_REQUIRED`. Over HTTP it maps to 409. A client that knows the code reports that the deletion waits for the owner's confirmation. A client that does not know it treats it as a refusal (section 1).

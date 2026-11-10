# RFC 0011: Refresh credential rotation

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §7.

A device's refresh credential (spec v0 §15.2) never changes after pairing. The server keeps its SHA-256 and compares every `POST /v1/token` against that one hash. A copy of the credential therefore works for as long as the device is not revoked: a copy in a backup, a copy that moved to another computer with the user's files, or a copy that a process running as the user read. Two computers can then run as the same device.

This RFC lets a device replace its own credential about once a day. The device generates the new credential, keeps it, and sends it with a token request; the server stores it in place of the old one in the same request. From then on, every copy of the old credential is refused. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

The schema change adds two fields:

| Message | Field | Section |
|---|---|---|
| `TokenRequest` | `next_refresh_credential = 3` | 1 |
| `AccessToken` | `rotated = 3` | 1 |

## 1. Rotating on the server

A client may set `TokenRequest.next_refresh_credential` to a credential it generated the same way the server generates one at pairing: 32 random bytes, base64url-encoded without padding, so 43 characters.

When the request's `refresh_credential` is the principal's current credential and the principal is active, the server replaces the stored hash with the SHA-256 of `next_refresh_credential`, in the same transaction that checks the presented one. It answers with an access token and `rotated` set. From then on, the old credential gets `UNAUTHENTICATED` like any wrong credential. A `next_refresh_credential` that is not 43 base64url characters is refused with `INVALID_ARGUMENT` and changes nothing.

Without `next_refresh_credential`, the server answers as before, with `rotated` unset. The server keeps no other state for rotation: no pending credential, no history of old ones and no deadline. `DeviceCredentials.access` from `POST /v1/claim` and `POST /v1/pair/complete` never sets `rotated`.

The server does not revoke a principal when an old credential comes back. A process of the device itself that still holds the old credential (§2) is the common cause, and revoking would sign out the device that holds the new one. A rotation changes neither the credential's expiry (RFC 0006 §7) nor a revocation (RFC 0009).

## 2. Clients

**The process that rotates.** On one device, only the process that owns the stored credential rotates it, and it uses one credential for all its requests. The Rust clients use the process that holds the profile lock: the daemon, the macOS app, or a CLI command that opens the profile itself when no daemon runs. Its stored credential has two slots: the current credential and, while a rotation is unfinished, the staged one.

1. When its credential is older than the rotation period, 24 hours in 0.1.0, the process generates a new credential and stores it as staged. It waits until the store reports it written before it sends the request. If the write fails, it refreshes without rotating and tries again at its next refresh.
2. It sends the current credential with the staged one in `next_refresh_credential`.
3. If the answer has `rotated` set, it stores the staged credential as current and clears the staged slot.
4. If the answer is an access token without `rotated`, the server does not support this RFC. It clears the staged slot and keeps the current credential.
5. If no answer arrives, it keeps both slots.
6. Whenever the staged slot is set, the process presents the staged credential first, without `next_refresh_credential`. If it is accepted, the server stored it in a request whose answer was lost, so the process makes it current as in step 3. If it is refused with `UNAUTHENTICATED`, the server never stored it, so the process clears the staged slot and presents the current credential.

Each slot is kept until the server has decided about it, so a lost request, a lost answer or a crash at any point leaves the process with a credential the server accepts.

**Processes that only read.** A process that only reads the stored credential, such as a CLI command that calls the server directly while the daemon runs, never sets `next_refresh_credential`. The owning process may rotate while such a process still holds the old credential, and the owning process may not yet have made the new credential current (step 5). So when a process that only reads gets `UNAUTHENTICATED` from `POST /v1/token`, it reads the stored credential again, once. It presents the staged credential if one is stored, then the current one, skipping a credential it has already presented. It treats the device as signed out only if every one of them is refused. Presenting a credential never changes the server's state, so this cannot interfere with the owning process.

## 3. Two copies of one credential

Suppose two computers hold the same credential. The first one to rotate replaces the stored hash, and the other is refused at its next token request, within one access-token lifetime (15 minutes, spec v0 §15.2). It is then signed out (RFC 0006 §7). Its WebSockets close when it can no longer authenticate.

A copy that is only stored, such as one in a backup, stops working when the original next rotates: within one rotation period of the moment the copy was made, if the original runs during that time. A copy that rotates first wins instead, and the original is signed out. That is the expected outcome after the user moves to a new computer and stops using the old one, and it is also what happens when someone runs a stolen copy. Rotation does not tell the two apart. In the second case, another active principal of the user can revoke the shared principal (RFC 0009). If none remains, owner recovery creates one first. Pairing again alone does not revoke the shared principal. Detecting a copy on the computer that holds it, before it refreshes, is up to the client and is not part of this RFC.

Rotating more often does not help a copy beyond rotating first, because the server keeps only one credential per principal.

## 4. Older clients and servers, and encodings

An older client never sets `next_refresh_credential`, so the server never rotates its credential, and the client works as before without the protection of §3.

The server accepts request bodies as binary protobuf and as proto3 JSON (spec v0 §15.1). The two encodings differ for a server without this RFC:

- **Binary.** Such a server decodes the request and ignores field 3, so it answers without `rotated`, and the client keeps its current credential (§2 step 4).
- **JSON.** Such a server refuses a JSON body that names a field it does not know, so the request fails. A client that sends JSON must therefore not set `next_refresh_credential` unless it knows the server supports this RFC.

The Rust clients in this repository send every request body as binary protobuf (`application/x-protobuf`, core/net), `POST /v1/token` included, and no client in this repository sends JSON to `POST /v1/token`. So the server and the clients can be released in either order.

## 5. Changes to RFC 0006 §7

In RFC 0006 §7, "**Credential expiry.**" paragraph:

- "A refresh credential stops working when its principal is revoked or its `expires_at_ms` passes." is replaced by "A refresh credential stops working when its principal is revoked, when its `expires_at_ms` passes, or when the device replaces it with a new one (RFC 0011 §1)."
- "It does not retry the request." is replaced by "It does not retry the request, except that a process that does not rotate the credential first reads the stored credential again once and presents what it holds (RFC 0011 §2)."

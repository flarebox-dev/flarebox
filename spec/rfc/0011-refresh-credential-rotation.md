# RFC 0011: Refresh credential rotation

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §7.

A device's refresh credential (spec v0 §15.2) never changes after pairing. The server keeps its SHA-256 and compares every `POST /v1/token` against that one hash. A copy of the credential therefore works for as long as the device is not revoked: a copy in a Time Machine backup, a copy that Migration Assistant moved to another Mac, or a copy that a process running as the user read. Two computers can then run as the same device.

This RFC makes the server replace the credential about once a day and makes the device confirm each replacement. Once the original device confirms a replacement, every earlier copy stops working. If a copy confirms first, the original stops working instead. Either way, at most one computer keeps running as the device. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

The schema change adds two fields:

| Message | Field | Section |
|---|---|---|
| `TokenRequest` | `rotate = 3` | 1 |
| `AccessToken` | `next_refresh_credential = 3` | 1 |

## 1. Issuing a successor

A client asks for rotation by setting `TokenRequest.rotate`. Only a client that can store a new credential before it uses it sets it (§3).

The server keeps, for each principal, the hash of its current credential, the time it issued that credential, and at most one successor hash. When `POST /v1/token` presents the current credential with `rotate` set, and the current credential is older than the rotation period, the server:

1. Generates a successor the same way as a credential at pairing: 32 random bytes, base64url-encoded.
2. Stores its SHA-256 as the principal's successor. A successor already stored for the principal is replaced.
3. Answers with the access token as before and the successor in `AccessToken.next_refresh_credential`.

The current credential stays valid. Without `rotate`, or before the rotation period has passed, the answer carries no successor and the server stores nothing new. The rotation period is 24 hours in 0.1.0. A credential issued before this RFC counts as due at once.

`DeviceCredentials.access` from `POST /v1/claim` and `POST /v1/pair/complete` never carries a successor.

## 2. Confirming a successor

When `POST /v1/token` presents the principal's successor, the server makes it the current credential, records the time, and forgets both the previous credential and the successor, in one transaction. It answers with an access token and no new successor. From then on, the previous credential gets `UNAUTHENTICATED` like any wrong credential.

A presented credential that is neither the current one nor the successor is refused with `UNAUTHENTICATED`, as today. The server does not revoke the principal when an old credential comes back. A stale process of the device itself (§3) is the common cause, and revoking would sign out the device that holds the confirmed credential.

A revocation (RFC 0009) also forgets the successor. The credential's expiry (RFC 0006 §7) does not change when a successor is confirmed.

## 3. Clients

A client sets `rotate` only where it can keep both credentials until the successor is confirmed:

1. When an answer carries a successor, the client stores it next to the current credential, and waits until the store reports it written. If the write fails, it keeps the current credential and asks again at its next refresh.
2. It then presents the successor at once with `rotate` unset.
3. If that succeeds, it stores the successor as its only credential.
4. If it gets `UNAUTHENTICATED` for the successor, it drops the successor and goes on with the current credential. A later successor replaced it, or another copy confirmed first, in which case the current credential is refused too and the client is signed out (RFC 0006 §7).
5. If it gets no answer, it keeps both. A client that starts with both stored presents the successor first.

On one device, only the process that owns the device's stored credential sets `rotate`, and it uses one credential for all its requests. The Rust clients use the process that holds the profile lock: the daemon, the macOS app, or a CLI command that opens the profile itself when no daemon runs. A process that only reads the stored credential, such as a CLI command that calls the server directly while the daemon runs, never sets `rotate`. The owning process may confirm a successor while such a process still holds the old credential. So when a process that does not rotate gets `UNAUTHENTICATED` from `POST /v1/token`, it reads the stored credential again, once. If the stored credential differs from the one it presented, it presents the stored one. Only if that is refused too does it treat the device as signed out.

## 4. Two copies of one credential

Suppose two computers hold the same current credential. Each may get a successor, and each new successor replaces the last (§1). The first one that presents a successor still stored makes it current (§2). The other then fails with its successor, falls back to the old credential (§3 step 4) and is refused. It is signed out at that point, or when its access token expires (15 minutes, spec v0 §15.2). Its WebSockets close when it can no longer authenticate.

A copy that is only stored, such as one in a backup, stops working once the original confirms its next successor: within one rotation period plus one access-token lifetime of the copy, if the original refreshes in that time. If both computers run, at most one of them still works after that time, and either may be the one. A copy that refreshes while the original is offline wins, and the original is signed out when it comes back. That is the expected outcome after Migration Assistant moves a user to a new Mac. Rotation does not tell the two cases apart. Detecting a copy on the computer that holds it, before it refreshes, is up to the client and is not part of this RFC.

## 5. Older clients and servers

A server without this RFC ignores `rotate`, so a new client never gets a successor and works as before. An older client never sets `rotate`, so the server never rotates its credential, and the client works as before without the protection of §4.

## 6. Changes to RFC 0006 §7

In RFC 0006 §7, "**Credential expiry.**" paragraph:

- "A refresh credential stops working when its principal is revoked or its `expires_at_ms` passes." is replaced by "A refresh credential stops working when its principal is revoked, when its `expires_at_ms` passes, or when its principal confirms a successor (RFC 0011 §2)."
- "It does not retry the request." is replaced by "It does not retry the request, except that a process that does not rotate the credential first reads the stored credential again once (RFC 0011 §3)."

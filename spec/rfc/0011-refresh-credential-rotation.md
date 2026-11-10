# RFC 0011: Refresh credential rotation

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §7.

A device's refresh credential (spec v0 §15.2) never changes after pairing. The server keeps its SHA-256 and compares every `POST /v1/token` against that one hash. A copy of the credential therefore works for as long as the device is not revoked: a copy in a backup, a copy that moved to another computer with the user's files, or a copy that a process running as the user read. Two computers can then run as the same device.

This RFC lets a device replace its own credential about once a day. The device generates the new credential, keeps it, and sends it with a token request that carries a deadline. The server stores it in place of the old one if the request arrives before the deadline. From then on, every copy of the old credential is refused. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

The schema change adds five fields:

| Message | Field | Section |
|---|---|---|
| `TokenRequest` | `rotation = 3` | 1 |
| `TokenRequest` | `next_refresh_credential = 4` | 1 |
| `TokenRequest` | `rotate_before_ms = 5` | 1 |
| `AccessToken` | `server_time_ms = 3` | 1 |
| `AccessToken` | `rotated = 4` | 1 |

## 1. The server

The Account DO handles the token requests of one principal one at a time, so each request sees the effect of every request handled before it.

**Server time.** For each principal, the server keeps a rotation clock: the latest `server_time_ms` it has answered for that principal, 0 at pairing. When a request sets `rotation`, the transaction that checks the presented credential takes T, the later of the server's current time and the rotation clock, in Unix milliseconds, and stores T as the rotation clock. The server sends the answer only after that transaction is durable, and the answer carries T as `server_time_ms`. So the times in one principal's answers never decrease, even when the server's clock moves backward, and the rules below compare deadlines with T, never with the clock itself. They need no assumption about the server's clock. Without `rotation`, the answer carries neither `server_time_ms` nor `rotated`, so it is the same answer as before this RFC, and the request leaves the rotation clock as it is.

**Rotating.** A request that sets `next_refresh_credential` also sets `rotation` and `rotate_before_ms`. `next_refresh_credential` is a credential the client generated the same way the server generates one at pairing: 32 random bytes, base64url-encoded without padding, so 43 characters. A request with a malformed `next_refresh_credential`, or without `rotation` or `rotate_before_ms`, is refused with `INVALID_ARGUMENT` and changes nothing.

When the presented credential is the active principal's current one:

- If T is at most `rotate_before_ms`, the server replaces the stored hash with the SHA-256 of `next_refresh_credential` in the same transaction that checked the presented one, and answers with an access token and `rotated` set. From then on, the old credential gets `UNAUTHENTICATED` like any wrong credential.
- Otherwise it answers with an access token, `rotated` unset, and the stored credential unchanged. A request that arrives after its deadline can never change the credential.

Apart from the rotation clock, the server keeps no state for rotation: no pending credential and no history of old ones. `DeviceCredentials.access` from `POST /v1/claim` and `POST /v1/pair/complete` carries neither new field.

The server does not revoke a principal when an old credential comes back. A process of the device itself that still holds the old credential (§2) is the common cause, and revoking would sign out the device that holds the new one. A rotation changes neither the credential's expiry (RFC 0006 §7) nor a revocation (RFC 0009).

## 2. Clients

**The process that rotates.** On one device, only the process that owns the stored credential rotates it, and it uses one credential for all its requests. The Rust clients use the process that holds the profile lock: the daemon, the macOS app, or a CLI command that opens the profile itself when no daemon runs. Its stored credential has two slots: the current credential, and, while a rotation's outcome is unknown, the staged credential with its deadline. It sets `rotation` on every token request.

1. When its credential is older than the rotation period, 24 hours in 0.1.0, the process refreshes with the current credential and takes the answer's `server_time_ms` S. It generates a new credential and stores it as staged with the deadline D = S + 60 seconds. It waits until the store reports it written. If the write fails, it tries again at its next refresh. An answer without `server_time_ms` comes from a server without this RFC, and the process does not rotate.
2. It sends the current credential with the staged one in `next_refresh_credential` and D in `rotate_before_ms`.
3. If the answer has `rotated` set, it stores the staged credential as current and clears the staged slot.
4. If the answer is an access token without `rotated`, the request arrived after D and the server will never store the staged credential, so the process clears the staged slot.
5. If no answer arrives, it keeps both slots. The outcome is unknown, because the request may still arrive before D.
6. While the staged slot is set, also after a restart, each refresh presents the current credential and proposes the same staged credential again, with the same D. It never generates a new credential while the staged slot is set.
   - Accepted with `rotated` set: as in step 3.
   - Accepted without `rotated`, so with a `server_time_ms` later than D: the staged credential was not stored before that answer, and the server checks every later proposal against a time at least that late (§1), so it can never store it. The process clears the staged slot.
   - No answer: as in step 5.
   - Refused with `UNAUTHENTICATED`: it presents the staged credential. If that is accepted, the delayed request stored it, and the process makes it current as in step 3. If that is refused too, another copy rotated the credential, and the device is signed out (RFC 0006 §7).

A refusal of the staged credential alone never clears it, because its request may still arrive. The process clears it only after the server has answered for a time later than D, or has said that it did not store it. Two delayed orders show the difference:

- The rotation request is slow, and the client proposes the staged credential again before it arrives. If the second proposal is handled first and before D, it stores the staged credential and the client makes it current; the slow request then presents a credential that is no longer stored and changes nothing. If the slow request is handled first and before D, it stores the staged credential, and the client's next refresh is refused for the current credential and accepted for the staged one.
- The rotation request is slow and arrives after D. The server answers it without `rotated`, or nobody reads the answer. The client's next refresh is answered for a time later than D without `rotated`, and the client clears the staged slot.

Both orders still hold if the server's clock moves backward. In the first, the client keeps both slots until an answer settles the outcome, so a clock change cannot make it drop the credential the server stores. In the second, the client clears the staged slot only after an answer with a time T0 later than D. The server sent that answer only after its rotation clock durably held T0. So a delayed request that arrives later, even while the server's clock reads a time before D, is checked against a time of at least T0 and is not stored.

A proposal lost before it reaches the server does not stall the rotation either. While the server's clock is behind the rotation clock, the answers carry the rotation clock, a time at or before D, so the next proposal of the same staged credential is stored. If only the answer was lost, the server already stored the staged credential, and the next refresh recovers it through the refusal of the current credential (step 6).

In every order, the slots the client keeps include the one credential the server accepts.

**Reading the stored credential.** Every process that reads the two slots reads them as one state that the owning process stored at some moment, even while that process is changing them. How a store achieves that is a client matter; the design note describes it for the Rust clients.

**Processes that only read.** A process that only reads the stored credential, such as a CLI command that calls the server directly while the daemon runs, never sets `rotation` or `next_refresh_credential`. The owning process may rotate while such a process still holds the old credential. So when a process that only reads gets `UNAUTHENTICATED` from `POST /v1/token`, it reads the stored credential again, once. It presents the current credential and then the staged one, skipping a credential it has already presented, and treats the device as signed out only if every one of them is refused. Presenting a credential without a proposal never changes the server's state, so this cannot interfere with the owning process.

## 3. Two copies of one credential

Suppose two computers hold the same credential. The first one whose rotation the server stores replaces the stored hash. The other is refused at its next token request, within one access-token lifetime (15 minutes, spec v0 §15.2), and is signed out (RFC 0006 §7). Its WebSockets close when it can no longer authenticate.

A copy that is only stored, such as one in a backup, stops working when the original's next rotation is stored. That happens within one rotation period of the moment the copy was made, under an assumption this RFC does not guarantee: that the original runs during that period, refreshes when it is due, and its rotation reaches the server before its deadline. An original that is off, offline or keeps losing its rotation requests rotates later, and the copy works until then.

A copy that rotates first wins instead, and the original is signed out. That is the expected outcome after the user moves to a new computer and stops using the old one, and it is also what happens when someone runs a stolen copy. Rotation does not tell the two apart. In the second case, another active principal of the user can revoke the shared principal (RFC 0009). If none remains, owner recovery creates one first. Pairing again alone does not revoke the shared principal. Detecting a copy on the computer that holds it, before it refreshes, is up to the client and is not part of this RFC.

Rotating more often does not help a copy beyond rotating first, because the server keeps one credential per principal.

## 4. Older clients and servers, and encodings

An older client never sets the new fields, so the server never rotates its credential, and its answers are the ones it got before. The client works as before without the protection of §3.

The server accepts request bodies as binary protobuf and as proto3 JSON (spec v0 §15.1). The two encodings differ for a server without this RFC:

- **Binary.** Such a server decodes the request and ignores the new fields. Its answer carries no `server_time_ms`, so a client following §2 never proposes a credential to it.
- **JSON.** Such a server refuses a JSON body that names a field it does not know, with `400`. A client that sends JSON sets the new fields only for a server it knows supports this RFC. For example, it can send its first request with `rotation` set and, if that is refused as a malformed body, continue without the new fields.

Answers carry the new fields only for requests that set `rotation`, so a JSON client that never sets it gets exactly the answers it got before, whichever server it talks to.

In this repository:

- The Rust clients send every request body as binary protobuf (`application/x-protobuf`, core/net), `POST /v1/token` included.
- The deployment test client (server/scripts/deploy-test/client.ts) sends `POST /v1/token` as JSON. It never sets the new fields, and it reads answers with unknown fields ignored, so its upgrade run, which starts on a server without this RFC, works unchanged.

So the server and the clients can be released in either order.

## 5. Changes to RFC 0006 §7

In RFC 0006 §7, "**Credential expiry.**" paragraph:

- "A refresh credential stops working when its principal is revoked or its `expires_at_ms` passes." is replaced by "A refresh credential stops working when its principal is revoked, when its `expires_at_ms` passes, or when the device replaces it with a new one (RFC 0011 §1)."
- "It does not retry the request." is replaced by "It does not retry the request, except that a process that does not rotate the credential first reads the stored credential again once and presents what it holds, and the process that rotates presents its staged credential (RFC 0011 §2)."

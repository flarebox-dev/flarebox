# RFC 0014: Session leases

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0009 §2, RFC 0006 §7, and RFC 0003 §3 and §4.

Before the Space DO handles any request or WebSocket message, it asks the Account DO whether the principal is still active, with its kind and scope, and whether a rewind holds the space closed. Before it pushes a commit, it asks again for every live connection. Both calls run while the Space DO takes no other event, so every commit of a space waits for two round trips to the one Account DO of the deployment. That round trip, not the commit's own work, sets how many commits a space applies per second.

This RFC lets the Space DO reuse an answer for a few seconds. A revocation and a rewind still take effect for every request that starts after them. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. Leases

Each answer of the Account DO to a session check carries `lease_ms`. The Space DO may use the answer for every request and message of that principal until `lease_ms` has passed, counted from the moment it sent the check. It then asks again. `lease_ms` is 5 seconds, or the time left until the principal's credential expires (RFC 0006 §7) if that is shorter. So an expired principal is refused at its expiry, as before.

The Space DO keeps leases in memory only. A Space DO that restarts, for any reason, has none and asks the Account DO again. A revocation the space recorded (`revoked_devices`, RFC 0009 §2) comes before any lease.

When a principal has no running lease, the Space DO takes no other event while it waits for the answer, and it stores the lease before the next event runs. For a WebSocket upgrade, the Space DO may wait one turn of its event loop before it sends that check, and every upgrade that arrives meanwhile and needs a check joins it. The check also covers every other principal with an open WebSocket to the space whose lease has ended or is past half, so one wait renews them all. Checked one principal at a time, a space with more connections than round trips fit in one lease would never catch up: every lease would end before its principal's next event, and every event and every reconnection would wait for the Account DO. When an event uses a lease that is past half of its `lease_ms`, the Space DO may renew that lease while it goes on taking events. One check covers every principal whose lease is still running and was used by an event after it passed half. The Space DO has at most one such check in flight, and a check stops counting as in flight once the longest `lease_ms` among its principals has passed since it was sent. A renewed lease also counts from the moment its check was sent. The Space DO stores a renewal's answer only if it answered no admission call (§3) between sending that check and storing the answer. For each principal, it keeps the answer to the check sent last. So an answer to a check sent before an admission call is never stored after that call, and a recorded revocation still comes before every lease.

The Worker still reads the principal's current row before it serves any authenticated route. Leases apply only inside the Space DO.

## 2. Revocation

The Account DO stores the revocation and a notice for every space in one transaction, then delivers each notice. A space that takes a notice records the revocation and closes the principal's sockets with 4003 at once. The Account DO answers 204 when every space has taken its notice. If a space did not take its notice, the Account DO waits until `lease_ms` has passed since it stored the revocation, and then answers 204. Every lease of that principal comes from an answer computed before the revocation, so every lease has ended by then. The notice stays queued and is retried as RFC 0009 §2 describes.

The bound of RFC 0009 §2 is unchanged. No access token, refresh credential or socket of the revoked principal authorizes anything that starts after the 204. Before the 204, a space that has not taken its notice may still serve the principal, commits included, until its lease runs out. A 204 may take up to 5 seconds longer while a space can't be reached.

## 3. Rewind closure

A lease also carries whether the space is closed (RFC 0003 §4). The Space DO's rewind marker can't replace that check. Before preparing writes the marker, the space has none. After the restore, the restored database predates the marker and still holds the old history until completion runs.

So admission closes the space for leases first. The Account DO answers every check of the space as closed from just before it asks the Space DO for the admission data (RFC 0003 §3, step 1). The Space DO drops all of its leases when it answers that call. Any lease the Space DO takes after that call says closed. By §1, an answer to a check sent before that call was either stored before the call, and the call drops it, or arrives after the call, and the Space DO discards it. So every request that starts after admission sees the space closed, as RFC 0003 §4 requires.

Two effects are new:

- A refused admission can leave the space closed to sessions for up to one lease after the refusal.
- After a rewind is completed or failed, a Space DO can go on answering the space as closed for up to one lease.

Both delays are at most 5 seconds. In both, the space is closed when it could be open, never open when it must be closed.

## 4. RFC 0009, RFC 0006 and RFC 0003 hold these changes

RFC 0009 §2, RFC 0006 §7 and RFC 0003 §3 and §4 already carry this RFC's changes to them: a revocation's 204 waits for each space's notice or lease (§2), and rewind admission closes the space for leases first (§3).

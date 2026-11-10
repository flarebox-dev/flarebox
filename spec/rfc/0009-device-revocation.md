# RFC 0009: Device revocation

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §7.

An owner who loses a computer, or has one stolen, removes it from the deployment so that it stops syncing. Adding a device through owner recovery (docs/design/deploy-and-upgrade.md §2) does not remove any device, and `flarebox setup` makes recovery the normal way to add one, so revocation ships in the same release. This RFC defines who may revoke a device, what a revocation stops and how fast, what the revoked device does, and why a device cannot revoke itself. It uses the routes of RFC 0006 §7 and changes no schema. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. Who may revoke

`DELETE /v1/devices/{device_id}` revokes a principal when all of these hold:

1. The caller authenticates with an access token of an active principal (RFC 0006 §2) that has `can(principal, "device.manage")` (RFC 0006 §3).
2. The target belongs to the caller's user. In 0.1.0 a deployment has one user, its owner, so every device belongs to the owner. A `device_id` that is unknown or belongs to another user is `NOT_FOUND`.
3. The target is not the caller's own principal. A request to revoke the caller's own principal is refused with `FORBIDDEN` and changes nothing (§5).

Revoking a principal that is already revoked succeeds with 204 and changes nothing.

An owner who has lost every device first adds a new one with owner recovery (deploy-and-upgrade.md §2), or with `flarebox setup`, which runs the same recovery after a Cloudflare login. The owner then lists the devices from the new one and revokes the lost ones.

## 2. What a revocation stops

The Account DO sets `revoked_at_ms` on the principal's row in one transaction, then answers 204. From that moment:

| Credential or channel | Effect |
|---|---|
| Refresh credential | `POST /v1/token` answers `UNAUTHENTICATED` (401), the same answer as for an unknown device or a wrong credential (RFC 0006 §7). |
| Access tokens | Every request is refused with `UNAUTHENTICATED` (401), although the token's signature stays valid until it expires (at most 15 minutes after issue, spec v0 §15.2). The Worker reads the principal's current row before it serves any authenticated route, and the Space DO reads it again before each HTTP RPC, each WebSocket upgrade and each WebSocket message. |
| Open WebSockets | The next message from the device closes its socket with 4003 (spec v0 §7.1). A socket that sends nothing is closed with 4003 when the space takes the revocation notice, or when the space next pushes a commit, whichever comes first. A notice that cannot be delivered is retried every 30 seconds until the space takes it. |
| Pairing codes the device started | `POST /v1/pair/complete` refuses them with `FORBIDDEN`, as it refuses a wrong code. |

The bound is therefore zero for every request that starts after the 204: no access token, refresh credential or socket of the revoked principal authorizes anything after that. A request the server already accepted before the revocation may still finish. For example, a commit that passed its device check before the revocation was committed may still be applied.

A revocation does not stop or undo these:

- **Chunk tickets.** A download or upload ticket from `GetDownloadTargets` or `PrepareUpload` is signed for one chunk and does not name a device. It works until it expires, at most one hour after it was issued (spec v0 §8 and §10). A ticket covers only a chunk the device could already read or was uploading, and the device cannot get a new ticket.
- **Data already on the device.** Revocation stops access to the server. It cannot delete files the device already downloaded. Flarebox has no remote wipe.
- **Commits.** Commits the device made stay in each space's history and keep its `device_id`.
- **Share links.** Links belong to the owner (RFC 0005), not to the device that created them. The owner lists and revokes them separately.
- **Principals the device paired.** Revoking a principal does not revoke the principals it issued (RFC 0006 §6).

A revocation is permanent. No route clears `revoked_at_ms`. To use the computer again, the owner pairs it again, which creates a new principal with a new `device_id`.

## 3. What the revoked device sees and does

The revoked device learns about the revocation from the first answer it gets after it: `UNAUTHENTICATED` from `POST /v1/token`, `UNAUTHENTICATED` from another request after its token refresh failed, a 401 on a WebSocket upgrade, or a socket closed with 4003. It then:

1. Stops syncing every linked folder. It sends no further commit, upload or download, and it does not retry the refused request (RFC 0006 §1 and §7).
2. Keeps every local file as it is. It deletes, moves and rewrites nothing in a linked folder, and keeps its sync state and any commit it had not sent.
3. Shows each folder as halted, with the reason that the server revoked this device or refused its credentials.

The device syncs again only after it is paired again (§2).

## 4. Device list

`GET /v1/devices` (RFC 0006 §7) is the list an owner revokes from. It returns every principal of the caller's user, revoked ones included, with `current` set on the caller's own principal so that a client can show which device is this one and offer no revoke action for it. Listing needs the same `device.manage` capability as revoking.

## 5. A device cannot revoke itself

RFC 0006 §7 allowed a principal to revoke itself. This RFC removes that permission, for two reasons:

- **The last device.** A caller is always an active principal. If it cannot be its own target, every revocation leaves at least one active principal, the caller. The device list API therefore cannot revoke the last active device, and an owner never loses every device through a revocation. The only way back would be owner recovery, which needs access to the Cloudflare account.
- **Lost devices.** Revocation exists for devices the owner no longer holds, and those cannot send a request. A device the owner still holds can be revoked from another device.

A client that wants to sign this computer out without another device at hand deletes its local credential. That stops the computer from syncing but leaves its principal active on the server until the owner revokes it from another device.

## 6. Changes to RFC 0006 §7

In RFC 0006 §7, "**Revoke.**" paragraph, the sentence "A principal may revoke itself." is replaced by "A principal may not revoke itself: the request is refused with `FORBIDDEN` and changes nothing (RFC 0009 §5)."

## 7. Clients in 0.1.0

The command-line client adds two commands on `flarebox-net`'s `Api::list_devices` and `Api::revoke_device`:

- `flarebox device list [--json]` lists the principals of the device's user, marks this one, and shows each device's state and when it last refreshed a token.
- `flarebox device revoke <device_id>` revokes one by its ID. It takes no names, because two devices may share one.

Not covered here: the macOS app's settings window, which calls the same `Api` methods through `core/ffi`; revoking the principals a revoked principal issued; remote wipe.

# RFC 0009: Device revocation

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §7.

An owner who loses a computer, or has one stolen, removes it from the deployment so that it stops syncing. Adding a device through owner recovery (docs/design/deploy-and-upgrade.md §2) does not remove any device, and `flarebox setup` makes recovery the normal way to add one, so revocation ships in the same release. This RFC defines who may revoke a device, what a revocation stops and how fast, what the revoked device does, and when a device may revoke itself. It uses the routes of RFC 0006 §7 and changes no schema. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. Who may revoke

`DELETE /v1/devices/{device_id}` revokes a principal when all of these hold:

1. The caller authenticates with an access token of an active principal (RFC 0006 §2) that has `can(principal, "device.manage")` (RFC 0006 §3).
2. The target belongs to the caller's user. In 0.1.0 a deployment has one user, its owner, so every device belongs to the owner. A `device_id` that is unknown or belongs to another user is `NOT_FOUND`.
3. If the target is the caller's own principal and no other principal of kind device of its user is active, the request carries `?confirm=last-device`. Without it, the request is refused with `CONFIRMATION_REQUIRED` (409) and changes nothing (§5).

The Account DO checks rule 1 again in the same step that stores the revocation, because another revocation may have revoked the caller after the Worker checked its token. Of two devices that revoke each other at the same time, one succeeds and the other gets `UNAUTHENTICATED` (401). Rule 3 is checked in that step too, so of the last two devices of a user that revoke themselves at the same time without confirming, one succeeds and the other gets `CONFIRMATION_REQUIRED`.

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

`GET /v1/devices` (RFC 0006 §7) is the list an owner revokes from. It returns every principal of the caller's user, revoked ones included, with `current` set on the caller's own principal so that a client can show which device is this one and offer to sign it out instead of a revoke action. Listing needs the same `device.manage` capability as revoking.

## 5. A device revokes itself when it signs out

A device revokes its own principal when the person signs it out, so that a copy of its refresh credential, in a backup or on a discarded disk, stops working too. A credential issued in 0.1.0 never expires, so without this the copy would keep full access until another device revoked it.

**The last device.** If no other principal of kind device of the user is active, a revocation leaves the owner without a device, and the only way back is owner recovery, which needs access to the Cloudflare account. The server therefore revokes the last active device only on a request that carries `?confirm=last-device`, and refuses one without it with `CONFIRMATION_REQUIRED`. A client sends the confirmation only after it has told the person that this is their last device and what getting back in takes. Principals of other kinds do not count, because they cannot pair a device.

**Order and failure.** A client stops syncing, then sends the revocation without the confirmation, and only then unlinks its folders and deletes its credential. The server's answer decides, because only the server sees every device at once: a device list read earlier cannot tell two devices that sign out at the same time that one of them will be the last. A client takes any HTTP 409 as that refusal, whatever its body, because a body can be lost on the way. It then keeps its credential and folders, syncs again, and tells the person that this is their last device and that getting back in needs owner recovery outside the client. It sends the confirmation only after the person accepts. If the server cannot be reached or does not answer in time, the client cannot tell whether this is the last device, so it also keeps everything and tells the person the same risk. If the person accepts, it signs out on the device without the server, and its principal stays active until another device revokes it, which the device list shows. An `UNAUTHENTICATED` answer means another device, or an earlier attempt of this sign-out, already revoked the device, and the sign-out goes on.

## 6. Changes to RFC 0006 §7

In RFC 0006 §7, "**Revoke.**" paragraph, the sentence "A principal may revoke itself." is replaced by "A principal may revoke itself when it signs out. If it is its user's last active device, the request must carry `?confirm=last-device`, or it is refused with `CONFIRMATION_REQUIRED` and changes nothing (RFC 0009 §5)."

## 7. Clients in 0.1.0

The command-line client adds two commands on `flarebox-net`'s `Api::list_devices` and `Api::revoke_device`:

- `flarebox device list [--json]` lists the principals of the device's user, marks this one, and shows each device's state and when it last refreshed a token.
- `flarebox device revoke <device_id>` revokes one by its ID. It takes no names, because two devices may share one. It refuses this device's own ID, because revoking it would leave the credential on disk unusable instead of signing the device out.

Signing out through `core/driver` (`Driver::sign_out`, which the macOS app's Sign Out uses) revokes the device as §5 describes, with `Api::revoke_self`. When the server answers that this is the last active device, the app asks the person a second time, and says that they cannot sign back in from the app, before it confirms.

Not covered here: the macOS app's device list, which calls the same `Api` methods through `core/ffi`; revoking the principals a revoked principal issued; remote wipe.

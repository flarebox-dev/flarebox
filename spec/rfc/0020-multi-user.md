# RFC 0020: Several users per deployment and per space

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §4.1, §5.2 and §7.1, RFC 0003 §1 and §6, RFC 0004 §4, RFC 0005 §2, RFC 0006 §2, §3 and §6, RFC 0009 §1 and §2, RFC 0010 §1 and RFC 0014 §2. Registers the feature `people` (RFC 0022). RFC 0021 adds the browser principal and the ways a person signs in.

A 0.1 deployment has one person in it. This RFC lets several people share a deployment and its spaces. An owner invites people, each space has members with a role, the server refuses what a role does not allow, and removing a person revokes every credential they hold. Download tickets they already hold work until they expire, at most one hour. Every change of who may do what is a commit in the space's log, with who made it.

Proposing is a write level, like direct writing, that a role or a credential's scope grants to people and agents alike. An outside collaborator can be invited to propose only, an agent's credential proposes only unless its user grants it direct write, and the drafts RFC that adds the proposal routes plugs into this model without changing it.

A bare section number such as §3 refers to this RFC. Code references are to `main` at `48a1668`.

## 1. Problem

What 0.1 has:

- **One user.** A deployment has one user, its owner (RFC 0009 §1), stored in the Account DO's `users` table with no role (`server/src/account/schema.ts`). The claim inserts it (`redeemClaim` in `server/src/account/claim.ts`), and owner recovery finds it as the earliest user (`ownerOf`). No route adds a user or a member (`server/src/worker.ts`).
- **Spaces belong to their creator in the Account DO.** `spaces.owner_id` decides the space list (`listSpaces` in `server/src/account/account.ts`), who may rewind (`rewind`) and who manages share links (RFC 0005 §2, `ownsSpace` in `server/src/share/links.ts`).
- **Members exist in the Space DO.** Spec v0 §4.1 defines `members (user_id, role, added_seq)` with the roles `owner`, `editor` and `viewer` (`server/src/space/schema.ts`). `init` inserts the creator as `owner`. The Space DO refuses a non-member, refuses a viewer's commit and upload preparation, tells a viewer's device that it is read-only in `Welcome`, and lets only the owner read usage (`server/src/space/space.ts`).
- **A change record is reserved.** `ChangeRecord.member` is a `MemberUpdated` with no fields (`spec/proto/koidrive/v1/model.proto`). The driver applies it as a no-op (`docs/design/driver.md`).
- **Policy points allow everything.** `policy.can` and `policy.authorize` (`server/src/policy.ts`) accept every known principal, and `knownPrincipal` admits only full-scope devices.
- **Roles are read once per connection.** HTTP admission reads the member's role before it awaits anything, a commit then awaits content and upload checks, and a WebSocket keeps the role it had at upgrade (`sessionOf` in `server/src/space/space.ts`).

## 2. Model

| Record | Where | Fields that matter |
|---|---|---|
| User | Account DO `users` | `user_id`, `name`, `kind` (`person` or `service`), deployment `role` (`owner`, `admin` or `member`), `removed_at_ms` |
| Principal | Account DO `devices` (RFC 0006 §2) | Belongs to exactly one user. `kind` is `device`, `browser` (RFC 0021) or `agent`. `scope` NULL means everything its user may do |
| Member | Space DO `members` | `user_id`, `role` (`owner`, `editor`, `contributor` or `viewer`), `added_seq`, new `updated_seq` and `added_by_user_id` |
| Member grant | Space DO `member_grants` (new) | A membership an invite reserved and has not activated yet (§3.2) |
| Invite | Account DO `invites` (new) | Code hash, kind, inviter, optional space and role, expiry, state |
| Membership index | Account DO `space_members` (new) | Which spaces a user belongs to, for listing only |

The Space DO's `members` table is the authority for roles in a space. The Space DO authorizes each op inside the commit transaction (RFC 0006 §3), and a member change must be ordered with commits in the same log. The Account DO's index lists a user's spaces. It never grants access.

### 2.1 Write levels

A space has three levels of access to its accepted tree, the tree every device syncs:

| Level | May | Capabilities |
|---|---|---|
| Read | Read and sync the tree, its versions, trash and log | `space.read`, `log.read` |
| Propose | Read, plus write into drafts and submit proposals that a decider accepts or rejects (the drafts RFC) | `space.propose` |
| Direct | Propose, plus commit to the accepted tree | `space.write` |

A role grants a highest level. A principal's scope can lower it and never raise it. The level is the same for every kind of principal: a person invited as `contributor` and an agent credential limited to `space.propose` get the same refusal for a commit, from the same check. Deciding on proposals (`proposal.decide`) comes only with direct write, because accepting publishes to the accepted tree. So a propose-only principal can never publish its own proposal by any route.

**Every route that changes the accepted tree, or who may change it, needs direct write.** Besides commits and decisions, `space.rewind` (RFC 0003 replaces the tree) and `member.manage` (it can give any user, the caller's own included, a higher role) do. A scope that lacks `space.write` in a space cannot carry `proposal.decide`, `space.rewind` or `member.manage` there. The Account DO refuses to issue such a scope, and a check that meets one treats those capabilities as absent. `member.manage` never grants a role above the acting principal's own level in that space.

### 2.2 Who may do what

For a capability that belongs to a space:

> allowed(principal, space, capability) = the capability is in the row of the user's current member role in that space, and the principal's scope allows it in that space.

For a capability that belongs to the deployment:

> allowed(principal, capability) = the capability is in the row of the user's deployment role, the user is not removed, and the principal's scope allows it.

A user with no member row in a space has no space capability there. An unknown role, kind, level or scope denies everything, which extends RFC 0006 §2's fail-closed rule.

**No credential exceeds its issuer.** A principal that creates another principal (pairing, the device authorization of RFC 0021, agent credentials later) gives it a scope that allows no more than the issuer's own effective authority, in every space and in the deployment, and an expiry no later than the issuer's. Pairing copies the issuer's scope and expiry. A full-scope device therefore pairs a full-scope device, as today, and a restricted principal can pair only one as restricted as itself. A child keeps its credential after its issuer is revoked (RFC 0006 §6, RFC 0009 §2). That is safe because the child never had more than its issuer had when it was made, and removing the user revokes both (§3.5).

**Capabilities that only a full-scope principal holds.** `user.invite`, `user.manage`, `user.admin`, `space.recover`, `device.manage.any` and `agent.issue` create or reassign credentials, users or ownership. No scope can carry them. Only a principal with NULL scope, whose authority is its user's whole role, uses them. Two of them are deliberate exceptions to the issuer rule. A recovery code (§3.1) creates a full-scope principal for another user, and `space.recover` (§3.5) makes a user an owner. An admin who uses either can act as that user or that owner. The admin role has that power by design (§10, decision 6). The server records the acting principal, and no scoped principal ever has it.

### 2.3 Capability table

Space capabilities:

| Capability | Action | viewer | contributor | editor | owner |
|---|---|---|---|---|---|
| `space.read` | Tree, versions, trash, downloads | yes | yes | yes | yes |
| `log.read` | Sync replay and any later log API | yes | yes | yes | yes |
| `space.propose` | Drafts, their uploads, submitting proposals | | yes | yes | yes |
| `space.write` | Commit, and `PrepareUpload` for a commit | | | yes | yes |
| `proposal.decide` | Accept or reject a proposal | | | yes | yes |
| `share.manage` | RFC 0005 owner API | | | | yes |
| `member.manage` | Add, change and remove members, space invites | | | | yes |
| `space.rewind` | RFC 0003 | | | | yes |
| `space.usage` | RFC 0010 | | | | yes |

Syncing needs `space.read` and `log.read`, because `Resume` replays full records. The read RPCs check `space.read`. Neither makes a policy decision today, so both checks are new call sites.

Deployment capabilities:

| Capability | Action | member | admin | owner |
|---|---|---|---|---|
| `space.create` | `POST /v1/spaces` | yes | yes | yes |
| `device.pair` | Add a principal to one's own user, within §2.2's rule | yes | yes | yes |
| `device.manage` | List and revoke one's own user's principals | yes | yes | yes |
| `device.manage.any` | List and revoke another user's principals | | yes | yes |
| `user.invite` | Invite a new person to the deployment | | yes | yes |
| `user.manage` | List users, remove users who are not admins, issue recovery codes for users other than the owner | | yes | yes |
| `user.admin` | Make, unmake or remove an admin | | | yes |
| `space.recover` | Add an owner to a space that has none (§3.5) | | yes | yes |
| `agent.issue` | Reserved for the agent credential RFC | yes | yes | yes |

`space.create`, `device.pair` and `device.manage` are RFC 0006 §3's capabilities. The table lives in `server/src/policy.ts` as data, and the authorization tests are generated from it (§8).

`contributor` is new. A client of a contributor sees read-only in `Welcome`, which is accurate until the client supports drafts. Clients do not offer `contributor` when they invite or change a role until the drafts RFC ships, because a contributor can do no more than a viewer before that.

## 3. Design

### 3.1 Users and deployment roles

The claimer of a deployment is its owner (`docs/design/deploy-and-upgrade.md` §2). There is one owner, enforced in storage by a partial unique index on `users(role) WHERE role = 'owner'`. The first claim inserts the user with role `owner`. Owner recovery finds the user whose role is `owner` instead of the earliest user.

User names are unique in a deployment, compared after Unicode case folding, because members tell each other's commits apart by name. A name that collides is refused with `ALREADY_EXISTS` when a user is created or renamed. Existing deployments have one user, so no stored name collides.

| Method and path | Capability | Body and reply |
|---|---|---|
| `GET /v1/users?cursor=&limit=` | `user.manage` | `UserList`, ordered by `created_at_ms` then `user_id`, at most 100 per page |
| `PATCH /v1/users/{user_id}` | `user.manage` and `user.admin` | `SetUserRole` with `member` or `admin`; 204. The owner role cannot be set or changed here |
| `DELETE /v1/users/{user_id}` | `user.manage`, and `user.admin` to remove an admin | `UserRemoval` (§3.5) |
| `POST /v1/users/{user_id}/recovery` | `user.manage`, and `user.admin` for an admin. The target cannot be the deployment's owner (`FORBIDDEN`) | `RecoveryCode` |
| `GET /v1/me` | any | `UserInfo` of the caller's user |

`PATCH` needs `user.admin` because its only legal changes make or unmake an admin.

**Recovery for people other than the owner.** A person who loses every principal keeps their user. An admin, from a full-scope principal, asks for a recovery code for that user. No one gets a recovery code for the deployment's owner: the owner recovers with the recovery key (`docs/design/deploy-and-upgrade.md` §2), so an admin cannot gain `user.admin` by recovering the owner. The code is single-use, lasts 24 hours, is stored as a hash with the issuing principal, and is redeemed where a pairing code is (`POST /v1/pair/complete`) or in a browser (RFC 0021). The principal it creates belongs to that user with full scope, so memberships and attribution stay. Its authority is the target user's role, not the admin's. The target user's device list shows the new principal and who issued its code. The code is refused if the user is removed when it is issued or redeemed. The last-device warning (RFC 0009 §1) changes for users other than the owner: "ask an admin for a recovery code".

### 3.2 Invites

An invite is a single-use code. A **user invite** (`user.invite`) creates a new user and that user's first principal, and may also add the user to one space with a role. A **space invite** (`member.manage` in its space) adds an existing user, who redeems it from a full-scope principal. It creates no user and no principal. A scoped principal cannot redeem a space invite, because that would add its user to a space outside its scope. Space owners who are not admins send only space invites (§10, decision 1).

**Create.** `POST /v1/invites` with `CreateInvite`. An invite that names a space needs `member.manage` there and a role no higher than the creator's level in that space (§2.1). The code is 128 random bits, shown once as a link or as 26 base32 characters, and stored only as a hash. An invite lasts at most 7 days. Redemption failures go through the per-client limiter that pairing uses (`wrongCode` in `server/src/account/claim.ts`).

**Two points of effect.** An invite that names a space has an effect in the Account DO and an effect in the Space DO. Each is checked in the transaction that makes it.

- The Account DO effect happens at step 3 below. The invite is consumed, and a user invite creates the user and the principal. Step 3 checks, in the same Account DO transaction that a revocation or a removal runs in, that the invite is still reserved by this secret, has not expired and has not been revoked, that the inviting principal is active and its user not removed, and, for a user invite, that the inviter's user still holds `user.invite`. A space invite needs no deployment capability here. For an invite that names a space, step 3 also checks that the inviting principal's current scope still allows `member.manage` there and the invited role's level, so a scope narrowed after the reservation stops the invite.
- The Space DO effect happens when the grant activates (step 4). The activation transaction reads the inviter's current member row and orders the grant against every other change to the invited user's membership. A grant that fails there is cancelled. An invite that took effect in the Account DO can therefore end with no membership: a user invite leaves the new user with an account and without that space, and the inviter can send a new invite. This fails closed.

**Redeem.** `POST /v1/invites/redeem` with `RedeemInvite`: the code, a `redemption_secret` of 256 random bits, and for a user invite the new user's name and the new principal's name and platform. The client stores the secret durably before it sends the request and keeps it until it has the answer. A user invite is redeemed without authentication. A space invite needs an access token of a full-scope principal. The steps:

1. **Reserve.** In one Account DO transaction: find the invite by code hash; refuse if it is expired, revoked or redeemed, or reserved with another secret; check the Account DO conditions of step 3; record the secret's hash, the redeeming user (preallocated for a user invite, the caller for a space invite) and a `grant_id`. A second redeemer with another secret gets the wrong-code answer. A retry with the same secret continues from the step it reached.
2. **Reserve the grant**, if the invite names a space. `reserveGrant(grant_id, user_id, role, inviter_user_id, inviter_scope)` on the Space DO records a pending row in `member_grants`. A pending grant authorizes nothing and does not count as an owner. The Space DO refuses it with `FORBIDDEN` if the inviter's current role in the space, intersected with the recorded scope, lacks `member.manage` or is below the invited role, or if the grant was already cancelled or its id is at or below the space's retirement cutoff (below). It refuses it with `QUOTA_EXCEEDED` when no seat is free or the space holds 1,000 grant rows (below). A refusal fails the redemption and cancels the invite. If the space cannot be reached, or the Account DO holds calls to it during a rewind (§3.4), the answer is `RETRY_LATER` and the reservation stays. This check refuses early. Step 4 decides.
3. **Commit.** In one Account DO transaction: the checks above, and that the reservation is less than 10 minutes old. For a user invite, create the user and the principal (the fence of §3.5 applies). Mark the invite redeemed and queue `activateGrant(grant_id)` in the outbox.
4. **Activate and answer.** The Account DO calls `activateGrant` once before it answers, and the outbox repeats it until the Space DO acknowledges. In one Space DO transaction:
   - A grant that is not pending is left as it is.
   - If the inviter's current member row, intersected with the scope recorded at reservation, lacks `member.manage` or is below the invited role, the grant is cancelled.
   - If the invited user is already a member, the grant is superseded. An invite never lowers or overwrites a role.
   - Otherwise the Space DO adds the member and appends the member commit (§3.3), and the grant becomes active.

   Every other change to a user's membership in the space, by any route, supersedes that user's pending grants in the same transaction (§3.3). A delayed activation therefore never undoes a later removal or role change. The answer, `RedeemInviteReply`, carries the credentials for a user invite and the grant's state as the table below maps it.

**Crash and loss.** The Account DO's alarm drives every reservation left between steps. A reservation whose step 3 has not run is cancelled when its invite expires or is revoked, or when the reservation is 10 minutes old. In the last case the invite becomes redeemable again, with any secret, so a client that lost its secret does not lock the invite until it expires. Cancelling sends `cancelGrant(grant_id)`, which marks a pending grant cancelled and touches nothing else, so it never removes a membership. A `cancelGrant` for an id the Space DO holds no row for, above its retirement cutoff, records a cancelled row, so a `reserveGrant` delivered after its cancellation is refused. The Account DO sends it only for a reservation that did not reach step 3, so it never meets an active grant. A cancelled grant stays as a tombstone, so a delayed `reserveGrant` cannot bring it back.

A retry after step 3 whose answer was lost presents the same secret and gets the same credentials. For a user invite the first refresh credential is an HMAC, under the deployment's server key, of the principal id and the redemption secret. The server stores only its hash, as for every refresh credential, and computes the same credential again for a retry, so two retries whose answers arrive in either order carry identical credentials. Replay stops when the principal first refreshes or is revoked, or 24 hours after redemption, whichever comes first. The replay runs in a transaction with the fence of §3.5.

**Grant states.**

| Grant state | Meaning | `grant_state` in the reply | A later `activateGrant` | A later `cancelGrant` |
|---|---|---|---|---|
| (none) | A user invite without a space | `none` | | |
| `pending` | Reserved; the invite has not reached step 3, or its activation is not delivered yet | `pending` | Active, superseded or cancelled | Cancelled (sent only before step 3) |
| `active` | Added the member | `active` | No change | Not sent |
| `superseded` | The user was already a member at activation, or another change to the user's membership came first; nothing changed | `superseded` | No change | Not sent |
| `cancelled` | Ended before it took effect, or the inviter lacked the authority at activation | `refused` | No change | No change |

Every state except `pending` is terminal. A terminal grant whose member later left never adds that member again.

**Capacity.** A space holds at most 1,000 members. A pending grant for a user who is not yet a member holds a seat from the moment `reserveGrant` records it, so the count is members plus such pending grants. `reserveGrant` and an ordinary add (`PUT .../members/{user_id}` for a new member) are refused with `QUOTA_EXCEEDED` when no seat is free. A refused reservation fails the redemption before step 3, so the invite never takes effect without a seat. Activation then always fits: it uses the seat its grant holds, and a grant that ends superseded or cancelled frees it.

A space also holds at most 1,000 grant rows in any state. `reserveGrant` is refused with `QUOTA_EXCEEDED` while it holds 1,000, so a pending grant that blocks retirement cannot let terminal rows grow without bound, and the capture of §3.4 stays bounded. Retirement frees rows.

**Retiring grant rows.** The outbox retries until delivery, so no duration bounds when an old message arrives. Grant ids are therefore a sequence the Account DO assigns, increasing across the deployment, and each Space DO keeps a retirement cutoff. When every grant at or below some id N for that space is terminal in the Account DO and every message about them has been acknowledged, the Account DO sends `retireGrants(N)`. The Space DO deletes its terminal rows at or below N and raises its cutoff to N in one transaction. A message naming a grant at or below the cutoff that the Space DO no longer holds is refused, so a retired id never becomes unknown. The cutoff is part of the member state a rewind keeps (§3.4).

**List and revoke.** `GET /v1/invites` lists the invites the caller's user created. With `user.manage` it lists every invite, and with `member.manage` in a space it also lists that space's invites. `DELETE /v1/invites/{invite_id}` is allowed to the same callers. Revoking a reserved invite cancels its grant as above.

### 3.3 Members

| Method and path | Capability | Body and reply |
|---|---|---|
| `GET /v1/spaces/{space_id}/members?cursor=&limit=` | `space.read` | `MemberList`, ordered by `added_seq`, at most 100 per page |
| `PUT /v1/spaces/{space_id}/members/{user_id}` | `member.manage` | `SetMember`; adds an existing user or changes a role |
| `DELETE /v1/spaces/{space_id}/members/{user_id}` | `member.manage`, or the member's own full-scope principal leaving | 204 |

`MemberInfo` takes ids, roles and sequence numbers from the Space DO. The Worker adds each user's name and kind from the Account DO for display only. No check reads them.

**A change is a commit.** The Space DO applies each member change in one transaction that updates `members`, raises the space's `member_version`, and appends a server commit with an empty `device_id`, as for a trash purge (RFC 0006 §8). The same transaction supersedes the user's pending grants in the space (§3.2). Its one change record is a `MemberUpdated` that names the member, the new role (empty for a removal), the acting user, principal and principal kind, the invite or rewind that caused it, and the `member_version`. The log therefore answers who let a person in, when, with which role, and whether a person or an agent did it.

**At least one owner.** A change that would leave the space without an owner is refused with `INVALID_ARGUMENT`. The Space DO is a single writer, so of two owners who demote each other at the same time, the second is refused. The only exception is the removal of a removed user (§3.5).

**Checks at the point of effect.** The server reads a role where the effect happens, not only when a request arrives:

- The commit transaction reads the caller's current member row and passes that role to `policy.authorize`, so a commit that starts after a change sees the new role, whatever it awaited before.
- `PrepareUpload` checks `space.write` (or `space.propose` for a draft upload) after its last await, before it issues targets. This replaces the separate viewer guard.
- Read RPCs and download targets check `space.read` after their last await, before they answer.
- `Resume` replay and each live push check the socket's user's current row before they send.
- Share-link owner actions. Links live in the Account DO, so the Space DO cannot check a role in the transaction that writes one. Instead the Account DO does the node and password work first. As the last step it calls the Space DO's `authorizeShare(operation, link_id, principal, scope)` with the authenticated principal and its current scope. In one Space DO transaction that call checks `share.manage` for that principal (its user's current role intersected with its scope) and records a single-use grant bound to the space, the operation (create, update or revoke), the link, the principal and the scope it checked. The grant expires 30 seconds later. The Account DO performs the change only with an unexpired grant that matches, and records the grant id as consumed in the same transaction, so a repeated delivery has no second effect. Listing links is a read: the Account DO asks the Space DO whether the principal holds `share.manage` before it answers. A demotion after the Space DO's transaction is like a demotion after a commit's transaction started, and the grant's 30 seconds bound the gap.

These are reads of one indexed row in the Space DO's own SQLite.

**Live sessions.** After a member's role changes or the member is removed, the Space DO closes that user's sockets with a new close code 4005, "access changed". A 0.1 client reconnects after a close code it does not know (`core/net/src/session.rs`). If the user was removed, the upgrade is refused with 403, and the client stops that folder with `Forbidden`. If the user became a viewer or contributor, the `Welcome` says read-only. Chunk tickets already issued keep working until they expire, at most one hour (RFC 0009 §2).

**The space list.** The Account DO keeps `space_members (user_id, space_id, role, member_version, list_seq)` and lists a user's spaces from it instead of `spaces.owner_id`. The Space DO sends each change through a new outbox table `member_notices`, delivered by its alarm until taken, the way revocation notices go the other way today (`deliverNotices` in `server/src/account/account.ts`). The Account DO applies a notice only if its `member_version` is higher than the row's. A removal keeps the row with a NULL role, so a late older notice cannot bring the membership back. `member_version` never goes back, across rewinds too (§3.4). A row gets a new `list_seq`, above every existing one, each time it becomes visible: at its first add and when a removed member is added again. Pages follow `list_seq` as today, so a late addition is not skipped (RFC 0007). Rows migrated from `spaces` keep its `list_seq`, so existing cursors stay valid. The Account DO answers a notice that adds a removed user with a removal notice (§3.5).

**Checks that move to the Space DO.** Rewind admission and the share-link owner API check `spaces.owner_id` today. They check the caller's role in the Space DO instead. Rewind admission already calls the Space DO (`rewindInfo`), which answers the role in the same call. `spaces.owner_id` stays as the space's creator. Space names stay unique per creator, and a client shows the creator's name beside a space name, because two members can each share a space called "Documents".

### 3.4 A rewind keeps the current members

A rewind undoes changes to files. It does not undo who may use the space. Removing someone and then rewinding must not let them back in, and a rewind must not restore an earlier owner over the current one. So the Space DO keeps membership across a rewind, as it keeps revocations (`revoked_devices`).

1. The Space DO's `rewindPrepare` returns the current `members` rows, every `member_grants` row (terminal ones included, so retries stay idempotent after the rewind), the retirement cutoff and `member_version`. A space has at most 1,000 members, counting the seats pending grants hold, and at most 1,000 grant rows, so the answer stays below about 200 KB. The Account DO stores it as rows of a `rewind_members` table, not as one value. Prepare can be retried. The space is closed from admission (RFC 0003 §4, RFC 0014 §3), so every retry captures the same state, and the Account DO keeps the first capture of each attempt.

   The restore erases every Space DO state that could mark the rewind, the rewind marker included (RFC 0003 §3), so the gate is the Account DO's rewind record, which no restore reaches. While a space has a rewind record that is neither completed nor failed, every session check of the space answers closed (RFC 0014 §3), so no principal reaches a member route or an RPC. The Account DO also sends that space no call of this RFC: `reserveGrant`, `activateGrant`, `cancelGrant`, `retireGrants` and member removals wait in the outbox until the record ends, and a member notice from the space is answered `RETRY_LATER` and not applied. A redemption that needs `reserveGrant` meanwhile answers `RETRY_LATER`.

   Holding sends does not stop a call that was sent before admission and is delivered later, because calls through separate stubs have no order. So the Space DO also applies each of these calls only under a lease that says the space is open, taken and kept as for a principal's request (RFC 0014 §1 and §3). Without one it asks the Account DO, and while the space is closed it answers `RETRY_LATER` and applies nothing. The outbox keeps an unacknowledged call. Admission drops every lease, and a restored Space DO has none, so no such call is applied between admission and completion. A call applied before admission is in the capture, and a later one is refused and delivered again after completion.
2. `RewindCompletion` carries the stored rows. A completion retry returns the stored receipt, as today, and never captures again. In the completion transaction the Space DO replaces `members` and `member_grants` with them, restores the cutoff, sets `member_version` to the stored one plus one, deletes every restored `member_notices` row, and appends server commits from `restore_seq` + 1 with a `MemberUpdated` for each user whose row in the restored database differs from its captured row, naming the rewind. The completed space therefore has the captured roster, and its new history records how that roster differs from the restore point. Each commit holds at most 100 records, so it stays far below the 1 MiB frame limit. The Space DO then queues one full notice of the space's members for the Account DO's index.
3. The Account DO applies a full notice by replacing every index row of that space whose `member_version` is lower. Held outbox calls are delivered after this. A failed rewind releases them at once.

A restore into a database from before this RFC runs the migration first, then the same completion. The restored log still shows the member changes before the restore point, and the new history's first commits show how the current members differ from them.

RFC 0003 §6 changes: "members" moves from "Restored to the restore point" to "Kept", with a pointer here.

### 3.5 Removing a user

`DELETE /v1/users/{user_id}` needs `user.manage`, and `user.admin` to remove an admin. It cannot remove the deployment's owner. It answers `UserRemoval`:

- Without `?confirm=true`, if the index shows spaces where the user is the only owner, it changes nothing and answers `removed: false` with those spaces, so an admin can add another owner first. This is advice from an index that may lag, not a guarantee.
- Otherwise it removes the user and answers `removed: true` (step 2).

1. In one transaction the Account DO sets `removed_at_ms`, revokes every principal of the user (devices, browsers and agents), queues a revocation notice for every space, as RFC 0009 §2 does for one principal, and queues a member removal for every space where the index lists the user. A crash after this transaction loses none of them.
2. It answers once every space has taken its notices, or once `lease_ms` (RFC 0014 §1) has passed since step 1 committed, whichever comes first. Every lease of those principals comes from an answer computed before step 1, so every lease has ended by then (RFC 0014 §2). The notices stay queued until taken. A repeated `DELETE` of a removed user answers the same way and changes nothing.
3. The outbox delivers the member removals queued in step 1. They do not delay the answer, because the user can no longer authenticate. A removal caused by a removed user is applied even when it leaves the space without an owner, and the commit says so. The Account DO answers any member notice, a full notice included, that lists a removed user as a member with a member removal for that user.

**The fence.** A removed user gets no new principal and no new token. Every transaction that stores a credential or an access grant checks, in that same transaction, that the user is not removed and that the issuer, if any, is still active and its user not removed. That covers pairing (`completePairing` stores the device after an awaited preparation), invite redemption (§3.2 step 3), recovery codes, owner recovery, the sign-in and device authorization of RFC 0021, and later agent issuance. `activeDevice` also requires the user not to be removed, so token refresh and session checks (RFC 0014) refuse every principal of a removed user, even one whose own row looks active.

**Spaces left without an owner.** The index can lag, and an owner can leave between a check and a removal, so this RFC does not promise that a removal never leaves a space without an owner. It gives a way back instead. A space whose `members` has no owner row accepts one change from a full-scope principal whose user has `space.recover`: adding an owner, recorded as a member commit with that actor. The check reads the Space DO's own table, not the index.

**Space creation.** `createSpace` initializes the Space DO and then records the space. The recording transaction checks that the user is not removed. If the user was removed meanwhile, the Account DO records the space with no index row and tells the Space DO to remove its owner, so it becomes a space without an owner that `space.recover` can claim.

Download tickets the user's principals already hold keep working until they expire, at most one hour (RFC 0009 §2). The user's row stays, so commits, versions and decisions still show the user's name. A removed user cannot come back under the same `user_id`. A new invite creates a new user.

### 3.6 Export and restore of membership

Member changes are log records and reach the export, but a restore starts from a snapshot and replays only later records (RFC 0004 §4 and §5), and snapshots hold only nodes and versions (`spec/proto/koidrive/v1/export.proto`). So:

- Each snapshot also writes `snap/<start_seq>/members.json`: the `members` rows and `member_version` read with the snapshot's first page. The Space DO records the file's bytes in its export state before the first write, so a retry writes the same bytes, and the manifest lists the file with its size and SHA-256, as it lists pages. Replay after the snapshot skips every `MemberUpdated` whose `member_version` is not above the file's. A manifest without the entry is a snapshot from before this RFC. A manifest that lists the file while the file is missing or its hash differs is a damaged export (RFC 0004 §5). RFC 0004 §4 gains the file and the manifest entry.
- An R2-only restore into a deployment rebinds people, because user ids belong to the old Account DO, which the export does not hold. The restored space gets the restoring operator as its only owner. The operator sees the restored member list and can add people back. Restoring a snapshot without the file gives the same result, so format_version 1 stays.

### 3.7 What proposals and agent credentials add later

These belong to later RFCs. This RFC fixes only the shapes they extend.

- **Proposals (the drafts RFC).** Routes that create drafts, write into them and submit need `space.propose`. Deciding needs `proposal.decide`, which only direct-write roles and scopes carry (§2.1). The drafts RFC records for each decision the deciding principal, its kind and its user. A decision is not a claim that a person reviewed anything: the log and the apps show "accepted by the agent X of Kim" as plainly as "accepted by Kim on her laptop".
- **Agent credentials.** A principal of kind `agent` belongs to a user, the user accountable for it, and carries a non-NULL scope. Its scope proposes only by default: `space.read`, `log.read` and `space.propose` in the spaces it is issued for. The issuing user may add `space.write`, and with it `proposal.decide`, in a space where that user has direct write (§10, decision 7). Issuing one needs `agent.issue` and follows the issuer rule of §2.2. A scope change drops the principal's leases (RFC 0014 §1).
- **Service users.** A user of kind `service` has no devices, only agent principals. It is a member of spaces with a role like a person, so an outside agent product can be a `contributor` in one space and an `editor` in another. The log shows its name as a service.
- **Attribution.** Every `CommitRecord` carries `device_id` and `user_id`. With this RFC, membership changes carry the acting principal and its kind, and invites carry who issued them.

## 4. Wire and schema changes

Protobuf, all additive. No field is deleted or renumbered.

| Message | Fields |
|---|---|
| `MemberUpdated` (existing, empty) | `user_id = 1`, `role = 2`, `actor_user_id = 3`, `actor_device_id = 4`, `invite_id = 5`, `actor_kind = 6`, `rewind_id = 7`, `member_version = 8` |
| `UserInfo` (new) | `user_id = 1`, `name = 2`, `kind = 3`, `role = 4`, `created_at_ms = 5`, `removed_at_ms = 6` |
| `UserList` | `users = 1`, `next_cursor = 2` |
| `SetUserRole` | `role = 1` |
| `UserRemoval` | `removed = 1`, `sole_owner_space_ids = 2` |
| `RecoveryCode` | `code = 1`, `expires_at_ms = 2` |
| `CreateInvite` | `kind = 1`, `space_id = 2`, `role = 3`, `expires_in_ms = 4` |
| `Invite` | `invite_id = 1`, `code = 2` (only in the create reply), `kind = 3`, `space_id = 4`, `role = 5`, `expires_at_ms = 6`, `created_by_user_id = 7`, `state = 8` |
| `InviteList` | `invites = 1`, `next_cursor = 2` |
| `RedeemInvite` | `code = 1`, `redemption_secret = 2`, `user_name = 3`, `device_name = 4`, `platform = 5` |
| `RedeemInviteReply` | `credentials = 1` (`DeviceCredentials`, user invites only), `user_id = 2`, `space_id = 3`, `grant_state = 4` |
| `MemberInfo` | `user_id = 1`, `name = 2`, `user_kind = 3`, `role = 4`, `added_seq = 5`, `updated_seq = 6`, `added_by_user_id = 7` |
| `MemberList` | `members = 1`, `next_cursor = 2` |
| `SetMember` | `role = 1` |

A 0.1 client decodes the new `MemberUpdated` fields as unknown and applies the record as a no-op, so the record needs no `Hello` feature (RFC 0006 §5). Close code 4005 "access changed" joins spec v0 §7.1. Role `contributor` joins spec v0 §4.1. `ALREADY_EXISTS` and `QUOTA_EXCEEDED` are existing codes. No new `Error.Code`.

Account DO, one forward-only migration: `users` gains `kind TEXT NOT NULL DEFAULT 'person'`, `role TEXT NOT NULL DEFAULT 'member'` and `removed_at_ms INTEGER`, the existing user becomes `owner` on a claimed deployment, and the unique index for one owner is created. New tables `invites`, `recovery_codes`, `space_members` (filled from `spaces`) and `rewind_members`.

Space DO, one forward-only migration: `members` gains `updated_seq INTEGER NOT NULL DEFAULT 0` and `added_by_user_id TEXT`. New tables `member_grants` and `member_notices`. New meta keys `member_version` and `grant_cutoff`.

Text changes in other documents: in RFC 0005 §2 and RFC 0010 §1, "the space's owner" means a member with role `owner` in the Space DO. In RFC 0003 §1, "the caller is not the space's owner" means the same. RFC 0003 §6 changes as §3.4 says. RFC 0004 §4 gains `members.json` (§3.6). RFC 0006 §2 gains the kind `browser` (RFC 0021) and §3 gains the capabilities of §2.3. RFC 0006 §6 gains the issuer rule of §2.2. RFC 0009 §1 gains "or the caller has `device.manage.any`", and its last-device warning changes for users other than the owner (§3.1). RFC 0009 §2 notes that removing a member leaves issued chunk tickets valid until they expire. RFC 0014 §2 applies to all principals of a user at once when the user is removed (§3.5).

## 5. Feature and rollout

The routes of §3.1 to §3.3 and §3.5 are the feature `people` (RFC 0022). While it is off, they answer 404. The authorization of §2, the point-of-effect checks of §3.3, the moved owner checks and the rewind rule of §3.4 apply whether it is on or off. A deployment with one user has only owners, so these checks decide as today, and the generated authorization tests prove it for the owner rows.

## 6. Migration from 0.1

- A claimed deployment's one user becomes its owner. An unclaimed deployment has no user, and its first claim inserts the owner (§3.1).
- Every space keeps its one `owner` member, and the index gets that row with the space's `list_seq`.
- Nothing changes for 0.1 clients of the owner. The space list, which now comes from the index, holds the same spaces in the same order.
- A 0.1 client of a person removed from a space stops that folder with `Forbidden` on reconnect. A 0.1 client of a viewer or contributor gets read-only from `Welcome`, as spec v0 §9.2 defines. A 0.1 client applies `MemberUpdated` as a no-op.
- An invited person needs a client that can redeem an invite. 0.1 clients cannot.
- Existing share links stay. Their manager was the owner, who remains an owner.

## 7. Failure and retry

| Failure | Behaviour |
|---|---|
| The space cannot be reached, or its rewind is open, during a redemption | Before step 3: `RETRY_LATER`, and the reservation stays. After: the answer says `pending` and the outbox activates the grant later |
| A crash between steps of a redemption | The retry with the same secret continues from the stored step. The alarm cancels a reservation whose invite ended before step 3 |
| A lost redemption answer | A retry with the same secret gets the same credentials, until the principal first refreshes, is revoked, or 24 hours pass |
| Two redemptions of one code | The first to reserve wins. The other gets the wrong-code answer |
| The inviter loses authority during a redemption | Before step 3 commits: step 3 refuses and the grant is cancelled. After: the Account DO effect stays, and activation cancels the grant if the inviter's current role no longer allows it |
| A member change reaches a user with a pending grant | The change supersedes the grant, and a later activation changes nothing |
| An outbox call is due while the space's rewind is open | The Account DO holds it until the rewind record is completed or failed |
| An outbox call sent before admission arrives during the rewind | The Space DO has no open lease and answers `RETRY_LATER`. The call stays in the outbox |
| A client loses its redemption secret after step 1 | The reservation is cancelled after 10 minutes, and the invite can be redeemed again |
| A member notice is lost, late or from before a rewind | The outbox retries it. `member_version` keeps an older one from undoing a newer one. A rewind deletes restored notices |
| A member is removed while one of their commits is in flight | The commit transaction reads the current row: the commit is kept if its transaction ran before the change and refused after |
| A pairing completes while its user is being removed | The storing transaction sees `removed_at_ms` and refuses |
| A user is removed while a space cannot take its notice | The answer comes after `lease_ms` (RFC 0014 §2). The notice stays queued until taken |
| The server crashes after the first step of a user removal | The member removals were queued in that step and are delivered later |
| The last owner leaves or is removed during a user removal | The space may end up without an owner. `space.recover` adds one |
| Two owners remove each other | The second change is refused, and the space keeps an owner |

## 8. Tests

- **Authorization matrix.** A test generated from the tables of §2.3 calls every route and RPC as each role, each principal kind and each scope level, and expects allowed or `FORBIDDEN` as the table says. A device scoped to propose and an agent scoped to propose get the same answers. The owner rows match 0.1's answers.
- **Delegation.** A restricted principal pairs only a principal as restricted as itself, with an expiry no later than its own. A scope without `space.write` that names `space.rewind`, `member.manage` or `proposal.decide` is refused at issuance and denied at use. A scoped principal is refused recovery codes, `space.recover` and every other full-scope-only capability. `member.manage` cannot grant a role above the actor's level.
- **Invites.** Single use under two concurrent redemptions with different secrets. An unauthenticated and an authenticated redeemer of one code. Expiry and revocation at each step. A pending grant that does not count as an owner while the inviting owner leaves. Cancellation that leaves an existing membership alone. A delayed `reserveGrant` after cancellation, including one that arrives after a `cancelGrant` that found no row. An inviter principal whose scope loses `member.manage` between the reservation and step 3: step 3 refuses. A lost answer. Inviter revocation, removal and demotion before each step. An inviter removed from the space after step 3 and before activation: the grant is cancelled and no member is added. Inviter principal revocation after step 3: the membership is added. A non-admin space owner's space invite redeems. A scoped principal is refused a space invite. A viewer with a pending editor grant who is removed before the activation arrives stays removed. A reservation abandoned for 10 minutes is cancelled and the invite redeems again with a new secret. Step 3 more than 10 minutes after the reservation is refused. A space holding 1,000 grant rows refuses `reserveGrant` with `QUOTA_EXCEEDED`. A delayed message about a retired grant id. A space with 999 members and two concurrent redemptions (one reserves, the other gets `QUOTA_EXCEEDED`), and an ordinary add refused while a pending grant holds the last seat. Activation for a user who is already a member ends superseded. Two same-secret retries whose answers arrive in reverse order carry identical credentials. A delayed activation of a terminal grant after the member left.
- **Members.** A role change and a removal close the user's sockets with 4005, and a reconnect gets 403 or read-only. A commit that awaited uploads while its user was demoted is refused. A client test in `core/net` shows that an unknown close code followed by a 403 ends in `StopReason::Forbidden`. A demoted editor's unsent local edits stay on disk. The last-owner rule holds under concurrent changes. Duplicate user names are refused after case folding.
- **Index.** Concurrent join, removal, rejoin and out-of-order notices converge. A page cursor taken before a join still sees the new space.
- **Rewind.** A space with 1,000 members whose restored roster differs in every row completes the rewind in commits that each fit a frame. Remove a member, rewind to before the removal: the member stays removed. An activated grant keeps its terminal state across a rewind. Transfer ownership, rewind: the current owner stays. Rewind into a database from before this RFC. A pending notice restored by the rewind is not applied. An `activateGrant`, a `cancelGrant` and a member removal that fall due between admission and completion are held, and applied after completion. An `activateGrant` sent before admission and delivered after the capture, and one delivered after the restore, are refused with `RETRY_LATER` and applied after completion. A failed rewind releases held calls at once. A full notice that lists a removed user draws a member removal.
- **User removal.** A pairing paused in preparation while its user is removed gets no device. A crash after step 1 still removes the user from every space, and `space.recover` then works for a space whose only owner was the user. An admin is refused a recovery code for the owner. A removed user's download ticket stops working at its expiry, at most one hour. Token refresh and session checks refuse a removed user's principals. The removal answers within `lease_ms` when a space cannot take its notice. A space left without an owner accepts an owner from `space.recover` and from no one else.
- **Export.** A restore from a snapshot with `members.json` lists the members and skips older replayed changes. One without it restores with the operator as owner. A manifest whose `members.json` hash differs is reported as damaged.
- **Share links.** A demotion between `authorizeShare` and the link write: the write succeeds only within the grant's 30 seconds, and a second write with the same grant is refused.
- **Model.** The Rust space model (`core/sim/src/space`) gains member changes, grants and the rewind rule, and the conformance scenarios gain cases for them. A property: no commit by a user appears in the log after that user's removal from the space.

## 9. Alternatives considered

1. **The Account DO holds roles, and spaces cache them in the lease.** One place for everything about people. Rejected: a role change would not be ordered with the space's commits, and the commit transaction would depend on a cached copy.
2. **Proposals as an agent-only workflow.** A person can need propose-only access too, and two write models would need two sets of checks.
3. **Let a rewind restore permissions.** It brings back removed people and earlier owners, and every index and notice would need reconciling across histories.
4. **Add the member first and remove it if the invite fails.** An unfinished invite could count as an owner, and the compensating removal could delete a membership the user had anyway.
5. **Refuse every user removal that would leave a space without an owner.** The check reads an index that lags, and owners can leave in between. A recovery capability on the Space DO's own table holds instead.
6. **An external authorization service.** OpenFGA is an Apache-2.0 authorization engine modeled on Google's Zanzibar (github.com/openfga/openfga, read 2026-11-12). Four roles per space fit a table, and an external service would put a network call into every commit and add a second store that must agree with the Space DO's transaction. Worth another look if per-folder permissions come.
7. **Finer roles from the start.** Google Drive's API has owner, organizer, fileOrganizer, writer, commenter and reader (developers.google.com/workspace/drive/api/guides/ref-roles, read 2026-11-12). Koidrive has no comments and no shared-drive organizer. The capability table lets a role be added as a row, as `contributor` is here.
8. **Per-folder permissions.** Out of scope. `policy.authorize` already receives each op's ancestry (RFC 0006 §3), so a later RFC can add them without moving the check.
9. **Several organizations in one deployment.** Not designed. One deployment per customer keeps tenancy out of the server.

## 10. Decisions

The project owner decided these on 2026-11-12, each as recommended:

1. Only the deployment's owner and admins bring a new person into the deployment, through user invites. A space owner adds people who already have an account, through space invites.
2. Editors may decide on proposals. Only owners create public share links.
3. `users.kind` with `person` and `service` exists now. Service users are issued with the agent credential RFC.
4. Admins may revoke other people's principals.
5. Multi-user ships when it is finished, behind the feature `people`.
6. The deployment's owner and admins may take over a space that has no owner left and may issue recovery codes for other people, from full-scope principals only, recorded in the space's log and the user's device list.
7. The user who issues an agent may give it direct write, in spaces where that user has direct write.

Open questions:

1. Transferring the deployment's owner role to another person, and what owner recovery recovers afterwards.
2. Quotas per user, and whether a service user counts as a seat.
3. Cleaning up a space recorded without an owner because its creator was removed during creation, if nobody recovers it.
4. A space setting that requires a person's decision on proposals.

# RFC 0019: Move preconditions

Status: proposed. Applies to spec v0 (format_version 1). Amends the `Move` op of spec v0 §6, spec v0 §6.2 R7, RFC 0001 §6, RFC 0017 §3, and RFC 0018 §4, §7 and §8, and adds a field to `NodeState`.

Some moves carry no change of the sending device's user. A device that sent a folder under a pending name owes the rename to its own name (sync-engine design §2.6, RFC 0017). A device that finds after a server loss that the server lost a position another device set sends the move again (design §4.4). The server cannot tell such a move from a user's move, so it applies the move's spec v0 rules to it, and another device's user loses a change made after seeing the node:

- When another device's `Delete` lands while the owed rename is already built, the rename revives the folder and its trash group (R8). When another device renamed the folder to `b` instead, the owed rename puts it back at its own name (R5). The device cannot avoid this, because the move is on the wire before it hears the change (GUN-597).
- When the owed rename lands first, a `Delete` built before it counts the rename as a change it did not see. R7 keeps the folder, and every device ends with the folder the user deleted, empty. A device that hears the rename before it sends its delete keeps the folder the same way (design §2.4).

This RFC lets a `Move` name the place the sender expects the node at, and makes such a move yield to every change of the node that it did not see. A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only:

- `Place`: `parent_id = 1`, `name = 2`. It names a place in the tree.
- `Move`: `from = 5`, a `Place`. Field 4 is RFC 0017's `pending_for`.
- `NodeState`: `meta_edit_rev = 13`, a uint64. Fields 10 and 11 are RFC 0017's, and 12 is RFC 0018's.

## 2. The check

A `Move` with `from` applies only while all of these hold:

- the node is live;
- its parent is `from.parent_id`, which resolves through the commit's earlier merges (spec v0 §6, R2) like every node ID in the commit;
- its name equals `from.name`, compared byte for byte after NFC as RFC 0001 §6 compares names;
- the new parent is live.

The server checks this when it applies the op, against the tree as the commit's earlier ops left it. The check comes after the op's `REJECTED` checks (`NOT_FOUND`, `ROOT`, `INVALID_NAME`, `PARENT_NOT_DIR`, `CYCLE`) and before every other rule: the no-op rule of RFC 0001 §6, R8, R1 and the depth limit. When the check fails, the server changes nothing for the op, RFC 0017's mark included, and answers it with `SUPERSEDED`.

A live node has live ancestors, so a `Move` with `from` revives nothing: not the node, not its trash group, and not the chain above the new parent.

A `Move` with `from` that applies changes the node's parent, name and `meta_rev` like any `Move`, and leaves the node's meta edit revision (§3) as it was. It is a placement (RFC 0002 §9) only when it changes the node's parent. A `Move` with `from` to the node's current parent, a no-op one included, leaves the placement as it was.

## 3. The meta edit revision

Each node has a meta edit revision: the seq of the newest commit that created it or changed its parent, name or status, other than by a `Move` with `from`, or that carried a user's move to the place such a move gave it (below). Every op that sets `meta_rev` sets it to the same seq, except an applied `Move` with `from`. The server keeps the device of the commit that set it, as it does for `meta_rev`. `NodeState.meta_edit_rev` reports it. It is below `meta_rev` while a `Move` with `from` made the node's newest meta change, and above it after a user's move to that place.

This amends RFC 0001 §6, as RFC 0018 §3 amends RFC 0016 §1. When another device's resend of a lost move lands first (§4), the author's own send of that move finds the node at its target already. RFC 0001 §6 would apply it without a change, so no revision would record the author's move, and a `Delete` with a base below the resend would trash the node. So a `Move` without `from` that RFC 0001 §6 makes a no-op, while the node's meta edit revision is below its `meta_rev`, is `APPLIED` as a user's move. It leaves `meta_rev`, the parent and the name as they are, sets the meta edit revision and its device to this commit, and makes the placement the committing device's, as RFC 0002 §9 does for every `Move` that names the node's current parent. The result's changes hold a `NodeState` for the node. RFC 0001 §6 stands for every other no-op `Move`.

R7 reads the meta edit revision and its device where it read `meta_rev` and its device. So a `Delete` trashes a node whose only change since its base is a move that carried no user's change, and keeps a node that a user renamed, moved or revived after its base, as before. R5 is unchanged: among moves that apply, the later commit wins.

## 4. Clients

A device sets `from` on a `Move` that carries no change of its own user:

- the rename of a node from its pending name to the name it waits for: the rename a device owes (design §2.6) and RFC 0017's finishing move;
- a move that sends again a position the server lost and that this device's user did not set: the move of a node S forgot that L holds as S did (design §4.4, GUN-548).

Every other `Move` goes without `from`, so a user's move keeps R5 and R8: it wins over a concurrent move it did not see and revives a node another device deleted.

`from` is the node's place in R as the commit's earlier ops leave it, when the commit is built.

A finish whose origin is a mark in R (RFC 0017 §3) no longer requires the head. A server that sets marks has the check of §2 (§6), which refuses the move for every change of the node, and R1 gives it a conflict name if another node holds the name when it arrives. A finish of origin legacy keeps `require_head`, since its server may predate both RFCs and ignore `from`. A commit that sends a lost position again keeps `require_head` too: its server may also predate this RFC, and the commit can hold creates that this RFC does not cover (§8).

The author of a lost move sends it again without `from` (design §4.4), also when R already holds the node at its target through a record whose `meta_edit_rev` is below its `meta_rev`. §3 then records the author's move, so it beats a later delete as before.

A `SUPERSEDED` result of a `Move` with `from` ends what made the move: the owed rename, RFC 0017's finish, or the resend. The result is acknowledged once R holds every commit below its seq (design §2.7), so R already holds the change that superseded the move. When L holds the node at the move's target, S takes that place. S keeps the content it holds, or, when S forgot the node (design §4.4), takes the shape L held when the commit was built, content included, as design §4.4 does for such a node that R holds trashed (GUN-548). The change then reaches the disk as a remote change: the delete removes the folder, the rename moves it to the other device's name, and content R holds that S does not hold downloads over L's, as any remote edit does. When L holds the node anywhere else, the user moved or deleted it, and S stays, so that change is sent as usual. Either way the device ends as if it had heard the change before it built the move.

A device whose user deleted a node it held as S plans the delete past a record of another device's commit when all of these hold:

- R held S's shape for the node just before the record, its place and its content, and no record R took since S took that shape moved the node's `edit_rev` or `meta_edit_rev`;
- the record keeps the node live and leaves its `edit_rev` and `meta_edit_rev` as R held them;
- L no longer holds the node.

S takes the record's shape, so the delete goes out with a base past the record, and R7 trashes the node under §3 and RFC 0018 §4. This replaces RFC 0018 §7's list of conditions, which it widens: the record may now also move the node's `meta_rev` and place. A record that moves `edit_rev` or `meta_edit_rev` carries a user's change and is never taken. The first condition also rules out a record that follows another change R took after S's shape, a user's change that R took and then undid included, so a delete never passes over a change its user did not see.

A record of another device's commit that moves a node's meta edit revision but not its `meta_rev` (§3) is a remote change to a device whose L no longer holds the node. S forgets the node, and the device downloads it again, as RFC 0018 §7 does for a record that moves only the edit revision. A device that took a resend's shape above therefore keeps the author's move that follows it. A device takes a missing `meta_edit_rev` (0) as equal to `meta_rev`. A device that upgrades its store sets each stored node's meta edit revision to its `meta_rev`, with a nullable column whose NULL reads as `meta_rev` or with an `ALTER TABLE` and an `UPDATE`.

## 5. Resends and RFC 0018

- A resend of the same commit (RFC 0002 §2) keeps `from`, and its stored result answers it. Digest v1 (RFC 0006 §8) keeps `Move` fields 1 to 3 and digest v2 (RFC 0018 §6) adds only `UpdateContent` field 3, so neither covers `from`. As for RFC 0015's `requires`, a reuse of a `commit_id` that differs only in its precondition is not detected. A client never sends one: the engine sets `from` once, when it builds the commit, and resends that stored commit as it is.
- A change planned again after the server lost commits goes out in a new commit, whose `from` the engine takes from the R it holds then. A place has no revisions, so a seq the restored server gives again cannot make the check hold by accident.
- The meta edit revision does for places what RFC 0018's edit revision does for content. A move resent with `from` leaves it, as an `UpdateContent` with `resend` leaves the edit revision, so a delete made after the loss passes over both. This closes RFC 0018 §11's first item for a rename that keeps the parent (GUN-580). The author of a lost move sends it again without `from`, also after another device's resend put the node at its target (§3, §4), so its move still beats a later delete, as the author's lost edit does (RFC 0018 §3).

## 6. Mixed versions and stored state

- An old client sends no `from`. The server applies its moves by spec v0, and each sets the meta edit revision like `meta_rev`. The server cannot tell an owed rename from a user's move, so an old client keeps the behaviour this RFC fixes, and no worse.
- A server release that sets RFC 0017's marks has the check of §2 and the meta edit revision of §3. No server sets marks without them.
- A server before this RFC ignores `from` and sends no `meta_edit_rev`. It sets no marks, so a new client's finishes there are of origin legacy and keep `require_head` (§4), as RFC 0017 §3 sends them. A resent position keeps `require_head` too. A new client reads the missing `meta_edit_rev` as `meta_rev`, so its deletes plan as before. No `Welcome` feature announces this RFC.
- A server that upgrades adds the meta edit revision and its device to each node, as nullable columns whose NULL reads as `meta_rev` and its device, so the migration rewrites no row.
- An export (RFC 0004) carries `meta_edit_rev` in `NodeState`, but not its device. A restore gives the meta edit revision the device of `meta_rev` when the two are equal, and no device otherwise, so R7 counts it as another device's change. A restore can then make a delete keep a node it would have trashed, and never makes it trash one. A record without `meta_edit_rev` gets `meta_rev` and its device. RFC 0004 §5's replay compares `max(content_rev, meta_rev, edit_rev, meta_edit_rev)`, since a record of §3 moves the meta edit revision alone.

## 7. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` gains these scenarios:

- A `Move` with `from` at the node's place is `APPLIED`. A `Delete` with a base below it then trashes the node.
- A `Move` with `from` on a trashed node is `SUPERSEDED`, and the node and its trash group stay in the trash.
- A `Move` with `from` after another device renamed the node is `SUPERSEDED`, and the node keeps that name.
- A `Move` with `from` into a trashed folder is `SUPERSEDED`, and nothing is revived.
- A `Move` with `from` whose `from.parent_id` names a folder an earlier `CreateDir` of the commit merged is `APPLIED`.
- A `Move` without `from` on a trashed node revives it (R8), and a `Delete` with a base below that move keeps it (R7).
- After a `Move` with `from`, a `Move` without `from` to the same place is `APPLIED` with a `NodeState` whose `meta_edit_rev` is its seq and whose `meta_rev` is unchanged. A `Delete` with a base below the first move then keeps the node.
- A no-op `Move` without `from` on a node whose meta edit revision equals its `meta_rev` changes nothing (RFC 0001 §6).

## 8. Not covered

- A create sent again after a loss carries no precondition, and R8 would revive a trashed parent chain for it. Its commit keeps `require_head`, so the server refuses it when any commit lands first, the delete of its parent included. A `Delete` that lands after it keeps the created node, as R7 keeps any node created after its base.
- A move resent with `from` into another folder is a placement (§2), so a `Delete` built before it keeps the node under RFC 0002 §9, as for any node it did not see under its parent.
- A user can rename a node away from its pending name and back while the owed rename is in flight. The check holds, and the owed rename applies.

## 9. Cost

A `Move` with `from` adds comparisons on node rows the server already reads, and up to about 300 bytes to the commit: a 36-character ID and a name of at most 255 bytes. Each node stores one more revision and device. A user's move to the place a resend gave the node adds one revision write. `NodeState` grows by up to 11 bytes.

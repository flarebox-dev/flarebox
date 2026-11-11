# RFC 0012: Trash groups across a loss

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §4.1.

Spec v0 §4.1 names the trash group of a `Delete` op `<seq>:<op index>`. A server restored from a backup loses the commits after the backup and gives their seqs to later commits. A `Delete` in such a later commit can then get the trash group of a lost `Delete`. A client that identifies deletions by trash group, as the mass-delete guard does (design §6.1), takes the two deletions for one. A dismissed deletion then takes the removals of the new one, and no notice opens. A lost deletion that is still being counted keeps counting through the new one's tombstones, and the two can cross a limit together that neither crosses alone.

This RFC names a trash group after its commit instead of the commit's seq. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. Group names

Every node a `Delete` op trashes gets the trash group `<commit_id>:<i>`, where `commit_id` is the `commit_id` of the commit that holds the op and `i` is the op's 0-based index in that commit. This replaces `<seq>:<i>` in spec v0 §4.1. Nothing else about trash groups changes: a node trashed earlier keeps its own group, R8 revives a group, and a purge removes one.

No two commits share a `commit_id`, also across a loss:

- Within a history, the server applies a `commit_id` at most once. A second commit with a stored `commit_id` gets the stored result, or `INVALID_ARGUMENT` when it differs (RFC 0002 §3).
- Every client mints a new UUIDv7 for each commit it plans (RFC 0002 §2), and the server refuses a `commit_id` of any other form. After a loss, the only commit that carries a lost `commit_id` is that same commit, sent again.
- The server's own commits mint their own UUIDv7 and hold only purges, so they make no group.

So a commit that takes the seq of a lost commit takes none of its trash groups.

## 2. A resend after a loss

A device sends a commit again with the same `commit_id` only while it is the commit in flight. If the server accepted it, the response was lost, and the server then lost the commit, the device resends it unchanged (RFC 0002 §2, *w* = *a*). Its `Delete` ops trash under the same groups as before, so other devices take it for the same deletion: it stays counted, or dismissed.

A change that the device plans again after the server lost its accepted commits (*w* < *a*) goes out in a new commit with a new `commit_id`. Its `Delete` is a new deletion and can open a second notice, for removals that really happen again.

## 3. Clients

A client treats a trash group as an opaque string. It compares groups only for equality and reads no seq, order or commit from one. `ListTrash` and `NodeState.trash_group` carry the new form unchanged.

A trashed node's change record grows with the group: `12345:0` has 7 bytes, and a group of the new form 38 to 40.

## 4. Mixed versions

Nodes trashed before the upgrade keep their `<seq>:<i>` groups. A new group starts with a 36-character UUID with hyphens, and an old one has only digits before the colon, so the two forms are never equal. A loss after the upgrade can therefore not bring back an old group, and the server needs no data migration. Clients need no change, since they already treat groups as opaque, so no `Welcome` feature announces this RFC. A server downgraded to an older release names groups `<seq>:<i>` again and reopens the gap.

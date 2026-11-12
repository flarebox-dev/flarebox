# RFC 0016: An edit to the content a file already holds

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §6.2 R3.

After a server loses commits, more than one device can send the same lost edit again: its author, and every device that downloaded it before the loss (sync-engine design §4.4, GUN-389). The first send applies. Each later one meets content that another device set after its `base_seq`, so R3 makes it a conflict copy that holds the bytes the file already holds.

This RFC applies such an edit without a change, and states the property of version IDs that lets a client send the edit at all. A bare section number such as §2 refers to this RFC.

## 1. Unseen content equal to the edit

An `UpdateContent` whose target file holds unseen content (R3: `content_rev` above `base_seq`, made by another device) equal to the op's content is `APPLIED` and changes nothing. The server makes no version, no conflict copy and no revision, and the result's changes hold no `NodeState` for the file. Two contents are equal when their `size`, `manifest_digest` and `executable` are equal; `mtime_ns` is not compared. A file in the trash stays in the trash: R8 does not revive it, since the edit holds nothing the trashed content lacks.

An `UpdateContent` with equal content that is not unseen keeps the rule of spec v0: it makes a new version and a new content revision. A client may rely on that to send an edit back to the bytes the server holds, which another device's later edit then meets as unseen (GUN-438).

## 2. Version IDs

A server never gives one `version_id` to two versions of a space, also when it restores its storage from a backup and loses commits within one history. A `Restore` of a version goes through the same rule as an `UpdateContent`: it makes a new version with a new ID, or changes nothing under §1. So a client that sees, in a restored server's tree, a version ID it saw before sees the very version it saw then. The server mints each version ID as a UUIDv7 with 74 random bits.

## 3. Clients

After a loss, a device sends a file's content again as an edit when the restored server holds the file at a version that the device saw replaced by that content (§2), whoever made the content. The author sends it again too, and the second send is applied by §1.

## 4. Compatibility

No message changes. A client sees `APPLIED` where an older server returned `CONFLICT_COPY` with a copy of bytes the file already held. It acknowledges the result as any `APPLIED` edit: the content it sent becomes synced, and R already holds it.

## 5. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` covers §1 in `rfc0016-unseen-content-equal-to-the-edit-is-applied-and-changes-nothing`, with a seen equal edit that still makes a revision, and `rfc0016-unseen-content-equal-to-the-edit-leaves-a-trashed-file-in-the-trash`.

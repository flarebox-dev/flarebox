# RFC 0017: A delete beats a resend it did not see

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §6.2 R7 and R8, and adds fields to `UpdateContent` and `NodeState`.

After a server loses commits, a device that downloaded another device's edit before the loss sends it again (RFC 0016 §3, sync-engine design §4.4). This resend holds no change of the sending device's user. A delete that another device's user makes after the loss should beat it, since that user may have held the content the resend brings back (GUN-548). A resend sent with `require_head` (RFC 0002 §4) yields to a delete the server takes first. A delete the server takes after the resend was built without it: R7 counts the resend as a content change the delete did not see and keeps the file (GUN-552).

This RFC lets a resend say what it is, and lets a delete pass over a resend it did not see. A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only:

- `UpdateContent`: `resend = 3`, a bool.
- `NodeState`: `edit_rev = 10`, a uint64.

## 2. The edit revision

Each node has an edit revision: the seq of the newest commit that changed its content other than by a resend, or that created it. A create, an `UpdateContent` without `resend`, a `Restore` of a version and a conflict copy set it to the commit's seq, as they set `content_rev`. An `UpdateContent` with `resend` that changes the content moves `content_rev` and leaves the edit revision as it was. `NodeState.edit_rev` reports it. The server also keeps the device of that change, as it does for `content_rev`.

## 3. Deletes

R7 reads the edit revision. A `Delete` keeps a node of the subtree it trashes, and the folders between that node and the deleted one, when the node's edit revision is above `base_seq` and another device made that change, where R7 read `content_rev` before. The checks on `meta_rev` and on the parent stay as they are. So a delete trashes content that a resend put on the server after the delete's base, and keeps any other content change it did not see, a change before the resend included.

R3 still reads `content_rev`. An `UpdateContent` built without the resend meets content it did not see and makes a conflict copy, as before, or is applied without a change when its content is equal (RFC 0016 §1).

## 4. A resend on a trashed node

An `UpdateContent` with `resend` whose target is in the trash is answered `SUPERSEDED` and changes nothing. R8 does not revive the node. The delete that trashed it was made by a user who may have held the content, as in §3.

## 5. Clients

A device sets `resend` on an `UpdateContent` that sends again content another device made and the server lost, with no change of its own user in it. The author of a lost edit sends it again without `resend`, so its edit still beats a later delete (R8, GUN-438).

A device takes a missing `edit_rev` (0) as equal to `content_rev`.

A device whose user deleted a file it held as S does, before a resend of it arrived in the log, plans the delete against the resend: when a record moves the file's `content_rev` and keeps its `edit_rev`, R held S's content and position just before it, and L no longer holds the file, S takes the record's shape. The delete is then sent with a base past the resend, and §3 applies it. A listing shows no such step and changes nothing here.

## 6. Mixed versions

- An old client never sets `resend`, so a new server changes nothing for it.
- An old server ignores `resend` and sends no `edit_rev`. A new client reads `edit_rev` as `content_rev`, so it keeps a file a resend changed, as before. It keeps `require_head` on resends, which old and new servers both honour.
- A server that upgrades sets the edit revision of each node to its `content_rev` and device. A tree restored from an export (RFC 0004) without `edit_rev` gets the same.

## 7. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` gains three scenarios:
- A resend, then a `Delete` with a base below it, is `APPLIED` and trashes the file.
- After a resend, an `UpdateContent` with a base below it and other content makes a conflict copy.
- A resend on a trashed file is `SUPERSEDED`, and the file stays in the trash.

## 8. Cost

An `UpdateContent` grows by two bytes when `resend` is set. Each node stores one more revision and device, and `NodeState` grows by up to 11 bytes.

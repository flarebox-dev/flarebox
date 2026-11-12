# RFC 0015: Delete preconditions

Status: proposed. Applies to spec v0 (format_version 1). Amends the `Delete` op of spec v0 §6.

An answer to a conflict question (design §2.7) removes one file of the pair and keeps the other. Keep the original sends `Delete(copy)`. The server checks only the copy against the commit's `base_seq` (R7), so a `Delete(copy)` still applies when another device renamed or deleted the original after the person answered. Both files then leave the live tree, which is the opposite of the answer (GUN-464). `require_head` (RFC 0002 §4) would refuse such a commit, but it also refuses one when any other commit lands in the space between build and arrival. Spaces that take 100 or more commits a second would refuse the answer every time.

This RFC lets a `Delete` name the node the client keeps, at the revisions it saw. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. Messages

New fields only:

- `NodeRevisions`: `node_id = 1`, `content_rev = 2`, `meta_rev = 3`. It names a node at the revisions of its `NodeState`.
- `Delete`: `requires = 2`, a `NodeRevisions`.

## 2. The check

A `Delete` with `requires` applies only while the node `requires` names is live, with `content_rev` and `meta_rev` equal to the given ones. The server checks this when it applies the op, against the tree as the commit's earlier ops left it. The check comes after the op's other checks (`NOT_FOUND`, `ROOT`, a target already trashed) and before the target is trashed, so R7 runs only on a delete whose precondition holds.

When the check fails, the server trashes nothing for the op and answers it with `SUPERSEDED`. That is the code R7 gives a delete that a concurrent change kept from applying, so a client handles both alike: the change it did not see wins, and the client decides again. A node the server does not hold, or holds in the trash, is not live, so a precondition on it fails.

The check reads the named node only. A rename of a folder above it changes none of its revisions and keeps it live, so the precondition holds. A delete of a folder above it trashes it, so the precondition fails.

## 3. Clients

A conflict answer that removes one file of the pair sends its `Delete` with `requires` set to the file it keeps, at the revisions R holds for it when the commit is built. That covers Keep the original's `Delete(copy)` and Keep the copy's final `Delete(copy)`, after the original took the copy's content. The engine builds the answer only while the original is live in R at the meta revision the question was asked with, so the precondition holds exactly when the server's original is still the one the answer was built against.

A `SUPERSEDED` result clears the answer, as for R7 (design §2.7). After the next pull, the engine ends the question if the original was renamed, moved or deleted, and asks it again under a new ID if either file took new content.

A resend of the same commit (RFC 0002 §2) keeps the precondition, and its stored result answers it. The request digest of RFC 0006 §8 hashes the ops' encoding, so it covers `requires`, and a commit sent again with another precondition is refused as a different request.

## 4. Mixed versions

- An old client sends no `requires`, and the server checks nothing new.
- A server before this RFC ignores the unknown field and applies the `Delete` as before, so a new client of an old server gets the behaviour this RFC fixes, and no worse. No `Welcome` feature announces this RFC.

## 5. Cost

A `Delete` with `requires` adds one read of a node by its primary key and about 50 bytes to the commit: a 36-character ID and two revisions.

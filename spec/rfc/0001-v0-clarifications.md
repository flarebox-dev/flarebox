# RFC 0001: v0 clarifications

Status: proposed. Applies to spec v0 (format_version 1).

Spec v0 was drafted outside this repository. This RFC records rules the first server and engine implementations needed that v0 leaves open. Section and rule numbers refer to spec v0. None of these changes touches the schema.

## 1. Name keys (§6.1)

`name_key` is `NFD(casefold(NFD(name)))`, the Unicode canonical caseless match, instead of `casefold(NFC(name))`. Full case folding of an NFC string can yield a string that is not normalized. For example, `ΐ` (U+0390) and `Ϊ́` (U+03AA U+0301) are case variants of each other, but under the v0 formula they get different keys and could both be live in one folder. casefold is still Unicode default full case folding (statuses C and F); server and clients must use the same Unicode version.

## 2. Reserved name at the root

`.flarebox` is reserved directly under the space root, compared by `name_key`, so `.Flarebox` is reserved too. A `CreateDir`, `CreateFile`, `Move` or `Restore` that would leave a live node with that name directly under the root is rejected with `REJECTED` and reason `INVALID_NAME`. The name is allowed at any other depth. Clients keep their local state in `.flarebox/` at the root of the synced folder and never sync it.

## 3. UpdateContent on a directory

An `UpdateContent` whose target is a directory is rejected with `REJECTED` and reason `NOT_FILE` (R11). `NOT_FILE` joins the reject reasons listed in §8.

## 4. CreateDir onto a live file

R2 (merge) applies only when the existing live node with the same `name_key` is a directory. When it is a file, the new directory goes through R1: it is created under a conflict name and the result is `APPLIED_RENAMED`.

## 5. Long conflict names (§6.1)

The device name is cut to 32 Unicode scalar values after `/` and NUL are removed; an empty result becomes `unknown device`. If `<stem> (conflict from <device> <YYYY-MM-DD>[ <n>])<ext>` is longer than 255 bytes, scalar values are removed from the end of the stem until it fits, so the suffix and the extension survive. If it is still too long with an empty stem, scalar values are removed from the end of the whole name until it fits. The result is normalized to NFC.

## 6. No-op Move

A `Move` of a live node to its current parent under its current name, compared byte for byte after NFC, is `APPLIED` with no change record. `meta_rev` and `meta_dev` keep their values, so a no-op Move never counts as an unseen change and can never make another device's concurrent `Delete` keep the node under R7. A rename that changes only case is not a no-op.

## 7. Commit size (§13)

The server stores each commit's change records in one log row. A commit whose serialized `CommitRecord` and `CommitResult` together exceed 7,900,000 bytes is rejected with `LIMIT_EXCEEDED` and changes nothing; the bound sits below the 8 MB Durable Object SQLite row limit ([Durable Objects limits](https://developers.cloudflare.com/durable-objects/platform/limits/)). A client that gets this splits the commit. For a `Delete` of a very large subtree, it deletes subfolders in earlier commits.

## 8. Share-link password KDF (§15.2)

Known deviation: share-link passwords use PBKDF2-HMAC-SHA256 with 100,000 iterations. The [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html) recommends 600,000, but workerd rejects WebCrypto PBKDF2 above 100,000 iterations ([cloudflare/workerd#1346](https://github.com/cloudflare/workerd/issues/1346)). Each link stores its algorithm and parameters, so a stronger KDF can replace this one without migrating existing links. Five wrong passwords lock a link for 15 minutes.

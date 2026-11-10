# RFC 0010: Space storage usage

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §3.

The owner of a deployment pays for its R2 bucket. This RFC defines a read that tells the owner how many bytes of chunks a space holds, split by what holds them: live files, the trash and old versions. The trash and old versions are what the owner can free by waiting out their retention. The read changes nothing and adds no schema. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. The endpoint

`GET /v1/spaces/{space_id}/usage` returns a `SpaceUsage`. The body is protobuf, or proto3 JSON, like the other endpoints of spec v0 §15.1.

The server answers when the caller authenticates with an access token of an active principal (RFC 0006 §2) that has `can(principal, "space.usage")` (RFC 0006 §3), and the caller's user is the owner of the space. Any other member, and a caller whose user is not a member, gets `FORBIDDEN` (403). A space that a rewind holds closed answers `RETRY_LATER` (503), as for every other route of a space (RFC 0003 §4).

## 2. What counts

The Space DO reads every version it keeps and gives each one a holder:

- **live**: the current version of a file in the live tree;
- **trash**: the current version of a file in the trash;
- **old versions**: a version that a newer one replaced and that the space keeps until `version_retention_days` pass (spec v0 §12). This includes the old versions of trashed files.

A chunk counts once, under the first of live, trash and old versions that holds it. A chunk that a live file and an old version share counts as live, because deleting the old version would not free it. A chunk a file lists twice also counts once. Each chunk row of the Space DO stands for one object in the bucket, so the four byte fields together are what the bucket holds for the space, apart from the cases of §3.

| Field | Meaning |
|---|---|
| `live_files` | Files in the live tree. |
| `live_bytes` | Bytes of the chunks that live files hold. |
| `trashed_files` | Files in the trash. A trashed folder counts its files. |
| `trash_bytes` | Bytes of the chunks that trashed files hold and no live file holds. |
| `old_versions` | Old versions the space keeps, of live and trashed files. |
| `old_version_bytes` | Bytes of the chunks that only old versions hold. |
| `unreferenced_bytes` | Bytes of the chunk rows that no version holds (refcount 0). |

`unreferenced_bytes` covers two kinds of chunk. The first is an upload that no commit has used yet. The server granted it, but it may not have reached the bucket, and GC removes the row after its hold if no commit uses it. The second is a chunk whose last version went away and that waits out `chunk_grace_days`, at least 30 days (RFC 0003 §5, rule 4), before GC deletes it. The owner cannot free these bytes sooner.

Bytes are chunk sizes as the server checked them at upload (spec v0 §8), not file sizes. They can be lower than the sum of the file sizes, because files and versions share chunks.

## 3. What does not count

- The objects of the R2 export (RFC 0004). They are a copy of the space's metadata, written under the space's export prefix.
- Objects that the orphan sweep will delete, and objects of a key incarnation that a rewind retired (RFC 0003 §5). The Space DO keeps no size for them.
- The Durable Objects' own storage. Cloudflare bills it separately from R2.
- After a rewind, a chunk that a restored version holds but whose object the bucket no longer has (RFC 0003 §5, rule 9) still counts under its holder until a device uploads it again.

The numbers are bytes held at the time of the read. R2 bills the average of each day's peak storage over the month, in GB-months rounded up ([R2 pricing](https://developers.cloudflare.com/r2/pricing/), read 2026-11-10), and also bills operations. A client that shows a cost estimate must say so.

## 4. Cost

The server computes the answer on each request, with no running totals. Keeping totals would mean one more row write per chunk each time a file goes to the trash, comes back, or gets a new version, because each of those moves its chunks to another holder. Rows written cost 50 times more of the Free plan's daily allowance than rows read (100,000 written and 5 million read per day, [Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/), read 2026-11-10). A read is rare by comparison.

The read costs about two rows read per version, plus one per page of a version with more than 4,096 chunks and one per unreferenced chunk. A client reads usage when the owner asks for it and does not poll.

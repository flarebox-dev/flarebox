# RFC 0005: Read-only share links

Status: proposed. Applies to spec v0 (format_version 1). Tracks GUN-275.

The owner of a space can share one file or one folder through a link. A recipient opens the link in a browser, browses the folder, and downloads files. The recipient installs nothing and needs no account. Links are read-only. A link can have an expiry and a password. This RFC defines the owner API, the public API, the web viewer, and what a link can reach. A bare section number such as §4 refers to this RFC; sections of spec v0 are written as spec v0 §N. RFC 0001 §8 (password key derivation and lockout) applies unchanged.

The schema changes add one file, `share.proto`, and one error code:

| Message | Fields | Section |
|---|---|---|
| `CreateShareLinkRequest` (new) | `node_id = 1`, `expires_at_ms = 2`, `no_expiry = 3`, `password = 4` | 2 |
| `ShareLink` (new) | `link_id = 1`, `url = 2`, `space_id = 3`, `node_id = 4`, `created_at_ms = 5`, `expires_at_ms = 6`, `has_password = 7`, `revoked_at_ms = 8` | 2 |
| `ShareLinkList` (new) | `links = 1`, `next_cursor = 2` | 2 |
| `ShareInfo` (new) | `password_required = 1`, `kind = 2`, `name = 3`, `root_id = 4`, `size = 5`, `mtime_ms = 6`, `expires_at_ms = 7` | 3 |
| `ShareUnlockRequest` (new) | `password = 1` | 3 |
| `ShareEntry` (new) | `node_id = 1`, `name = 2`, `kind = 3`, `size = 4`, `mtime_ms = 5` | 3 |
| `ShareListing` (new) | `entries = 1`, `path = 2`, `next_cursor = 3` | 3 |
| `Error.Code` | `CODE_NOT_FOUND = 15` | 2, 3 |

## 1. Token

A link is identified by its `link_id`: 16 bytes from `crypto.getRandomValues`, written as 22 characters of unpadded base64url. The link's URL is `<origin>/s/<link_id>`. The `link_id` is the only secret that grants access. It is the primary key of `share_links` in the Account DO (spec v0 §4.2), and the owner API returns it so that the owner can copy a link again later.

A guess succeeds with probability `n / 2^128` for n live links. With a million live links and a billion guesses, that is below 3 × 10^-24. Rate limiting (§5) bounds guesses further, but the entropy alone makes guessing infeasible. Rate limiting exists to bound cost and load.

The Worker accepts only a `link_id` of exactly 22 base64url characters whose last character leaves the four unused bits zero (one of `A`, `Q`, `g` or `w`), so that one 16-byte value has exactly one spelling. Any other value gets the same `NOT_FOUND` reply as an unknown link, without any Durable Object call.

## 2. Owner API

Only the space's owner manages links: the user whose `user_id` is the space's `owner_id` in the Account DO. The caller authenticates with a device access token, as for every other endpoint of spec v0 §15.1. Bodies are protobuf, or proto3 JSON, as there.

| Method and path | Body | Reply |
|---|---|---|
| `POST /v1/spaces/{space_id}/links` | `CreateShareLinkRequest` | `ShareLink`, 201 |
| `GET /v1/spaces/{space_id}/links?cursor=` | none | `ShareLinkList` |
| `DELETE /v1/spaces/{space_id}/links/{link_id}` | none | empty, 204 |

Errors:

| Condition | Error code | HTTP status |
|---|---|---|
| Missing, invalid or expired access token, or a revoked device | `UNAUTHENTICATED` | 401 |
| The caller is not the space's owner, or the space does not exist | `FORBIDDEN` | 403 |
| A field is out of range (below) | `INVALID_ARGUMENT` | 400 |
| The node is not a live node of the space | `NOT_FOUND` | 404 |
| The space already has 1,000 active links | `LIMIT_EXCEEDED` | 413 |
| A rewind holds the space closed (RFC 0003 §4); create only | `RETRY_LATER` | 503 |
| `DELETE` of a `link_id` that is not a link of this space | `NOT_FOUND` | 404 |

**Create.** `node_id` names a live file or folder of the space, the root included. The link points to that node by `node_id`, not by path.
- **Expiry.** With `expires_at_ms` 0 and `no_expiry` false, the link expires 7 days after creation. A nonzero `expires_at_ms` must lie more than one minute and at most 365 days after the request. `no_expiry` true makes a link that never expires, and is refused together with a nonzero `expires_at_ms`. A link never expires unless its owner asked for that.
- **Password.** An empty `password` makes a link without one. Otherwise the password has 8 to 256 Unicode scalar values and is stored as RFC 0001 §8.1 describes. The server never returns or logs it.
- An active link is one that is neither revoked nor expired. A space has at most 1,000 active links.
- `ShareLink.url` is `<origin>/s/<link_id>`, where the origin is the one the request reached.

**List.** All links of the space, active or not, newest first, 100 per page. `expires_at_ms` 0 means no expiry. `revoked_at_ms` 0 means not revoked. Records are kept after a link ends, so the owner can see what was shared. v0 does not prune them.

**Revoke.** Sets `revoked_at_ms`. Revoking a revoked link again changes nothing and returns 204. Revocation is the only change a link takes: to change a link's expiry or password, the owner revokes it and creates another. Revoking works while a rewind holds the space closed, because it touches only the Account DO.

## 3. Public API

These endpoints take no access token. Every reply carries `Cache-Control: no-store`, `X-Robots-Tag: noindex, nofollow` and `Referrer-Policy: no-referrer`. Bodies are proto3 JSON when the request asks for `application/json`, and protobuf otherwise.

| Method and path | Body | Reply |
|---|---|---|
| `GET /s/{link_id}` | none | the viewer page (§8) |
| `GET /v1/share/{link_id}` | none | `ShareInfo` |
| `POST /v1/share/{link_id}/unlock` | `ShareUnlockRequest` | empty, 204, with a session cookie |
| `GET /v1/share/{link_id}/list?dir=&cursor=` | none | `ShareListing` |
| `GET /v1/share/{link_id}/file/{node_id}` | none | the file's bytes (§7) |

`GET /s/{link_id}` serves the same static page for every well-formed `link_id` and does not look the link up. Only the API calls the page makes reveal whether a link works.

Every API request resolves the link in the Account DO first, and then asks the Space DO. Resolution fails in this order:

| Condition | Error code | HTTP status |
|---|---|---|
| The client is over a rate limit (§5) | `RETRY_LATER` with `retry_after_ms` | 429 |
| The link is unknown, revoked or expired | `NOT_FOUND` | 404 |
| The link has a password and the request has no valid session (§4) | `UNAUTHENTICATED` | 401 |
| A rewind holds the space closed | `RETRY_LATER` | 503 |
| The requested node is outside what the link reaches (§6) | `NOT_FOUND` | 404 |

Unknown, revoked and expired links give the same reply, so the reply does not tell a guesser whether a `link_id` ever existed. For a protected link without a session, `GET /v1/share/{link_id}` returns 401 with `ShareInfo` holding only `password_required = true`. It reveals nothing about the shared node, not even its name or kind.

- **`ShareInfo`** describes the link's root node: its kind, name and `node_id` (`root_id`), and for a file its size and modification time in milliseconds. `expires_at_ms` is the link's expiry, or 0.
- **`ShareListing`** lists the live children of the folder `dir` (default: the link's root). Entries are sorted by `name_key`, then `node_id`, at most 500 per page, and each page fits in 1 MiB. `path` lists the folders from the link's root down to `dir`, both included, so the viewer can show where the recipient is. A file link has no listing: `list` on it returns `INVALID_ARGUMENT`.
- An entry's `size` and `mtime_ms` come from the current version of a file and are 0 for a folder.

## 4. Passwords and sessions

`POST /v1/share/{link_id}/unlock` checks the password exactly as RFC 0001 §8 describes, lockout included: 5 consecutive failures lock the link for 15 minutes, a locked link answers `RETRY_LATER` (429) without checking the password, and the state belongs to the link, not to the client. A wrong password answers `UNAUTHENTICATED` (401). An unlock of a link without a password answers 204 and sets no cookie.

A correct password sets a session cookie:

```
Set-Cookie: fb_share=v1.<expires_at_ms>.<mac>; Path=/v1/share/<link_id>; Max-Age=3600; HttpOnly; Secure; SameSite=Strict
```

- `mac` is the base64url HMAC-SHA256, under a key derived from the server key (spec v0 §15.2) with the label `flarebox/share-session/v1`, of `<link_id>|<expires_at_ms>|<password_salt>`, where `password_salt` is the link's salt in base64url.
- A session is valid while `expires_at_ms` (one hour after the unlock) has not passed and the MAC matches. It is bound to one link and to that link's password: a session for one link never opens another.
- The session never replaces the link check. Every request still resolves the link, so a session for a revoked or expired link opens nothing.
- The path covers `/v1/share/<link_id>` and everything below it, and no other link's path ([RFC 6265 §5.1.4](https://www.rfc-editor.org/rfc/rfc6265#section-5.1.4)).
- `SameSite=Strict` keeps the cookie out of cross-site requests. The page and its API calls are same-origin, so the cookie reaches the API, and a download the page starts carries it too.

## 5. Rate limiting

The Account DO counts public requests per client, in memory, in fixed windows. The client is the `CF-Connecting-IP` address, or its /64 prefix for IPv6. Every public API request is counted before the link is looked up. `GET /s/{link_id}` serves a static page and is not counted.

| Limit | Window | Effect when exceeded |
|---|---|---|
| 300 public API requests per client | 1 minute | `RETRY_LATER` (429) until the window ends |
| 30 failed lookups per client (unknown, revoked or expired `link_id`) | 1 hour | `RETRY_LATER` (429) for every public API request until the window ends |

- The Account DO is a single object, so these counts are exact while it runs. They live in memory and start over when the object restarts. That keeps them free of storage writes: on the Workers Free plan, a stored counter would spend the 100,000 rows that a Durable Object account may write per day ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/), accessed 2026-11-10).
- The table holds at most 10,000 clients. When it is full, windows that have ended are dropped first, then the oldest entries.
- The password lockout of §4 is stored and survives restarts. Rate limiting does not replace it.

This RFC uses the Account DO rather than the Workers Rate Limiting binding, for three reasons:
- every public API request already calls the Account DO to check revocation (§9), so the count costs no extra request;
- [the binding's limits are approximate and apply separately in each Cloudflare location](https://developers.cloudflare.com/workers/runtime-apis/bindings/rate-limit/) (accessed 2026-11-10);
- whether the binding is available on the Workers Free plan is still unverified as of 2026-11-10.

Turnstile is left for later: each deployment would need a widget registered for its hostname ([Turnstile plans](https://developers.cloudflare.com/turnstile/plans/), accessed 2026-11-10), which a one-click deploy cannot create.

## 6. What a link reaches

A link reaches its root node and, for a folder, the folder's subtree as it is at the time of each request. A node N is reachable through a link with root R when all of these hold:
- N is live;
- following `parent_id` from N reaches R, and every node on the way, R included, is live.

The Space DO checks this on every listing and download, walking at most 256 levels (the depth limit of spec v0 §13). So:
- A node moved out of the subtree stops being reachable at once. A node moved into it becomes reachable.
- A trashed node is not reachable, and neither is anything below it. Restoring it from the trash makes it reachable again.
- Moving or renaming R itself keeps the link working, because the link follows R's `node_id`.
- If R is trashed or purged, the link reaches nothing, and every request returns `NOT_FOUND`. The link record stays until the owner revokes it. If R is restored, the link works again.

Requests name nodes only by `node_id`. The server never takes a path, an R2 key or a chunk hash from a recipient.

## 7. Downloads

`GET /v1/share/{link_id}/file/{node_id}` returns the current version of a reachable file:

1. The Space DO resolves the version's chunk list and, for each chunk, the key of its current incarnation (RFC 0003 §5, rule 5). Every chunk must be `live` and hold an incarnation. Otherwise the reply is `RETRY_LATER` (503): after a rewind, a chunk whose incarnation was stripped stays unavailable until a device uploads it again (RFC 0003 §5, rule 9).
2. The Worker reads the first chunk from R2, checks its size and its SHA-256 against the chunk list, and only then sends the response headers. A first chunk that is missing or fails the check gives `INTERNAL` (502) and sends no file bytes.
3. The Worker then sends the chunks in order. It reads a chunk only when the recipient is ready for more, so it holds one chunk (at most 16 MiB) at a time. Before reading each later chunk, it resolves the link again in the Account DO; these checks do not count toward the limits of §5. Each chunk is checked in full before any of its bytes are sent.
4. If a later chunk is missing or fails its check, or the link has been revoked or has expired in the meantime, the Worker logs the cause and aborts the response. The recipient's browser reports a failed download: the response has fewer bytes than its `Content-Length`, and no byte of a failed chunk was sent.

The response carries:
- `Content-Type: application/octet-stream` and `Content-Length` set to the version's size;
- `Content-Disposition: attachment`, with the file name both as an ASCII fallback (`filename`) and as UTF-8 (`filename*`, RFC 6266);
- `X-Content-Type-Options: nosniff` and `Content-Security-Policy: default-src 'none'; sandbox`.

A browser therefore saves a shared file instead of rendering it, and an HTML file shared through a link cannot run script on the deployment's origin. v0 has no previews, and no `Range` support or resume.

## 8. Web viewer

The viewer is a static page served with Workers Static Assets, from `server/web/`. It has plain HTML, CSS and JavaScript, no build step, no third-party code, and makes no request to another origin. It is English only. Its footer says that Flarebox is not affiliated with Cloudflare, Inc. It shows:
- a password form when `ShareInfo.password_required` is set;
- for a file link, the file's name, size and a download button;
- for a folder link, the folder's entries with their size and modification time, a path bar, and a download button per file. Pages beyond the first load on request.

Every page and asset of the viewer is served with:

```
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'
Referrer-Policy: no-referrer
X-Content-Type-Options: nosniff
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
Permissions-Policy: camera=(), microphone=(), geolocation=()
X-Robots-Tag: noindex, nofollow
```

The page renders every name with `textContent`, never as HTML. The Worker serves `/s/{link_id}` itself, with these headers, and the assets under `/viewer/` take the same headers from `server/web/_headers`.

## 9. Revocation and expiry

- Every public API request resolves the link in the Account DO, which reads the link's record. So a revocation applies to every request that starts after it, with no cache to expire. Each chunk of a download in progress also resolves the link again (§7), so a revocation stops a running download before its next chunk.
- Expiry is checked the same way, against the Account DO's clock: a link whose `expires_at_ms` is not in the future is expired.
- A session cookie does not outlive its link (§4).

## 10. Rewinds and new histories

Links live in the Account DO, which a rewind does not restore (RFC 0003 §6). A link names a space and a `node_id` and does not depend on the history:
- During a rewind, from admission until the record ends, every public API request for a link into that space returns `RETRY_LATER` (503). The Space DO checks the closure with the Account DO on each public call, so no recipient sees the restored state before completion.
- After a rewind, a link shows the restored state. If its root did not exist at the restore point, or is trashed there, the link reaches nothing (§6). If the root exists, its subtree is the restored one.
- Links created after the restore point keep working when their root exists in the restored state. Revocations made after the restore point stay in force.
- A download that was running when the rewind began fails at its next chunk, because the link resolves to `RETRY_LATER`.
- Chunk keys come from the current history's rows on every request (§7), so a download never reads a retired incarnation.

## 11. Size limits

| Item | Limit | Reason |
|---|---|---|
| `link_id` | 16 random bytes | §1 |
| Active links per space | 1,000 | Bounds the owner's list |
| Expiry | Default 7 days, at most 365 days unless `no_expiry` | A forgotten link ends by default |
| Password | 8 to 256 Unicode scalar values | Lockout allows 480 guesses per link per day |
| Listing page | 500 entries and 1 MiB | The bound of a server message (spec v0 §7.2) |
| Chunks per downloaded file | 4,096 (one `GetVersionChunks` page) | About two subrequests per chunk, under the Workers Paid limit of 10,000 per request |

A file with more than 4,096 chunks (at least 4 GiB, and about 16 GiB with average chunks) answers `LIMIT_EXCEEDED` (413), and the viewer says the file is too large to download through a link.

[Workers limits](https://developers.cloudflare.com/workers/platform/limits/) (accessed 2026-11-10) give a Workers Free request 10 ms of CPU and 50 subrequests, and a Workers Paid request up to 5 minutes of CPU and 10,000 subrequests. Hashing each chunk (§7) spends CPU in proportion to the file's size, and each chunk costs about two subrequests, so on the Free plan a download can stop after a few MiB. The exact size depends on the CPU and is unverified, so on the Free plan, links suit small files.

## 12. Whole-folder download (proposed, not in v0)

Downloading a whole folder as one zip file needs:
- a streaming zip writer with ZIP64, because a folder can exceed 4 GiB;
- names stored as UTF-8 with the zip's UTF-8 flag;
- a bound on files and chunks per request, under the same subrequest and CPU limits as §11, or a way to split the work across requests;
- the same per-chunk check and revocation check as §7.

The zip would use the stored method (no compression), so the Worker only copies verified bytes. The open questions are the bound and what the recipient sees when a folder exceeds it. This RFC defers whole-folder download to a later RFC.

## 13. Not covered

- Creating links from the macOS app. The API in §2 is what the app and CLI call.
- Turnstile (§5), previews, `Range` and resumed downloads, bulk revocation, editing a link, and pruning old link records.
- Links for members other than the owner. Spec v0 has one owner per space.

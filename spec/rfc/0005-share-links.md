# RFC 0005: Read-only share links

Status: proposed. Applies to spec v0 (format_version 1).

The owner of a space can share one file or one folder through a link. A recipient opens the link in a browser, browses the folder, and downloads files. The recipient installs nothing and needs no account. Links are read-only. A link can have an expiry and a password. This RFC defines the owner API, the public API, the web viewer, and what a link can reach. A bare section number such as §4 refers to this RFC; sections of spec v0 are written as spec v0 §N. Passwords follow RFC 0001 §8, with the one change in §4.

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

The `link_id` is part of every public URL. If the deployment keeps request logs, such as Workers Logs or a Logpush job, those logs hold the `link_id`s of the links that were used. Only members of the owner's Cloudflare account can read those logs, and they can read the bucket as well. The server itself never logs a `link_id`, a password or a session.

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
- **Expiry.** With `expires_at_ms` 0 and `no_expiry` false, the link expires 7 days after creation. A nonzero `expires_at_ms` must lie more than one minute and at most 365 days after the request. `no_expiry` true makes a link that never expires, and is refused together with a nonzero `expires_at_ms`. So only a link created with `no_expiry` lives forever.
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

Every request to `GET /v1/share/{link_id}`, `list` and `file` resolves the link in the Account DO first, and then asks the Space DO. Resolution fails in this order:

| Condition | Error code | HTTP status |
|---|---|---|
| The client is over a rate limit (§5) | `RETRY_LATER` with `retry_after_ms` | 429 |
| The link is unknown, revoked or expired | `NOT_FOUND` | 404 |
| The link has a password and the request has no valid session (§4) | `UNAUTHENTICATED` | 401 |
| A rewind holds the space closed | `RETRY_LATER` | 503 |
| The requested node is outside what the link reaches (§6) | `NOT_FOUND` | 404 |

`POST /v1/share/{link_id}/unlock` needs no session, because it is the request that creates one. It is counted for rate limiting and refused for an unknown, revoked or expired link like the others, and then checks the password (§4).

Unknown, revoked and expired links give the same reply, so the reply does not tell a guesser whether a `link_id` ever existed. For a protected link without a session, `GET /v1/share/{link_id}` returns 401 with `ShareInfo` holding only `password_required = true`. It reveals nothing about the shared node, not even its name or kind.

- **`ShareInfo`** describes the link's root node: its kind, name and `node_id` (`root_id`), and for a file its size and modification time in milliseconds. `expires_at_ms` is the link's expiry, or 0.
- **`ShareListing`** lists the live children of the folder `dir` (default: the link's root). Entries are sorted by `name_key`, then `node_id`, at most 500 per page, and each page fits in 1 MiB. `path` lists the folders from the link's root down to `dir`, both included, so the viewer can show where the recipient is. A file link has no listing: `list` on it returns `INVALID_ARGUMENT`.
- An entry's `size` and `mtime_ms` come from the current version of a file and are 0 for a folder.

## 4. Passwords and sessions

`POST /v1/share/{link_id}/unlock` checks the password exactly as RFC 0001 §8 describes, lockout included, with one change: the public API reports an unknown, revoked or expired link as `NOT_FOUND` (404), where RFC 0001 §8.2 rule 1 says `FORBIDDEN`. A locked link answers `RETRY_LATER` (429). A wrong password answers `UNAUTHENTICATED` (401). An unlock of a link without a password answers 204 and sets no cookie. Only `/unlock` requests count as password attempts: an info, list or file request without a valid session gets 401 and changes neither the failure count nor the lock.

A correct password sets a session cookie:

```
Set-Cookie: fb_share=v1.<expires_at_ms>.<mac>; Path=/v1/share/<link_id>; Max-Age=3600; HttpOnly; Secure; SameSite=Strict
```

- `mac` is the base64url HMAC-SHA256, under a key derived from the server key (spec v0 §15.2) with the label `koidrive/share-session/v1`, of `<link_id>|<expires_at_ms>|<password_salt>`, where `password_salt` is the link's salt in base64url.
- A session is valid while `expires_at_ms` (one hour after the unlock) has not passed and the MAC matches. It is bound to one link and to that link's password: a session for one link never opens another.
- The session never replaces the link check. Every request still resolves the link, so a session for a revoked or expired link opens nothing.
- The path covers `/v1/share/<link_id>` and everything below it, and no other link's path ([RFC 6265 §5.1.4](https://www.rfc-editor.org/rfc/rfc6265#section-5.1.4)).
- `SameSite=Strict` keeps the cookie out of cross-site requests. The page and its API calls are same-origin, so the cookie reaches the API, and a download the page starts carries it too.

## 5. Rate limiting

The Account DO counts public requests per client, in memory, in fixed windows. These counts are best-effort load control. They are not a durable quota and not a cost cap: they start over when the object restarts, and a client dropped from a full table starts over too. Resistance to guessing comes from the entropy of §1, and resistance to password guessing from the durable lockout of §4. The client is the `CF-Connecting-IP` address, or its /64 prefix for IPv6. Every public API request is counted before the link is looked up. `GET /s/{link_id}` serves a static page and is not counted.

| Limit | Window | Effect when exceeded |
|---|---|---|
| 300 public API requests per client | 1 minute | `RETRY_LATER` (429) until the window ends |
| 30 failed lookups per client (unknown, revoked or expired `link_id`) | 1 hour | `RETRY_LATER` (429) for every public API request until the window ends |

- The Account DO is a single object, so these counts are exact while it runs. Keeping them in memory keeps them free of storage writes: on the Workers Free plan, a stored counter would spend the 100,000 rows that a Durable Object account may write per day ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/), accessed 2026-11-10).
- The table holds at most 10,000 clients. When it is full, windows that have ended are dropped first, then the oldest entries.
- The password lockout of §4 is stored, changes atomically, and survives restarts. Rate limiting does not replace it.

This RFC uses the Account DO rather than the Workers Rate Limiting binding, for two reasons:
- every public API request already calls the Account DO to check revocation (§9), so the count costs no extra request;
- [the binding's limits are approximate and apply separately in each Cloudflare location](https://developers.cloudflare.com/workers/runtime-apis/bindings/rate-limit/) (accessed 2026-11-10).

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

1. The Space DO resolves the version's chunk list and, for each chunk, the key of its current incarnation (RFC 0003 §5, rule 5). Every chunk must be `live` and hold an incarnation. Otherwise the reply is `RETRY_LATER` (503): after a rewind, a chunk whose incarnation was stripped stays unavailable until a device uploads it again (RFC 0003 §5, rule 9). The Space DO also returns its `history_id`.
2. The download is fenced to its start. The fence is the space's `history_id` from step 1 and the space's rewind generation from the link's resolution: the number of rewinds the Account DO has ever admitted for the space (RFC 0003 §5, rule 6). Admission raises the generation before any restore can happen, so every rewind changes it, including one that later fails.
3. A file without chunks, an empty file, gets 200 with the response headers below, `Content-Length: 0` and no R2 read, once the link, the session and the reach checks have passed. This holds for a file link and for a file reached through a folder link.
4. Otherwise the Worker opens the first chunk in R2 and checks it (below), and only then sends the response headers. A first chunk that is missing or fails a check gives `INTERNAL` (502) and sends no file bytes.
5. The Worker then sends the chunks in order, opening each one only when the recipient has taken the previous one. Before opening each later chunk, it asks the Space DO to check the fence. The Space DO resolves the link again in the Account DO, which does not count toward the limits of §5, and requires that the link is still valid, the space is open, the generation is unchanged, and its own `history_id` is unchanged.
6. If the fence check fails, or a later chunk is missing or fails a check, the Worker logs the cause and aborts the response. The recipient's browser reports a failed download, because the response has fewer bytes than its `Content-Length`. A response never continues with a chunk list fetched after it started.

The check of a chunk object: it exists, its size equals the chunk's size, and if R2 holds a SHA-256 checksum for it, that checksum equals the chunk's hash. The Worker does not hash the bytes again. Every chunk object is written by the upload relay, which has R2 verify the body against the chunk's SHA-256, and is written only if the key is free, at a key whose incarnation is never reused (RFC 0003 §5). So an object at a key holds the bytes that were uploaded for it. These checks catch a missing object and an object that is not the one uploaded for the key. They do not detect bytes that changed inside R2 after the upload.

The response carries:
- `Content-Type: application/octet-stream` and `Content-Length` set to the version's size;
- `Content-Disposition: attachment`, with the file name both as an ASCII fallback (`filename`) and as UTF-8 (`filename*`, RFC 6266);
- `X-Content-Type-Options: nosniff` and `Content-Security-Policy: default-src 'none'; sandbox`.

A browser therefore saves a shared file instead of rendering it, and an HTML file shared through a link cannot run script on the deployment's origin. A refusal of this endpoint has a plain-text body, because a browser that follows a download link shows it as a page. v0 has no previews, and no `Range` support or resume.

The server tests for this section cover:
- an empty file through a file link and through a folder link: 200, `Content-Length: 0`, the attachment headers, and no R2 read;
- a first chunk that is missing, has the wrong size or another stored checksum: 502 before any file byte;
- a later chunk that is missing or fails a check: the response aborts without any byte of that chunk;
- a file with more chunks than §11 allows: 413.

## 8. Web viewer

`GET /s/{link_id}` serves the viewer page, and its assets are `/viewer/viewer.js` and `/viewer/viewer.css`. The page holds no third-party code and makes no request to another origin. It has no inline script or style, since the CSP below forbids both. [Build the share-link viewer](../../docs/share-links-web.md) describes the web package `server/web/` that builds it. The viewer is English only. It shows:
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

The page renders every name with `textContent`, never as HTML. The Worker serves `/s/{link_id}` itself, with these headers, and the assets under `/viewer/` take the same headers from `server/web/public/_headers`, which the build copies into its output.

## 9. Revocation and expiry

- Every public API request resolves the link in the Account DO, which reads the link's record. So a revocation applies to every request that starts after it, with no cache to expire. A download in progress resolves the link again before each later chunk (§7), so a revocation stops it before its next chunk.
- Expiry is checked the same way, against the Account DO's clock: a link whose `expires_at_ms` is not in the future is expired.
- A session cookie does not outlive its link (§4).

## 10. Rewinds and new histories

Links live in the Account DO, which a rewind does not restore (RFC 0003 §6). A link names a space and a `node_id` and does not depend on the history:
- During a rewind, from admission until the record ends, every public API request for a link into that space returns `RETRY_LATER` (503). The Space DO checks the closure with the Account DO on each public call, so no recipient sees the restored state before completion. `/unlock` touches only the Account DO and still works.
- After a rewind, a link shows the restored state. If its root did not exist at the restore point, or is trashed there, the link reaches nothing (§6). If the root exists, its subtree is the restored one.
- Links created after the restore point keep working when their root exists in the restored state. Revocations made after the restore point stay in force.
- A download in progress stops before its next chunk once a rewind of its space has been admitted, whether that rewind is still running, has completed or has failed, because the rewind generation changed (§7). A rewind that starts and ends while the recipient's browser is not reading is caught the same way.
- A new request takes its chunk keys from the current history's rows (§7), so it never reads an incarnation that the rewind stripped. A file whose restored version lost an incarnation answers `RETRY_LATER` (503) until a device uploads the chunk again.

The server tests for this section cover:
- a rewind that is admitted while a download runs and is still open: every public request for the space answers 503, and the download aborts before its next chunk without sending bytes of the new history;
- a rewind that runs to completion between two reads of a download: the download aborts before its next chunk, and the bytes it sent are a prefix of the version it started with;
- after that rewind: a link whose root is absent at the restore point reaches nothing, a file whose version is absent at the restore point is not found through a folder link, a file whose restored version lost a stripped incarnation answers 503, and other files serve their restored versions.

## 11. Size limits

| Item | Limit | Reason |
|---|---|---|
| `link_id` | 16 random bytes | §1 |
| Active links per space | 1,000 | Bounds the owner's list |
| Expiry | Default 7 days, at most 365 days unless `no_expiry` | A forgotten link ends by default |
| Password | 8 to 256 Unicode scalar values | Lockout allows 480 guesses per link per day |
| Listing page | 500 entries and 1 MiB | The bound of a server message (spec v0 §7.2) |
| Chunks per downloaded file | 4,096 (one `GetVersionChunks` page) | Two subrequests per chunk (an R2 read and a fence check), under the Workers Paid default of 10,000 per request |

A file with more than 4,096 chunks (at least 4 GiB, and about 16 GiB with average chunks) answers `LIMIT_EXCEEDED` (413), and the viewer says the file is too large to download through a link.

[Workers limits](https://developers.cloudflare.com/workers/platform/limits/#subrequests) (accessed 2026-11-10) give a Workers Free request 50 subrequests, and 1,000 to Cloudflare services such as R2 and Durable Objects. A Workers Paid request gets 10,000 by default. On the Free plan a download therefore stops after about 490 chunks, at least about 490 MiB. The Worker cannot tell which plan it runs on, so it cannot refuse such a download before it starts. The Free plan also gives a request 10 ms of CPU. Streaming copies bytes without hashing them, but whether a large download fits in that budget is unverified.

## 12. Not covered

- Creating links from the macOS app. `koi share` calls the API of §2.
- Downloading a whole folder as one zip file.
- Turnstile (§5), previews, `Range` and resumed downloads, bulk revocation, editing a link, and pruning old link records.
- Links for members other than the owner. Spec v0 has one owner per space.

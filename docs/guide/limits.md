# Limits

Your server runs on your Cloudflare account, so Cloudflare's plan limits apply to it. Flarebox works on the Workers Free plan. The figures below are Cloudflare's, as its documentation stated them on 2026-11-11. Check the linked pages for current values.

## Workers Free plan

| What | Free allowance | Source |
|---|---|---|
| Requests to Workers | 100,000 a day, reset at 00:00 UTC | [Workers limits](https://developers.cloudflare.com/workers/platform/limits/#daily-requests) |
| Durable Object requests | 100,000 a day | [Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/) |
| Durable Object duration | 13,000 GB-s a day | same |
| Durable Object rows read | 5 million a day | same |
| Durable Object rows written | 100,000 a day | same |
| Durable Object storage | 5 GB for the account, 1 GB for each object | [Durable Objects limits](https://developers.cloudflare.com/durable-objects/platform/limits/) |

Daily allowances reset at 00:00 UTC. On the Free plan, an operation past an allowance fails with an error ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)).

Each space is one Durable Object. It stores the space's file tree, its history and its change log, but not file contents. A sync session is a WebSocket to that object, and Cloudflare bills 20 incoming WebSocket messages as one request ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)).

**What Flarebox does at a limit.** When a space's Durable Object reports a daily limit, the server tells devices to wait until the next 00:00 UTC, or 5 minutes in the first half hour after it, and each folder shows the wait in its status. See [Quota waits](status.md#quota-waits). When the limit hits while the server checks the device, the server only says to retry later, so the folder retries without showing a reset time. When the Worker itself is over its daily request limit, Cloudflare answers with error 1027 before Flarebox's code runs ([Workers limits](https://developers.cloudflare.com/workers/platform/limits/#daily-requests)). The folders can't reach the server then, and keep retrying.

## R2

R2 includes free usage each month. Beyond it, Cloudflare bills your account ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)):

| What | Free each month |
|---|---|
| Storage | 10 GB-month |
| Class A operations, such as uploads and listings | 1 million |
| Class B operations, such as downloads | 10 million |
| Data transfer out | Free, without a limit |

R2 holds the contents of your files, of the files in each space's trash and of old versions. `flarebox usage` shows how much each space stores. R2 also holds the server's own exports of each space's file tree, which `flarebox usage` doesn't count. The R2 page of the Cloudflare dashboard shows what you are billed for.

## Flarebox's own limits

| What | Limit |
|---|---|
| A file or folder name | 1 to 255 bytes of UTF-8. Not `.` or `..`, and no `/` |
| A space, device or user name | at most 255 bytes of UTF-8 |
| Folder depth | 256 |
| Trash and old versions | kept for 30 days |
| Pairing code | works once, for 10 minutes |
| Claim code | works for one hour |

## Known issues

A file with a hard link inside an ignored folder may not have its moves synced (GUN-368).

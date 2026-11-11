# Deploy and upgrade design

Status: draft for review. Scope: how an owner deploys the server (`server/`) to an empty account, how the first owner claims a fresh deployment, and how storage and protocol versions move forward when the owner deploys a newer release. Out of scope: a real deploy to an account (it needs credentials), README and website text, and release tagging. Plan: Phase 4 of the implementation plan. Protocol references such as "spec §9.1" point to the v0 protocol draft that `spec/proto` implements.

## Summary

- The deploy button deploys `server/` on its own: the button clones that directory into the owner's Git account as a new repository, creates the Durable Objects and the R2 bucket named in `wrangler.jsonc`, and deploys it with Workers Builds. The owner enters no secrets, but must activate R2 for the account first, a one-time dashboard checkout that no deploy tool can do for them. CI proves that a copy of `server/` outside the monorepo builds, passes a dry-run deploy and can be claimed locally; only a real deploy can prove the account-level steps.
- The first owner claims the deployment with a one-time code that the Worker writes to its own logs, which only account members and tokens allowed to read the Worker's logs can see. Persistent Workers Logs make the code findable after the fact, and query strings are redacted from them so that chunk tickets never persist. A correct code always redeems; wrong guesses are throttled per address and no longer burn the owner's code, and the claim is a single transaction, so the first valid claim wins and every later one is refused.
- An owner who has lost every device recovers with a key they set as a Worker secret. Whoever can set the Worker's secrets can already replace its code, so recovery gives nobody access they did not have. Recovery adds a device; it does not revoke existing devices or sessions.
- Each Durable Object class keeps an append-only list of SQLite migration steps and records how many it has applied. An object applies the missing steps in one transaction when it next starts, so upgrades need no maintenance step and any number of releases can be skipped. An object whose storage is newer than its code refuses to start instead of misreading it, so a rollback across a migration fails closed.
- Clients learn the server release and protocol range from `GET /v1/server` and `Welcome`. The server keeps the previous protocol minor version (N-1). Clients derive upgrade notices from fields that exist today; no spec or proto change is needed.

## 1. Deploy

The button URL names the subdirectory: `https://deploy.workers.cloudflare.com/?url=https://github.com/koidrive/koidrive/tree/<ref>/server`. The button clones that directory as the root of a new repository in the owner's GitHub or GitLab account, lets the owner rename the Worker and the resources, provisions the resources the Wrangler configuration names (Durable Objects and R2 buckets are supported), builds with Workers Builds and deploys ([Deploy buttons](#ref-deploy)). Later pushes to that repository redeploy ([Workers Builds](#ref-builds)).

**R2 must be active on the account first.** A bucket can only be created on an account with an R2 subscription, which the owner adds once in the dashboard (Storage & databases, R2, Overview) by completing a checkout flow; R2 includes free monthly usage and bills usage beyond it ([R2 get started](#ref-r2)). Wrangler cannot do this step: on an account where R2 was never enabled, creating a bucket fails with "Please enable R2 Subscription for your account. [code: 10042]" ([workers-sdk#15468](#ref-r2-cli)). Nothing in `server/` can check it before deploying, because the account is only known to the button and to Workers Builds. The button provisions the bucket during the deploy, so on such an account the deploy should stop at provisioning with that error and no Worker running; the owner completes the checkout and runs the deploy again. What the button's page shows in that case is *unverified* and is the first item of the real-deploy checklist below. The owner guide puts R2 activation before the button.

What this needs from `server/`:

- **Self-contained.** Its own `package.json`, lockfile and LICENSE, generated protobuf code committed under `src/gen`, and no imports from outside the directory, because the button treats the subdirectory as the repository root and the build fails otherwise ([Deploy buttons](#ref-deploy), Limitations). A CI job copies `server/` to an empty directory, installs from the lockfile and runs `wrangler deploy --dry-run`, which bundles the Worker and validates the configuration without credentials.
- **Default names for every resource.** `wrangler.jsonc` names the Worker (`koidrive`), both Durable Object classes and the R2 bucket. The single Durable Object migration tag `v1` creates `AccountDO` and `SpaceDO` as SQLite classes. Tags are append-only (spec §9.4); the deploy check fails if a released tag is renamed or removed.
- **No secrets.** There is no `.dev.vars.example`, because the button would ask the owner for every secret it lists ([Deploy buttons](#ref-deploy)). The Account DO creates the server key on first use.
- **Persistent logs without credentials.** `observability.enabled` is set so that Worker logs, the claim code included, are stored and searchable in the dashboard under the Worker's Observability tab ([Workers Logs](#ref-logs)). Without it the code is visible only to someone watching real-time logs at the moment it was issued. Invocation logs record each request's URL, and relay URLs carry one-hour bearer chunk tickets in their query strings, so `observability.redact_query_string` is set as well (its default is false in the Wrangler 4.158.0 configuration schema), and the deploy check fails without it. The server's own log lines never contain tickets, tokens, refresh credentials or recovery keys; a test checks that for an upload and a download. Logs are kept for 3 days on Workers Free and 7 days on Workers Paid ([Workers Logs](#ref-logs)), and anyone with the Workers Metadata Read-Only role or higher can read them ([Workers roles](#ref-roles)). The claim code is the only secret logged on purpose: it is useful only before the deployment has an owner, for at most an hour.
- **Explicit deploy script.** `package.json` gets `"deploy": "wrangler deploy"`, which the button pre-fills, and descriptions of the bindings under `cloudflare.bindings`, which the setup page shows. That text is user-facing, so it ships as a draft for the project owner's approval.

**Real-deploy checklist.** A dry run and a local claim prove the bundle and the configuration, not the account-level steps. Before the button is published, a credentialed run should check, each *unverified* until then:

1. An account where R2 was never enabled: the deploy stops with a clear R2 subscription error and no Worker running; after the checkout, running the deploy again succeeds.
2. An empty account with R2 active: the button creates the Worker, both Durable Object classes and the bucket, and `/setup` then the claim work.
3. A release tag as `<ref>` in the button URL.
4. The pnpm version Workers Builds uses for `packageManager: pnpm@12`.
5. The dashboard shows the claim code in the Observability tab, and request URLs there have no query strings.

 Phase 0 suggested a separate distribution repository for the button; it is not needed while the subdirectory builds on its own, and it stays an option if the cloned tests and tooling turn out to be a burden for owners.

## 2. First owner

**Threat model.** Assume a stranger finds the Worker's URL before the owner opens it. The stranger must not become the owner, must not take over later, and should not be able to stop the owner from claiming. The trust root is access to the account that hosts the Worker: reading its logs, or changing its settings.

**Claim (from #8).** While the deployment has no owner, `GET /setup` makes the Account DO issue a one-time code (12 characters from a 32-symbol alphabet, 60 bits, valid for one hour) and the Worker writes it to its log. Anyone who can read that log while the code is valid can make the first claim: account members and API tokens with the Workers Metadata Read-Only role or higher, not only administrators ([Workers roles](#ref-roles)). Owners who share their account should claim right after deploying. `POST /v1/claim` with that code creates the owner and their first device and returns its credentials (spec §15.2). Four gaps:

1. Without persistent logs the code is lost unless someone was watching live logs (fixed by §1).
2. Wrong attempts are counted per code, so ten guesses from anyone delete the owner's code, and a stranger can repeat that after every new code.
3. Two concurrent `/setup` requests can store two codes, and a claim compares against only one of them.
4. An owner who loses every device has no way back short of changing the code.

**Changes** (in their own pull request, because they are security behaviour):

- A correct claim code or recovery key always redeems. Only wrong attempts are throttled: each client address (`CF-Connecting-IP`, which the edge sets ([HTTP headers](#ref-headers))) may send ten wrong codes in a ten-minute window, and further wrong codes from it get `RETRY_LATER` until the window ends. Requests without the header, which only happen off the edge, share one separate bucket that no real address can fall into. Failure rows expire with their window and are capped in number. Wrong codes never delete the code, so nobody behind the owner's network or anywhere else can stop the owner's claim. Resistance to guessing comes from the code's entropy, not the throttle: a billion wrong guesses succeed with probability about 10^9 / 2^60 ≈ 9 × 10^-10.
- Issuing a code and redeeming it each run as one synchronous transaction, with every fallible step (hashing, the new device's credentials and its access token) done before it. Codes are compared as fixed-length SHA-256 digests with `crypto.subtle.timingSafeEqual`. At most one code is live; the first valid claim creates the owner and their first device together; every later claim is refused with `FORBIDDEN` whatever code it carries. Nothing resets the claimed state: not time, restarts, upgrades or `/setup`.
- **Recovery** requires the owner. The owner generates a random key, for example with `openssl rand -base64 32` (256 bits from a cryptographic generator), sets it as the Worker secret `KOIDRIVE_RECOVERY_KEY` in the dashboard or with `wrangler secret put` ([Secrets](#ref-secrets)), and sends it as the code of a `POST /v1/claim`. On a claimed deployment that request returns credentials for a new device of the existing owner; it does not create a user or delete data. On a deployment without an owner, a correct, unused key makes its sender the owner, as a claim code does, and voids any code `/setup` issued; `koi setup` uses this to claim a server it has just deployed. Recovery is for lost devices: it does not revoke existing devices or sessions, so a device someone else holds keeps working until it is revoked. The one exception is a claim that carries a `setup_attempt`, which `koi setup` sends when it retries a claim: it revokes the owner's earlier devices from the same attempt that have never refreshed a token. The key's consumption and the new device are stored in one transaction, so a failure leaves the key unused. A key value works once (the Account DO keeps its hash), and the owner deletes the secret afterward. Because a correct key always redeems, the key's randomness is what keeps strangers out while it is set; the server cannot check randomness, so it refuses keys shorter than 32 characters as a floor against mistakes, and the owner guide tells owners to generate the key rather than choose one. Anyone who can set the Worker's secrets can already deploy different code, so this grants nothing new. On a claimed deployment, a wrong key counts against the client's address and is throttled the same way whether the Worker has a key, a key that is too short, or none, so the answers do not show whether a key is set. The rejected alternative, a recovery code in the logs like the claim code, would hand the owner's files to anyone with read-only log access.

Recovery reuses `ClaimRequest` and its endpoint, so it needs no proto change; spec §15.2 needs a sentence for it and for the per-address attempt count, which the project owner should approve with the pull request. The owner revokes the lost devices from the new one (spec/rfc/0009-device-revocation.md).

## 3. Upgrades

### 3.1 Storage

- `server/src/migrations.ts` runs a list of steps (SQL or a function) and records the count under `schema_version` in a key-value table the first step creates: `meta` for the Space DO, `settings` for the Account DO. Each object runs it in its constructor inside `transactionSync`, so a failed step leaves the previous version and the object retries on its next start. Objects migrate one at a time when they next start; a deployment never needs a migration command.
- Steps are append-only. A merged step never changes, because objects that already ran it would not run it again; the tests pin the schema digest after every step. A step must leave a database that already has its effect unchanged (`IF NOT EXISTS`, column checks), so that storage written by older unversioned builds migrates in place.
- The Account DO had no version until now. Its first two steps are the schemas of #8 and #25, verbatim with `IF NOT EXISTS`, so storage from either build starts at version 0 and ends at the current schema.
- If the stored version is higher than the code's step count, the constructor throws and every request to that object fails. Rolling back to an older release is safe only while no new step has run; after that the owner rolls forward. This is what spec §9.4 means by forward-only.
- A step that rewrites many rows must fit in one object start. A data migration too large for that runs in batches from an alarm and keeps its own progress marker; none exists yet.
- The space `format_version` (spec §9.2) is separate. Storage migrations never change it; only an explicit owner action does.
- During a deploy, each Durable Object runs one version at a time while Worker instances may still run the previous one ([Gradual deployments](#ref-gradual)), so Worker-to-Durable-Object RPC methods change additively within a release: add a method or an optional argument, remove the old one a release later.

### 3.2 Versions clients see

- `SERVER_VERSION` (`server/src/version.ts`) equals the version in `server/package.json`; a test keeps them equal. Clients read it from `ServerInfo.server_version` and `Welcome.server_version`.
- `PROTO_MIN` and `PROTO_MAX` are in `ServerInfo`. `Hello` carries the client's range; the server picks the highest common minor version and reports it in `Welcome.proto`, or sends `Goodbye` with code 4002 and its range when there is none (spec §9.1). A test keeps `PROTO_MIN ≤ max(1, PROTO_MAX − 1)`, so a release always serves clients one minor version behind.

### 3.3 Upgrade notices

Clients decide from fields that exist today. The client knows its own range; the server's comes from `GET /v1/server`.

| Condition | Who sees it | Notice |
|---|---|---|
| `Goodbye` 4002 and the client's `proto_max` is below the server's `proto_min` | anyone on that device | Blocking: update this app. |
| `Goodbye` 4002 and the client's `proto_min` is above the server's `proto_max` | anyone on that device | Blocking: the server is too old for this app; the owner must update the server. |
| `Welcome.proto` is below the client's `proto_max` | the owner | The server runs an older release (`server_version`); update it. |
| The server's `proto_max` is above the client's `proto_max` | anyone on that device | Update this app soon; the next server release may stop serving it. |
| `Welcome.latest_release` is not empty | the owner | A newer server release exists. |

`latest_release` stays empty for now. Filling it means the Worker asks GitHub for the latest release, for example once a day; spec §4.2 already lists a setting for it. Whether that check is on by default is the project owner's decision. Without it, the first three rows still tell an owner that a newer client met an older server.

### 3.4 How owners upgrade

- **Deployed with the button.** The owner's repository is a copy, not a fork, so GitHub offers no "Sync fork" ([Deploy buttons](#ref-deploy), [workers-sdk discussion #14307](#ref-sync)). The owner replaces its files with `server/` from the new release, keeps the `name` and `bucket_name` values in `wrangler.jsonc` (the button writes the names chosen at deploy time there, and a different name deploys a new, empty Worker), and pushes; Workers Builds deploys it.
- **Deployed with Wrangler.** Check out the release, put the Worker `name` and `bucket_name` the deployment uses back into `server/wrangler.jsonc` if they differ from the defaults (the release's file has the defaults, and deploying with them creates a new, empty Worker), then in `server/` run `pnpm install --frozen-lockfile` and `pnpm run deploy`.
- Storage migrates as objects start; clients reconnect and see the new `server_version`. Releases can be skipped. Downgrades across a migration are refused (§3.1).

A workflow shipped as `server/.github/workflows/update.yml` would become an "Update Koidrive" action in the owner's copy that fetches a release, keeps the owner's names and pushes. It is a proposal for a follow-up, not part of this work.

## 4. Tests

All run in local workerd through the Vitest integration; none needs an account.

- **Migrations** (`server/test/migrations.test.ts`): for each class and each earlier schema version, including the unversioned Account DO storage of #8 and #25, the test rewrites an object's storage to that version, puts a row in every table, restarts the object twice and checks that the schema equals a fresh object's, the version is current and every row survived. It then checks that an upgraded Space DO accepts a commit and an upgraded Account DO can be claimed, that storage from a newer release is refused, and that the schema after every step matches its pinned digest.
- **Protocol** (`server/test/protocol.test.ts`): the reported versions and the N-1 invariant; the highest common version is chosen; a client one minor version behind is served; clients newer or older than the server's range get `Goodbye` 4002 naming the range.
- **Bootstrap** (with the claim changes): the first claim wins and concurrent claims produce one owner; later claims are refused; a correct code redeems even from a throttled address; throttling expires and missing addresses get their own bucket; recovery is refused without the secret, with a wrong key and with a used key, returns a device of the existing owner with the right key, and leaves the key unused when storing the device fails.
- **Deploy**: the isolated copy, configuration checks (persisted logs with redacted query strings among them), dry-run deploy and local claim in CI, each step with a time limit; and a test that no server log line carries a ticket, token or credential during an upload and a download.

## 5. Pull requests

1. This note, versioned storage migrations and the migration and protocol tests.
2. Claim hardening and recovery (security behaviour, separate approval).
3. Deploy configuration (persisted logs with redacted query strings, deploy script, binding descriptions), the isolated deploy check in CI, the log test, and a draft owner guide under `docs/`.

## 6. Decisions for the project owner

1. Recovery through a Worker secret (§2), and the spec §15.2 sentence for it.
2. Whether the server checks for new releases by default (§3.3).
3. Whether to add the "Update Koidrive" workflow (§3.4).
4. The binding descriptions the deploy page shows (§1).

## References

- <a id="ref-deploy"></a>Cloudflare. "Deploy to Cloudflare buttons." [Link](https://developers.cloudflare.com/workers/platform/deploy-buttons/), last updated 2026-10-12, accessed 2026-11-10.
- <a id="ref-builds"></a>Cloudflare. "Workers Builds." [Link](https://developers.cloudflare.com/workers/ci-cd/builds/), checked in the Phase 0 verification on 2026-11-09.
- <a id="ref-logs"></a>Cloudflare. "Workers Logs." [Link](https://developers.cloudflare.com/workers/observability/logs/workers-logs/), last updated 2026-10-12, accessed 2026-11-10.
- <a id="ref-headers"></a>Cloudflare. "Cloudflare HTTP headers", `CF-Connecting-IP`. [Link](https://developers.cloudflare.com/fundamentals/reference/http-headers/), accessed 2026-11-10.
- <a id="ref-r2"></a>Cloudflare. "Get started", Before you begin. R2 docs. [Link](https://developers.cloudflare.com/r2/get-started/#before-you-begin), accessed 2026-11-10.
- <a id="ref-r2-cli"></a>cloudflare/workers-sdk issue #15468, "R2: support an explicit first-time subscription enablement flow in Wrangler" (opened 2026-09-02, open). [Link](https://github.com/cloudflare/workers-sdk/issues/15468), accessed 2026-11-10.
- <a id="ref-roles"></a>Cloudflare. "Workers roles and permissions." [Link](https://developers.cloudflare.com/workers/authorization/workers/), accessed 2026-11-10.
- <a id="ref-secrets"></a>Cloudflare. "Secrets." [Link](https://developers.cloudflare.com/workers/configuration/secrets/), accessed 2026-11-10.
- <a id="ref-gradual"></a>Cloudflare. "Gradual deployments", Durable Objects. [Link](https://developers.cloudflare.com/workers/configuration/versions-and-deployments/gradual-deployments/), accessed 2026-11-10.
- <a id="ref-sync"></a>cloudflare/workers-sdk discussion #14307, a request for a fork relationship for button deploys. [Link](https://github.com/cloudflare/workers-sdk/discussions/14307), checked in the Phase 0 verification on 2026-11-09.

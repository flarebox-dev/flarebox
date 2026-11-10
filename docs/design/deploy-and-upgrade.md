# Deploy and upgrade design

Status: draft for review. Scope: how an owner deploys the server (`server/`) to an empty account, how the first owner claims a fresh deployment, and how storage and protocol versions move forward when the owner deploys a newer release. Out of scope: a real deploy to an account (it needs credentials), README and website text, and release tagging. Plan: Phase 4 of the implementation plan. Protocol references such as "spec §9.1" point to the v0 protocol draft that `spec/proto` implements.

## Summary

- The deploy button deploys `server/` on its own: the button clones that directory into the owner's Git account as a new repository, creates the Durable Objects and the R2 bucket named in `wrangler.jsonc`, and deploys it with Workers Builds. The owner enters no secrets. CI proves that a copy of `server/` outside the monorepo builds and passes a dry-run deploy.
- The first owner claims the deployment with a one-time code that the Worker writes to its own logs, which only people with access to the account can read. Persistent Workers Logs make the code findable after the fact. Wrong guesses no longer let a stranger burn the owner's code, and the claim is a single transaction, so the first valid claim wins and every later one is refused.
- An owner who has lost every device recovers with a key they set as a Worker secret. Whoever can set the Worker's secrets can already replace its code, so recovery gives nobody access they did not have.
- Each Durable Object class keeps an append-only list of SQLite migration steps and records how many it has applied. An object applies the missing steps in one transaction when it next starts, so upgrades need no maintenance step and any number of releases can be skipped. An object whose storage is newer than its code refuses to start instead of misreading it, so a rollback across a migration fails closed.
- Clients learn the server release and protocol range from `GET /v1/server` and `Welcome`. The server keeps the previous protocol minor version (N-1). Clients derive upgrade notices from fields that exist today; no spec or proto change is needed.

## 1. Deploy

The button URL names the subdirectory: `https://deploy.workers.cloudflare.com/?url=https://github.com/flarebox-dev/flarebox/tree/<ref>/server`. The button clones that directory as the root of a new repository in the owner's GitHub or GitLab account, lets the owner rename the Worker and the resources, provisions the resources the Wrangler configuration names (Durable Objects and R2 buckets are supported), builds with Workers Builds and deploys ([Deploy buttons](#ref-deploy)). Later pushes to that repository redeploy ([Workers Builds](#ref-builds)).

What this needs from `server/`:

- **Self-contained.** Its own `package.json`, lockfile and LICENSE, generated protobuf code committed under `src/gen`, and no imports from outside the directory, because the button treats the subdirectory as the repository root and the build fails otherwise ([Deploy buttons](#ref-deploy), Limitations). A CI job copies `server/` to an empty directory, installs from the lockfile and runs `wrangler deploy --dry-run`, which bundles the Worker and validates the configuration without credentials.
- **Default names for every resource.** `wrangler.jsonc` names the Worker (`flarebox`), both Durable Object classes and the R2 bucket. The single Durable Object migration tag `v1` creates `AccountDO` and `SpaceDO` as SQLite classes. Tags are append-only (spec §9.4); the deploy check fails if a released tag is renamed or removed.
- **No secrets.** There is no `.dev.vars.example`, because the button would ask the owner for every secret it lists ([Deploy buttons](#ref-deploy)). The Account DO creates the server key on first use.
- **Persistent logs.** `observability.enabled` is set so that Worker logs, the claim code included, are stored and searchable in the dashboard under the Worker's Observability tab ([Workers Logs](#ref-logs)). Without it the code is visible only to someone watching real-time logs at the moment it was issued.
- **Explicit deploy script.** `package.json` gets `"deploy": "wrangler deploy"`, which the button pre-fills, and descriptions of the bindings under `cloudflare.bindings`, which the setup page shows. That text is user-facing, so it ships as a draft for the project owner's approval.

Unverified until a real deploy: whether a tag works as `<ref>` in the button URL, and which pnpm version Workers Builds uses for `packageManager: pnpm@12`. Phase 0 suggested a separate distribution repository for the button; it is not needed while the subdirectory builds on its own, and it stays an option if the cloned tests and tooling turn out to be a burden for owners.

## 2. First owner

**Threat model.** Assume a stranger finds the Worker's URL before the owner opens it. The stranger must not become the owner, must not take over later, and should not be able to stop the owner from claiming. The trust root is access to the account that hosts the Worker: reading its logs, or changing its settings.

**Claim (from #8).** While the deployment has no owner, `GET /setup` makes the Account DO issue a one-time code (12 characters from a 32-symbol alphabet, 60 bits, valid for one hour) and the Worker writes it to its log. `POST /v1/claim` with that code creates the owner and their first device and returns its credentials (spec §15.2). Four gaps:

1. Without persistent logs the code is lost unless someone was watching live logs (fixed by §1).
2. Wrong attempts are counted per code, so ten guesses from anyone delete the owner's code, and a stranger can repeat that after every new code.
3. Two concurrent `/setup` requests can store two codes, and a claim compares against only one of them.
4. An owner who loses every device has no way back short of changing the code.

**Changes** (in their own pull request, because they are security behaviour):

- Wrong attempts count per code and per client address (`CF-Connecting-IP`, which the edge sets ([HTTP headers](#ref-headers))). Ten wrong guesses lock out that address for that code, not the code. Guessing stays hopeless: a million addresses with ten guesses each succeed with probability about 10^7 / 2^60 ≈ 9 × 10^-12.
- Issuing a code and redeeming it each run as one synchronous transaction, with the hashing done before it. At most one code is live; the first valid claim creates the owner; every later claim is refused with `FORBIDDEN` whatever code it carries. Nothing resets the claimed state: not time, restarts, upgrades or `/setup`.
- **Recovery** requires the owner. The owner sets a Worker secret `FLAREBOX_RECOVERY_KEY` (at least 32 characters) in the dashboard or with `wrangler secret put` ([Secrets](#ref-secrets)) and sends it as the code of a `POST /v1/claim`. On a claimed deployment that request returns credentials for a new device of the existing owner; it does not create a user, delete data or revoke devices. A key value works once (the Account DO keeps its hash), and the owner deletes the secret afterwards. Anyone who can set the Worker's secrets can already deploy different code, so this grants nothing new. The rejected alternative, a recovery code in the logs like the claim code, would hand the owner's files to anyone with read-only log access.

Recovery reuses `ClaimRequest` and its endpoint, so it needs no proto change; spec §15.2 needs a sentence for it and for the per-address attempt count, which the project owner should approve with the pull request. Revoking the lost devices needs a device-management route, which does not exist yet.

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
- **Deployed with Wrangler.** Check out the release, then in `server/` run `pnpm install --frozen-lockfile` and `pnpm run deploy`.
- Storage migrates as objects start; clients reconnect and see the new `server_version`. Releases can be skipped. Downgrades across a migration are refused (§3.1).

A workflow shipped as `server/.github/workflows/update.yml` would become an "Update Flarebox" action in the owner's copy that fetches a release, keeps the owner's names and pushes. It is a proposal for a follow-up, not part of this work.

## 4. Tests

All run in local workerd through the Vitest integration; none needs an account.

- **Migrations** (`server/test/migrations.test.ts`): for each class and each earlier schema version, including the unversioned Account DO storage of #8 and #25, the test rewrites an object's storage to that version, puts a row in every table, restarts the object twice and checks that the schema equals a fresh object's, the version is current and every row survived. It then checks that an upgraded Space DO accepts a commit and an upgraded Account DO can be claimed, that storage from a newer release is refused, and that the schema after every step matches its pinned digest.
- **Protocol** (`server/test/protocol.test.ts`): the reported versions and the N-1 invariant; the highest common version is chosen; a client one minor version behind is served; clients newer or older than the server's range get `Goodbye` 4002 naming the range.
- **Bootstrap** (with the claim changes): the first claim wins and concurrent claims produce one owner; later claims are refused; another address's wrong guesses do not lock the owner's code; recovery is refused without the secret, with a wrong key and with a used key, and with the right key returns a device of the existing owner.
- **Deploy**: the isolated copy and dry-run deploy in CI.

## 5. Pull requests

1. This note, versioned storage migrations and the migration and protocol tests.
2. Claim hardening and recovery (security behaviour, separate approval).
3. Deploy configuration (observability, deploy script, binding descriptions), the isolated dry-run deploy in CI, and a draft owner guide under `docs/`.

## 6. Decisions for the project owner

1. Recovery through a Worker secret (§2), and the spec §15.2 sentence for it.
2. Whether the server checks for new releases by default (§3.3).
3. Whether to add the "Update Flarebox" workflow (§3.4).
4. The binding descriptions the deploy page shows (§1).

## References

- <a id="ref-deploy"></a>Cloudflare. "Deploy to Cloudflare buttons." [Link](https://developers.cloudflare.com/workers/platform/deploy-buttons/), last updated 2026-10-12, accessed 2026-11-10.
- <a id="ref-builds"></a>Cloudflare. "Workers Builds." [Link](https://developers.cloudflare.com/workers/ci-cd/builds/), checked in the Phase 0 verification on 2026-11-09.
- <a id="ref-logs"></a>Cloudflare. "Workers Logs." [Link](https://developers.cloudflare.com/workers/observability/logs/workers-logs/), last updated 2026-10-12, accessed 2026-11-10.
- <a id="ref-headers"></a>Cloudflare. "Cloudflare HTTP headers", `CF-Connecting-IP`. [Link](https://developers.cloudflare.com/fundamentals/reference/http-headers/), accessed 2026-11-10.
- <a id="ref-secrets"></a>Cloudflare. "Secrets." [Link](https://developers.cloudflare.com/workers/configuration/secrets/), accessed 2026-11-10.
- <a id="ref-gradual"></a>Cloudflare. "Gradual deployments", Durable Objects. [Link](https://developers.cloudflare.com/workers/configuration/versions-and-deployments/gradual-deployments/), accessed 2026-11-10.
- <a id="ref-sync"></a>cloudflare/workers-sdk discussion #14307, a request for a fork relationship for button deploys. [Link](https://github.com/cloudflare/workers-sdk/discussions/14307), checked in the Phase 0 verification on 2026-11-09.

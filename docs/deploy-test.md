# Testing a deploy on a real account

> Flarebox is not affiliated with Cloudflare, Inc. This guide is for maintainers; it is not part of the owner guide.

The **Deploy test** workflow (`.github/workflows/deploy-test.yml`) deploys `server/` to a real Cloudflare account under a throwaway name, claims it, tests it over HTTPS and deletes everything again. It covers what CI's dry run and local `wrangler dev` checks cannot (the real-deploy checklist in [design/deploy-and-upgrade.md](design/deploy-and-upgrade.md)): Durable Objects and R2 on Cloudflare's network, the claim code in the Worker's own log, point-in-time recovery for a rewind, and an upgrade over existing data. It runs only when started by hand from `main`, because it needs the account's API token, and only after you approve it.

## What a run does

Every run uses one name, `flarebox-test-<run id>-<run attempt>`, for the Worker and for its R2 bucket, and the run deletes only resources it recorded creating. The scripts are in `server/scripts/deploy-test/`.

1. **Trust check.** A first job, which gets no secrets, refuses the run unless it was started from `main` of `flarebox-dev/flarebox` for a commit on `main`, and unless the `deploy-test` environment is protected as described under [Set up the protected environment](#set-up-the-protected-environment). Only then does the job that holds the secrets wait for your approval.
2. **Preflight.** Checks that no Worker and no R2 bucket has the run's name. If either exists, the run stops before creating anything and teardown deletes nothing.
3. **Deploy.** After the preflight, copies the tracked files of `server/` at the commit under test (`git archive`), sets the Worker `name` and the `bucket_name` in `wrangler.jsonc` to the run's name, as an owner renames them on the deploy button's page, creates the bucket, then runs `pnpm install --frozen-lockfile` and `pnpm run deploy`, the deploy command the button uses. Wrangler creates both Durable Object classes from the configuration's migrations. The button provisions the bucket itself; Wrangler does not create a bucket that has a name ([automatic provisioning](https://developers.cloudflare.com/workers/wrangler/configuration/#automatic-provisioning), accessed 2026-11-10), so the run creates it first. The run records the bucket's creation date as soon as it created it. It records the Worker's immutable ID only when no Worker has the name right before its own `pnpm run deploy` and the Worker exists once that command succeeded, even if a later step fails. A Worker that appears while dependencies install stops the run, and a failed install or deploy records no Worker, so teardown leaves any Worker with the name alone and fails the job. It then checks through the API that the new version takes all traffic and waits for `/v1/server` to answer.
4. **Claim.** Starts `wrangler tail` on the Worker, opens `/setup`, reads the one-time claim code from the Worker's log and redeems it with `POST /v1/claim`. No person is involved. The code is never printed, and Wrangler's diagnostic log file, which by default records every tail message ([Wrangler system environment variables](https://developers.cloudflare.com/workers/wrangler/system-environment-variables/), accessed 2026-11-10), is turned off with `WRANGLER_WRITE_LOGS=false` and pointed at a directory that is deleted afterwards. The step fails if any file in Wrangler's directories or the deployed copy still holds the code's log line.
5. **Smoke test** (mode `smoke`), a Vitest suite in Node (`smoke.ts`) that stops at the first failure:
   - `/v1/server` reports an owner, `/setup` keeps the notice, and a second claim is refused;
   - a second device pairs and refreshes its own token, and both devices list a new space;
   - chunks upload through the Worker into R2, which checks each SHA-256 (a body with other bytes is refused), files are committed, and committed chunks are not uploaded again;
   - the second device reads the tree and downloads every file;
   - a share link to a file without a password serves its viewer page, its info and the file; a link to a folder with a password serves nothing before unlocking, lists and downloads after it, and stops when revoked;
   - R2 holds the chunks and the space's metadata export: `space.json` and the first log segment, which the server writes five minutes after the first commit (RFC 0004);
   - the space is rewound to a point between two commits on the real Durable Object (RFC 0003): the result names the restored seq, the tree is the one at that point, a commit from the old history is refused, the new history's snapshot and descriptor appear in R2, and a new commit lands.
6. **Upgrade** (mode `upgrade`), in place of the smoke test: deploys the earlier release, claims it, pairs a second device and writes a space with files and a share link (`upgrade-write.ts`), then deploys the commit under test to the same Worker and bucket and checks (`upgrade-verify.ts`) that both devices' credentials still refresh, the space keeps its head, tree and file contents, the old share link still serves its file, and the old space takes new commits and a new link. Each Durable Object runs its missing schema steps when it first starts on the new code and fails every request if a step fails ([design §3.1](design/deploy-and-upgrade.md#31-storage)), so when the commit under test adds schema steps, the objects that the earlier release wrote answer only if those steps ran. When both commits have the same number of schema steps, no step runs, and a passing upgrade shows only that the data survives the new code. The `upgrade-verify.ts` log gives each class's schema step count before and after, so check it to see which case a run covered.
7. **Teardown**, which runs whatever happened before, as long as the secrets are set. It deletes only what this run recorded creating: the Worker only if the Worker now carrying the run's name has the recorded ID, the bucket only if it has the recorded creation date. Anything else with the name is left alone and fails the job. It deletes the Worker with `force=true`, which also deletes the Durable Object namespaces it implements ([Delete Worker](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/methods/delete/), accessed 2026-11-10), checks that no namespace of the Worker is left, then deletes every object in the bucket and the bucket, which must be empty to be deleted ([Delete Bucket](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/methods/delete/), accessed 2026-11-10). It refuses any name that does not start with `flarebox-test-`, and fails the job if anything it created is left.
8. **Usage summary**, in the log and on the run's summary page: the test client's request count and bytes, the objects and bytes in R2 at teardown (chunks and export separately), and, from the GraphQL Analytics API, Worker requests, subrequests and errors, Durable Object requests and stored bytes, and R2 operations by type. Analytics figures can lag by several minutes, so a run's own last requests may be missing from them; an unavailable figure is reported as such and does not fail the run.

A smoke run takes about 15 minutes, most of it waiting for the export's log segment. It stores about 2 MiB in R2 and makes about 60 requests to the Worker, and deletes all of it at the end.

## Before the first run

The account needs:
- **R2 activated**, as for any owner (see [deploy.md](deploy.md), What you need). Without it, creating the bucket fails with code 10042 and the run stops before deploying.
- **A `workers.dev` subdomain.** Every account has one that is configurable in the dashboard ([workers.dev](https://developers.cloudflare.com/workers/configuration/routing/workers-dev/), accessed 2026-11-10); if the account has never opened **Workers & Pages**, open it once. Without a subdomain the deploy prints no address and the run stops.

## Set up the protected environment

The token can deploy and delete every Worker and bucket in the test account, so it must reach only code that was merged to `main`, and only when you approve. The `deploy-test` environment already has this configuration and both secrets (checked 2026-11-10). The steps below give the required configuration. Use them to check the environment after any change to it, or to recreate it before adding the secrets again if someone deletes it. They are repository settings that the workflow checks but cannot apply itself, and the first job refuses the run unless all of them hold.

1. Open the repository on GitHub and click **Settings**.
2. In the left sidebar, click **Environments**, then **New environment**.
3. Type `deploy-test` and click **Configure environment**.
4. Select **Required reviewers**, add `gunwooterry` and nobody else, and leave **Prevent self-review** unselected, because you both start and approve runs. Click **Save protection rules**. Any one listed reviewer can approve a run ([Required reviewers](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments#required-reviewers), accessed 2026-11-10), so the workflow refuses a list with anyone else on it.
5. Deselect **Allow administrators to bypass configured protection rules** and click **Save protection rules**. Administrators can bypass the rules by default ([Allow administrators to bypass configured protection rules](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments#allow-administrators-to-bypass-configured-protection-rules), accessed 2026-11-10). Check this setting by hand whenever the environment changes, because the workflow cannot confirm it (see below).
6. Under "Deployment branches and tags", choose **Selected branches and tags**. Click **Add deployment branch or tag rule**, choose **Branch** as the ref type, type `main` and click **Add rule**. Add no other rule.

Each run then waits on its run page until you click **Review deployments**, select `deploy-test` and **Approve and deploy**. An environment that GitHub creates on its own has no protection rules, which is why the workflow refuses to use one that lacks them ([Managing environments for deployment](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments), accessed 2026-11-10). The workflow's own checks run code from the commit being tested, so they are a second line of defense; the boundary is your approval of a run from `main`. The environments API documentation does not describe the administrator bypass setting, so the first job refuses the run only when the API reports `can_admins_bypass` as `true`; a missing field is not evidence that bypass is off. For this repository the API reported `false` on 2026-11-10.

## Secrets

The `deploy-test` environment already holds both under **Environment secrets** (checked 2026-11-10). To replace one, for example when rolling the token, update it there or from a terminal with `gh secret set <NAME> --env deploy-test`, which reads the value from standard input. Never add them as repository or organization secrets: workflows that run for a pull request from a branch of this repository receive those, so anyone who can push a branch could change a workflow to read them. Environment secrets reach a job only after the environment's rules pass. Never paste the values anywhere else; the workflow never prints them.

| Secret | Holds |
|---|---|
| `CLOUDFLARE_API_TOKEN` | An account API token with the permissions below, limited to the test account. |
| `CLOUDFLARE_ACCOUNT_ID` | The 32-character ID of the test account, shown under **Account ID** on the account's Workers & Pages overview. |

Without either secret the deploy job's first step fails with a message naming the missing ones, and nothing is deployed.

## The token's permissions

Create a custom token (**Manage account**, **Account API tokens**, **Create Token**, **Custom token**). Under **Account Resources**, include only the test account. Give it these permissions and nothing else; no zone permissions, no account-wide administrator role and never the Global API Key:

| Permission | API name | Used to |
|---|---|---|
| Account, Workers Scripts, Edit | Workers Scripts Write | check that no Worker has the run's name and read the new Worker's ID, deploy the Worker and its Durable Object classes, read its deployments, tail its log for the claim code, list Durable Object namespaces and delete the Worker |
| Account, Workers R2 Storage, Edit | Workers R2 Storage Write | check that no bucket has the run's name, create the bucket and read its creation date, list, read and delete its objects, and delete the bucket |
| Account, Account Analytics, Read | Account Analytics Read | read the usage figures of the summary (optional: without it they show as unavailable) |

The API reference lists the permission each call accepts: Workers Scripts Write for [reading a Worker](https://developers.cloudflare.com/api/resources/workers/subresources/beta/subresources/workers/methods/get/), [deleting a Worker](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/methods/delete/), [starting a tail](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/subresources/tail/methods/create/), [listing deployments](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/subresources/deployments/methods/list/) and [listing Durable Object namespaces](https://developers.cloudflare.com/api/resources/durable_objects/subresources/namespaces/methods/list/); Workers R2 Storage Write for [creating](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/methods/create/) and [deleting a bucket](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/methods/delete/) and [deleting an object](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/subresources/objects/methods/delete/). The Analytics API takes Account Analytics Read ([Configure an Analytics API token](https://developers.cloudflare.com/analytics/graphql-api/getting-started/authentication/api-token-auth/)). All pages accessed 2026-11-10. *Unverified until the first run:* that `wrangler deploy` and `wrangler tail` need nothing beyond Workers Scripts Edit when `CLOUDFLARE_ACCOUNT_ID` is set, and that [reading a bucket](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/methods/get/), [listing objects](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/subresources/objects/methods/list/) and [reading an object](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/subresources/objects/methods/get/) accept Workers R2 Storage Edit, whose reference pages name no permission. A missing permission fails the call with HTTP 403 and the l... (line truncated to 2000 chars)

Anyone holding this token can deploy, read and delete every Worker and bucket in the account, so use an account that holds nothing else of value, give the token an expiry date, and roll it if a run log ever shows it.

## Running it

Open **Actions**, **Deploy test**, **Run workflow**, keep `main` under "Use workflow from", pick the mode, then approve the deployment on the run page. Or use the GitHub CLI:

```
gh workflow run deploy-test.yml --ref main -f mode=smoke
gh workflow run deploy-test.yml --ref main -f mode=upgrade -f from_ref=<commit or tag>
```

For `upgrade`, `from_ref` is the release to upgrade from. Left empty, it is the newest tag before the commit under test; there is no release tag yet, so until the first release the run needs `from_ref`, an ancestor of the commit under test at or after `8c9bf4a`, the first commit whose `server/` deploys on its own. Runs never overlap: a second run waits for the first.

## If teardown fails

The job fails and names what is left: something this run created that it could not delete, or something with the run's name that it did not create and left alone. Delete the run's own leftovers by hand: the Worker `flarebox-test-<run id>-<attempt>` under **Workers & Pages**, which removes its Durable Objects, then empty and delete the R2 bucket of the same name under **R2 object storage**. A run cancelled normally still tears down, because GitHub keeps running a step whose condition is `always()` ([Workflow cancellation reference](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-cancellation), accessed 2026-11-10). A force-cancelled run skips that condition ([Force cancel a workflow run](https://docs.github.com/en/rest/actions/workflow-runs#force-cancel-a-workflow-run), accessed 2026-11-10), and a runner that dies outright never reaches teardown, so check for `flarebox-test-` Workers and buckets after a force-cancelled run or an infrastructure failure.

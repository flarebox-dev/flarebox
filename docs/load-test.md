# Load testing a deploy on a real account

> This guide is for maintainers; it is not part of the owner guide.

The **Load test** workflow (`.github/workflows/load-test.yml`) deploys `server/` to the deploy test's Cloudflare account under a throwaway name, claims it, commits to it from up to 400 paired devices in steps of rising load, reads what the Durable Objects did from the Analytics API, and deletes everything again. It answers how many commits per second one space takes before its latency turns up, and whether more spaces add commits per second or share one limit.

It uses the Deploy test's protected `deploy-test` environment, secrets, trust checks, deploy, claim and teardown ([deploy-test.md](deploy-test.md)). The scripts are `load.ts`, `load-model.ts` and `workers-plan.ts` in `server/scripts/deploy-test/`.

## What a run does

1. **Trust check.** The same checks as the Deploy test: the run must start from `main` of `koidrive/koidrive` for a commit on `main`, and the `deploy-test` environment must be protected. Then the job waits for your approval.
2. **Plan check.** Workers Free allows 100,000 Durable Object rows written a day ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/), accessed 2026-11-12), which one load step uses up within minutes. `workers-plan.ts` reads the account's subscriptions and goes on only if one of them is a paid Workers plan. The deploy-test token has no billing permission, so the read is normally refused; the run then goes on only if it was started with `workers_paid=confirmed`. Nothing is deployed when the check fails.
3. **Deploy and claim**, exactly as in the Deploy test, for the commit the run started from.
4. **Setup.** The owner creates the spaces the last step needs. Each space gets two stored 1 KiB chunks and one small file per writer. The owner then pairs one device for every writer and for every subscriber.
5. **Calibration.** Five writers commit for 2 minutes. The run reads the rows the Durable Objects wrote in that time from the Analytics API, divides by the commits, and sets the run's commit budget: `max_rows_written` divided by 1.25 times the rows per commit. If the Analytics API reports no rows within 10 minutes, the run assumes 60 rows per commit. When the budget runs out, the writers stop and the steps end.
6. **Steps.** At each step the run starts the step's writers, waits `warmup_seconds`, then measures for `measure_seconds`. Each writer is its own paired device with its own WebSocket. It sends Hello and then one commit at a time, each a single `UpdateContent` of its own file that switches between the two stored chunks, so no commit touches R2 and no two writers touch the same node. Writers do not send Resume, so the server pushes nothing to them. Each space has one live subscriber (Hello, then Resume), so every commit still pays for one push and one Account DO check of the subscribers. Writers send Reauth before their access token expires. During every step the owner times `GET /v1/spaces` every 2 seconds. Both the token check in the Worker and the listing ask the Account DO and nothing else, so this probe shows how long the Account DO keeps requests waiting. The run stops after a step in which more than 10% of commits failed, the median commit took over 10 seconds, or no commit was accepted.
7. **Analytics.** Three minutes after the last step, the run reads each step's measuring window from `durableObjectsInvocationsAdaptiveGroups` and `durableObjectsPeriodicGroups` by namespace and object, and the Space DO's stored bytes from `durableObjectsSqlStorageGroups`. It waits up to 20 minutes for a storage sample taken after the load. The field names come from the GraphQL schema ([Durable Objects metrics and analytics](https://developers.cloudflare.com/durable-objects/observability/metrics-and-analytics/), which points to introspection, accessed 2026-11-12).
8. **Results.** A table in the log and on the run's summary page, and `load-results.json` as an artifact of the run (kept 30 days). It holds every step's counts, the calibration, the storage samples and a timeline of commits.
9. **Teardown and usage summary**, exactly as in the Deploy test, whatever happened before.

## Scenarios

| Input | Steps | Each step | Answers |
|---|---|---|---|
| `s1` | Writers: 1, 5, 10, 25, 50, 100, 200 | One space | Commits per second of one space |
| `s4` | Spaces: 1, 4, 16 | 25 writers in each space | Whether commits per second grow with spaces, or stop at a limit the spaces share |

The `steps` input replaces the list, for example `1,5` for a short trial. A step may have at most 400 writers in all, because one runner holds every connection.

## Reading the table

| Column | Meaning |
|---|---|
| Commits/s | Accepted commits in the measuring window divided by its length |
| Commit ms | From sending a commit to its answer, at the runner, including the network to Cloudflare |
| Failed | Refused commits by error code, and commits with no answer within 60 seconds or before the socket closed |
| Closes | WebSocket closes by code that the run did not ask for, and failed reconnects |
| Push lag | From the commit's `committed_at_ms` to its arrival at the subscriber; runner and server clocks may differ by some milliseconds |
| Account probe | Time of `GET /v1/spaces` |
| Generator CPU | CPU time of the run's one Node.js process over the window. Near 100%, the runner limits the numbers, not the server |
| Account DO CPU | CPU time of the Account DO over the window, and its requests per second. The deployment has one Account DO, and one object runs on one thread, so near 100% means it is the limit |
| Busiest SpaceDO CPU | CPU time of the busiest Space DO over the window |
| Rows written per commit | Rows written by every Durable Object in the window divided by accepted commits |

Analytics figures are samples and can include a few seconds outside a window. Commit times are measured at the runner, so they include the network between the GitHub-hosted runner and Cloudflare.

## Cost

Every run needs the account on Workers Paid. Usage beyond the monthly allowance is billed: for example $1.00 per million rows written after 50 million, and $0.15 per million Durable Object requests after 1 million ([pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/), accessed 2026-11-12). The commit budget keeps a run's rows written under `max_rows_written` (40 million by default), but rows written by other runs in the same month count against the same allowance, so lower it when the account has used part of its allowance. Check the account's usage under **Workers & Pages** before a run and after it.

## Running it

Open **Actions**, **Load test**, **Run workflow**, keep `main`, choose the inputs, then approve the deployment on the run page. Or:

```
gh workflow run load-test.yml --ref main -f scenario=s1 -f workers_paid=confirmed
gh workflow run load-test.yml --ref main -f scenario=s4 -f workers_paid=confirmed
gh workflow run load-test.yml --ref main -f scenario=s1 -f steps=1,5 -f warmup_seconds=30 -f measure_seconds=60 -f workers_paid=confirmed
```

Runs of this workflow wait for each other; they do not wait for Deploy test runs, which use other names. A full `s1` run takes about two hours.

*Unverified until the first run:* that the subscriptions read is refused for the deploy-test token, and that the Analytics API fields above return data for this account.

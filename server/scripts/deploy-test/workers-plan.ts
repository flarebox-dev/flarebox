// SPDX-License-Identifier: AGPL-3.0-or-later

// Refuses the load test (docs/load-test.md) before it deploys anything unless the test account is on Workers Paid. It
// reads the account's subscriptions; the deploy-test token has no billing permission, so when the read is refused it
// takes the word of whoever started the run instead (WORKERS_PAID=confirmed). Usage, from server/, with
// CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID set:
//   node scripts/deploy-test/workers-plan.ts

import { requireAccount, subscriptions } from "./cloudflare.ts";
import { workersPlan } from "./load-model.ts";

let listed: { name: string; price: number; state: string }[] | undefined;
let unreadable = "";
try {
  listed = (await subscriptions(requireAccount())).map((subscription) => ({
    name: subscription.rate_plan?.public_name || subscription.rate_plan?.id || subscription.id,
    price: subscription.price ?? 0,
    state: subscription.state ?? "",
  }));
} catch (error) {
  unreadable = error instanceof Error ? error.message : String(error);
}
const plan = workersPlan(listed, unreadable, process.env.WORKERS_PAID === "confirmed");
if (plan.ok) {
  console.log(plan.message);
} else {
  console.error("::error::" + plan.message);
  process.exitCode = 1;
}

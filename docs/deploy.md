# Deploying and upgrading a Koidrive server

> **Draft.** Not reviewed for publication; do not link it from the README or the website yet. This guide cannot be followed end to end yet: the steps marked *not yet available* need app and CLI support that does not exist, and steps marked *unverified* have not been run against a real account. Koidrive is not affiliated with Cloudflare, Inc.

This guide is for the owner: the person who deploys the server to their own Cloudflare account and uses it first. The design behind it is in [design/deploy-and-upgrade.md](design/deploy-and-upgrade.md).

## What you need

- A Cloudflare account with R2 activated. If you have never used R2, open the dashboard, go to **Storage & databases**, **R2**, **Overview**, and complete the checkout flow there. R2 includes free monthly usage and bills usage beyond it ([R2 get started](https://developers.cloudflare.com/r2/get-started/#before-you-begin)). Do this before deploying: neither the deploy button nor Wrangler can do it for you, and without it the bucket cannot be created ([workers-sdk#15468](https://github.com/cloudflare/workers-sdk/issues/15468)).
- A GitHub or GitLab account. The deploy button copies the server into a new repository there.

## Deploy

1. Open the deploy button link for the release you want: `https://deploy.workers.cloudflare.com/?url=https://github.com/koidrive/koidrive/tree/<release>/server` (*unverified*: the link format for a release tag).
2. Sign in, pick the Git account for the new repository, and keep or change the names of the Worker and the R2 bucket. Write the names down: you keep them when you upgrade.
3. Deploy. Cloudflare creates the Worker, its two Durable Objects and the R2 bucket, builds the server and deploys it. You enter no secrets.
4. Note the Worker's address, for example `https://koidrive.<your-subdomain>.workers.dev`.

If the deploy stops with an error about the R2 subscription, R2 is not activated yet: complete the checkout described above and deploy again (*unverified*: what the deploy page shows in this case).

If you prefer the command line: clone the repository, then in `server/` run `pnpm install --frozen-lockfile` and `pnpm run deploy`. Wrangler asks you to sign in.

## Claim your server

Until someone claims it, a new server has no owner, and the first valid claim makes that person the owner for good. Claim it right after deploying.

1. Open `<your server>/setup` in a browser. It writes a one-time claim code, valid for one hour, to the Worker's log.
2. In the Cloudflare dashboard, open **Workers & Pages**, select your Worker, open **Observability** and search for `Koidrive claim code`.
3. Claim the server with `koi server claim <your server> <code> --user "<your name>"`, or enter the address and the code in the setup window of the macOS app. That device becomes your first device ([Set up](guide/setup.md#without-the-cloudflare-login)).

If the code expired, open `/setup` again for a new one. Someone who found your address cannot see the code, and their wrong guesses do not affect yours.

**Who can read the code.** The Worker's logs are kept for 3 days on Workers Free and 7 days on Workers Paid ([Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/)). Anyone in your account, or any API token, with the Workers Metadata Read-Only role or higher can read them ([Workers roles](https://developers.cloudflare.com/workers/authorization/workers/)), and could claim the server while the code is valid. Once the server has an owner, or after an hour, the code is useless. The logs hide request query strings, so they never keep file download or upload links.

## If you lose every device

`koi server add-device` adds a new device for you, with the Cloudflare login and a `koi` built with the server bundle ([Devices](guide/devices.md#if-you-lose-every-device)). It sets a one-time recovery key on your Worker, adds the device to your existing account and deletes the key. Until Koidrive has a public OAuth client, that login works only for the project's test account. Without it, do the same steps by hand, as long as you can still change the Worker's settings:

1. Make a long random key, for example with `openssl rand -hex 32`. It must be at least 32 characters.
2. In the dashboard, open your Worker's **Settings**. Under **Variables and Secrets**, select **Add**, choose the type **Secret**, name it `KOIDRIVE_RECOVERY_KEY` with that value, and select **Deploy** ([Secrets](https://developers.cloudflare.com/workers/configuration/secrets/)). Or, in `server/`, run `pnpm exec wrangler secret put KOIDRIVE_RECOVERY_KEY --name <your Worker>`; without `--name`, Wrangler uses the name in `wrangler.jsonc`, which is `koidrive` unless you changed it.
3. On the new device, claim the server with the key instead of a claim code: `koi server claim <your server> <key> --user "<your name>"`. The device joins your existing account, which keeps its name. A new secret can take a moment to reach the server: if the claim says the deployment already has an owner right after you set the key, wait a minute and run it again.
4. Delete the secret. Each key works once; set a new one to recover again.

Recovery does not sign out your other devices. If a lost device may be in someone else's hands, it keeps working until you revoke it with `koi device revoke` ([Devices](guide/devices.md#revoke-a-device)).

## Upgrade

Your server keeps working while you upgrade: each part moves its stored data to the new layout the first time it starts on the new release. You can skip releases.

**Deployed with the button.** Your repository is a copy, so it does not update itself.

1. Download the release's source and take its `server/` directory.
2. Replace the files in your repository with it, except that `wrangler.jsonc` must keep your Worker `name` and your `bucket_name`. With different names, Cloudflare deploys a new, empty server next to yours.
3. Commit and push. Cloudflare builds and deploys the new release.

**Deployed from the command line.**

1. Check out the new release.
2. If you deployed under names other than the defaults (`koidrive` for the Worker and the bucket), put your names back into `server/wrangler.jsonc`: the release's file has the defaults, and deploying with them creates a new, empty server next to yours.
3. In `server/`, run `pnpm install --frozen-lockfile` and `pnpm run deploy`.

*Not yet available:* the apps will show a notice when the server is older than they expect, and an app more than one protocol version behind the server, or ahead of it, will stop syncing and say which side to update.

## Going back to an older release

Do not. Once a newer release has moved your stored data forward, an older release refuses to start rather than misread it, and your server stops answering until you deploy that newer release or a later one again.

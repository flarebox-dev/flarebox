# Deploying and upgrading a Flarebox server

> **Draft.** Not reviewed for publication; do not link it from the README or the website yet. Steps marked *unverified* have not been run against a real account. Flarebox is not affiliated with Cloudflare, Inc.

This guide is for the owner: the person who deploys the server to their own Cloudflare account and uses it first. The design behind it is in [design/deploy-and-upgrade.md](design/deploy-and-upgrade.md).

## What you need

- A Cloudflare account. R2 needs a payment method on file.
- A GitHub or GitLab account. The deploy button copies the server into a new repository there.

## Deploy

1. Open the deploy button link for the release you want: \`https://deploy.workers.cloudflare.com/?url=https://github.com/flarebox-dev/flarebox/tree/<release>/server\` (*unverified*: the link format for a release tag).
2. Sign in, pick the Git account for the new repository, and keep or change the names of the Worker and the R2 bucket. Write the names down: you keep them when you upgrade.
3. Deploy. Cloudflare creates the Worker, its two Durable Objects and the R2 bucket, builds the server and deploys it. You enter no secrets.
4. Note the Worker's address, for example \`https://flarebox.<your-subdomain>.workers.dev\`.

If you prefer the command line: clone the repository, then in \`server/\` run \`pnpm install --frozen-lockfile\` and \`pnpm run deploy\`. Wrangler asks you to sign in.

## Claim your server

Until someone claims it, a new server has no owner, and the first valid claim makes that person the owner for good.

1. Open \`<your server>/setup\` in a browser. It writes a one-time claim code, valid for one hour, to the Worker's log.
2. In the Cloudflare dashboard, open **Workers & Pages**, select your Worker, open **Observability** and search for \`Flarebox claim code\`. Only people with access to your account can read this log.
3. Enter the server address and the code in the Flarebox app or run \`flarebox server claim\`. That device becomes your first device.

If the code expired, open \`/setup\` again for a new one. Someone else who found your address cannot see the code; a wrong guess from their address does not invalidate yours.

## If you lose every device

You can add a new device without losing anything, as long as you can still change the Worker's settings.

1. Make a long random key, for example with \`openssl rand -base64 32\` (at least 32 characters).
2. In the dashboard, open your Worker, then **Settings**, **Variables and Secrets**, and add a secret named \`FLAREBOX_RECOVERY_KEY\` with that value. Or run \`wrangler secret put FLAREBOX_RECOVERY_KEY\` in \`server/\`.
3. In the app or the CLI, claim the server again and give the key instead of a claim code. The new device belongs to your existing account.
4. Delete the secret. Each key works once; set a new one if you need to recover again.

## Upgrade

Your server keeps working while you upgrade: each part moves its stored data to the new layout the first time it starts on the new release. You can skip releases.

**Deployed with the button.** Your repository is a copy, so it does not update itself.

1. Download the release's source and take its \`server/\` directory.
2. Replace the files in your repository with it, except that \`wrangler.jsonc\` must keep your Worker \`name\` and your \`bucket_name\`. With a different name, Cloudflare deploys a new, empty server next to yours.
3. Commit and push. Cloudflare builds and deploys the new release.

**Deployed from the command line.** Check out the new release, then in \`server/\` run \`pnpm install --frozen-lockfile\` and \`pnpm run deploy\`.

Your apps show a notice when the server is older than they expect. An app that is more than one protocol version behind the server, or ahead of it, stops syncing and says which side to update.

## Going back to an older release

Do not. Once a newer release has moved your stored data forward, an older release refuses to start rather than misread it, and your server stops answering until you deploy that newer release or a later one again.

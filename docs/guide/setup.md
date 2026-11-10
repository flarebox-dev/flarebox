# Set up

You need one server, in a Cloudflare account you own, and at least one device signed in to it. The first device claims the server and becomes its owner's device. Every other device joins later.

## What you need

- A Cloudflare account with R2 enabled. R2 includes free monthly usage and bills usage beyond it ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)). If R2 isn't enabled, `flarebox setup` opens the R2 page of the dashboard and waits up to 30 minutes while you complete its checkout.
- The `flarebox` command, [built with the server inside it](install.md#build).

## With flarebox setup

> [!WARNING]
> The Cloudflare login needs Flarebox's public OAuth client, which doesn't exist yet. Until it does, only members of the project's test account can log in. Use the steps under [Without the Cloudflare login](#without-the-cloudflare-login) instead.

```sh
flarebox setup
```

`flarebox setup` asks how to set up this machine:

1. Log in with Cloudflare. Your browser opens Cloudflare's login page. Pick the account and select Authorize. Then Flarebox looks for its servers in that account:
   - With no server, it deploys one. It creates an R2 bucket, uploads the server as a Worker, turns on its `workers.dev` address, and claims the server with this device.
   - With one server, it adds this device to it.
   - With several, it asks which one.
2. Enter a pairing code from another device. See [Add another device](#add-another-device).

The login asks for four permissions: reading the account's settings, managing its Workers, writing to its R2 buckets, and keeping access while the command runs (`account-settings.read`, `workers-scripts.admin`, `workers-r2.write` and `offline_access`). Flarebox revokes the login when the command ends. If it can't, it says so, and you can revoke it under Manage OAuth authorizations in the Cloudflare dashboard.

The device takes this machine's host name. Give it another with `--name`.

If a setup stops halfway, for example because the network dropped, run `flarebox setup` again. It finishes from where it stopped.

**On a machine without a browser**, add `--no-browser`. Flarebox prints the login page's address. Open it on any device, log in, and select Authorize. The browser then fails to load a page on `127.0.0.1`. Copy that page's address and paste it into the terminal. To forward the login over SSH instead, pick the callback port with `--port`.

**In a script**, use the commands that never ask questions:

| Command | What it does |
|---|---|
| `flarebox server deploy` | Logs in, deploys a new server and claims it. Stops if the account already has one |
| `flarebox server add-device [--server <worker>]` | Logs in and adds this device to the server in the account. `--server` names the Worker when there are several |
| `flarebox join <code>` | Joins with a pairing code |

## Without the Cloudflare login

Until the public client exists, deploy the server and claim it without `flarebox setup`:

1. Deploy the server with the deploy button or with `pnpm run deploy` in `server/`, as [Deploying and upgrading a Flarebox server](../deploy.md#deploy) describes.
2. Open `<your server>/setup` in a browser. The server writes a one-time claim code, valid for one hour, to the Worker's log. In the Cloudflare dashboard, open your Worker, then Observability, and search for `Flarebox claim code`.
3. Claim the server in the macOS app: enter the server's address and the claim code in its setup window. The app is not published yet, so build it as [apps/macos/README.md](../../apps/macos/README.md) describes.
4. Add the machines that use the command with a pairing code from the app, as the next section describes.

## Add another device

On a device that is already signed in to the macOS app, open Settings, choose Account and click Add a Device. The app shows a pairing code, which works once and for 10 minutes. On the new machine run:

```sh
flarebox join K7QM-2XPD@flarebox.alice.workers.dev
```

A code written as `<code>@<host>` carries the server's address. For a code without it, add the address with `--server https://flarebox.alice.workers.dev`. `flarebox setup <code>` does the same as `flarebox join`.

The command can't create a pairing code yet. With the Cloudflare login, `flarebox server add-device` adds a device without one.

## Create a space

The command can't create a space yet. The macOS app creates one during its setup, named Flarebox unless you change it. `flarebox usage` lists the spaces of your server. Then [link a folder](folders.md#link-a-folder) to one.

## Upgrade the server

```sh
flarebox server upgrade
```

This logs in with Cloudflare and uploads the server version built into your `flarebox`. Your files, devices and settings stay. Open sync sessions move to the new version within 5 minutes. The command stops if the server runs a newer version than your `flarebox`.

A server deployed with the deploy button deploys again whenever its repository changes, and that would replace the upgrade. The command warns you and asks first. Update that repository too, or answer no.

Never go back to an older server version. Once a newer version has moved the server's stored data forward, an older one refuses to start.

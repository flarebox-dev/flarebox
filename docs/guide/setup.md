# Set up

You need one server, in a Cloudflare account you own, and at least one device signed in to it. The first device claims the server and becomes its owner's device. Every other device joins later.

## What you need

- A Cloudflare account with R2 enabled. R2 includes free monthly usage and bills usage beyond it ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)). If R2 isn't enabled, `koi setup` opens the R2 page of the dashboard and waits up to 30 minutes while you complete its checkout.
- The `koi` command, [built with the server inside it](install.md#build).

## With koi setup

> [!WARNING]
> The Cloudflare login needs Koidrive's public OAuth client, which doesn't exist yet. Until it does, only members of the project's test account can log in. Use the steps under [Without the Cloudflare login](#without-the-cloudflare-login) instead.

```sh
koi setup
```

On a terminal, `koi setup` asks how to set up this machine:

1. Log in with Cloudflare. Your browser opens Cloudflare's login page. Pick the account and select Authorize. Then Koidrive looks for its servers in that account:
   - With no server, it deploys one. It creates an R2 bucket, uploads the server as a Worker, turns on its `workers.dev` address, and claims the server with this device.
   - With one server, it adds this device to it.
   - With several, it asks which one.
2. Enter a pairing code from another device, written as `<code>@<host>`. For a code without the host, run `koi join <code> --server <address>` instead. See [Add another device](#add-another-device).

The login asks for four permissions: reading the account's settings, managing its Workers, writing to its R2 buckets, and keeping access while the command runs (`account-settings.read`, `workers-scripts.admin`, `workers-r2.write` and `offline_access`). Koidrive revokes the login when the command ends. If it can't, it says so, and you can revoke it under Manage OAuth authorizations in the Cloudflare dashboard.

The device takes this machine's host name. Give it another with `--name`.

If a setup stops halfway, for example because the network dropped, run `koi setup` again. It finishes from where it stopped.

Without a terminal on stdin and stderr, with `--json`, or with the `CI` environment variable set, `koi setup` asks nothing. It lists the commands below and exits. `koi setup --interactive` asks for the guided setup explicitly, and fails when no terminal is attached.

**On a machine without a browser**, add `--interactive --no-browser`. Koidrive prints the login page's address. Open it on any device, log in, and select Authorize. The browser then fails to load a page on `127.0.0.1`. Copy that page's address and paste it into the terminal. To forward the login over SSH instead, pick the callback port with `--port`.

**In a script**, use the commands that never ask questions:

| Command | What it does |
|---|---|
| `koi server claim <server-url> <code> --user <name>` | Claims a server deployed with the deploy button |
| `koi join <code>` | Joins with a pairing code |

`koi server deploy` and `koi server add-device [--worker <name>]` log in with Cloudflare in a browser, so a person runs them on a terminal with `--interactive`. Without it, with or without `--json`, they open no browser: they stop with `auth_required` and name the same command with `--interactive`. `deploy` deploys a new server and claims it, and stops if the account already has one. `add-device` adds this device to the server in the account without a pairing code, and `--worker` names the Worker when there are several. See [Agents and scripts](agents.md).

## Without the Cloudflare login

Until the public client exists, deploy the server and claim it without `koi setup`:

1. Deploy the server with the deploy button or with `pnpm run deploy` in `server/`, as [Deploying and upgrading a Koidrive server](../deploy.md#deploy) describes.
2. Open `<your server>/setup` in a browser. The server writes a one-time claim code, valid for one hour, to the Worker's log. In the Cloudflare dashboard, open your Worker, then Observability, and search for `Koidrive claim code`.
3. Claim the server with this device, which becomes your first device:

   ```sh
   koi server claim koidrive.alice.workers.dev ABCD-EFGH-JKLM --user "Alice Park"
   ```

   Give the server's address, the claim code and your name. The device takes this machine's host name; give it another with `--name`. A server that already has an owner refuses the claim code.
4. Add your other machines with a pairing code, as the next section describes.

## Add another device

On a device that is already signed in, run:

```sh
koi device invite
```

It prints a pairing code, which works once and for 10 minutes, and the command to run on the new machine:

```
K7QM-2XPD@koidrive.alice.workers.dev
On the new device, run:
  koi join K7QM-2XPD@koidrive.alice.workers.dev
The code works once, for about 10 minutes.
```

Only the code goes to stdout, so `code=$(koi device invite)` captures it. The rest goes to stderr.

In the macOS app, Settings, Account, Add a Device shows a pairing code without the server's address. To use it on a device with the `koi` command, run `koi join <code> --server <address>`.

A code written as `<code>@<host>` carries the server's address. For a code without it, add the address with `--server https://koidrive.alice.workers.dev`.

With the Cloudflare login, `koi server add-device` adds a device without a code.

## Create a space

```sh
koi space create Photos
```

This prints the new space's ID. Each of your spaces needs its own name, because `koi folder link --space` finds a space by its name: the server refuses a name you already use. Names are compared exactly after leading and trailing whitespace is removed, so `Photos` and `photos` are two names. `koi space list` lists your spaces with their IDs, and `--json` returns them as data. Then [link a folder](folders.md#link-a-folder) to a space.

## Upgrade the server

```sh
koi server upgrade
```

This logs in with Cloudflare and uploads the server version built into your `koi`. Without the Cloudflare login, upgrade as [Deploying and upgrading a Koidrive server](../deploy.md#upgrade) describes. Your files, devices and settings stay. Open sync sessions move to the new version within 5 minutes. The command stops if the server runs a newer version than your `koi`.

A server deployed with the deploy button deploys again whenever its repository changes, and that would replace the upgrade. The command warns you and asks first. Update that repository too, or answer no. `--yes` goes on without asking.

Never go back to an older server version. Once a newer version has moved the server's stored data forward, an older one refuses to start.

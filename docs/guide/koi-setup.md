# koi setup

`koi setup` sets up this machine in one run: it connects to a server, signs this machine in as a device, and links a folder. Its Cloudflare login works only for the project's test account, as [Set up with the Cloudflare login](setup.md#set-up-with-the-cloudflare-login) explains. A pairing code works for everyone.

## The guided setup asks two sets of questions

On a terminal, `koi setup` first asks how to connect this machine:

1. Log in with Cloudflare. Your browser opens Cloudflare's login page. Pick the account and select **Authorize**. Then Koidrive looks for its servers in that account:
   - With no server, it deploys one. It creates an R2 bucket, uploads the server as a Worker, turns on its `workers.dev` address, and claims the server with this device.
   - With one server, it adds this device to it.
   - With several, it asks which one.
2. Enter a pairing code from another device, written as `<code>@<host>`. For a code without the host, run `koi join <code> --server <address>` instead.

If R2 isn't turned on in the account, setup opens the R2 page of the dashboard and waits up to 30 minutes while you complete its checkout.

Once this machine is connected, setup asks which folder to sync:

- The shared folder: a space whose files are the same on each of your devices. When the server has none, setup proposes one named `Koidrive`. Otherwise it lists them, adds the end of the ID when two have the same name, and offers to create a new one.
- Where its files go on this machine. The default is `~/Koidrive`, shown as a full path. Press Enter to use it, or type another path. Setup asks before it creates a missing folder or a new shared folder. A folder that already holds files is linked only after you agree: its files upload, and the shared folder's files download into it. A path to a file is refused, and setup asks again.

Setup then links the folder, as `koi folder link` does, and starts background sync. It ends with the folder to add files to and `koi status`, which shows how sync is going. If you cancel a folder question, this machine stays connected and no folder is linked.

The daemon checks the folder again before it links it. If the folder was replaced, or files you didn't approve arrived while setup waited, it refuses the link, and you run `koi setup` again. A shared-folder name that matches several spaces is refused with their IDs. Paths that lead nowhere and folders without write access are refused before a pairing code is used.

After setup succeeds, it asks whether to send anonymous usage reports, unless you already chose or an environment variable decides it. Press Enter for yes. [Usage reports](telemetry.md) lists what they hold.

On an Apple silicon Mac, attended setup also installs the matching release of Koidrive.app in `/Applications`, or `~/Applications` when `/Applications` is not writable, and requests that it open. It verifies the release checksum, the app's Developer ID and notarization before replacing an older copy. A verified equal or newer app needs no download. Add `--no-app` to skip installation and opening. JSON, unattended setup, CI and other platforms skip this step.

If the app step fails, the device stays connected and its linked folders stay linked. Setup reports the completed work and the app failure separately. The JSON `data.app` field reports `skipped`, `present` or `failed`; an incomplete launch, handoff or cleanup appears as `problem` on a `present` result. A requested launch is not confirmation that the app attached. The `data.service` result describes background sync independently.

## Run it again to finish or check

If a setup stops halfway, for example because the network dropped, run `koi setup` again. It finishes from where it stopped. On a machine that is already connected, it doesn't log in or pair again. With a linked folder, it lists the linked folders without changing their links. Without one, it goes on to the folder questions.

## Options

| Option | Effect |
|---|---|
| `--name <name>` | The device's name. The default is this machine's host name |
| `--plain` | Numbered, line-oriented questions without color or animation. `KOI_PLAIN=1`, narrow terminals and `TERM=dumb` do the same. A nonempty `NO_COLOR` turns off color only |
| `--interactive` | Runs the guided setup even where setup would otherwise ask nothing. Fails with exit status 2 without a terminal, and can't be used with `--json` |
| `--no-browser` | Prints the login page's address instead of opening a browser. Needs `--interactive` |
| `--port <port>` | The local port for the login callback, for example to forward it over SSH |
| `--telemetry on` or `off` | Stores the usage-report choice without asking |
| `--no-app` | Skips installing and opening the macOS app |
| `--folder`, `--space`, `--pairing-code-stdin`, `--accept-existing-files` | Run a [scripted setup](#scripted-setup) |

Pairing codes and pasted login callback addresses are hidden while you type.

With `--no-browser`, open the printed address on any device, log in, and select **Authorize**. The browser then fails to load a page on `127.0.0.1`. Copy that page's address and paste it into the terminal.

## Without a terminal, setup asks nothing

Without a terminal on stdin and stderr, with `--json`, or with the `CI` environment variable set, `koi setup` asks nothing. Unless `--folder` asks for a scripted setup, it lists the setup commands and exits 0. With `--json` they are in `data.choices` and `next_commands`.

The commands that never ask questions are `koi server claim <server-url> <code> --user <name>` and `koi join <code>`.

## The Cloudflare login and what it may do

The login asks for four permissions: reading the account's settings, managing its Workers, writing to its R2 buckets, and keeping access while the command runs (`account-settings.read`, `workers-scripts.admin`, `workers-r2.write` and `offline_access`). Koidrive revokes the login when the command ends. If it can't, it says so, and you can revoke it under **Manage OAuth authorizations** in the Cloudflare dashboard.

`koi server deploy --interactive` deploys a new server and claims it, and stops if the account already has one. `koi server add-device --interactive` adds this device to the server in the account without a pairing code. `--worker <name>` names the Worker when there are several. Without `--interactive` and a terminal, these commands open no browser. They stop with `auth_required` (exit 4) and name the same command with `--interactive`.

## Scripted setup

To pair and link a folder in one command, give the pairing code on stdin:

```sh
printf '%s\n' "$KOI_PAIRING_CODE" | koi setup --pairing-code-stdin \
  --folder "$HOME/Koidrive" --space "$KOI_SPACE" --telemetry off --json
```

With `--folder`, setup asks nothing, with or without a terminal. `--space` takes a space's ID or name, as `koi space list` shows it, and must name a space that exists. Setup creates a missing folder. A folder that already holds files needs `--accept-existing-files`: its files upload, and the space's files download into it. Setup checks the arguments, the folder and the code before it uses the code, which works once.

On a machine that is already connected, `--pairing-code-stdin` is optional, and setup links the folder without pairing again. A code for another server is refused. A machine that isn't connected needs the code, or setup stops with `auth_required` (exit 4). Running the same command again changes nothing and reports `unchanged`.

With `--json`, `data` holds `account` (the device this machine is connected as), `folder` (the linked folder, with its `space_id`, `space_name` and `state`), `service` (the background service) and `app` (the macOS app step). [docs/json-schema/v1/koi-setup.json](../json-schema/v1/koi-setup.json) is the schema.

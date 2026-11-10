# Install

There is no packaged release of 0.1.0 yet: no Homebrew formula and no download. Build the `flarebox` command from source on macOS or Linux.

## What you need

- Git.
- Rust, installed with [rustup](https://rustup.rs/). The repository pins the toolchain in `rust-toolchain.toml`, and rustup installs that version the first time you build.
- Node.js 24 and pnpm, to pack the server. `server/package.json` names the pnpm version. Only `flarebox server deploy` and `flarebox server upgrade` use the packed server, but `flarebox setup` deploys through the same code, so pack it unless you only join existing servers.

## Build

```sh
git clone https://github.com/flarebox-dev/flarebox.git
cd flarebox/server
pnpm install --frozen-lockfile
node scripts/pack-bundle.mjs ../target/server-bundle
cd ..
FLAREBOX_SERVER_BUNDLE="$PWD/target/server-bundle" cargo install --locked --path cli
```

`pack-bundle.mjs` writes the server's Worker code, its static files and a manifest into `target/server-bundle`. It needs no Cloudflare account. `cargo install` builds the command with that server inside it and puts `flarebox` in `~/.cargo/bin`. Check it with `flarebox --version`.

A build without `FLAREBOX_SERVER_BUNDLE` works for everything except deploying and upgrading a server. Those commands stop with "this flarebox was built without a server bundle".

## Run the sync daemon

Folders sync only while the daemon runs:

```sh
flarebox daemon
```

It runs in the foreground until you press Ctrl-C or send it SIGTERM. Every other command talks to the daemon through a socket when it runs, and opens the profile itself when it doesn't. So `flarebox link` works without the daemon, and the folder starts syncing when the daemon starts.

The daemon logs at the `info` level. Set `FLAREBOX_LOG` to change it, for example `FLAREBOX_LOG=debug`. `--log-format json` writes one JSON object per line.

**Linux.** The repository has a systemd user unit, [cli/systemd/flarebox.service](../../cli/systemd/flarebox.service). It expects the command in `~/.local/bin`, so copy it there or change `ExecStart`. Then:

```sh
install -Dm644 cli/systemd/flarebox.service ~/.config/systemd/user/flarebox.service
systemctl --user daemon-reload
systemctl --user enable --now flarebox
```

Read its log with `journalctl --user -u flarebox`. To keep syncing while you are logged out, run `loginctl enable-linger "$USER"`.

**macOS.** Flarebox has no launch agent for the daemon yet. Run `flarebox daemon` in a terminal.

## Files on this machine

| Path | What it holds |
|---|---|
| `~/.config/flarebox/config.toml` | Your settings: [selective sync](folders.md#sync-only-part-of-a-space-on-this-device) and the [local trash](folders.md#the-local-trash) size. You edit it. Flarebox never writes it |
| `~/.local/state/flarebox/credential.json` | This device's credential. On macOS, Flarebox keeps it out of Time Machine backups |
| `~/.local/state/flarebox/spaces/` | The sync state of each linked folder |
| `.flarebox/` in each linked folder | The folder's marker, the record of the device that linked it, and the local trash |

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` replace `~/.config` and `~/.local/state` when they are set, on macOS too. The daemon's socket is in `$XDG_RUNTIME_DIR/flarebox/`, or in the state folder when `XDG_RUNTIME_DIR` is unset.

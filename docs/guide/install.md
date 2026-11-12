# Install

Install the `koi` command on Linux or macOS:

```sh
curl -fsSL https://koidrive.com/install.sh | sh
```

Downloads become available with the first published CLI release. Until then, build from source below.

The installer downloads the latest GitHub Release for x86_64 or arm64, checks its SHA256 checksum, and installs it in `~/.local/bin`. It needs `curl`, `tar`, `awk`, `grep`, `sed`, `mktemp`, and either `sha256sum` or `shasum`. It never needs root. Add `~/.local/bin` to your shell's `PATH` if the installer warns that it is missing, then run `koi --version`. The installer also warns when another program named `koi` comes first on `PATH`, such as the Koi theme switcher for KDE; put `~/.local/bin` before it. It refuses to replace a `koi` in the install directory that is another program.

Run the same command again to upgrade. To choose a version or another directory, set `KOIDRIVE_VERSION` or `KOIDRIVE_INSTALL_DIR` on the `sh` command:

```sh
curl -fsSL https://koidrive.com/install.sh | KOIDRIVE_VERSION=v0.1.0 KOIDRIVE_INSTALL_DIR="$HOME/bin" sh
```

`KOIDRIVE_RELEASE_BASE_URL` replaces `https://github.com/koidrive/koidrive/releases` for a trusted mirror or local tests. The base must serve tarballs and `SHA256SUMS` under `latest/download/` and `download/v<version>/`. This changes the source of both the binary and its checksum, so use only a source you trust.

On Linux with systemd, the installer offers to install a user service. For an unattended install, pass `sh -s -- --systemd` to install the unit or `sh -s -- --no-systemd` to skip it. Without a terminal, the default skips the unit. The installer reloads the units but does not start the daemon. Finish setup and link a folder before you enable it, as described below. Restart an existing service after an upgrade with `systemctl --user restart koidrive`.

The release binary includes the server bundle, so deploying a server needs neither Node.js nor Rust. The SHA256 file comes from the same release as the tarball. It detects corruption, not a compromised release account. Tagged macOS builds use the existing Developer ID signing certificate when configured, but the CLI is not notarized. Without the certificate, the workflow reports an unsigned build.

## Build from source

### What you need

- Git.
- Rust, installed with [rustup](https://rustup.rs/). The repository pins the toolchain in `rust-toolchain.toml`, and rustup installs that version the first time you build.
- Node.js 24 and pnpm, to pack the server. `server/package.json` names the pnpm version. Only `koi server deploy` and `koi server upgrade` use the packed server, but `koi setup` deploys through the same code, so pack it unless you only join existing servers.

### Build

```sh
git clone https://github.com/koidrive/koidrive.git
cd koidrive/server
pnpm install --frozen-lockfile
node scripts/pack-bundle.mjs ../target/server-bundle
cd ..
KOIDRIVE_SERVER_BUNDLE="$PWD/target/server-bundle" cargo install --locked --path cli
```

`pack-bundle.mjs` writes the server's Worker code, its static files and a manifest into `target/server-bundle`. It needs no Cloudflare account. `cargo install` builds the command with that server inside it and puts `koi` in `~/.cargo/bin`. Check it with `koi --version`.

A build without `KOIDRIVE_SERVER_BUNDLE` can't find or deploy a server in your Cloudflare account. `koi setup` with the Cloudflare login, `server deploy`, `server add-device`, `server upgrade` and `server delete` without `--bucket` stop with "this koi was built without a server bundle". Everything else works.

## Run the sync daemon

Folders sync only while the daemon runs:

```sh
koi _daemon
```

`_daemon` is the service entry point, so `koi --help` doesn't list it.

It runs in the foreground until you press Ctrl-C or send it SIGTERM. Commands that change this device's folders, such as `folder link`, `folder restart` and `folder retry`, talk to the daemon through a socket when it runs, and open the profile themselves when it doesn't. So `koi folder link` works without the daemon, and the folder starts syncing when the daemon starts. The `device` and `space` commands talk to the server directly. `history trash` and `history versions` read from the server too, but first ask the daemon, when it runs, which space the folder syncs.

The daemon logs at the `info` level. Set `KOIDRIVE_LOG` to change it, for example `KOIDRIVE_LOG=debug`. `--log-format json` writes one JSON object per line.

**Linux.** If the installer installed the systemd user service, enable it after setup and link:

```sh
systemctl --user enable --now koidrive
```

For a source build, the repository has the same user unit, [cli/systemd/koidrive.service](../../cli/systemd/koidrive.service). It expects the command in `~/.local/bin`, so copy it there or change `ExecStart`. Then:

```sh
install -Dm644 cli/systemd/koidrive.service ~/.config/systemd/user/koidrive.service
systemctl --user daemon-reload
systemctl --user enable --now koidrive
```

Read its log with `journalctl --user -u koidrive`. To keep syncing while you are logged out, run `loginctl enable-linger "$USER"`.

**macOS.** Koidrive has no launch agent for the daemon yet. Run `koi _daemon` in a terminal.

## Files on this machine

| Path | What it holds |
|---|---|
| `~/.config/koidrive/config.toml` | Your settings: [selective sync](folders.md#sync-only-part-of-a-space-on-this-device), the [local trash](folders.md#the-local-trash) size and how many [uploads run at once](folders.md#uploads-at-once). You edit it. Koidrive never writes it |
| `~/.local/state/koidrive/credential.json` | This device's credential. On macOS, Koidrive keeps it out of Time Machine backups |
| `~/.local/state/koidrive/spaces/` | The sync state of each linked folder |
| `.koidrive/` in each linked folder | The folder's marker, the record of the device that linked it, and the local trash |

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` replace `~/.config` and `~/.local/state` when they are set to absolute paths, on macOS too. Koidrive ignores a relative value. The daemon's socket is in `$XDG_RUNTIME_DIR/koidrive/`, or in the state folder when `XDG_RUNTIME_DIR` isn't set to an absolute path.

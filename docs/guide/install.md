# Install

Install the `koi` command on Linux or macOS:

```sh
curl -fsSL https://koidrive.com/install.sh | sh
```

Downloads become available with the first published CLI release. Until then, build from source below.

The installer downloads the latest GitHub Release for x86_64 or arm64, checks its SHA256 checksum, and installs it in `~/.local/bin`. It needs `curl`, `tar`, `awk`, `grep`, `sed`, `mktemp`, and either `sha256sum` or `shasum`. It never needs root. Add `~/.local/bin` to your shell's `PATH` if the installer warns that it is missing, then run `koi --version`. The installer also warns when another program named `koi` comes first on `PATH`, such as the Koi theme switcher for KDE; put `~/.local/bin` before it. It refuses to replace a `koi` in the install directory that is another program.

Run the same command again to upgrade. The installer restarts the [background service](#background-sync) of every profile that runs one, so each runs the new `koi`. To choose a version or another directory, set `KOIDRIVE_VERSION` or `KOIDRIVE_INSTALL_DIR` on the `sh` command:

```sh
curl -fsSL https://koidrive.com/install.sh | KOIDRIVE_VERSION=v0.1.0 KOIDRIVE_INSTALL_DIR="$HOME/bin" sh
```

`KOIDRIVE_RELEASE_BASE_URL` replaces `https://github.com/koidrive/koidrive/releases` for a trusted mirror or local tests. The base must serve tarballs and `SHA256SUMS` under `latest/download/` and `download/v<version>/`. This changes the source of both the binary and its checksum, so use only a source you trust.

The release binary includes the server bundle, so deploying a server needs neither Node.js nor Rust. The SHA256 file comes from the same release as the tarball. It detects corruption, not a compromised release account. Tagged macOS builds use the existing Developer ID signing certificate when configured, but the CLI is not notarized. Without the certificate, the workflow reports an unsigned build.

## Install with Homebrew

On macOS or Linux with [Homebrew](https://brew.sh):

```sh
brew install koidrive/tap/koidrive
koi setup
```

To upgrade:

```sh
brew update && brew upgrade koidrive
```

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

## Background sync

Linking the first folder installs a service that runs the sync daemon, starts it, and waits until the daemon answers. If the service can't start, the folder stays linked and the command says why. Unlinking the last folder stops the service and removes it. `koi setup`, `koi join`, `koi folder link` and `koi folder unlink` bring the service in line with the linked folders. To repair a stopped or missing service, run `koi service start`.

When more than one `koi` is installed, for example one in `~/.local/bin` and one inside the macOS app, the newest runs the service. A command that brings the service in line keeps the binary the service runs when that binary reports a version at least as new as its own, and otherwise points the service at itself. An older `koi` hands `koi service start` to the newer binary, so it never opens a profile that a newer version upgraded. When the daemon that runs is older than the binary the service then runs, the command restarts it on that binary.

`koi status` shows the service on its `Background service` line. With `--json`, `status` and the commands above add it as `service`, with `state` set to `running`, `stopped`, `switched_off`, `not_installed`, `unavailable` or `off`, and `problem`, when koi knows the cause, saying why it isn't running.

Give each profile its own `XDG_STATE_HOME`. The service and the daemon's socket are named with a hash of the state folder, so profiles that share `XDG_RUNTIME_DIR` sync side by side. Two profiles that share a state folder can't.

**Linux.** Koidrive uses a systemd user service, `koidrive.service`. It writes the unit, based on [cli/systemd/koidrive.service](../../cli/systemd/koidrive.service), to `~/.local/state/koidrive/systemd/` and enables it with `systemctl --user enable`. Read its log with `journalctl --user -u koidrive`, or with the unit name that `koi status` shows for another profile.

A user service stops when you log out and starts at your next login, unless lingering is on for your user. With lingering, it starts when the machine boots and keeps running after you log out ([systemd/User](https://wiki.archlinux.org/title/Systemd/User)). Sync needs that on a server, so Koidrive turns lingering on with `loginctl enable-linger` when the system allows it without a password. When it doesn't, for example over SSH, the link says so. Then run:

```sh
sudo loginctl enable-linger "$USER"
```

Koidrive never turns lingering off, since other services of yours may need it. Run `loginctl disable-linger` yourself to turn it off.

Containers, WSL without systemd, and shells started with `su` or `sudo -u` have no systemd user session. There the link still succeeds and says that Koidrive can't sync in the background. Log in as the user directly, or turn lingering on and log in again, then run `koi service start`. On a system without systemd, run `koi _daemon` with your own service manager.

**macOS.** Koidrive installs the launch agent `~/Library/LaunchAgents/dev.koidrive.koi.plist` and loads it with `launchctl bootstrap gui/$UID`. It runs while you are logged in and starts again at each login. Its log is in `~/Library/Logs/Koidrive/`. The daemon keeps the log under 10 MiB and moves the previous one to a file ending in `.log.1`. The first time, macOS shows a "Background Items Added" notification. Leave koi switched on in System Settings > General > Login Items > Allow in the Background. If you switch it off, macOS stops the agent, Koidrive leaves it off, and `koi status` shows `switched off in Login Items`. To turn it back on, switch koi on and run `koi service start`. Over SSH, with nobody logged in to the desktop, launchd has no session for the agent, so log in to the Mac once and run `koi service start`.

macOS privacy settings treat the launch agent as a program of its own, so the access you gave Terminal doesn't carry over. When you link a folder in Documents, Desktop, Downloads or iCloud Drive, or on an external or network volume, macOS asks whether koi may access it. Click Allow. If you clicked Don't Allow, `koi status` names the folder. Open System Settings > Privacy & Security > Files & Folders and switch the folder on under koi, or add koi under Full Disk Access. Then run `koi folder restart <folder>`. macOS keeps these permissions across upgrades for a koi signed with a Developer ID. For an unsigned build it asks again after each upgrade.

The macOS app syncs on its own and doesn't use this service. A folder syncs with the app or with the command, whichever linked it: Koidrive refuses to link a folder that the other one syncs.

## Run the daemon yourself

For scripts and machines where something else runs the daemon, turn the service off in `config.toml`:

```toml
[service]
background = false
```

Koidrive then never installs, starts or removes the service, so unlink every folder before you turn it off, and link them again after. Folders then sync only while this runs:

```sh
koi _daemon
```

`_daemon` is the service's entry point, so `koi --help` doesn't list it. It runs in the foreground until you press Ctrl-C or send it SIGTERM. Commands that change this device's folders, such as `folder link`, `folder restart` and `folder retry`, talk to the daemon through a socket when it runs, and open the profile themselves when it doesn't. The `device` and `space` commands talk to the server directly. `history trash` and `history versions` read from the server too, but first ask the daemon, when it runs, which space the folder syncs.

The daemon logs at the `info` level. Set `KOIDRIVE_LOG` to change it, for example `KOIDRIVE_LOG=debug`. `--log-format json` writes one JSON object per line.

## Files on this machine

| Path | What it holds |
|---|---|
| `~/.config/koidrive/config.toml` | Your settings: [selective sync](folders.md#sync-only-part-of-a-space-on-this-device), the [local trash](folders.md#the-local-trash) size, how many [uploads run at once](folders.md#uploads-at-once), and whether the [background service](#background-sync) is on. You edit it. Koidrive never writes it |
| `~/.local/state/koidrive/credential.json` | This device's credential. On macOS, Koidrive keeps it out of Time Machine backups |
| `~/.local/state/koidrive/spaces/` | The sync state of each linked folder |
| `~/.local/state/koidrive/systemd/koidrive.service` | On Linux, the background service's unit |
| `~/Library/LaunchAgents/dev.koidrive.koi.plist` | On macOS, the background service's launch agent. Its log is in `~/Library/Logs/Koidrive/` |
| `.koidrive/` in each linked folder | The folder's marker, the record of the device that linked it, and the local trash |

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` replace `~/.config` and `~/.local/state` when they are set to absolute paths, on macOS too. Koidrive ignores a relative value. The daemon's socket is `$XDG_RUNTIME_DIR/koidrive/daemon-<hash>.sock`, named with a hash of the state folder, or `daemon.sock` in the state folder when `XDG_RUNTIME_DIR` isn't set to an absolute path.

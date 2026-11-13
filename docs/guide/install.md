# Install

Install the `koi` command on Linux (x86_64 or arm64) or on a Mac with Apple silicon:

```sh
curl -fsSL https://koidrive.com/install.sh | sh
```

The installer prints `Installed koi to /home/alice/.local/bin/koi`. Check it with `koi --version`, which prints the version, such as `koi 0.1.0-alpha.1`.

The installer downloads the latest [GitHub release](https://github.com/koidrive/koidrive/releases) for your system, checks it against the release's `SHA256SUMS` file and installs `koi` in `~/.local/bin`. It never needs root. The release includes the server, so you can deploy a server without Node.js or Rust. On macOS, the release's `koi` is signed with a Developer ID and notarized.

## Fix a PATH warning

If the installer warns that `~/.local/bin` is not on `PATH`, add it to your shell profile. If it warns that `koi` on `PATH` runs another program, such as the Koi theme switcher for KDE, put `~/.local/bin` before it. The installer never replaces a `koi` in the install directory that is another program.

## Update Koidrive

```sh
koi update
```

`koi update` asks GitHub for the newest release and upgrades the `koi` on this computer. For a `koi` from the installer, it runs the installer again for the folder that holds `koi`. The new `koi` then takes over the [background service](#background-sync). When this computer already runs the newest release, it prints `Koidrive 0.1.0-alpha.1 is the newest release.` and changes nothing. Running the install command again also upgrades `koi`, and restarts the background service of every profile that runs one.

`koi status` shows an `Update:` line when a newer release is out. [Updates](service.md#updates) covers which install `koi update` upgrades, the daily update check and how to turn it off.

To install a given version or into another directory, set `KOIDRIVE_VERSION` or `KOIDRIVE_INSTALL_DIR` for `sh`:

```sh
curl -fsSL https://koidrive.com/install.sh | KOIDRIVE_VERSION=v0.1.0-alpha.1 KOIDRIVE_INSTALL_DIR="$HOME/bin" sh
```

The installer needs `curl`, `tar`, `awk`, `grep`, `sed`, `mktemp`, and `sha256sum` or `shasum`. `KOIDRIVE_RELEASE_BASE_URL` replaces `https://github.com/koidrive/koidrive/releases` with a mirror that serves the same files. The checksum comes from the same place as the binary, so use only a mirror you trust.

## Build from source

You need Git, Rust installed with [rustup](https://rustup.rs/), Node.js 24 and pnpm. rustup installs the toolchain that `rust-toolchain.toml` pins the first time you build, and `server/package.json` names the pnpm version.

```sh
git clone https://github.com/koidrive/koidrive.git
cd koidrive/server
pnpm install --frozen-lockfile
node scripts/pack-bundle.mjs ../target/server-bundle
cd ..
KOIDRIVE_SERVER_BUNDLE="$PWD/target/server-bundle" cargo install --locked --path cli
```

`pack-bundle.mjs` packs the server into `target/server-bundle`, without a Cloudflare account. `cargo install` builds `koi` with that server inside it and puts it in `~/.cargo/bin`. Check it with `koi --version`.

A build without `KOIDRIVE_SERVER_BUNDLE` can't find or deploy a server in your Cloudflare account. `koi setup` with the Cloudflare login, `koi server deploy`, `add-device` and `upgrade`, and `koi server delete` without `--bucket` stop with "this koi was built without a server bundle". Everything else works, including `koi server claim` and `koi join`.

## Background sync

Linking the first folder installs a background service that runs the sync daemon and starts it. Unlinking the last folder stops the service and removes it. If the service stopped or is missing, run `koi service start`. The `Background service` line of `koi status` shows its state.

[Background service and files](service.md) describes the service on Linux and macOS, its logs, how to run the daemon yourself and the files Koidrive keeps on this machine.

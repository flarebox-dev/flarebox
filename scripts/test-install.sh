#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu

binary=${1:?Usage: test-install.sh <binary> <target>}
target=${2:?Usage: test-install.sh <binary> <target>}
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
export HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config"
export FLAREBOX_RELEASE_BASE_URL="file://$tmp/releases"
export FLAREBOX_VERSION=v0.0.0
unset FLAREBOX_INSTALL_DIR
mkdir -p "$HOME" "$tmp/package" "$tmp/releases/download/v0.0.0" "$tmp/releases/latest" "$tmp/bin"
cp "$binary" "$tmp/package/flarebox"
cp "$root/cli/systemd/flarebox.service" "$tmp/package/flarebox.service"
archive=flarebox-$target.tar.gz
release=$tmp/releases/download/v0.0.0
tar -czf "$release/$archive" -C "$tmp/package" flarebox flarebox.service
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$release" && sha256sum "$archive") > "$release/SHA256SUMS"
else
    (cd "$release" && shasum -a 256 "$archive") > "$release/SHA256SUMS"
fi
cp "$release/SHA256SUMS" "$tmp/good-sums"
ln -s ../download/v0.0.0 "$tmp/releases/latest/download"

pass() { printf 'PASS %s\n' "$1"; }
reject() {
    if sh "$root/install.sh" --no-systemd > "$tmp/rejected.log" 2>&1; then
        cat "$tmp/rejected.log"
        printf 'Expected installation to fail\n' >&2
        exit 1
    fi
    cat "$tmp/rejected.log"
    grep -F "$2" "$tmp/rejected.log"
    cmp "$binary" "$HOME/.local/bin/flarebox"
    pass "$1"
}

sed '$d' "$root/install.sh" | HOME="$tmp/truncated" sh -s -- --no-systemd
[ ! -e "$tmp/truncated" ]
if head -n 60 "$root/install.sh" | HOME="$tmp/truncated" sh -s -- --no-systemd > "$tmp/truncated.log" 2>&1; then
    printf 'Expected a truncated function body to fail parsing\n' >&2
    exit 1
fi
[ ! -e "$tmp/truncated" ]
pass 'truncated download does not install anything'

cat "$root/install.sh" | sh -s -- --no-systemd > "$tmp/install.log" 2>&1
cat "$tmp/install.log"
grep -q 'not on PATH' "$tmp/install.log"
cmp "$binary" "$HOME/.local/bin/flarebox"
"$HOME/.local/bin/flarebox" --version
pass 'piped install, checksum, executable, and PATH warning'
sh "$root/install.sh" --no-systemd
cmp "$binary" "$HOME/.local/bin/flarebox"
pass 'repeat installation'
printf 'old binary\n' > "$HOME/.local/bin/flarebox"
sh "$root/install.sh" --no-systemd
cmp "$binary" "$HOME/.local/bin/flarebox"
pass 'upgrade replaces the old binary'
unset FLAREBOX_VERSION
FLAREBOX_INSTALL_DIR="$HOME/custom bin" sh "$root/install.sh" --no-systemd
cmp "$binary" "$HOME/custom bin/flarebox"
pass 'latest release and custom install directory'
export FLAREBOX_VERSION=0.0.0
printf '%064d  %s\n' 0 "$archive" > "$release/SHA256SUMS"
reject 'checksum mismatch preserves the installed binary' 'Checksum mismatch'
printf '%064d  other.tar.gz\n' 0 > "$release/SHA256SUMS"
reject 'missing checksum entry' 'exactly one entry'
cat "$tmp/good-sums" "$tmp/good-sums" > "$release/SHA256SUMS"
reject 'duplicate checksum entries' 'exactly one entry'
cp "$tmp/good-sums" "$release/SHA256SUMS"
mv "$release/$archive" "$tmp/saved.tar.gz"
reject 'missing tarball' 'curl:'
mv "$tmp/saved.tar.gz" "$release/$archive"
FLAREBOX_VERSION=../../bad reject 'invalid version' 'FLAREBOX_VERSION must be'
export FLAREBOX_VERSION=0.0.0
FLAREBOX_INSTALL_DIR=relative reject 'relative install directory' 'FLAREBOX_INSTALL_DIR must be'
unset FLAREBOX_INSTALL_DIR

if [ "$(uname -s)" = Linux ]; then
    cat > "$tmp/bin/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$HOME/systemctl.log"
EOF
    chmod +x "$tmp/bin/systemctl"
    PATH="$tmp/bin:$PATH" FLAREBOX_INSTALL_DIR="$HOME/custom bin" sh "$root/install.sh" --systemd
    grep -F "ExecStart=\"$HOME/custom bin/flarebox\" daemon" "$XDG_CONFIG_HOME/systemd/user/flarebox.service"
    grep -x -- '--user daemon-reload' "$HOME/systemctl.log"
    [ "$(wc -l < "$HOME/systemctl.log" | tr -d ' ')" = 1 ]
    pass 'systemd unit follows the custom directory without starting the daemon'
    PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/no-tty" sh "$root/install.sh" < /dev/null
    [ ! -e "$tmp/no-tty/systemd/user/flarebox.service" ]
    [ "$(wc -l < "$HOME/systemctl.log" | tr -d ' ')" = 1 ]
    pass 'noninteractive default does not install or start the service'
    printf 'Passed 13 installer tests on %s\n' "$(uname -s)"
else
    printf 'Passed 11 installer tests on %s\n' "$(uname -s)"
fi

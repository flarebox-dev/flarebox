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
export KOIDRIVE_RELEASE_BASE_URL="file://$tmp/releases"
export KOIDRIVE_VERSION=v0.0.0
unset KOIDRIVE_INSTALL_DIR
mkdir -p "$HOME" "$tmp/package" "$tmp/releases/download/v0.0.0" "$tmp/releases/latest" "$tmp/bin"
cp "$binary" "$tmp/package/koi"
cp "$root/cli/systemd/koidrive.service" "$tmp/package/koidrive.service"
archive=koidrive-$target.tar.gz
release=$tmp/releases/download/v0.0.0
tar -czf "$release/$archive" -C "$tmp/package" koi koidrive.service
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
    cmp "$binary" "$HOME/.local/bin/koi"
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
cmp "$binary" "$HOME/.local/bin/koi"
"$HOME/.local/bin/koi" --version
pass 'piped install, checksum, executable, and PATH warning'
sh "$root/install.sh" --no-systemd
cmp "$binary" "$HOME/.local/bin/koi"
pass 'repeat installation'
printf 'old Koidrive sync client\n' > "$HOME/.local/bin/koi"
sh "$root/install.sh" --no-systemd
cmp "$binary" "$HOME/.local/bin/koi"
pass 'upgrade replaces the old binary'
printf '#!/bin/sh\n' > "$HOME/.local/bin/koi"
cp "$HOME/.local/bin/koi" "$tmp/unrelated"
if sh "$root/install.sh" --no-systemd > "$tmp/unrelated.log" 2>&1; then
    cat "$tmp/unrelated.log"
    printf 'Expected installation over another koi to fail\n' >&2
    exit 1
fi
cat "$tmp/unrelated.log"
grep -F "$HOME/.local/bin/koi is another program" "$tmp/unrelated.log"
cmp "$tmp/unrelated" "$HOME/.local/bin/koi"
cp "$binary" "$HOME/.local/bin/koi"
pass 'another program named koi in the install directory is kept'
unset KOIDRIVE_VERSION
KOIDRIVE_INSTALL_DIR="$HOME/custom bin" sh "$root/install.sh" --no-systemd
cmp "$binary" "$HOME/custom bin/koi"
pass 'latest release and custom install directory'
mkdir -p "$tmp/other"
printf '#!/bin/sh\n' > "$tmp/other/koi"
chmod +x "$tmp/other/koi"
PATH="$tmp/other:$PATH" sh "$root/install.sh" --no-systemd > "$tmp/other.log" 2>&1
cat "$tmp/other.log"
grep -F "koi on PATH runs $tmp/other/koi, another program" "$tmp/other.log"
PATH="$HOME/.local/bin:$tmp/other:$PATH" sh "$root/install.sh" --no-systemd > "$tmp/first.log" 2>&1
if grep -q 'another program' "$tmp/first.log"; then exit 1; fi
pass 'another koi on PATH is reported'
PATH="$HOME/.local/bin:$PATH" KOIDRIVE_INSTALL_DIR="$HOME/.local/bin/" sh "$root/install.sh" --no-systemd > "$tmp/slash.log" 2>&1
cat "$tmp/slash.log"
if grep -q -e 'Warning' "$tmp/slash.log"; then exit 1; fi
cmp "$binary" "$HOME/.local/bin/koi"
pass 'trailing slash in the install directory is the same directory'
ln -s "$HOME/.local/bin" "$tmp/linked"
PATH="$tmp/linked:$PATH" sh "$root/install.sh" --no-systemd > "$tmp/linked.log" 2>&1
cat "$tmp/linked.log"
if grep -q -e 'Warning' "$tmp/linked.log"; then exit 1; fi
pass 'install directory reached through a symlink on PATH'
mkdir -p "$tmp/older"
printf '#!/bin/sh\n# Koidrive sync client\n' > "$tmp/older/koi"
chmod +x "$tmp/older/koi"
PATH="$tmp/older:$HOME/.local/bin:$PATH" sh "$root/install.sh" --no-systemd > "$tmp/older.log" 2>&1
cat "$tmp/older.log"
grep -F "koi on PATH runs $tmp/older/koi, another Koidrive install" "$tmp/older.log"
if grep -q 'another program' "$tmp/older.log"; then exit 1; fi
pass 'an older Koidrive koi on PATH is named as one'
export KOIDRIVE_VERSION=0.0.0
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
KOIDRIVE_VERSION=../../bad reject 'invalid version' 'KOIDRIVE_VERSION must be'
export KOIDRIVE_VERSION=0.0.0
KOIDRIVE_INSTALL_DIR=relative reject 'relative install directory' 'KOIDRIVE_INSTALL_DIR must be'
unset KOIDRIVE_INSTALL_DIR

if [ "$(uname -s)" = Linux ]; then
    cat > "$tmp/bin/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$HOME/systemctl.log"
EOF
    chmod +x "$tmp/bin/systemctl"
    PATH="$tmp/bin:$PATH" KOIDRIVE_INSTALL_DIR="$HOME/custom bin" sh "$root/install.sh" --systemd
    grep -F "ExecStart=\"$HOME/custom bin/koi\" daemon" "$XDG_CONFIG_HOME/systemd/user/koidrive.service"
    grep -x -- '--user daemon-reload' "$HOME/systemctl.log"
    [ "$(wc -l < "$HOME/systemctl.log" | tr -d ' ')" = 1 ]
    pass 'systemd unit follows the custom directory without starting the daemon'
    PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/no-tty" sh "$root/install.sh" < /dev/null
    [ ! -e "$tmp/no-tty/systemd/user/koidrive.service" ]
    [ "$(wc -l < "$HOME/systemctl.log" | tr -d ' ')" = 1 ]
    pass 'noninteractive default does not install or start the service'
    printf 'Passed 18 installer tests on %s\n' "$(uname -s)"
else
    cat > "$tmp/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -s) printf 'Darwin\n' ;;
    -m) printf 'x86_64\n' ;;
    *) exit 1 ;;
esac
EOF
    cat > "$tmp/bin/sysctl" <<'EOF'
#!/bin/sh
[ "$*" = '-n hw.optional.arm64' ] || exit 1
printf '%s\n' "$KOIDRIVE_TEST_ARM64"
EOF
    chmod +x "$tmp/bin/uname" "$tmp/bin/sysctl"
    hash=$(awk 'NR == 1 {print $1}' "$tmp/good-sums")
    for candidate in aarch64 x86_64; do
        selected=koidrive-$candidate-apple-darwin.tar.gz
        if [ "$selected" != "$archive" ]; then
            cp "$release/$archive" "$release/$selected"
        fi
        printf '%s  %s\n' "$hash" "$selected" > "$release/SHA256SUMS"
        if [ "$candidate" = aarch64 ]; then arm64=1; else arm64=0; fi
        PATH="$tmp/bin:$PATH" KOIDRIVE_TEST_ARM64=$arm64 sh "$root/install.sh" --no-systemd > "$tmp/arch.log" 2>&1
        cat "$tmp/arch.log"
        grep -F "Downloading $selected" "$tmp/arch.log"
        cmp "$binary" "$HOME/.local/bin/koi"
        pass "macOS x86_64 shell selects $candidate with hw.optional.arm64=$arm64"
    done
    printf 'Passed 18 installer tests on %s\n' "$(uname -s)"
fi

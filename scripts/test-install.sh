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
mkdir -p "$HOME" "$tmp/package" "$tmp/releases/download/v0.0.0" "$tmp/releases/latest" "$tmp/bin" "$tmp/services"
export PATH="$tmp/services:$PATH"
cat > "$tmp/services/systemctl" <<'EOF'
#!/bin/sh
set -f
printf '%s\n' "$*" >> "$HOME/services.log"
[ "$1 $2" = '--user list-units' ] || exit 0
if [ -e "$HOME/nobus" ]; then
    printf 'Failed to connect to bus: No medium found\n' >&2
    exit 1
fi
shift 2
state= patterns=
for arg; do
    case $arg in
        --state=*) state=${arg#--state=} ;;
        -*) ;;
        *) patterns="$patterns $arg" ;;
    esac
done
{
    printf '%s\n' 'other.service active' 'koidrive-backup.service active' 'koidrive-0badf00d.service failed'
    [ ! -e "$HOME/running" ] || sed 's/$/ active/' "$HOME/running"
} | while read -r unit active; do
    [ -z "$state" ] || [ "$active" = "$state" ] || continue
    for pattern in $patterns; do
        case $unit in
            $pattern) printf '%s loaded %s running Koidrive\n' "$unit" "$active"; break ;;
        esac
    done
done
EOF
cat > "$tmp/services/launchctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$HOME/services.log"
[ "$*" = list ] || exit 0
printf 'PID\tStatus\tLabel\n101\t0\tcom.example.other\n102\t0\tdev.koidrive.koiz\n104\t0\tdev.koidrive.koi.backup\n-\t0\tdev.koidrive.koi.0badf00d\n'
[ ! -e "$HOME/running" ] || sed 's/^/103\t0\t/' "$HOME/running"
EOF
cat > "$tmp/services/ps" <<'EOF'
#!/bin/sh
if [ "$1 $2" = '-o cgroup=' ] && [ -e "$HOME/cgroup" ]; then
    cat "$HOME/cgroup"
    exit 0
fi
exec /bin/ps "$@"
EOF
chmod +x "$tmp/services/systemctl" "$tmp/services/launchctl" "$tmp/services/ps"
cp "$binary" "$tmp/package/koi"
archive=koidrive-$target.tar.gz
release=$tmp/releases/download/v0.0.0
tar -czf "$release/$archive" -C "$tmp/package" koi
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$release" && sha256sum "$archive") > "$release/SHA256SUMS"
else
    (cd "$release" && shasum -a 256 "$archive") > "$release/SHA256SUMS"
fi
cp "$release/SHA256SUMS" "$tmp/good-sums"
ln -s ../download/v0.0.0 "$tmp/releases/latest/download"

pass() { printf 'PASS %s\n' "$1"; }
reject() {
    if sh "$root/install.sh" > "$tmp/rejected.log" 2>&1; then
        cat "$tmp/rejected.log"
        printf 'Expected installation to fail\n' >&2
        exit 1
    fi
    cat "$tmp/rejected.log"
    grep -F "$2" "$tmp/rejected.log"
    cmp "$binary" "$HOME/.local/bin/koi"
    pass "$1"
}

sed '$d' "$root/install.sh" | HOME="$tmp/truncated" sh
[ ! -e "$tmp/truncated" ]
if head -n 60 "$root/install.sh" | HOME="$tmp/truncated" sh > "$tmp/truncated.log" 2>&1; then
    printf 'Expected a truncated function body to fail parsing\n' >&2
    exit 1
fi
[ ! -e "$tmp/truncated" ]
pass 'truncated download does not install anything'

cat "$root/install.sh" | sh > "$tmp/install.log" 2>&1
cat "$tmp/install.log"
grep -q 'not on PATH' "$tmp/install.log"
cmp "$binary" "$HOME/.local/bin/koi"
"$HOME/.local/bin/koi" --version
pass 'piped install, checksum, executable, and PATH warning'
sh "$root/install.sh"
cmp "$binary" "$HOME/.local/bin/koi"
pass 'repeat installation'
printf 'old Koidrive sync client\n' > "$HOME/.local/bin/koi"
sh "$root/install.sh"
cmp "$binary" "$HOME/.local/bin/koi"
pass 'upgrade replaces the old binary'
printf '#!/bin/sh\n' > "$HOME/.local/bin/koi"
cp "$HOME/.local/bin/koi" "$tmp/unrelated"
if sh "$root/install.sh" > "$tmp/unrelated.log" 2>&1; then
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
KOIDRIVE_INSTALL_DIR="$HOME/custom bin" sh "$root/install.sh"
cmp "$binary" "$HOME/custom bin/koi"
pass 'latest release and custom install directory'
mkdir -p "$tmp/other"
printf '#!/bin/sh\n' > "$tmp/other/koi"
chmod +x "$tmp/other/koi"
PATH="$tmp/other:$PATH" sh "$root/install.sh" > "$tmp/other.log" 2>&1
cat "$tmp/other.log"
grep -F "koi on PATH runs $tmp/other/koi, another program" "$tmp/other.log"
PATH="$HOME/.local/bin:$tmp/other:$PATH" sh "$root/install.sh" > "$tmp/first.log" 2>&1
if grep -q 'another program' "$tmp/first.log"; then exit 1; fi
pass 'another koi on PATH is reported'
PATH="$HOME/.local/bin:$PATH" KOIDRIVE_INSTALL_DIR="$HOME/.local/bin/" sh "$root/install.sh" > "$tmp/slash.log" 2>&1
cat "$tmp/slash.log"
if grep -q -e 'Warning' "$tmp/slash.log"; then exit 1; fi
cmp "$binary" "$HOME/.local/bin/koi"
pass 'trailing slash in the install directory is the same directory'
ln -s "$HOME/.local/bin" "$tmp/linked"
PATH="$tmp/linked:$PATH" sh "$root/install.sh" > "$tmp/linked.log" 2>&1
cat "$tmp/linked.log"
if grep -q -e 'Warning' "$tmp/linked.log"; then exit 1; fi
pass 'install directory reached through a symlink on PATH'
mkdir -p "$tmp/older"
printf '#!/bin/sh\n# Koidrive sync client\n' > "$tmp/older/koi"
chmod +x "$tmp/older/koi"
PATH="$tmp/older:$HOME/.local/bin:$PATH" sh "$root/install.sh" > "$tmp/older.log" 2>&1
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
    services='koidrive.service
koidrive-1a2b3c4d.service'
    restarts='--user restart koidrive.service
--user restart koidrive-1a2b3c4d.service'
else
    services='dev.koidrive.koi
dev.koidrive.koi.1a2b3c4d'
    restarts="kickstart -k gui/$(id -u)/dev.koidrive.koi
kickstart -k gui/$(id -u)/dev.koidrive.koi.1a2b3c4d"
fi
rm -f "$HOME/services.log"
sh "$root/install.sh"
if grep -E -e '--user restart|kickstart' "$HOME/services.log"; then
    printf 'Expected an upgrade to leave services that are not running alone\n' >&2
    exit 1
fi
pass 'an upgrade leaves services that are not running alone'
printf '%s\n' "$services" > "$HOME/running"
rm -f "$HOME/services.log"
sh "$root/install.sh" > "$tmp/restart.log"
grep -E -e '--user restart|kickstart' "$HOME/services.log" > "$tmp/restarted"
if [ "$(cat "$tmp/restarted")" != "$restarts" ] || [ "$(grep -c -F 'Restarted the background service' "$tmp/restart.log")" != 2 ]; then
    cat "$HOME/services.log" "$tmp/restart.log"
    printf 'Expected an upgrade to restart exactly the running services of every profile\n' >&2
    exit 1
fi
rm "$HOME/running"
pass 'an upgrade restarts the running background service of every profile'

if [ "$(uname -s)" = Linux ]; then
    perl -e '$0 = "/opt/custom bin/koi _daemon"; sleep 300' &
    daemon=$!
    touch "$HOME/nobus"
    sh "$root/install.sh" 2> "$tmp/nobus.log"
    if ! grep -F "the koi daemon with process ID $daemon still runs the old koi" "$tmp/nobus.log"; then
        cat "$tmp/nobus.log"
        printf 'Expected an upgrade without a systemd user manager to name the daemon that still runs the old koi\n' >&2
        exit 1
    fi
    pass 'an upgrade without a systemd user manager names the daemon that still runs the old koi'
    printf '0::/user.slice/user-%s.slice/user@%s.service/app.slice/koidrive-1a2b3c4d.service\n' "$(id -u)" "$(id -u)" > "$HOME/cgroup"
    sh "$root/install.sh" 2> "$tmp/nobus.log"
    rm "$HOME/nobus" "$HOME/cgroup"
    kill "$daemon"
    if ! grep -F "Log in as $(id -un) and run: systemctl --user restart koidrive-1a2b3c4d.service" "$tmp/nobus.log"; then
        cat "$tmp/nobus.log"
        printf 'Expected an upgrade without a systemd user manager to give the command that restarts the unit\n' >&2
        exit 1
    fi
    pass 'an upgrade without a systemd user manager gives the command that restarts the unit'
fi

if [ "$(uname -s)" = Linux ]; then
    printf 'Passed 20 installer tests on %s\n' "$(uname -s)"
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
        PATH="$tmp/bin:$PATH" KOIDRIVE_TEST_ARM64=$arm64 sh "$root/install.sh" > "$tmp/arch.log" 2>&1
        cat "$tmp/arch.log"
        grep -F "Downloading $selected" "$tmp/arch.log"
        cmp "$binary" "$HOME/.local/bin/koi"
        pass "macOS x86_64 shell selects $candidate with hw.optional.arm64=$arm64"
    done
    printf 'Passed 20 installer tests on %s\n' "$(uname -s)"
fi

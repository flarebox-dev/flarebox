#!/bin/sh
# SPDX-License-Identifier: Apache-2.0

main() {
    set -eu

    fail() {
        printf 'koi install: %s\n' "$*" >&2
        exit 1
    }

    [ "$#" -eq 0 ] || fail 'Usage: sh install.sh'

    case "$(uname -s)" in
        Linux) os=unknown-linux-musl ;;
        Darwin) os=apple-darwin ;;
        *) fail 'Supported systems are Linux and macOS' ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) fail 'Supported architectures are x86_64 and arm64' ;;
    esac
    if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
    fi

    for command in curl tar mktemp awk grep sed; do
        command -v "$command" >/dev/null 2>&1 || fail "Required command not found: $command"
    done
    if command -v sha256sum >/dev/null 2>&1; then
        checksum=sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        checksum=shasum
    else
        fail 'Install sha256sum or shasum to verify the download'
    fi

    base=${KOIDRIVE_RELEASE_BASE_URL:-https://github.com/koidrive/koidrive/releases}
    base=${base%/}
    version=${KOIDRIVE_VERSION:-latest}
    if [ "$version" = latest ]; then
        download=$base/latest/download
    else
        version=${version#v}
        printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || fail 'KOIDRIVE_VERSION must be a version such as v0.1.0'
        download=$base/download/v$version
    fi

    archive=koidrive-$arch-$os.tar.gz
    tmp=$(mktemp -d)
    staged=
    trap 'rm -rf "$tmp"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT
    trap 'exit 1' HUP INT TERM
    printf 'Downloading %s (%s)\n' "$archive" "$version"
    curl -fsSL --retry 3 -o "$tmp/$archive" -- "$download/$archive"
    curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" -- "$download/SHA256SUMS"
    expected=$(awk -v name="$archive" '$2 == name { count++; hash = $1 } END { if (count == 1) print hash; else exit 1 }' "$tmp/SHA256SUMS") || fail 'SHA256SUMS must contain exactly one entry for the tarball'
    [ "${#expected}" -eq 64 ] || fail 'Invalid SHA256SUMS entry'
    case "$expected" in *[!0-9a-f]*) fail 'Invalid SHA256SUMS entry' ;; esac
    if [ "$checksum" = sha256sum ]; then
        actual=$(sha256sum "$tmp/$archive")
    else
        actual=$(shasum -a 256 "$tmp/$archive")
    fi
    [ "${actual%% *}" = "$expected" ] || fail 'Checksum mismatch; the existing installation has not changed'
    printf 'SHA256 verified for %s\n' "$archive"
    tar -xzOf "$tmp/$archive" koi > "$tmp/koi"
    [ -s "$tmp/koi" ] || fail 'The tarball contains no koi binary'

    install_dir=${KOIDRIVE_INSTALL_DIR:-$HOME/.local/bin}
    case "$install_dir" in /*) ;; *) fail 'KOIDRIVE_INSTALL_DIR must be an absolute path' ;; esac
    case "$install_dir" in *[[:cntrl:]]*) fail 'KOIDRIVE_INSTALL_DIR cannot contain control characters' ;; esac
    while [ "$install_dir" != / ] && [ "${install_dir%/}" != "$install_dir" ]; do install_dir=${install_dir%/}; done
    [ ! -d "$install_dir/koi" ] || fail 'The destination koi is a directory'
    if [ -f "$install_dir/koi" ] && ! grep -q 'Koidrive sync client' "$install_dir/koi"; then
        fail "$install_dir/koi is another program. Move it away, or install elsewhere with KOIDRIVE_INSTALL_DIR"
    fi
    mkdir -p "$install_dir"
    staged=$(mktemp "$install_dir/.koidrive.XXXXXXXX")
    cat "$tmp/koi" > "$staged"
    chmod 755 "$staged"
    mv -f "$staged" "$install_dir/koi"
    staged=
    printf 'Installed koi to %s/koi\n' "$install_dir"
    other=$(command -v koi 2>/dev/null || true)
    if [ -z "$other" ] || ! [ "$other" -ef "$install_dir/koi" ]; then
        case ":${PATH:-}:" in
            *":$install_dir:"*) ;;
            *) printf 'Warning: %s is not on PATH. Add it to your shell profile.\n' "$install_dir" >&2 ;;
        esac
        if [ -z "$other" ]; then
            :
        elif grep -q 'Koidrive sync client' "$other" 2>/dev/null; then
            printf 'Warning: koi on PATH runs %s, another Koidrive install. Put %s before it on PATH.\n' "$other" "$install_dir" >&2
        else
            printf 'Warning: koi on PATH runs %s, another program. Put %s before it on PATH.\n' "$other" "$install_dir" >&2
        fi
    fi

    restart() {
        service=$1
        shift
        if "$@"; then
            printf 'Restarted the background service %s\n' "$service"
        else
            printf 'Warning: koi is installed, but the background service %s did not restart. Run: %s\n' "$service" "$*" >&2
        fi
    }
    if [ "$os" = apple-darwin ] && agents=$(launchctl print "gui/$(id -u)" 2>/dev/null); then
        for label in $(printf '%s\n' "$agents" | awk '$1 == "services" && $2 == "=" {inside = 1; next} inside && $1 == "}" {inside = 0} inside && $1 != "0" && ($3 == "dev.koidrive.koi" || $3 ~ /^dev\.koidrive\.koi\.[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]$/) {print $3}'); do
            restart "$label" launchctl kickstart -k "gui/$(id -u)/$label"
        done
    elif [ "$os" = unknown-linux-musl ] && units=$(systemctl --user list-units --type=service --state=active --plain --no-legend 'koidrive.service' 'koidrive-*.service' 2>/dev/null); then
        for unit in $(printf '%s\n' "$units" | awk '$1 == "koidrive.service" || $1 ~ /^koidrive-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]\.service$/ {print $1}'); do
            restart "$unit" systemctl --user restart "$unit"
        done
    else
        for pid in $(ps -ww -u "$(id -u)" -o pid= -o args= 2>/dev/null | awk '{pid = $1; sub(/^ *[0-9]+ +/, "")} $0 == "koi _daemon" || $0 ~ /\/koi _daemon$/ {print pid}'); do
            unit=$(ps -ww -o cgroup= -p "$pid" 2>/dev/null | tr ';' '\n' | sed -n 's|.*/\(koidrive[-0-9a-f]*\.service\)$|\1|p' | head -n 1)
            if [ -n "$unit" ]; then
                printf 'Warning: koi is installed, but the background service %s still runs the old koi, because this shell cannot reach the systemd user manager. Log in as %s and run: systemctl --user restart %s\n' "$unit" "$(id -un)" "$unit" >&2
            else
                printf 'Warning: koi is installed, but the koi daemon with process ID %s still runs the old koi. Restart it with the service manager that started it.\n' "$pid" >&2
            fi
        done
    fi
}

main "$@"

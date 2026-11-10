#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs install-hook.sh without root against fake runner directories under a temporary root,
# with the current user standing in for root:wheel. It checks a fresh install, a second run, and
# that read errors and a failure while applying leave every runner either installed or unchanged.
# Needs macOS (chflags, BSD stat).
set -euo pipefail

installer="$(cd "$(dirname "$0")" && pwd)/install-hook.sh"
tmp="$(mktemp -d)"
trap 'chflags -R 0 "$tmp" 2>/dev/null || true; chmod -R u+rwx "$tmp" 2>/dev/null || true; rm -rf "$tmp"' EXIT
me="$(id -un)"
grp="$(id -gn)"
failures=0
out=""
code=0

new_root() {
  root="$(mktemp -d "$tmp/root.XXXXXX")"
  chgrp "$grp" "$root"
  for n in 1 2 3 4; do
    mkdir -p "$root/Users/ghrunner/runner-$n"
    echo '{}' > "$root/Users/ghrunner/runner-$n/.runner"
    printf 'LANG=en_US.UTF-8\nJAVA_HOME=/opt/java\n' > "$root/Users/ghrunner/runner-$n/.env"
    echo "/Users/ghrunner/.cargo/bin:/usr/bin:/bin" > "$root/Users/ghrunner/runner-$n/.path"
  done
}
runner() { echo "$root/Users/ghrunner/runner-$1"; }
install_as_me() {
  code=0
  out="$(FLAREBOX_INSTALL_ROOT="$root" FLAREBOX_INSTALL_OWNER="$me" FLAREBOX_INSTALL_GROUP="$grp" "$installer" 2>&1)" || code=$?
}
snapshot() {
  local dir="$1" file
  for file in .env .path; do
    if [ -e "$dir/$file" ]; then
      echo "$file $(stat -f '%Su:%Sg %Lp %Sf %m' "$dir/$file") $(shasum < "$dir/$file")"
    else
      echo "$file missing"
    fi
  done
  ls -A "$dir"
}
expect() {
  local name="$1"
  shift
  if "$@"; then
    echo "ok   $name"
  else
    echo "FAIL $name"
    echo "$out"
    failures=$((failures + 1))
  fi
}
installed() {
  local dir="$1"
  [ "$(cat "$dir/.env")" = "$(printf 'LANG=en_US.UTF-8\nJAVA_HOME=/opt/java\nACTIONS_RUNNER_HOOK_JOB_STARTED=%s' "$root/usr/local/libexec/flarebox/job-started.sh")" ] &&
    [ "$(cat "$dir/.path")" = "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/Users/ghrunner/.cargo/bin" ] &&
    [ "$(stat -f '%Sf' "$dir/.env")" = uchg ] && [ "$(stat -f '%Sf' "$dir/.path")" = uchg ] &&
    [ ! -e "$dir/.env.flarebox-new" ] && [ ! -e "$dir/.path.flarebox-new" ]
}
all_installed() {
  [ "$code" -eq 0 ] && cmp -s "$(dirname "$installer")/job-started.sh" "$root/usr/local/libexec/flarebox/job-started.sh" &&
    installed "$(runner 1)" && installed "$(runner 2)" && installed "$(runner 3)" && installed "$(runner 4)"
}
unchanged() { [ "$(snapshot "$1")" = "$2" ]; }
no_hook() { [ ! -e "$root/usr/local/libexec/flarebox" ]; }
failed_with() { [ "$code" -ne 0 ] && [[ "$out" == *"$1"* ]]; }
verified() { [[ "$out" == *"Verification"* && "$out" != *FAILED* && "$out" == *"Installed."* ]]; }

new_root
install_as_me
expect "a fresh install sets up the hook and every runner" all_installed
expect "the verification passes" verified
install_as_me
expect "a second run keeps one hook line and the other settings" all_installed

new_root
before1="$(snapshot "$(runner 1)")"
before3="$(snapshot "$(runner 3)")"
chmod 000 "$(runner 3)/.env"
install_as_me
chmod 644 "$(runner 3)/.env"
expect "an unreadable .env stops the install with an error" failed_with "cannot read $(runner 3)/.env; nothing was changed."
expect "an unreadable .env leaves the runners before it unchanged" unchanged "$(runner 1)" "$before1"
expect "an unreadable .env leaves its own runner unchanged" unchanged "$(runner 3)" "$before3"
expect "an unreadable .env installs no hook" no_hook

new_root
rm "$(runner 2)/.path"
mkdir "$(runner 2)/.path"
install_as_me
expect "a .path that is a directory stops the install" failed_with "$(runner 2)/.path is not a regular file; nothing was changed."
expect "a .path that is a directory installs no hook" no_hook

new_root
before1="$(snapshot "$(runner 1)")"
mkdir "$tmp/fake-grep"
printf '#!/bin/sh\nexit 2\n' > "$tmp/fake-grep/grep"
chmod +x "$tmp/fake-grep/grep"
code=0
out="$(PATH="$tmp/fake-grep:$PATH" FLAREBOX_INSTALL_ROOT="$root" FLAREBOX_INSTALL_OWNER="$me" FLAREBOX_INSTALL_GROUP="$grp" "$installer" 2>&1)" || code=$?
expect "a grep read error stops the install instead of dropping settings" failed_with "cannot filter $(runner 1)/.env; nothing was changed."
expect "a grep read error leaves the runners unchanged" unchanged "$(runner 1)" "$before1"
expect "a grep read error installs no hook" no_hook

new_root
chflags uappnd "$(runner 2)/.path"
before2="$(snapshot "$(runner 2)")"
before3="$(snapshot "$(runner 3)")"
install_as_me
expect "a failure while applying runner-2 stops with an error" failed_with "$(runner 2) is back to its previous state."
expect "the runner before the failure is fully installed" installed "$(runner 1)"
expect "the failing runner is restored with its files, owners, modes and flags" unchanged "$(runner 2)" "$before2"
expect "the runners after the failure are unchanged" unchanged "$(runner 3)" "$before3"
expect "the error names the installed and unchanged runners" failed_with "installed in: $(runner 1). Not changed: $(runner 2) and the runners after it."
chflags nouappnd "$(runner 2)/.path"

new_root
rm "$(runner 4)/.runner"
before4="$(snapshot "$(runner 4)")"
install_as_me
expect "a directory without .runner stops the install" failed_with "is not a configured runner directory"
expect "a directory without .runner leaves it unchanged" unchanged "$(runner 4)" "$before4"

new_root
code=0
out="$(FLAREBOX_INSTALL_ROOT="$root" "$installer" 2>&1)" || code=$?
expect "without sudo it refuses to run" failed_with "run this with sudo."
expect "without sudo it installs no hook" no_hook

if [ "$failures" -ne 0 ]; then
  echo "$failures install-hook.sh test(s) failed."
  exit 1
fi
echo "All install-hook.sh tests passed."

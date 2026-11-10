#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Installs job-started.sh as the job-started hook of Flarebox's self-hosted Mac runners. Run it
# with sudo from a checkout on the Mac that hosts the runners, then restart each runner while it
# is idle. It copies the hook to a root-owned path, and in each runner directory it writes .env
# (with ACTIONS_RUNNER_HOOK_JOB_STARTED) and .path (system directories first, so the runner finds
# /bin/bash for the hook), makes both files owned by root and immutable, and prints what a
# maintainer should check. Running it again is safe. CONTRIBUTING.md explains why.
#
# Usage: sudo .github/runner/install-hook.sh [RUNNER_DIR...]
# The default runner directories are /Users/ghrunner/runner-1 to /Users/ghrunner/runner-4.
set -euo pipefail

runner_user=ghrunner
hook_dir=/usr/local/libexec/flarebox
hook="$hook_dir/job-started.sh"
runner_path="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/Users/$runner_user/.cargo/bin"
source_hook="$(cd "$(dirname "$0")" && pwd)/job-started.sh"

if [ "$(id -u)" -ne 0 ]; then
  echo "error: run this with sudo." >&2
  exit 1
fi
if [ "$#" -eq 0 ]; then
  set -- "/Users/$runner_user/runner-1" "/Users/$runner_user/runner-2" "/Users/$runner_user/runner-3" "/Users/$runner_user/runner-4"
fi
for dir in "$@"; do
  if [ ! -f "$dir/.runner" ]; then
    echo "error: $dir is not a configured runner directory (it has no .runner file)." >&2
    exit 1
  fi
done

install -d -o root -g wheel -m 755 "$hook_dir"
install -o root -g wheel -m 755 "$source_hook" "$hook"

for dir in "$@"; do
  for file in .env .path; do
    if [ -e "$dir/$file" ]; then
      chflags nouchg "$dir/$file"
    fi
  done
  if [ -f "$dir/.path" ]; then
    echo "$dir/.path was: $(cat "$dir/.path")"
  fi
  env_new="$(mktemp)"
  if [ -f "$dir/.env" ]; then
    grep -v '^ACTIONS_RUNNER_HOOK_JOB_STARTED=' "$dir/.env" > "$env_new" || true
  fi
  echo "ACTIONS_RUNNER_HOOK_JOB_STARTED=$hook" >> "$env_new"
  install -o root -g wheel -m 644 "$env_new" "$dir/.env"
  rm -f "$env_new"
  path_new="$(mktemp)"
  echo "$runner_path" > "$path_new"
  install -o root -g wheel -m 644 "$path_new" "$dir/.path"
  rm -f "$path_new"
  chflags uchg "$dir/.env" "$dir/.path"
done

check() {
  local writable=no
  if sudo -u "$runner_user" test -w "$1"; then
    writable=YES
  fi
  printf '%-48s %-12s %-11s %-6s %s writable: %s\n' "$1" "$(stat -f '%Su:%Sg' "$1")" "$(stat -f '%Sp' "$1")" "$(stat -f '%Sf' "$1" | sed 's/^-$/none/')" "$runner_user" "$writable"
  if [ "$writable" = YES ]; then
    failures=$((failures + 1))
  fi
}

echo
echo "Verification (path, owner, mode, flags, whether $runner_user can write it):"
failures=0
for path in /usr/local /usr/local/libexec "$hook_dir" "$hook"; do
  check "$path"
done
for dir in "$@"; do
  check "$dir/.env"
  check "$dir/.path"
  echo "  hook in $dir/.env: $(grep '^ACTIONS_RUNNER_HOOK_JOB_STARTED=' "$dir/.env")"
  echo "  PATH in $dir/.path: $(cat "$dir/.path")"
done
echo "jq for the hook: $(/usr/bin/jq --version)"
if ! cmp -s "$source_hook" "$hook"; then
  echo "error: $hook differs from $source_hook." >&2
  failures=$((failures + 1))
fi
if [ "$failures" -ne 0 ]; then
  echo "error: $failures check(s) failed; see the lines above." >&2
  exit 1
fi
echo
echo "Installed. Restart each runner while it is idle, for example:"
for dir in "$@"; do
  echo "  sudo launchctl kickstart -k system/dev.flarebox.runner-${dir##*-}"
done
echo "Then check that a job's Set up runner log shows 'Self-hosted runner guard: ...'."

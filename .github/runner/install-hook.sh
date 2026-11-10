#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Installs job-started.sh as the job-started hook of Flarebox's self-hosted Mac runners. Run it
# with sudo from a checkout on the Mac that hosts the runners, then restart each runner while it
# is idle. It copies the hook to a root-owned path, and in each runner directory it writes .env
# (with ACTIONS_RUNNER_HOOK_JOB_STARTED) and .path (system directories first, so the runner finds
# /bin/bash for the hook), makes both files owned by root and immutable, and checks the result.
# CONTRIBUTING.md explains why.
#
# It reads and prepares every file before it changes anything, so a read error changes nothing.
# It then updates one runner at a time; if that fails or the script gets SIGTERM, SIGINT or SIGHUP,
# it restores the runner's previous files, owners, modes and flags and stops, so each runner is
# either fully installed or unchanged. Running it again is safe.
#
# Usage: sudo .github/runner/install-hook.sh [RUNNER_DIR...]
# The default runner directories are /Users/ghrunner/runner-1 to /Users/ghrunner/runner-4.
# FLAREBOX_INSTALL_ROOT, FLAREBOX_INSTALL_OWNER and FLAREBOX_INSTALL_GROUP let
# test-install-hook.sh run it without root against fake runner directories. Run as root, it
# refuses them.
set -euo pipefail

root="${FLAREBOX_INSTALL_ROOT:-}"
owner="${FLAREBOX_INSTALL_OWNER:-root}"
group="${FLAREBOX_INSTALL_GROUP:-wheel}"
runner_user=ghrunner
hook_dir="$root/usr/local/libexec/flarebox"
hook="$hook_dir/job-started.sh"
hook_line="ACTIONS_RUNNER_HOOK_JOB_STARTED=$hook"
runner_path="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/Users/$runner_user/.cargo/bin"
source_hook="$(cd "$(dirname "$0")" && pwd)/job-started.sh"

die() {
  echo "error: $*" >&2
  exit 1
}

if [ "$(id -u)" -eq 0 ]; then
  if [ -n "${FLAREBOX_INSTALL_ROOT+set}${FLAREBOX_INSTALL_OWNER+set}${FLAREBOX_INSTALL_GROUP+set}" ]; then
    die "FLAREBOX_INSTALL_ROOT, FLAREBOX_INSTALL_OWNER and FLAREBOX_INSTALL_GROUP are for tests without root; unset them to install as root. Nothing was changed."
  fi
elif [ "$owner" != "$(id -un)" ]; then
  die "run this with sudo."
fi
if [ "$#" -eq 0 ]; then
  set -- "$root/Users/$runner_user/runner-1" "$root/Users/$runner_user/runner-2" "$root/Users/$runner_user/runner-3" "$root/Users/$runner_user/runner-4"
fi

staging="$(mktemp -d)"
keep_staging=no
cleanup() {
  if [ "$keep_staging" = no ]; then
    chflags -R 0 "$staging" 2>/dev/null || true
    rm -rf "$staging"
  fi
}
trap cleanup EXIT

installed=()
current_dir=""
current_stage=""
roll_back_current() {
  echo "error: restoring the previous .env and .path of $current_dir." >&2
  if restore_runner "$current_dir" "$current_stage"; then
    echo "error: $current_dir is back to its previous state." >&2
  else
    keep_staging=yes
    echo "error: restoring $current_dir failed. Its previous files are in $current_stage (backup.env, backup.path); restore them by hand." >&2
  fi
  echo "error: installed in: ${installed[*]:-none}. Not changed: $current_dir and the runners after it." >&2
}
on_signal() {
  trap '' TERM INT HUP
  echo "error: interrupted by SIG$1." >&2
  if [ -n "$current_dir" ]; then
    roll_back_current
  else
    echo "error: installed in: ${installed[*]:-none}. The other runners are unchanged." >&2
  fi
  exit "$2"
}
trap 'on_signal TERM 143' TERM
trap 'on_signal INT 130' INT
trap 'on_signal HUP 129' HUP

# Stage: read and prepare everything. Nothing outside $staging changes here.
cat "$source_hook" > "$staging/hook" || die "cannot read $source_hook; nothing was changed."
n=0
for dir in "$@"; do
  n=$((n + 1))
  s="$staging/$n"
  mkdir "$s"
  [ -f "$dir/.runner" ] || die "$dir is not a configured runner directory (it has no .runner file); nothing was changed."
  for file in .env .path; do
    if [ -e "$dir/$file" ] || [ -L "$dir/$file" ]; then
      [ -f "$dir/$file" ] || die "$dir/$file is not a regular file; nothing was changed."
      cat "$dir/$file" > "$s/old$file" || die "cannot read $dir/$file; nothing was changed."
      cp -p "$dir/$file" "$s/backup$file" || die "cannot back up $dir/$file; nothing was changed."
      stat -f '%Su:%Sg %Lp %Sf' "$dir/$file" > "$s/stat$file" || die "cannot read the owner, mode and flags of $dir/$file; nothing was changed."
    fi
  done
  : > "$s/new.env"
  if [ -f "$s/old.env" ]; then
    grep -v '^ACTIONS_RUNNER_HOOK_JOB_STARTED=' "$s/old.env" > "$s/new.env" || [ $? -eq 1 ] || die "cannot filter $dir/.env; nothing was changed."
  fi
  echo "$hook_line" >> "$s/new.env"
  echo "$runner_path" > "$s/new.path"
  if [ -f "$s/old.path" ]; then
    echo "$dir/.path was: $(cat "$s/old.path")"
  fi
done

apply_runner() {
  local dir="$1" s="$2" file
  install -o "$owner" -g "$group" -m 644 "$s/new.env" "$dir/.env.flarebox-new" || return 1
  install -o "$owner" -g "$group" -m 644 "$s/new.path" "$dir/.path.flarebox-new" || return 1
  for file in .env .path; do
    if [ -e "$dir/$file" ]; then
      chflags nouchg "$dir/$file" || return 1
    fi
  done
  mv -f "$dir/.env.flarebox-new" "$dir/.env" || return 1
  mv -f "$dir/.path.flarebox-new" "$dir/.path" || return 1
  chflags uchg "$dir/.env" "$dir/.path" || return 1
}

restore_runner() {
  local dir="$1" s="$2" file status=0
  rm -f "$dir/.env.flarebox-new" "$dir/.path.flarebox-new" || status=1
  for file in .env .path; do
    if [ -e "$dir/$file" ]; then
      chflags 0 "$dir/$file" || status=1
    fi
    if [ -f "$s/backup$file" ]; then
      cp -p "$s/backup$file" "$dir/$file" || status=1
      if ! cmp -s "$s/backup$file" "$dir/$file" || [ "$(stat -f '%Su:%Sg %Lp %Sf' "$dir/$file")" != "$(cat "$s/stat$file")" ]; then
        status=1
      fi
    else
      rm -f "$dir/$file" || status=1
    fi
  done
  return "$status"
}

# Apply: the hook first, then one runner at a time.
install -d -o "$owner" -g "$group" -m 755 "$hook_dir"
install -o "$owner" -g "$group" -m 755 "$staging/hook" "$hook_dir/.job-started.sh.new"
mv -f "$hook_dir/.job-started.sh.new" "$hook"
n=0
for dir in "$@"; do
  n=$((n + 1))
  current_dir="$dir"
  current_stage="$staging/$n"
  if ! apply_runner "$dir" "$current_stage"; then
    echo "error: installing the hook in $dir failed." >&2
    roll_back_current
    exit 1
  fi
  current_dir=""
  installed+=("$dir")
done

# Verify owners, modes, flags and contents.
failures=0
fail() {
  echo "  FAILED: $*"
  failures=$((failures + 1))
}
check() {
  local path="$1" mode="$2" flag="$3" expected="$4" writable=no
  if [ "$(id -u)" -eq 0 ] && sudo -u "$runner_user" test -w "$path"; then
    writable=YES
  fi
  printf '%-56s %-14s %-4s %-12s %s writable: %s\n' "$path" "$(stat -f '%Su:%Sg' "$path")" "$(stat -f '%Lp' "$path")" "$(stat -f '%Sf' "$path")" "$runner_user" "$writable"
  [ "$(stat -f '%Su:%Sg' "$path")" = "$owner:$group" ] || fail "$path is not owned by $owner:$group."
  [ "$(stat -f '%Lp' "$path")" = "$mode" ] || fail "$path does not have mode $mode."
  if [ -n "$flag" ] && [[ ",$(stat -f '%Sf' "$path")," != *",$flag,"* ]]; then
    fail "$path does not have the $flag flag."
  fi
  if [ -n "$expected" ] && ! cmp -s "$expected" "$path"; then
    fail "$path does not have the expected contents."
  fi
  if [ "$writable" = YES ]; then
    fail "$runner_user can write $path."
  fi
}

echo
echo "Verification (path, owner, mode, flags, whether $runner_user can write it):"
check "$hook_dir" 755 "" ""
check "$hook" 755 "" "$staging/hook"
n=0
for dir in "$@"; do
  n=$((n + 1))
  check "$dir/.env" 644 uchg "$staging/$n/new.env"
  check "$dir/.path" 644 uchg "$staging/$n/new.path"
done
echo "Hook setting: $hook_line"
echo "PATH: $runner_path"
echo "jq for the hook: $(/usr/bin/jq --version)"
if [ "$failures" -ne 0 ]; then
  die "$failures check(s) failed; see the lines above."
fi

echo
echo "Installed. Restart each runner while it is idle, for example:"
for dir in "$@"; do
  echo "  sudo launchctl kickstart -k system/dev.flarebox.runner-${dir##*-}"
done
echo "Then check that a job's Set up runner log shows 'Self-hosted runner guard: ...'."

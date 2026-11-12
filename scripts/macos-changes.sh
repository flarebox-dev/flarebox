#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 BASE HEAD" >&2
  echo "Prints each file that changes between BASE and HEAD and can build or behave differently on macOS, with the reason." >&2
  exit 2
fi
base=$1
head=$2
platform_rust='target_os|target_vendor|target_arch|std::os::(macos|darwin)'

has() {
  local path=$1 pattern=$2 rev
  for rev in "$base" "$head"; do
    if git cat-file -e "$rev:$path" 2>/dev/null && git show "$rev:$path" | grep -E "$pattern" >/dev/null; then
      return 0
    fi
  done
  return 1
}

changed="$(git diff -z --name-only --no-renames "$base" "$head" | tr '\0' '\n')"
while IFS= read -r path; do
  reason=""
  case "$path" in
    "") continue ;;
    apps/macos/*) reason="macOS app" ;;
    core/ffi/*) reason="Swift bindings of the core" ;;
    core/fs/*) reason="file system layer, which differs per platform" ;;
    Cargo.lock | rust-toolchain.toml | .cargo/*) reason="toolchain or dependencies" ;;
    build.rs | */build.rs) reason="build script" ;;
    Cargo.toml | */Cargo.toml) has "$path" '^\[target\.' && reason="platform-specific dependencies" ;;
    *.rs) has "$path" "$platform_rust" && reason="platform-specific Rust" ;;
  esac
  if [ -n "$reason" ]; then
    printf '%s\t%s\n' "$path" "$reason"
  fi
done <<<"$changed"

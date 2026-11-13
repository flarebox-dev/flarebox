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
platform_rust='\btarget_(os|vendor|arch|env|abi|family|pointer_width|endian|has_atomic|feature)\b|std::os::(macos|darwin)'

has() {
  local path=$1 pattern=$2 rev
  for rev in "$base" "$head"; do
    if git cat-file -e "$rev:$path" 2>/dev/null && git show "$rev:$path" | grep -E "$pattern" >/dev/null; then
      return 0
    fi
  done
  return 1
}

normalize() {
  local part parts out=()
  IFS=/ read -r -a parts <<<"$1"
  for part in "${parts[@]}"; do
    case "$part" in
      "" | .) ;;
      ..) [ "${#out[@]}" -eq 0 ] || unset "out[$((${#out[@]} - 1))]" ;;
      *) out+=("$part") ;;
    esac
  done
  (IFS=/; printf '%s\n' "${out[*]}")
}

includes="$(
  for rev in "$base" "$head"; do
    git grep -I -o -E 'include!\("[^"]+"\)' "$rev" -- '*.rs' || true
  done | awk '{
    sub(/^[^:]*:/, "")
    i = index($0, ":include!(\"")
    printf "%s\t%s\n", substr($0, 1, i - 1), substr($0, i + 11, length($0) - i - 12)
  }' | while IFS=$'\t' read -r includer included; do
    printf '%s\t%s\n' "$includer" "$(normalize "$(dirname "$includer")/$included")"
  done | sort -u
)"

platform_includer() {
  local path=$1 depth=$2 includer
  [ "$depth" -lt 8 ] || return 1
  while IFS= read -r includer; do
    if has "$includer" "$platform_rust" || platform_includer "$includer" $((depth + 1)) >/dev/null; then
      printf '%s\n' "$includer"
      return 0
    fi
  done < <(awk -F '\t' -v path="$path" '$2 == path { print $1 }' <<<"$includes")
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
    *.rs)
      if has "$path" "$platform_rust"; then
        reason="platform-specific Rust"
      elif includer="$(platform_includer "$path" 0)"; then
        reason="Rust included by platform-specific $includer"
      fi
      ;;
  esac
  if [ -n "$reason" ]; then
    printf '%s\t%s\n' "$path" "$reason"
  fi
done <<<"$changed"

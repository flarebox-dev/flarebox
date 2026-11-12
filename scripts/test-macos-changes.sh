#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/macos-changes.sh"
repo="$(mktemp -d)"
trap 'rm -rf "$repo"' EXIT
cd "$repo"
git init -q
git config user.email test@example.com
git config user.name test
failed=0

write() {
  mkdir -p "$(dirname "$1")"
  printf '%s\n' "$2" > "$1"
}

commit() {
  git add -A
  git commit -qm "$1" --allow-empty
  git rev-parse HEAD
}

expect() {
  local name=$1 want=$2 got
  got="$("$script" "$base" HEAD | cut -f1 | sort | tr '\n' ' ')"
  if [ "$got" = "$want" ]; then
    echo "ok: $name"
  else
    echo "FAIL: $name: want '$want', got '$got'"
    failed=1
  fi
}

write core/engine/src/lib.rs 'pub fn a() {}'
write core/store/src/lib.rs '#[cfg(target_os = "macos")]
fn fullfsync() {}'
write core/cloudflare/src/bundle.rs '#[cfg(unix)]
fn mode() {}'
write cli/src/notify.rs 'fn n() { if cfg!(target_os = "linux") {} }'
write cli/Cargo.toml '[target.'"'"'cfg(target_os = "linux")'"'"'.dependencies]'
write core/engine/Cargo.toml '[dependencies]'
write server/src/index.ts 'export {}'
write core/fs/src/watch.rs 'pub fn w() {}'
write gone.rs '#[cfg(target_os = "windows")] fn w() {}'
base="$(commit base)"

write core/engine/src/lib.rs 'pub fn a() { let _ = 1; }'
write server/src/index.ts 'export const x = 1'
write core/engine/Cargo.toml '[dependencies]
serde = "1"'
commit portable >/dev/null
expect "portable Rust, server and a Cargo.toml without target sections" ""

base="$(git rev-parse HEAD)"
write core/store/src/lib.rs '#[cfg(target_os = "macos")]
fn fullfsync() { }'
write core/cloudflare/src/bundle.rs '#[cfg(unix)]
fn mode() { }'
write cli/src/notify.rs 'fn n() { if cfg!(target_os = "linux") { } }'
write cli/Cargo.toml '[target.'"'"'cfg(target_os = "linux")'"'"'.dependencies]
x = "1"'
write core/fs/src/watch.rs 'pub fn w() { }'
write apps/macos/App.swift 'let a = 1'
write core/ffi/src/lib.rs 'pub fn f() {}'
write Cargo.lock 'x'
write rust-toolchain.toml 'x'
write .cargo/config.toml 'x'
write core/net/build.rs 'fn main() {}'
rm gone.rs
commit platform >/dev/null
expect "platform-specific files, and not code shared by every Unix" ".cargo/config.toml Cargo.lock apps/macos/App.swift cli/Cargo.toml cli/src/notify.rs core/ffi/src/lib.rs core/fs/src/watch.rs core/net/build.rs core/store/src/lib.rs gone.rs rust-toolchain.toml "

base="$(git rev-parse HEAD)"
write core/engine/src/lib.rs '#[cfg(target_os = "macos")]
pub fn a() {}'
commit added-cfg >/dev/null
expect "a cfg added to a portable file" "core/engine/src/lib.rs "

base="$(git rev-parse HEAD)"
write core/chunk/src/simd.rs '#[cfg(target_arch = "aarch64")]
fn neon() {}'
commit arch >/dev/null
expect "a file gated on target_arch" "core/chunk/src/simd.rs "

base="$(git rev-parse HEAD)"
write core/net/src/tls.rs '#[cfg(target_env = "")]
fn system_roots() {}'
write core/net/src/target_dir.rs 'fn target_directory() {}'
commit env >/dev/null
expect "a file gated on target_env, and not a name that starts with target_" "core/net/src/tls.rs "

base="$(git rev-parse HEAD)"
{
  echo '#[cfg(target_os = "macos")]'
  seq 200000 | sed 's/.*/fn filler() {}/'
} > core/engine/src/big.rs
commit big-base >/dev/null
base="$(git rev-parse HEAD)"
echo 'fn tail() {}' >> core/engine/src/big.rs
commit big >/dev/null
expect "a file larger than a pipe buffer with the cfg near the top" "core/engine/src/big.rs "

base="$(git rev-parse HEAD)"
commit empty >/dev/null
expect "no changes" ""

if "$script" "$base" 2>/dev/null; then
  echo "FAIL: one argument was accepted"
  failed=1
else
  echo "ok: one argument is refused"
fi
if "$script" "$base" no-such-rev 2>/dev/null; then
  echo "FAIL: an unknown revision was accepted"
  failed=1
else
  echo "ok: an unknown revision fails"
fi

exit "$failed"

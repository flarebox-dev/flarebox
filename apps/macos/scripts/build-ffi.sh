#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Builds the Rust core (core/ffi) as a static library, generates its Swift
# bindings with UniFFI, and places both in the FlareboxFFI Swift package:
#   FlareboxFFI/Frameworks/flarebox_ffiFFI.xcframework  (library, header, modulemap)
#   FlareboxFFI/Sources/FlareboxFFI/flarebox_ffi.swift  (Swift API)
# Run it again after every change to core/ffi.
set -euo pipefail

macos_dir="$(cd "$(dirname "$0")/.." && pwd)"
repo_root="$(cd "$macos_dir/../.." && pwd)"
package_dir="$macos_dir/FlareboxFFI"
work_dir="$macos_dir/build/ffi"
export MACOSX_DEPLOYMENT_TARGET=14.0

cd "$repo_root"
cargo build --locked --release -p flarebox-ffi
lib="$repo_root/target/release/libflarebox_ffi.a"

rm -rf "$work_dir"
bindgen=(cargo run --locked --quiet -p flarebox-ffi --features bindgen --bin uniffi-bindgen-swift
  --target-dir "$repo_root/target/uniffi-bindgen" --)
"${bindgen[@]}" "$lib" "$work_dir/swift" --swift-sources
"${bindgen[@]}" "$lib" "$work_dir/include" --headers
"${bindgen[@]}" "$lib" "$work_dir/include" --modulemap --module-name flarebox_ffiFFI --modulemap-filename module.modulemap

rm -rf "$package_dir/Frameworks/flarebox_ffiFFI.xcframework"
xcodebuild -create-xcframework -library "$lib" -headers "$work_dir/include" \
  -output "$package_dir/Frameworks/flarebox_ffiFFI.xcframework"
mkdir -p "$package_dir/Sources/FlareboxFFI"
cp "$work_dir/swift/flarebox_ffi.swift" "$package_dir/Sources/FlareboxFFI/"
echo "FlareboxFFI is ready in $package_dir"

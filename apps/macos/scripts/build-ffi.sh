#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Builds the Rust core (core/ffi) as a static library, generates its Swift
# bindings with UniFFI, and places both in the KoidriveFFI Swift package:
#   KoidriveFFI/Frameworks/koidrive_ffiFFI.xcframework  (library, header, modulemap)
#   KoidriveFFI/Sources/KoidriveFFI/koidrive_ffi.swift  (Swift API)
# Run it again after every change to core/ffi.
set -euo pipefail

macos_dir="$(cd "$(dirname "$0")/.." && pwd)"
repo_root="$(cd "$macos_dir/../.." && pwd)"
package_dir="$macos_dir/KoidriveFFI"
work_dir="$macos_dir/build/ffi"
export MACOSX_DEPLOYMENT_TARGET=14.0

cd "$repo_root"
cargo build --locked --release -p koidrive-ffi
lib="$repo_root/target/release/libkoidrive_ffi.a"

rm -rf "$work_dir"
bindgen=(cargo run --locked --quiet -p koidrive-ffi --features bindgen --bin uniffi-bindgen-swift
  --target-dir "$repo_root/target/uniffi-bindgen" --)
"${bindgen[@]}" "$lib" "$work_dir/swift" --swift-sources
"${bindgen[@]}" "$lib" "$work_dir/include" --headers
"${bindgen[@]}" "$lib" "$work_dir/include" --modulemap --module-name koidrive_ffiFFI --modulemap-filename module.modulemap

rm -rf "$package_dir/Frameworks/koidrive_ffiFFI.xcframework"
xcodebuild -create-xcframework -library "$lib" -headers "$work_dir/include" \
  -output "$package_dir/Frameworks/koidrive_ffiFFI.xcframework"
mkdir -p "$package_dir/Sources/KoidriveFFI"
cp "$work_dir/swift/koidrive_ffi.swift" "$package_dir/Sources/KoidriveFFI/"
echo "KoidriveFFI is ready in $package_dir"

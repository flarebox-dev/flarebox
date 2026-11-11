#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Generates Koidrive.xcodeproj from project.yml with a pinned XcodeGen.
# The first run downloads XcodeGen into build/tools and verifies its SHA-256.
# Run it again after changing project.yml or adding or removing source files.
set -euo pipefail

xcodegen_version=2.46.0
xcodegen_sha256=4d9e34b62172d645eed6457cac13fc222569974098ef4ee9c3368bedf0196806

macos_dir="$(cd "$(dirname "$0")/.." && pwd)"
tools_dir="$macos_dir/build/tools/xcodegen-$xcodegen_version"
xcodegen="$tools_dir/xcodegen/bin/xcodegen"

if [ ! -x "$xcodegen" ]; then
  rm -rf "$tools_dir"
  mkdir -p "$tools_dir"
  curl -fsSL -o "$tools_dir/xcodegen.zip" \
    "https://github.com/yonaskolb/XcodeGen/releases/download/$xcodegen_version/xcodegen.zip"
  actual_sha256="$(shasum -a 256 "$tools_dir/xcodegen.zip" | cut -d ' ' -f 1)"
  if [ "$actual_sha256" != "$xcodegen_sha256" ]; then
    echo "error: XcodeGen $xcodegen_version download has SHA-256 $actual_sha256, expected $xcodegen_sha256." >&2
    rm -rf "$tools_dir"
    exit 1
  fi
  unzip -q "$tools_dir/xcodegen.zip" -d "$tools_dir"
fi

"$xcodegen" generate --spec "$macos_dir/project.yml" --project "$macos_dir"

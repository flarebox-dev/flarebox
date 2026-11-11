#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Builds Koidrive.app in the Release configuration without signing it and copies it to
# build/release/Koidrive.app. scripts/sign-app.sh signs it afterwards.
# Optional environment:
#   VERSION       the marketing version (CFBundleShortVersionString), for example 0.1.0
#   BUILD_NUMBER  the build number (CFBundleVersion)
set -euo pipefail

macos_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$macos_dir"

scripts/build-ffi.sh
scripts/generate-project.sh

overrides=()
if [ -n "${VERSION:-}" ]; then overrides+=("MARKETING_VERSION=$VERSION"); fi
if [ -n "${BUILD_NUMBER:-}" ]; then overrides+=("CURRENT_PROJECT_VERSION=$BUILD_NUMBER"); fi

xcodebuild -project Koidrive.xcodeproj -scheme Koidrive -configuration Release \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO ${overrides[@]+"${overrides[@]}"} build

built=build/DerivedData/Build/Products/Release/Koidrive.app
if [ ! -x "$built/Contents/MacOS/Koidrive" ]; then
  echo "error: xcodebuild reported success but $built/Contents/MacOS/Koidrive is missing." >&2
  exit 1
fi
rm -rf build/release
mkdir -p build/release
ditto "$built" build/release/Koidrive.app
echo "Built $macos_dir/build/release/Koidrive.app:"
plutil -p build/release/Koidrive.app/Contents/Info.plist | grep -E 'CFBundle(ShortVersionString|Version|Identifier)"'

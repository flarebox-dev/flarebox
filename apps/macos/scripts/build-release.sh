#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
set -euo pipefail

macos_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$macos_dir"

scripts/build-ffi.sh
scripts/generate-project.sh

koi_version="$(build/koi --version)"
koi_version="${koi_version#koi }"
if [ -n "${VERSION:-}" ] && [ "$VERSION" != "$koi_version" ]; then
  echo "error: koi --version reports $koi_version, but VERSION is $VERSION. Stamp the release version before building." >&2
  exit 1
fi
version="${VERSION:-$koi_version}"
overrides=("MARKETING_VERSION=${version%%-*}" "KOIDRIVE_VERSION=$version")
if [ -n "${BUILD_NUMBER:-}" ]; then overrides+=("CURRENT_PROJECT_VERSION=$BUILD_NUMBER"); fi

xcodebuild -project Koidrive.xcodeproj -scheme Koidrive -configuration Release \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO "${overrides[@]}" build

built=build/DerivedData/Build/Products/Release/Koidrive.app
for executable in MacOS/Koidrive Helpers/koi; do
  if [ ! -x "$built/Contents/$executable" ]; then
    echo "error: xcodebuild reported success but $built/Contents/$executable is missing." >&2
    exit 1
  fi
done
rm -rf build/release
mkdir -p build/release
ditto "$built" build/release/Koidrive.app
echo "Built $macos_dir/build/release/Koidrive.app:"
plutil -p build/release/Koidrive.app/Contents/Info.plist | grep -E '"(CFBundleShortVersionString|CFBundleVersion|CFBundleIdentifier|KoidriveVersion)"'
echo "Contents/Helpers/koi: $(build/release/Koidrive.app/Contents/Helpers/koi --version)"

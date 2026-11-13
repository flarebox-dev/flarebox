#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

binary=${1:?usage: test-homebrew-cask-install.sh <koi binary>}
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tap=koidrive/tap
token=$tap/koidrive
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1

if brew tap | grep -qxF "$tap" || [ -n "$(brew list --formula --versions koidrive 2>/dev/null)" ] || [ -n "$(brew list --cask --versions koidrive 2>/dev/null)" ]; then
    echo 'FAIL the cask fixture needs Homebrew without the Koidrive tap, formula or cask installed' >&2
    exit 1
fi

tmp=$(mktemp -d)
cleanup() {
    brew uninstall --cask --force "$token" > /dev/null 2>&1 || true
    brew uninstall --formula --force "$token" > /dev/null 2>&1 || true
    brew untap "$tap" > /dev/null 2>&1 || true
    rm -rf "$tmp"
}
trap cleanup EXIT
export XDG_CONFIG_HOME=$tmp/config
reported=$("$binary" --version)
version=${reported#koi }
tag=v$version
releases=$tmp/github.com/koidrive/koidrive/releases
release=$releases/download/$tag
mkdir -p "$release" "$tmp/package" "$tmp/Applications" "$tmp/image/Koidrive.app/Contents/MacOS"
cp "$binary" "$tmp/package/koi"
cp /usr/bin/true "$tmp/image/Koidrive.app/Contents/MacOS/Koidrive"
cat > "$tmp/image/Koidrive.app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.koidrive.Koidrive</string>
<key>CFBundleName</key><string>Koidrive</string>
<key>CFBundleExecutable</key><string>Koidrive</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
</dict></plist>
EOF
hdiutil create -quiet -volname Koidrive -srcfolder "$tmp/image" -format UDZO "$release/Koidrive-$version-arm64.dmg"
for target in aarch64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    tar -czf "$release/koidrive-$target.tar.gz" -C "$tmp/package" koi
done
(cd "$release" && shasum -a 256 ./*.tar.gz ./*.dmg | sed 's#  ./#  #' > SHA256SUMS)
brew tap-new --no-git "$tap"
repository=$(brew --repository "$tap")
mkdir -p "$repository/Casks"
KOIDRIVE_RELEASE_BASE_URL="file://$releases" bash "$root/scripts/homebrew-formula.sh" "$tag" "$release/SHA256SUMS" > "$repository/Formula/koidrive.rb"
KOIDRIVE_RELEASE_BASE_URL="file://$releases" bash "$root/scripts/homebrew-cask.sh" "$tag" "$release/SHA256SUMS" > "$repository/Casks/koidrive.rb"
if brew command trust > /dev/null 2>&1; then
    brew trust --formula "$token"
fi
brew install --cask --appdir="$tmp/Applications" "$token"
brew style --cask "$token"
brew audit --cask "$token"
[ -x "$tmp/Applications/Koidrive.app/Contents/MacOS/Koidrive" ] || { echo 'FAIL the cask did not install Koidrive.app in --appdir' >&2; exit 1; }
[ "$("$(brew --prefix)/bin/koi" --version)" = "$reported" ] || { echo 'FAIL the cask did not install its koi formula dependency' >&2; exit 1; }
brew list --cask --versions koidrive
brew list --formula --versions koidrive
brew uninstall --cask "$token"
[ ! -e "$tmp/Applications/Koidrive.app" ] || { echo 'FAIL the app remains after cask uninstall' >&2; exit 1; }
echo 'PASS same-token cask installs Koidrive.app in --appdir and its koi formula dependency, then removes the app on uninstall'

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Checks a release disk image and the app inside it.
# Usage: verify-release.sh <dmg> <signed|unsigned>
#   signed    Developer ID signature with the hardened runtime, notarization tickets stapled
#             to the app and the disk image, and Gatekeeper acceptance of both
#   unsigned  an ad hoc signature with the hardened runtime; Gatekeeper is expected to reject it
# Optional environment:
#   VERSION   the expected CFBundleShortVersionString
set -euo pipefail

if [ $# -ne 2 ] || { [ "$2" != signed ] && [ "$2" != unsigned ]; }; then
  echo "usage: $0 <dmg> <signed|unsigned>" >&2
  exit 2
fi
dmg="$1"
mode="$2"

fail() {
  echo "::error::$1"
  exit 1
}

echo "== hdiutil verify"
hdiutil verify "$dmg"

mountpoint="$(mktemp -d)"
cleanup() {
  hdiutil detach "$mountpoint" -quiet 2>/dev/null || true
  rmdir "$mountpoint" 2>/dev/null || true
}
trap cleanup EXIT
hdiutil attach "$dmg" -readonly -nobrowse -noautoopen -mountpoint "$mountpoint" >/dev/null
app="$mountpoint/Koidrive.app"
[ -x "$app/Contents/MacOS/Koidrive" ] || fail "$dmg does not contain Koidrive.app with its executable."
[ -L "$mountpoint/Applications" ] || fail "$dmg has no link to /Applications."

echo "== Info.plist"
plutil -p "$app/Contents/Info.plist" | grep -E 'CFBundle(ShortVersionString|Version|Identifier)"'
if [ -n "${VERSION:-}" ]; then
  actual="$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")"
  [ "$actual" = "$VERSION" ] || fail "The app's CFBundleShortVersionString is $actual, expected $VERSION."
fi

echo "== codesign --verify --deep --strict (app)"
codesign --verify --deep --strict --verbose=2 "$app" || fail "codesign could not verify the app's signature."
details="$(codesign --display --verbose=4 "$app" 2>&1)"
echo "$details" | grep -E '^(Identifier|Format|CodeDirectory|Signature|Authority|Timestamp|TeamIdentifier|Runtime Version)'
grep -qE '^CodeDirectory .*flags=0x[0-9a-f]*\(.*runtime' <<<"$details" || fail "The app is not signed with the hardened runtime."

echo "== Entitlements"
entitlements="$(codesign --display --entitlements - --xml "$app" 2>/dev/null)"
echo "${entitlements:-<none>}"
if grep -q 'get-task-allow' <<<"$entitlements"; then
  fail "The app has the get-task-allow entitlement, which notarization rejects."
fi

if [ "$mode" = unsigned ]; then
  grep -q '^Signature=adhoc' <<<"$details" || fail "The unsigned build is expected to carry an ad hoc signature."
  echo "== spctl (expected to reject an unsigned build)"
  spctl --assess --type execute --verbose=2 "$app" || true
  echo "Verified the unsigned disk image: ad hoc signature with the hardened runtime, not notarized."
  exit 0
fi

grep -q '^Authority=Developer ID Application: ' <<<"$details" || fail "The app is not signed with a Developer ID Application certificate."
grep -q '^Timestamp=' <<<"$details" || fail "The app's signature has no secure timestamp."

echo "== stapler validate (app)"
xcrun stapler validate "$app" || fail "The app has no valid stapled notarization ticket."
echo "== spctl (app)"
assessment="$(spctl --assess --type execute --verbose=2 "$app" 2>&1)" || { echo "$assessment"; fail "Gatekeeper rejects the app."; }
echo "$assessment"
grep -q 'source=Notarized Developer ID' <<<"$assessment" || fail "Gatekeeper accepts the app, but not as notarized Developer ID software."

echo "== codesign --verify --deep --strict (disk image)"
codesign --verify --deep --strict --verbose=2 "$dmg" || fail "codesign could not verify the disk image's signature."
echo "== stapler validate (disk image)"
xcrun stapler validate "$dmg" || fail "The disk image has no valid stapled notarization ticket."
echo "== spctl (disk image)"
assessment="$(spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg" 2>&1)" || { echo "$assessment"; fail "Gatekeeper rejects the disk image."; }
echo "$assessment"
grep -q 'source=Notarized Developer ID' <<<"$assessment" || fail "Gatekeeper accepts the disk image, but not as notarized Developer ID software."

echo "Verified the signed disk image: Developer ID, hardened runtime, notarized and stapled."

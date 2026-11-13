#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Usage: sign-app.sh <app> <identity>
#   identity  the SHA-1 hash or name of a Developer ID Application identity, or - for an
#             ad hoc signature (unsigned builds)
# Optional environment:
#   KEYCHAIN  the keychain that holds the identity
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <app> <identity>" >&2
  exit 2
fi
app="$1"
identity="$2"
macos_dir="$(cd "$(dirname "$0")/.." && pwd)"

helper="$app/Contents/Helpers/koi"
if [ ! -x "$helper" ]; then
  echo "error: $helper is missing; build the app with scripts/build-release.sh." >&2
  exit 1
fi

args=(--force --options runtime --sign "$identity")
if [ "$identity" = "-" ]; then
  args+=(--timestamp=none)
else
  args+=(--timestamp)
fi
if [ -n "${KEYCHAIN:-}" ]; then args+=(--keychain "$KEYCHAIN"); fi

codesign "${args[@]}" "$helper"
codesign "${args[@]}" --entitlements "$macos_dir/Koidrive.entitlements" "$app"
for code in "$helper" "$app"; do
  codesign --display --verbose=2 "$code" 2>&1 | grep -E '^(Identifier|Authority|TeamIdentifier|Timestamp|CodeDirectory)'
done

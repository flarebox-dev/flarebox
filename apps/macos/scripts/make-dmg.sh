#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Packages an app bundle into a compressed disk image with a link to /Applications.
# Usage: make-dmg.sh <app> <dmg>
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <app> <dmg>" >&2
  exit 2
fi
app="$1"
dmg="$2"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
ditto "$app" "$staging/$(basename "$app")"
ln -s /Applications "$staging/Applications"

rm -f "$dmg"
# hdiutil create sometimes fails with "Resource busy" on CI machines; retry before giving up.
for attempt in 1 2 3 4 5; do
  if hdiutil create -volname Flarebox -srcfolder "$staging" -fs HFS+ -format UDZO -ov "$dmg"; then
    break
  fi
  if [ "$attempt" -eq 5 ]; then
    echo "error: hdiutil create failed 5 times; see its output above." >&2
    exit 1
  fi
  echo "hdiutil create failed (attempt $attempt of 5); retrying in 10 seconds."
  sleep 10
done
hdiutil verify "$dmg"

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Submits a ZIP archive, disk image or installer package to Apple's notary service with an
# App Store Connect API key, waits for the result, and prints Apple's log if it is rejected.
# Usage: notarize.sh <file>
# Environment:
#   NOTARY_KEY_PATH   path to the API key's .p8 private key file
#   NOTARY_KEY_ID     the API key's ID
#   NOTARY_ISSUER_ID  the issuer ID of the team's API keys
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <file>" >&2
  exit 2
fi
file="$1"
: "${NOTARY_KEY_PATH:?is not set}" "${NOTARY_KEY_ID:?is not set}" "${NOTARY_ISSUER_ID:?is not set}"
auth=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")

echo "Submitting $(basename "$file") for notarization and waiting for the result."
status=0
result="$(xcrun notarytool submit "$file" "${auth[@]}" --wait --timeout 1h --output-format json)" || status=$?
echo "$result"
id="$(plutil -extract id raw -o - - <<<"$result" 2>/dev/null || true)"
verdict="$(plutil -extract status raw -o - - <<<"$result" 2>/dev/null || true)"

if [ "$status" -eq 0 ] && [ "$verdict" = "Accepted" ]; then
  echo "Notarization accepted (submission $id)."
  exit 0
fi
if [ -n "$id" ]; then
  echo "Apple's notarization log for submission $id:"
  xcrun notarytool log "$id" "${auth[@]}" || echo "Could not fetch the log for submission $id."
fi
echo "::error::Notarization of $(basename "$file") did not succeed: notarytool exited with $status and reported status '${verdict:-unknown}'. Apple's log above lists the issues."
exit 1

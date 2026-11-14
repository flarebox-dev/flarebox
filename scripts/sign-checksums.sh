#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

files=${1:?usage: sign-checksums.sh <directory>}
: "${RELEASE_ALLOWED_SIGNERS:?is not set}"
sums="$files/SHA256SUMS"
[ -f "$sums" ] || { echo "::error::$files has no SHA256SUMS."; exit 1; }
[ -n "${RELEASE_SIGNING_KEY:-}" ] || { echo "::error::Signing SHA256SUMS needs the secret RELEASE_SIGNING_KEY in the release-signing environment."; exit 1; }
key=$(mktemp)
trap 'rm -f "$key"' EXIT
printf '%s\n' "$RELEASE_SIGNING_KEY" > "$key"
rm -f "$sums.sig"
ssh-keygen -q -Y sign -f "$key" -n koidrive-release "$sums"
if ! ssh-keygen -q -Y verify -f "$RELEASE_ALLOWED_SIGNERS" -I koidrive -n koidrive-release -s "$sums.sig" < "$sums" > /dev/null; then
    echo "::error::RELEASE_SIGNING_KEY does not match the release key pinned in $RELEASE_ALLOWED_SIGNERS."
    exit 1
fi
echo "Signed SHA256SUMS with the release key pinned in $RELEASE_ALLOWED_SIGNERS."

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Notarizes a ZIP archive, disk image or installer package with an App Store Connect API key.
# It submits the file without waiting, prints the submission id, and then asks Apple for the
# submission's status until Apple reports Accepted, Invalid or Rejected. Network errors while
# asking are retried with backoff until the deadline, so a dropped connection does not lose a
# submission that Apple is still processing. On Invalid or Rejected it prints Apple's log.
# Usage: notarize.sh <file> [submission-id]
#   submission-id  an earlier submission of the same file to wait for instead of submitting again
# Environment:
#   NOTARY_KEY_PATH          path to the API key's .p8 private key file
#   NOTARY_KEY_ID            the API key's ID
#   NOTARY_ISSUER_ID         the issuer ID of the team's API keys
#   NOTARY_DEADLINE_SECONDS  how long to wait for Apple's answer (default 9000, 2.5 hours)
#   NOTARY_POLL_SECONDS      the wait between two status checks (default 60)
#   NOTARY_BACKOFF_SECONDS   the first wait after a network error, doubled up to 300 (default 15)
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: $0 <file> [submission-id]" >&2
  exit 2
fi
file="$1"
id="${2:-}"
name="$(basename "$file")"
: "${NOTARY_KEY_PATH:?is not set}" "${NOTARY_KEY_ID:?is not set}" "${NOTARY_ISSUER_ID:?is not set}"
auth=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")
deadline=$((SECONDS + ${NOTARY_DEADLINE_SECONDS:-9000}))
errors="$(mktemp)"
trap 'rm -f "$errors"' EXIT
poll="${NOTARY_POLL_SECONDS:-60}"
first_backoff="${NOTARY_BACKOFF_SECONDS:-15}"

field() {
  plutil -extract "$1" raw -o - - <<<"$2" 2>/dev/null || true
}

# notarytool reports a failed request as HTTPError(statusCode: <code or nil>, ...). No status
# code (the request never got an answer), a timeout, throttling and server errors are worth
# retrying; any other answer, such as 401 or 404, is not.
transient() {
  grep -qE 'NSURLErrorDomain|statusCode: (nil|408|429|5[0-9][0-9])' <<<"$1"
}

sleep_until_deadline() {
  local wait=$1
  if [ $((SECONDS + wait)) -gt "$deadline" ]; then wait=$((deadline - SECONDS)); fi
  if [ "$wait" -gt 0 ]; then sleep "$wait"; fi
}

if [ -z "$id" ]; then
  echo "Submitting $name for notarization."
  result="$(xcrun notarytool submit "$file" "${auth[@]}" --output-format json 2>"$errors")" || {
    cat "$errors"
    echo "$result"
    echo "::error::Submitting $name for notarization failed; notarytool's output is above. Nothing was submitted, so run the job again."
    exit 1
  }
  echo "$result"
  id="$(field id "$result")"
  if [ -z "$id" ]; then
    echo "::error::notarytool accepted $name but reported no submission id."
    exit 1
  fi
fi
echo "::notice title=Notarization of $name::Submission $id"
echo "Waiting for Apple's answer on submission $id for $name."

backoff="$first_backoff"
while true; do
  status=0
  result="$(xcrun notarytool info "$id" "${auth[@]}" --output-format json 2>"$errors")" || status=$?
  if [ "$status" -eq 0 ]; then
    backoff="$first_backoff"
    verdict="$(field status "$result")"
    case "$verdict" in
      Accepted)
        echo "Notarization accepted (submission $id)."
        exit 0
        ;;
      Invalid | Rejected)
        echo "$result"
        echo "Apple's notarization log for submission $id:"
        xcrun notarytool log "$id" "${auth[@]}" || echo "Could not fetch the log for submission $id."
        echo "::error::Apple reported submission $id for $name as $verdict. Apple's log above lists the issues."
        exit 1
        ;;
      *)
        echo "Submission $id is ${verdict:-in an unknown state}; asking again in $poll seconds."
        wait="$poll"
        ;;
    esac
  elif transient "$(cat "$errors")$result"; then
    cat "$errors"
    echo "Asking about submission $id failed with a network error; retrying in $backoff seconds."
    wait="$backoff"
    backoff=$((backoff * 2 > 300 ? 300 : backoff * 2))
  else
    cat "$errors"
    echo "$result"
    echo "::error::notarytool could not report the status of submission $id for $name (exit code $status); its output is above."
    exit 1
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "::error::Apple gave no final answer on submission $id for $name before the deadline. Apple may still accept it; xcrun notarytool info $id shows its status."
    exit 1
  fi
  sleep_until_deadline "$wait"
done

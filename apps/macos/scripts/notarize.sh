#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Notarizes a ZIP archive, disk image or installer package with an App Store Connect API key.
# It submits the file without waiting, prints the submission id, and then asks Apple for the
# submission's status until Apple reports Accepted, Invalid or Rejected. Network errors while
# asking are retried with backoff until the deadline, so a dropped connection does not lose a
# submission that Apple is still processing. A notarytool call that hangs is stopped at the
# deadline. On Invalid or Rejected it prints Apple's log. The output never shows the API key's
# path, ID or issuer ID.
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
poll="${NOTARY_POLL_SECONDS:-60}"
first_backoff="${NOTARY_BACKOFF_SECONDS:-15}"
out="$(mktemp)"
errors="$(mktemp)"
trap 'rm -f "$out" "$errors"' EXIT
# Each notarytool call runs in its own process group, so that a call that outlasts its time can be
# stopped together with the processes it started.
set -m

# bounded <seconds> <command...> runs the command with its output in $out and $errors and stops it
# after the given time. It returns the command's exit status, or 124 if the command was stopped.
bounded() {
  local end=$((SECONDS + $1))
  shift
  "$@" >"$out" 2>"$errors" &
  local pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$SECONDS" -ge "$end" ]; then
      kill -TERM -- "-$pid" 2>/dev/null || true
      sleep 2
      kill -KILL -- "-$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi
    sleep 0.2
  done
  wait "$pid"
}

# show prints what the last notarytool call wrote, without the API key's path, ID or issuer ID,
# which notarytool repeats in some errors.
show() {
  local text
  text="$(cat "$errors" "$out")"
  text="${text//"$NOTARY_KEY_PATH"/<notary key path>}"
  text="${text//"$NOTARY_KEY_ID"/<notary key ID>}"
  text="${text//"$NOTARY_ISSUER_ID"/<notary issuer ID>}"
  if [ -n "$text" ]; then printf '%s\n' "$text"; fi
}

field() {
  plutil -extract "$1" raw -o - "$out" 2>/dev/null || true
}

# notarytool reports a failed request as HTTPError(statusCode: <code or nil>, error: ...). A request
# that got no answer because the network was down or slow, throttling and server errors are worth
# retrying. Any other answer, such as 401 or 404, and any other URL error, such as an untrusted
# certificate (-1202), is not.
transient() {
  local text
  text="$(cat "$errors" "$out")"
  if grep -qE 'NSURLErrorDomain Code=' <<<"$text"; then
    grep -qE 'NSURLErrorDomain Code=-10(01|03|04|05|09|18|20)([^0-9]|$)' <<<"$text"
  else
    grep -qE 'statusCode: (nil|408|429|5[0-9][0-9])' <<<"$text"
  fi
}

remaining() {
  echo $((deadline - SECONDS))
}

sleep_until_deadline() {
  local wait=$1
  if [ "$wait" -gt "$(remaining)" ]; then wait="$(remaining)"; fi
  if [ "$wait" -gt 0 ]; then sleep "$wait"; fi
}

if [ -z "$id" ]; then
  echo "Submitting $name for notarization."
  status=0
  bounded "$(remaining)" xcrun notarytool submit "$file" "${auth[@]}" --output-format json || status=$?
  show
  if [ "$status" -eq 124 ]; then
    echo "::error::Submitting $name for notarization did not finish before the deadline. Run the job again."
    exit 1
  fi
  if [ "$status" -ne 0 ]; then
    echo "::error::Submitting $name for notarization failed; notarytool's output is above. Nothing was submitted, so run the job again."
    exit 1
  fi
  id="$(field id)"
  if [ -z "$id" ]; then
    echo "::error::notarytool accepted $name but reported no submission id."
    exit 1
  fi
fi
echo "::notice title=Notarization of $name::Submission $id"
echo "Waiting for Apple's answer on submission $id for $name."

backoff="$first_backoff"
while [ "$(remaining)" -gt 0 ]; do
  status=0
  bounded "$(remaining)" xcrun notarytool info "$id" "${auth[@]}" --output-format json || status=$?
  if [ "$status" -eq 124 ]; then
    break
  elif [ "$status" -eq 0 ]; then
    backoff="$first_backoff"
    verdict="$(field status)"
    case "$verdict" in
      Accepted)
        echo "Notarization accepted (submission $id)."
        exit 0
        ;;
      Invalid | Rejected)
        show
        echo "Apple's notarization log for submission $id:"
        status=0
        bounded 120 xcrun notarytool log "$id" "${auth[@]}" || status=$?
        show
        if [ "$status" -ne 0 ]; then echo "Could not fetch the log for submission $id."; fi
        echo "::error::Apple reported submission $id for $name as $verdict. Apple's log above lists the issues."
        exit 1
        ;;
      *)
        echo "Submission $id is ${verdict:-in an unknown state}; asking again in $poll seconds."
        wait="$poll"
        ;;
    esac
  elif transient; then
    show
    echo "Asking about submission $id failed with a network error; retrying in $backoff seconds."
    wait="$backoff"
    backoff=$((backoff * 2 > 300 ? 300 : backoff * 2))
  else
    show
    echo "::error::notarytool could not report the status of submission $id for $name (exit code $status); its output is above."
    exit 1
  fi
  sleep_until_deadline "$wait"
done
echo "::error::Apple gave no final answer on submission $id for $name before the deadline. Apple may still accept it; xcrun notarytool info $id shows its status."
exit 1

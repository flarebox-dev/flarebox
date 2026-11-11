#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs notarize.sh against a stub xcrun whose notarytool answers from a script of responses, so
# that the waiting and retrying can be checked without Apple credentials or the network.
# Usage: test-notarize.sh
set -euo pipefail

scripts="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin"
cat >"$work/bin/xcrun" <<'STUB'
#!/usr/bin/env bash
# notarytool's answers come from $STUB/answers, one line per info call: a status; network,
# untrusted or denied for a failed request; or hang for a request that never returns. Once the
# answers run out, every request fails with a network error. Errors repeat the arguments, as
# notarytool's own errors repeat the key's path.
[ "$1" = notarytool ] || exit 64
echo "$2 ${3:-}" >>"$STUB/calls"
case "$2" in
  submit)
    if [ -f "$STUB/submit-fails" ]; then
      echo "Error: HTTPError(statusCode: nil, error: Error Domain=NSURLErrorDomain Code=-1009) for $*" >&2
      exit 1
    fi
    echo '{"id":"11111111-2222-3333-4444-555555555555","message":"Successfully uploaded file","path":"x"}'
    ;;
  info)
    answer="$(head -n 1 "$STUB/answers")"
    answer="${answer:-network}"
    tail -n +2 "$STUB/answers" >"$STUB/rest" && mv "$STUB/rest" "$STUB/answers"
    case "$answer" in
      network)
        echo "Error: HTTPError(statusCode: nil, error: Error Domain=NSURLErrorDomain Code=-1009 \"The Internet connection appears to be offline.\") for $*" >&2
        exit 1
        ;;
      untrusted)
        echo "Error: HTTPError(statusCode: nil, error: Error Domain=NSURLErrorDomain Code=-1202 \"The certificate for this server is invalid.\") for $*" >&2
        exit 1
        ;;
      denied)
        echo "Error: HTTPError(statusCode: 401, error: Unauthenticated) for $*" >&2
        exit 1
        ;;
      hang)
        sleep 30
        echo "{\"id\":\"$3\",\"status\":\"Accepted\",\"name\":\"x\"}"
        ;;
      *) echo "{\"id\":\"$3\",\"status\":\"$answer\",\"name\":\"x\"}" ;;
    esac
    ;;
  log) echo "{\"issues\":[{\"severity\":\"error\",\"message\":\"The binary is not signed.\"}],\"request\":\"$*\"}" ;;
  *) exit 64 ;;
esac
STUB
chmod +x "$work/bin/xcrun"

failures=0

# run <name> <expected exit code> <comma-separated answers> [submission-id]: runs notarize.sh and leaves its output
# in $work/<name>.out and the stub's calls in $work/<name>/calls.
run() {
  local name=$1 expected=$2 answers=$3
  shift 3
  mkdir -p "$work/$name"
  tr ',' '\n' <<<"$answers" >"$work/$name/answers"
  : >"$work/$name/calls"
  local status=0
  STUB="$work/$name" PATH="$work/bin:$PATH" NOTARY_KEY_PATH="$key_path" NOTARY_KEY_ID="$key_id" NOTARY_ISSUER_ID="$issuer_id" \
    NOTARY_POLL_SECONDS=0 NOTARY_BACKOFF_SECONDS="${BACKOFF:-0}" NOTARY_DEADLINE_SECONDS="${DEADLINE:-60}" \
    "$scripts/notarize.sh" Koidrive.zip "$@" >"$work/$name.out" 2>&1 || status=$?
  if [ "$status" -ne "$expected" ]; then
    fail "$name" "exited with $status, expected $expected"
  fi
}

fail() {
  echo "FAIL $1: $2"
  sed 's/^/  | /' "$work/$1.out"
  failures=$((failures + 1))
}

expect_output() {
  grep -qF -- "$2" "$work/$1.out" || fail "$1" "the output lacks: $2"
}

expect_calls() {
  local calls
  calls="$(cut -d ' ' -f 1 "$work/$1/calls" | tr '\n' ' ')"
  [ "$calls" = "$2" ] || fail "$1" "notarytool calls were '$calls', expected '$2'"
}

id=11111111-2222-3333-4444-555555555555
key_path="$work/signing/notary-key.p8"
key_id=STUBKEY4242
issuer_id=69a6de7e-0000-4000-8000-stubissuer00

run network_errors_while_waiting_are_retried 0 "network,network,network,In Progress,Accepted"
expect_output network_errors_while_waiting_are_retried "Submission $id"
expect_output network_errors_while_waiting_are_retried "failed with a network error; retrying"
expect_output network_errors_while_waiting_are_retried "Notarization accepted (submission $id)."
expect_calls network_errors_while_waiting_are_retried "submit info info info info info "

run an_earlier_submission_is_awaited_without_submitting_again 0 "Accepted" "$id"
expect_calls an_earlier_submission_is_awaited_without_submitting_again "info "
expect_output an_earlier_submission_is_awaited_without_submitting_again "Notarization accepted (submission $id)."

run an_invalid_submission_prints_apples_log 1 "Invalid"
expect_output an_invalid_submission_prints_apples_log "The binary is not signed."
expect_output an_invalid_submission_prints_apples_log "Apple reported submission $id for Koidrive.zip as Invalid"
expect_calls an_invalid_submission_prints_apples_log "submit info log "

run a_rejected_submission_fails 1 "Rejected"
expect_output a_rejected_submission_fails "as Rejected"

run a_refused_status_request_fails_at_once 1 "denied,Accepted"
expect_output a_refused_status_request_fails_at_once "could not report the status of submission $id"
expect_calls a_refused_status_request_fails_at_once "submit info "

DEADLINE=3 BACKOFF=1 run the_deadline_ends_the_wait_and_names_the_submission 1 "network"
expect_output the_deadline_ends_the_wait_and_names_the_submission "Apple gave no final answer on submission $id"

mkdir -p "$work/a_failed_upload_fails_without_waiting" && touch "$work/a_failed_upload_fails_without_waiting/submit-fails"
run a_failed_upload_fails_without_waiting 1 "Accepted"
expect_output a_failed_upload_fails_without_waiting "Nothing was submitted"
expect_calls a_failed_upload_fails_without_waiting "submit "

DEADLINE=5 run an_untrusted_certificate_fails_at_once 1 "untrusted,Accepted"
expect_output an_untrusted_certificate_fails_at_once "could not report the status of submission $id"
expect_calls an_untrusted_certificate_fails_at_once "submit info "

started=$SECONDS
DEADLINE=2 run a_hanging_status_request_ends_at_the_deadline 1 "hang"
expect_output a_hanging_status_request_ends_at_the_deadline "Apple gave no final answer on submission $id"
if [ $((SECONDS - started)) -gt 10 ]; then
  fail a_hanging_status_request_ends_at_the_deadline "took $((SECONDS - started)) seconds with a 2-second deadline"
fi

for out in "$work"/*.out; do
  for value in "$key_path" "$key_id" "$issuer_id"; do
    if grep -qF -- "$value" "$out"; then
      fail "$(basename "$out" .out)" "the output shows the API key's path, ID or issuer ID ($value)"
    fi
  done
done

if [ "$failures" -ne 0 ]; then
  echo "test-notarize.sh: $failures check(s) failed."
  exit 1
fi
echo "test-notarize.sh: all 9 cases passed, and no output shows the API key's path, ID or issuer ID."

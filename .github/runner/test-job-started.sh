#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs job-started.sh against fake event payloads the way the runner calls it (bash -e, with
# only the base system on PATH) and checks which jobs it allows.
set -euo pipefail

hook="$(cd "$(dirname "$0")" && pwd)/job-started.sh"
tmp="$(mktemp -d)"
trap 'chmod -R u+rw "$tmp"; rm -rf "$tmp"' EXIT
failures=0

pr() { printf '{"action":"synchronize","pull_request":{"number":1,"head":{"ref":"x","repo":%s}},"repository":{"full_name":"flarebox-dev/flarebox"}}' "$1"; }
pr '{"full_name":"flarebox-dev/flarebox"}' > "$tmp/same-repo-pr.json"
pr '{"full_name":"someone/flarebox"}' > "$tmp/fork-pr.json"
pr 'null' > "$tmp/deleted-fork-pr.json"
printf '{"ref":"refs/heads/main","repository":{"full_name":"flarebox-dev/flarebox"},"pusher":{"name":"x"},"head_commit":null}' > "$tmp/push.json"
printf '{"action":"checks_requested","merge_group":{"head_ref":"refs/heads/gh-readonly-queue/main/pr-1-abc","base_sha":"abc"},"repository":{"full_name":"flarebox-dev/flarebox"}}' > "$tmp/merge-group.json"
printf '{"ref":"refs/heads/main","inputs":{},"repository":{"full_name":"flarebox-dev/flarebox"}}' > "$tmp/workflow-dispatch.json"
printf '{"pull_request":' > "$tmp/truncated.json"
printf '[]' > "$tmp/array.json"
cp "$tmp/push.json" "$tmp/unreadable.json"
chmod 000 "$tmp/unreadable.json"

# check NAME EXPECTED_EXIT EXPECTED_OUTPUT EVENT_NAME EVENT_PATH [REPOSITORY]
check() {
  local name="$1" want="$2" text="$3" event="$4" payload="$5" repo="${6-flarebox-dev/flarebox}" got=0 out
  out="$(env -i PATH=/usr/bin:/bin RUNNER_NAME=test GITHUB_REPOSITORY="$repo" GITHUB_EVENT_NAME="$event" GITHUB_EVENT_PATH="$payload" /bin/bash -e "$hook" 2>&1)" || got=$?
  if [ "$got" = "$want" ] && [[ "$out" == *"$text"* ]]; then
    echo "ok   $name"
  else
    echo "FAIL $name: exit $got (expected $want), output:"
    echo "$out"
    echo "     expected the output to contain: $text"
    failures=$((failures + 1))
  fi
}

check "pull request from the same repository runs" 0 "pull_request in flarebox-dev/flarebox may run here" pull_request "$tmp/same-repo-pr.json"
check "pull request from a fork is refused" 1 "comes from someone/flarebox" pull_request "$tmp/fork-pr.json"
check "pull_request_target from a fork is refused" 1 "comes from someone/flarebox" pull_request_target "$tmp/fork-pr.json"
check "pull_request_review from a fork is refused" 1 "comes from someone/flarebox" pull_request_review "$tmp/fork-pr.json"
check "pull request from a deleted fork is refused" 1 "names no head repository" pull_request "$tmp/deleted-fork-pr.json"
check "push runs" 0 "push in flarebox-dev/flarebox may run here" push "$tmp/push.json"
check "merge_group runs" 0 "merge_group in flarebox-dev/flarebox may run here" merge_group "$tmp/merge-group.json"
check "workflow_dispatch runs" 0 "workflow_dispatch in flarebox-dev/flarebox may run here" workflow_dispatch "$tmp/workflow-dispatch.json"
check "unreadable event payload is refused" 1 "unset or unreadable" push "$tmp/unreadable.json"
check "missing event payload is refused" 1 "unset or unreadable" push "$tmp/missing.json"
check "unset event payload path is refused" 1 "unset or unreadable" push ""
check "unset event name is refused" 1 "unset or unreadable" "" "$tmp/push.json"
check "truncated JSON is refused" 1 "is not a JSON object" push "$tmp/truncated.json"
check "JSON that is not an object is refused" 1 "is not a JSON object" merge_group "$tmp/array.json"
check "a job from another repository is refused" 1 "these runners serve only flarebox-dev/flarebox" push "$tmp/push.json" someone/flarebox
check "a job with no repository is refused" 1 "these runners serve only flarebox-dev/flarebox" push "$tmp/push.json" ""

if [ "$failures" -ne 0 ]; then
  echo "$failures job-started.sh test(s) failed."
  exit 1
fi
echo "All job-started.sh tests passed."

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs job-started.sh against fake event payloads the way the runner calls it (bash -e, with
# only the base system on PATH) and checks that it allows only pushes to main, merge queue runs
# for main and pull requests from branches of this repository.
set -euo pipefail

hook="$(cd "$(dirname "$0")" && pwd)/job-started.sh"
tmp="$(mktemp -d)"
trap 'chmod -R u+rw "$tmp"; rm -rf "$tmp"' EXIT
failures=0

queue_ref=refs/heads/gh-readonly-queue/main/pr-73-15cfab4073d9b7abc02f71c8f98f2b5a5502829d
pr() { printf '{"action":"synchronize","pull_request":{"number":1,"head":{"ref":"x","repo":%s}},"repository":{"full_name":"flarebox-dev/flarebox"}}' "$1"; }
push() { printf '{"ref":"%s","repository":{"full_name":"flarebox-dev/flarebox"},"pusher":{"name":"x"},"head_commit":null}' "$1"; }
queue() { printf '{"action":"checks_requested","merge_group":{"head_ref":"%s","base_ref":"%s","base_sha":"abc"},"repository":{"full_name":"flarebox-dev/flarebox"}}' "$1" "$2"; }
pr '{"full_name":"flarebox-dev/flarebox"}' > "$tmp/same-repo-pr.json"
pr '{"full_name":"someone/flarebox"}' > "$tmp/fork-pr.json"
pr null > "$tmp/deleted-fork-pr.json"
pr '{"full_name":"flarebox-dev/flarebox-other"}' > "$tmp/sibling-repo-pr.json"
printf '{"pull_request":{"head":{"repo":{"full_name":"flarebox-dev/flarebox"}}},"repository":{"full_name":"flarebox-dev/flarebox"}}' > "$tmp/pr-no-number.json"
push refs/heads/main > "$tmp/push-main.json"
push refs/heads/feature > "$tmp/push-feature.json"
push refs/tags/v1.0.0 > "$tmp/push-tag.json"
printf '{"repository":{"full_name":"flarebox-dev/flarebox"}}' > "$tmp/push-no-ref.json"
queue "$queue_ref" refs/heads/main > "$tmp/queue-main.json"
queue refs/heads/gh-readonly-queue/release/pr-1-abc refs/heads/release > "$tmp/queue-release.json"
queue "$queue_ref" refs/heads/release > "$tmp/queue-wrong-base.json"
queue refs/heads/gh-readonly-queue/main/pr-2-def refs/heads/main > "$tmp/queue-other-head.json"
printf '{"merge_group":{"head_ref":"%s"},"repository":{"full_name":"flarebox-dev/flarebox"}}' "$queue_ref" > "$tmp/queue-no-base.json"
printf '{"ref":"refs/heads/main","inputs":{},"repository":{"full_name":"flarebox-dev/flarebox"}}' > "$tmp/workflow-dispatch.json"
printf '{"schedule":"0 3 * * *","repository":{"full_name":"flarebox-dev/flarebox"}}' > "$tmp/schedule.json"
printf '{"pull_request":' > "$tmp/truncated.json"
printf '[]' > "$tmp/array.json"
cp "$tmp/push-main.json" "$tmp/unreadable.json"
chmod 000 "$tmp/unreadable.json"

# check NAME EXPECTED_EXIT EXPECTED_OUTPUT EVENT_NAME EVENT_PATH GITHUB_REF [REPOSITORY]
check() {
  local name="$1" want="$2" text="$3" event="$4" payload="$5" ref="$6" repo="${7-flarebox-dev/flarebox}" got=0 out
  out="$(env -i PATH=/usr/bin:/bin RUNNER_NAME=test GITHUB_REPOSITORY="$repo" GITHUB_EVENT_NAME="$event" GITHUB_EVENT_PATH="$payload" GITHUB_REF="$ref" /bin/bash -e "$hook" 2>&1)" || got=$?
  if [ "$got" = "$want" ] && [[ "$out" == *"$text"* ]]; then
    echo "ok   $name"
  else
    echo "FAIL $name: exit $got (expected $want), output:"
    echo "$out"
    echo "     expected the output to contain: $text"
    failures=$((failures + 1))
  fi
}

check "push to main runs" 0 "push on refs/heads/main in flarebox-dev/flarebox may run here" push "$tmp/push-main.json" refs/heads/main
check "merge queue run for main runs" 0 "merge_group on $queue_ref in flarebox-dev/flarebox may run here" merge_group "$tmp/queue-main.json" "$queue_ref"
check "pull request from the same repository runs" 0 "pull_request on refs/pull/1/merge in flarebox-dev/flarebox may run here" pull_request "$tmp/same-repo-pr.json" refs/pull/1/merge
check "pull request from a fork is refused" 1 "whose head is not in flarebox-dev/flarebox" pull_request "$tmp/fork-pr.json" refs/pull/1/merge
check "pull request from a deleted fork is refused" 1 "whose head is not in flarebox-dev/flarebox" pull_request "$tmp/deleted-fork-pr.json" refs/pull/1/merge
check "pull request from a repository with a longer name is refused" 1 "whose head is not in flarebox-dev/flarebox" pull_request "$tmp/sibling-repo-pr.json" refs/pull/1/merge
check "same-repository pull request on another pull request's ref is refused" 1 "does not name 'refs/pull/2/merge'" pull_request "$tmp/same-repo-pr.json" refs/pull/2/merge
check "same-repository pull request on its head ref is refused" 1 "does not name 'refs/pull/1/head'" pull_request "$tmp/same-repo-pr.json" refs/pull/1/head
check "same-repository pull request on main is refused" 1 "does not name 'refs/heads/main'" pull_request "$tmp/same-repo-pr.json" refs/heads/main
check "pull request payload with no number is refused" 1 "does not name 'refs/pull/1/merge'" pull_request "$tmp/pr-no-number.json" refs/pull/1/merge
check "pull request from the same repository in another repository's job is refused" 1 "these runners serve only flarebox-dev/flarebox" pull_request "$tmp/same-repo-pr.json" refs/pull/1/merge someone/flarebox
check "pull_request_target is refused" 1 "its event is pull_request_target" pull_request_target "$tmp/fork-pr.json" refs/heads/main
check "pull_request_target from the same repository is refused" 1 "its event is pull_request_target" pull_request_target "$tmp/same-repo-pr.json" refs/heads/main
check "pull_request_review is refused" 1 "its event is pull_request_review" pull_request_review "$tmp/same-repo-pr.json" refs/pull/1/merge
check "workflow_dispatch on main is refused" 1 "its event is workflow_dispatch" workflow_dispatch "$tmp/workflow-dispatch.json" refs/heads/main
check "schedule is refused" 1 "its event is schedule" schedule "$tmp/schedule.json" refs/heads/main
check "push to another branch is refused" 1 "it is a push to 'refs/heads/feature'" push "$tmp/push-feature.json" refs/heads/feature
check "push of a tag is refused" 1 "it is a push to 'refs/tags/v1.0.0'" push "$tmp/push-tag.json" refs/tags/v1.0.0
check "push whose payload names another ref is refused" 1 "its push payload does not name refs/heads/main" push "$tmp/push-feature.json" refs/heads/main
check "push whose payload names no ref is refused" 1 "its push payload does not name refs/heads/main" push "$tmp/push-no-ref.json" refs/heads/main
check "merge queue run for another branch is refused" 1 "these runners take only queue runs for main" merge_group "$tmp/queue-release.json" refs/heads/gh-readonly-queue/release/pr-1-abc
check "merge_group on a ref outside the queue is refused" 1 "these runners take only queue runs for main" merge_group "$tmp/queue-main.json" refs/heads/main
check "merge_group whose payload base is not main is refused" 1 "is not a queue run of $queue_ref into refs/heads/main" merge_group "$tmp/queue-wrong-base.json" "$queue_ref"
check "merge_group whose payload head is another queue ref is refused" 1 "is not a queue run of $queue_ref into refs/heads/main" merge_group "$tmp/queue-other-head.json" "$queue_ref"
check "merge_group whose payload has no base ref is refused" 1 "is not a queue run of $queue_ref into refs/heads/main" merge_group "$tmp/queue-no-base.json" "$queue_ref"
check "unreadable event payload is refused" 1 "unset or unreadable" push "$tmp/unreadable.json" refs/heads/main
check "missing event payload is refused" 1 "unset or unreadable" push "$tmp/missing.json" refs/heads/main
check "unset event payload path is refused" 1 "unset or unreadable" push "" refs/heads/main
check "unset event name is refused" 1 "unset or unreadable" "" "$tmp/push-main.json" refs/heads/main
check "unset ref is refused" 1 "it is a push to ''" push "$tmp/push-main.json" ""
check "truncated JSON is refused" 1 "is not a JSON object" push "$tmp/truncated.json" refs/heads/main
check "JSON that is not an object is refused" 1 "is not a JSON object" merge_group "$tmp/array.json" "$queue_ref"
check "a job from another repository is refused" 1 "these runners serve only flarebox-dev/flarebox" push "$tmp/push-main.json" refs/heads/main someone/flarebox
check "a job with no repository is refused" 1 "these runners serve only flarebox-dev/flarebox" push "$tmp/push-main.json" refs/heads/main ""

if [ "$failures" -ne 0 ]; then
  echo "$failures job-started.sh test(s) failed."
  exit 1
fi
echo "All job-started.sh tests passed."

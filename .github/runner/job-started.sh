#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Job-started hook for Koidrive's self-hosted Mac runners (ACTIONS_RUNNER_HOOK_JOB_STARTED).
# The runner runs it with `bash -e` before every job, and a nonzero exit fails the job before
# any step runs. It enforces what ci.yml routes to these runners: a push to refs/heads/main, a
# merge queue run for main (refs/heads/gh-readonly-queue/main/...), or a pull request whose head
# branch is in koidrive/koidrive. It refuses every other event, pull requests from forks
# included, whatever runs-on the workflow asked for. It fails closed: a job whose event it cannot
# read does not run. It uses only
# the macOS base system (/bin/bash and /usr/bin/jq). README.md in this directory says how to install it.
set -u

repository=koidrive/koidrive

refuse() {
  echo "::error::Self-hosted runner ${RUNNER_NAME:-unknown} refuses this job: $1"
  exit 1
}

if [ "${GITHUB_REPOSITORY:-}" != "$repository" ]; then
  refuse "it belongs to '${GITHUB_REPOSITORY:-}', and these runners serve only $repository."
fi

event_name="${GITHUB_EVENT_NAME:-}"
event_path="${GITHUB_EVENT_PATH:-}"
ref="${GITHUB_REF:-}"
if [ -z "$event_name" ] || [ -z "$event_path" ] || [ ! -r "$event_path" ]; then
  refuse "its event is unknown (GITHUB_EVENT_NAME '$event_name', GITHUB_EVENT_PATH '$event_path' unset or unreadable)."
fi
if ! /usr/bin/jq -e 'type == "object"' "$event_path" >/dev/null 2>&1; then
  refuse "its event payload $event_path is not a JSON object."
fi

case "$event_name" in
  push)
    if [ "$ref" != refs/heads/main ]; then
      refuse "it is a push to '$ref', and these runners take pushes only to refs/heads/main."
    fi
    if ! payload_ref="$(/usr/bin/jq -er '.ref | strings' "$event_path" 2>/dev/null)" || [ "$payload_ref" != "$ref" ]; then
      refuse "its push payload does not name $ref as its ref."
    fi
    ;;
  merge_group)
    case "$ref" in
      refs/heads/gh-readonly-queue/main/pr-?*) ;;
      *) refuse "it is a merge queue run for '$ref', and these runners take only queue runs for main (refs/heads/gh-readonly-queue/main/...)." ;;
    esac
    if ! base_ref="$(/usr/bin/jq -er '.merge_group.base_ref | strings' "$event_path" 2>/dev/null)" ||
      ! head_ref="$(/usr/bin/jq -er '.merge_group.head_ref | strings' "$event_path" 2>/dev/null)" ||
      [ "$base_ref" != refs/heads/main ] || [ "$head_ref" != "$ref" ]; then
      refuse "its merge_group payload is not a queue run of $ref into refs/heads/main."
    fi
    ;;
  pull_request)
    if ! head_repo="$(/usr/bin/jq -er '.pull_request.head.repo.full_name | strings' "$event_path" 2>/dev/null)" ||
      [ "$head_repo" != "$repository" ]; then
      refuse "it is a pull request whose head is not in $repository. Pull requests from forks run their macOS jobs on GitHub-hosted runners."
    fi
    if ! number="$(/usr/bin/jq -er '.pull_request.number | numbers' "$event_path" 2>/dev/null)" ||
      [ "$ref" != "refs/pull/$number/merge" ]; then
      refuse "its pull_request payload does not name '$ref' as the merge ref of its pull request."
    fi
    ;;
  *)
    refuse "its event is $event_name. These runners take only pushes to main, merge queue runs for main and pull requests from branches of $repository."
    ;;
esac

echo "Self-hosted runner guard: $event_name on $ref in $repository may run here."

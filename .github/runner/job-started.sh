#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Job-started hook for Flarebox's self-hosted Mac runners (ACTIONS_RUNNER_HOOK_JOB_STARTED).
# The runner runs it with `bash -e` before every job, and a nonzero exit fails the job before
# any step runs. It refuses a job from any repository other than flarebox-dev/flarebox, and a
# pull request event whose head repository is another repository, such as a fork, whatever
# runs-on the workflow asked for. It fails closed: a job whose event it cannot read does not run.
# It uses only the macOS base system (/bin/bash and /usr/bin/jq). CONTRIBUTING.md says how to
# install it.
set -u

repository=flarebox-dev/flarebox

refuse() {
  echo "::error::Self-hosted runner ${RUNNER_NAME:-unknown} refuses this job: $1"
  exit 1
}

if [ "${GITHUB_REPOSITORY:-}" != "$repository" ]; then
  refuse "it belongs to '${GITHUB_REPOSITORY:-}', and these runners serve only $repository."
fi

event_name="${GITHUB_EVENT_NAME:-}"
event_path="${GITHUB_EVENT_PATH:-}"
if [ -z "$event_name" ] || [ -z "$event_path" ] || [ ! -r "$event_path" ]; then
  refuse "its event is unknown (GITHUB_EVENT_NAME '$event_name', GITHUB_EVENT_PATH '$event_path' unset or unreadable)."
fi
if ! /usr/bin/jq -e 'type == "object"' "$event_path" >/dev/null 2>&1; then
  refuse "its event payload $event_path is not a JSON object."
fi

case "$event_name" in
  pull_request*)
    if ! head_repository="$(/usr/bin/jq -er '.pull_request.head.repo.full_name | strings' "$event_path" 2>/dev/null)"; then
      refuse "its $event_name payload names no head repository, so it may come from a deleted fork."
    fi
    if [ "$head_repository" != "$repository" ]; then
      refuse "its $event_name event comes from $head_repository. Pull requests from forks run their macOS jobs on GitHub-hosted runners."
    fi
    ;;
esac

echo "Self-hosted runner guard: $event_name in $repository may run here."

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Starts the property-test battery (.github/workflows/property-battery.yml) on
# GitHub Actions and prints the run URL. See docs/testing.md.
#
# Usage: scripts/battery.sh [--cases N] [--shards N] [--filter TEXT] [--mutations] [--wait] [REF]
#   REF           branch, tag or commit to test; defaults to the current branch,
#                 which must be pushed (the battery tests what is on GitHub)
#   --cases N     PROPTEST_CASES on each shard (the workflow's default is 8192)
#   --shards N    number of shards, each with its own seed (default 24, at most 256)
#   --filter TEXT test name filter for the property test binaries
#   --mutations   also run the mutation set (scripts/mutations.sh)
#   --wait        wait for the run and exit with its result
#
# BATTERY_WORKFLOW_REF names the branch the workflow file is read from
# (default main).
set -euo pipefail

usage() { sed -n 's/^# \{0,1\}//; 7,15p' "$0"; }

fields=()
ref=""
wait=false
while [ $# -gt 0 ]; do
  case "$1" in
    --cases | --shards | --filter)
      if [ $# -lt 2 ]; then
        echo "$1 needs a value" >&2
        exit 2
      fi
      fields+=(-f "${1#--}=$2")
      shift
      ;;
    --mutations) fields+=(-f mutations=true) ;;
    --wait) wait=true ;;
    -h | --help) usage; exit 0 ;;
    -*) usage >&2; exit 2 ;;
    *)
      if [ -n "$ref" ]; then
        usage >&2
        exit 2
      fi
      ref=$1
      ;;
  esac
  shift
done

if [ -z "$ref" ]; then
  if ! ref=$(git symbolic-ref --quiet --short HEAD); then
    echo "HEAD is detached: name the branch, tag or commit to test." >&2
    exit 2
  fi
  remote=$(git ls-remote origin "refs/heads/$ref" | cut -f1)
  if [ -z "$remote" ]; then
    echo "Branch $ref is not on origin. Push it first: the battery tests what is on GitHub." >&2
    exit 1
  fi
  if [ "$remote" != "$(git rev-parse HEAD)" ]; then
    echo "Note: origin/$ref is at ${remote:0:12}, not at your HEAD. The battery tests ${remote:0:12}." >&2
  fi
fi

output=$(gh workflow run property-battery.yml --ref "${BATTERY_WORKFLOW_REF:-main}" -f ref="$ref" ${fields[@]+"${fields[@]}"} 2>&1) || {
  echo "$output" >&2
  exit 1
}
url=$(grep -oE 'https://[^ ]+/actions/runs/[0-9]+' <<< "$output" | head -n 1 || true)
if [ -z "$url" ]; then
  echo "$output"
  echo "The battery started, but gh did not print the run URL. Find the run with: gh run list --workflow property-battery.yml" >&2
  if $wait; then
    echo "Cannot wait without the run URL; follow it with gh run watch <run-id> --exit-status." >&2
    exit 1
  fi
  exit 0
fi
echo "$url"
if $wait; then
  gh run watch "${url##*/}" --compact --exit-status --interval 60
fi

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

usage="usage: scripts/mutations.sh [--count] [--shard K/N]"
shard=0
shards=1
count_only=false
while [ $# -gt 0 ]; do
  case "$1" in
    --count) count_only=true ;;
    --shard)
      if ! [[ "${2:-}" =~ ^([0-9]+)/([1-9][0-9]*)$ ]] || [ "${BASH_REMATCH[1]}" -ge "${BASH_REMATCH[2]}" ]; then
        echo "--shard takes K/N with K < N, for example 0/4" >&2
        exit 2
      fi
      shard=${BASH_REMATCH[1]}
      shards=${BASH_REMATCH[2]}
      shift
      ;;
    *) echo "$usage" >&2; exit 2 ;;
  esac
  shift
done

names=()
files=()
exprs=()
kinds=()
mutation() {
  names+=("$1")
  files+=("$2")
  exprs+=("$3")
  kinds+=("$4")
}
source scripts/engine-mutations.sh

if $count_only; then
  echo "${#names[@]}"
  exit 0
fi

if [ -n "$(git status --porcelain --untracked-files=no)" ] || [ -n "$(git ls-files --others --exclude-standard -- '*.proptest-regressions')" ]; then
  echo "The checkout has uncommitted changes or untracked .proptest-regressions files. Commit or move them first: this script restores files with git." >&2
  exit 2
fi

bash scripts/test-engine-mutations.sh

restore() {
  git checkout -q -- .
  git ls-files -z --others --exclude-standard -- '*.proptest-regressions' | xargs -0 rm -f --
}
trap restore EXIT

cases=${MUTATION_CASES:-4096}
log=$(mktemp)
first_failure() { grep -m1 -oE 'Test failed: .{0,90}|panicked at [^ ]+' "$log" || echo "no panic message"; }

property_runs() {
  local i=$1 attempt
  shift
  for attempt in 0 1 2; do
    if [ -n "${MUTATION_SEED:-}" ]; then
      export PROPTEST_RNG_SEED=$((MUTATION_SEED + 3 * i + attempt))
    fi
    if ! PROPTEST_CASES=$cases "$@" > "$log" 2>&1; then
      caught="property tests (run $((attempt + 1))${PROPTEST_RNG_SEED:+, seed $PROPTEST_RNG_SEED})"
      return
    fi
    git ls-files -z --others --exclude-standard -- '*.proptest-regressions' | xargs -0 rm -f --
  done
}

echo "Baseline: building and running the unit, property and device tests without mutations"
caught=""
if ! "${unit[@]}" > "$log" 2>&1; then caught="unit tests"; fi
if [ -n "${MUTATION_SEED:-}" ]; then export PROPTEST_RNG_SEED=$MUTATION_SEED; fi
if [ -z "$caught" ] && ! PROPTEST_CASES=$cases "${property[@]}" > "$log" 2>&1; then caught="property tests"; fi
if [ -z "$caught" ] && ! PROPTEST_CASES=$cases "${devices[@]}" > "$log" 2>&1; then caught="device tests"; fi
if [ -n "$caught" ]; then
  tail -n 40 "$log"
  echo "Baseline: the $caught fail without any mutation ($(first_failure)). Fix that before running mutations."
  exit 1
fi
echo "Baseline: unit, property and device tests pass"

status=0
for i in "${!names[@]}"; do
  [ $((i % shards)) -eq "$shard" ] || continue
  name=${names[$i]} file=${files[$i]} kind=${kinds[$i]}
  if [ ! -f "$file" ]; then
    echo "$name: STALE ($file does not exist)"
    status=1
    continue
  fi
  perl -0pi -e "${exprs[$i]}" "$file"
  if git diff --quiet -- "$file"; then
    echo "$name: STALE (the pattern no longer matches $file)"
    status=1
    continue
  fi
  if ! { "${unit[@]}" --no-run && "${property[@]}" --no-run && "${devices[@]}" --no-run; } > "$log" 2>&1; then
    echo "$name: BROKEN (the mutated $file does not compile: $(grep -m1 -E '^error' "$log" || echo "see the cargo output"))"
    restore
    status=1
    continue
  fi
  caught=""
  if [ "$kind" = unit ] || [ "$kind" = both ]; then
    if ! "${unit[@]}" > "$log" 2>&1; then caught="unit tests"; fi
  fi
  if [ -z "$caught" ] && { [ "$kind" = prop ] || [ "$kind" = both ]; }; then
    property_runs "$i" "${property[@]}"
  fi
  if [ "$kind" = devices ]; then
    property_runs "$i" "${devices[@]}"
  fi
  restore
  if [ -n "$caught" ]; then
    echo "$name: caught by $caught: $(first_failure)"
  else
    echo "$name: SURVIVED (no $kind test failed with $file mutated)"
    status=1
  fi
done
exit "$status"

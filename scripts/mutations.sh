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

if [ -n "$(git status --porcelain --untracked-files=no)" ] || [ -n "$(git ls-files --others --exclude-standard -- '*.proptest-regressions' '*.cases/*.txt')" ]; then
  echo "The checkout has uncommitted changes or untracked regression case files. Commit or move them first: this script restores files with git." >&2
  exit 2
fi

export PROPTEST_CASES=${MUTATION_CASES:-4096}
export PROPTEST_RNG_SEED=${MUTATION_SEED:-0}
if ! python3 -c 'import sys; s = sys.argv[1]; sys.exit(not (s.isascii() and s.isdigit() and int(s) < 2**64))' "$PROPTEST_RNG_SEED"; then
  echo "MUTATION_SEED must be an unsigned 64-bit integer" >&2
  exit 2
fi

bash scripts/test-engine-mutations.sh

log=$(mktemp)
restore() {
  git checkout -q -- .
  git ls-files -z --others --exclude-standard -- '*.proptest-regressions' '*.cases/*.txt' | xargs -0 rm -f --
  if [ -f "${file:-}" ]; then
    touch "$file"
  fi
}
trap 'restore; rm -f "$log"' EXIT

failure_output() {
  local path
  cat "$log"
  while IFS= read -r -d '' path; do
    printf '\nRegression case: %s\n' "$path"
    cat "$path"
  done < <(git ls-files -z --modified --others --exclude-standard -- '*.proptest-regressions' '*.cases/*.txt')
}

echo "Baseline: building and running the unit, property and device tests without mutations (seed $PROPTEST_RNG_SEED)"
caught=""
if ! "${unit[@]}" > "$log" 2>&1; then caught="unit tests"; fi
if [ -z "$caught" ] && ! "${property[@]}" > "$log" 2>&1; then caught="property tests"; fi
if [ -z "$caught" ] && ! "${devices[@]}" > "$log" 2>&1; then caught="device tests"; fi
if [ -n "$caught" ]; then
  echo "Baseline: the $caught fail without any mutation. Fix that before running mutations."
  failure_output
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
    failure_output
    restore
    status=1
    continue
  fi
  caught=""
  if [ "$kind" = unit ] || [ "$kind" = both ]; then
    if ! "${unit[@]}" > "$log" 2>&1; then caught="unit tests"; fi
  fi
  if [ -z "$caught" ] && { [ "$kind" = prop ] || [ "$kind" = both ]; }; then
    if ! "${property[@]}" > "$log" 2>&1; then caught="property tests"; fi
  fi
  if [ "$kind" = devices ]; then
    if ! "${devices[@]}" > "$log" 2>&1; then caught="device tests"; fi
  fi
  if [ -n "$caught" ]; then
    echo "$name: failure output from $caught (seed $PROPTEST_RNG_SEED)"
    failure_output
  fi
  restore
  if [ -n "$caught" ]; then
    echo "$name: caught by $caught (seed $PROPTEST_RNG_SEED)"
  else
    echo "$name: SURVIVED (no $kind test failed with $file mutated)"
    status=1
  fi
done
exit "$status"

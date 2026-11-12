#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Lists the paths whose changes make the merge queue run the property battery:
# the workspace crates the battery's test binaries build from (every package
# that depends on proptest and its path dependencies), and the files that set
# how they build. With --check, fails unless .github/property-battery-paths
# holds exactly that list.
#
# Usage: scripts/battery-paths.sh [--check]
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
root=$PWD

crates=$(
  cargo metadata --format-version 1 --locked --no-deps | jq -r --arg root "$root/" '
    ([.packages[] | {key: .name, value: .}] | from_entries) as $packages
    | def deps($kinds): [.dependencies[] | select(.path != null and ((.kind // "normal") as $k | $kinds | index($k))) | .name];
    def closure: [recurse($packages[deps(["normal", "build"])[]])];
    [.packages[] | select(any(.dependencies[]; .name == "proptest"))
      | ., ($packages[deps(["normal", "build", "dev"])[]] | closure[])]
    | map(.manifest_path | ltrimstr($root) | rtrimstr("Cargo.toml"))
    | unique[]'
)
expected=$(
  echo "$crates"
  printf '%s\n' .cargo/ .github/property-battery-paths .github/workflows/property-battery.yml Cargo.lock Cargo.toml rust-toolchain.toml
)
expected=$(LC_ALL=C sort -u <<< "$expected")

if [ "${1:-}" != --check ]; then
  echo "$expected"
  exit 0
fi

committed=$(LC_ALL=C sort -u .github/property-battery-paths)
if [ "$committed" != "$expected" ]; then
  echo "::error::.github/property-battery-paths does not match the crates the property battery builds. Replace it with the output of scripts/battery-paths.sh. Difference (< committed, > expected):"
  diff <(echo "$committed") <(echo "$expected") || true
  exit 1
fi
echo ".github/property-battery-paths matches the $(wc -l <<< "$expected" | tr -d ' ') paths the property battery depends on."

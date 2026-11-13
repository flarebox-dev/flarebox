#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

tag=${1:?usage: homebrew-track.sh <tag> <formula>}
formula=${2:?usage: homebrew-track.sh <tag> <formula>}
: "${GH_REPO:?is not set}"

version_key() {
    [[ $1 =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)(-(alpha|rc)\.([0-9]+))?$ ]] || return 1
    local stage=2
    case ${BASH_REMATCH[5]} in alpha) stage=0 ;; rc) stage=1 ;; esac
    printf '%010d.%010d.%010d.%d.%010d\n' "$((10#${BASH_REMATCH[1]}))" "$((10#${BASH_REMATCH[2]}))" \
        "$((10#${BASH_REMATCH[3]}))" "$stage" "$((10#${BASH_REMATCH[6]:-0}))"
}

own=$(version_key "$tag") || { echo "::error::$tag is not a vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N tag." >&2; exit 1; }

if [[ $tag == *-* ]]; then
    releases=$(gh api --paginate "repos/$GH_REPO/releases" --jq '.[] | select(.draft == false) | .tag_name')
    stable=$(grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' <<< "$releases" | head -n 1 || true)
    if [ -n "$stable" ]; then
        echo "$tag is a prerelease and $stable is a stable release, so the formula stays on stable releases." >&2
        echo skip
        exit 0
    fi
fi

if [ ! -e "$formula" ]; then
    echo "The tap has no $formula yet, so $tag becomes the formula." >&2
    echo bump
    exit 0
fi
current=$(sed -nE 's#^ *url ".*/download/v([^/]+)/koidrive-[^/]+\.tar\.gz"$#\1#p' "$formula" | sort -u)
key=$(version_key "v$current") || { echo "::error::$formula names the release '$current', which is not one X.Y.Z, X.Y.Z-alpha.N or X.Y.Z-rc.N." >&2; exit 1; }
if [[ $own > $key ]]; then
    echo "The formula has $current and $tag is higher, so the formula moves to $tag." >&2
    echo bump
else
    echo "The formula has $current and $tag is not higher, so the formula stays." >&2
    echo skip
fi

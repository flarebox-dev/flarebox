#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

tag=${1:?usage: publish-release.sh <tag> <directory>}
files=${2:?usage: publish-release.sh <tag> <directory>}
: "${GH_REPO:?is not set}" "${PRERELEASE:?is not set}" "${PRERELEASE_TAGS_PUBLISH_AS:?is not set}" "${RELEASE_ALLOWED_SIGNERS:?is not set}"

version_key() {
    [[ $1 =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|rc)\.(0|[1-9][0-9]*))?$ ]] || return 1
    local stage=2
    case ${BASH_REMATCH[5]} in alpha) stage=0 ;; rc) stage=1 ;; esac
    printf '%010d.%010d.%010d.%d.%010d\n' "$((10#${BASH_REMATCH[1]}))" "$((10#${BASH_REMATCH[2]}))" \
        "$((10#${BASH_REMATCH[3]}))" "$stage" "$((10#${BASH_REMATCH[6]:-0}))"
}

own=$(version_key "$tag") || { echo "::error::$tag is not a vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N tag."; exit 1; }
case $PRERELEASE_TAGS_PUBLISH_AS in
    normal-release | prerelease) ;;
    *) echo "::error::PRERELEASE_TAGS_PUBLISH_AS is $PRERELEASE_TAGS_PUBLISH_AS, expected normal-release or prerelease."; exit 1 ;;
esac
if [[ $tag == *-* ]]; then tagged_prerelease=true; else tagged_prerelease=false; fi
[ "$PRERELEASE" = "$tagged_prerelease" ] || { echo "::error::PRERELEASE is $PRERELEASE, but $tag means $tagged_prerelease."; exit 1; }

dmg="Koidrive-${tag#v}-arm64.dmg"
sums="$files/SHA256SUMS"
[ -f "$files/$dmg" ] || { echo "::error::$files has no $dmg, and a release needs the signed app."; exit 1; }
[ -f "$sums" ] || { echo "::error::$files has no SHA256SUMS."; exit 1; }
[ -f "$sums.sig" ] || { echo "::error::$files has no SHA256SUMS.sig."; exit 1; }
assets=0
for path in "$files"/*; do
    name=${path##*/}
    case $name in SHA256SUMS | SHA256SUMS.sig) continue ;; esac
    case $name in
        koidrive-*.tar.gz | "$dmg") ;;
        *) echo "::error::$files has $name, which is neither a CLI tarball nor $dmg."; exit 1 ;;
    esac
    listed=$(awk -v name="$name" '$2 == name { count++ } END { print count + 0 }' "$sums")
    [ "$listed" = 1 ] || { echo "::error::SHA256SUMS lists $name $listed times, expected once."; exit 1; }
    assets=$((assets + 1))
done
entries=$(awk 'NF { count++ } END { print count + 0 }' "$sums")
[ "$entries" = "$assets" ] || { echo "::error::SHA256SUMS has $entries entries, but $files has $assets files to publish."; exit 1; }
(cd "$files" && sha256sum --strict -c SHA256SUMS) || { echo "::error::A file in $files does not match its SHA256SUMS entry."; exit 1; }
ssh-keygen -q -Y verify -f "$RELEASE_ALLOWED_SIGNERS" -I koidrive -n koidrive-release -s "$sums.sig" < "$sums" > /dev/null \
    || { echo "::error::SHA256SUMS.sig is not a signature of SHA256SUMS by the release key pinned in $RELEASE_ALLOWED_SIGNERS."; exit 1; }

before=$(gh api "repos/$GH_REPO/releases/latest" --jq .tag_name 2>/dev/null) || before=none

if [ "$PRERELEASE" = true ] && [ "$PRERELEASE_TAGS_PUBLISH_AS" = prerelease ]; then
    prerelease=true
    latest=false
else
    prerelease=false
    latest=true
    releases=$(gh api --paginate "repos/$GH_REPO/releases" --jq '.[] | select(.draft == false and .prerelease == false) | .tag_name')
    while IFS= read -r other; do
        key=$(version_key "$other") || continue
        if [[ $key > $own ]]; then
            echo "$other is a published release with a higher version, so $tag does not become Latest."
            latest=false
        fi
    done <<< "$releases"
fi

draft=$(gh release view "$tag" --json isDraft --jq .isDraft 2>/dev/null) || draft=missing
if [ "$draft" = missing ]; then
    gh release create "$tag" --draft --verify-tag --title "$tag" --notes 'Koidrive CLI for Linux and macOS, and the Koidrive app for macOS. See docs/guide/install.md for installation.'
    draft=true
fi
case "$draft" in
    true) gh release upload "$tag" "$files"/* --clobber ;;
    false)
        published=$(mktemp -d)
        trap 'rm -rf "$published"' EXIT
        gh release download "$tag" --dir "$published"
        for path in "$files"/*; do
            name=${path##*/}
            if ! cmp -s "$path" "$published/$name"; then
                echo "::error::$tag is already published with different bytes for $name. Published assets cannot be replaced; create a new version tag."
                exit 1
            fi
        done
        for path in "$published"/*; do
            name=${path##*/}
            [ -f "$files/$name" ] || { echo "::error::$tag has a published asset $name that is absent from this build. Published assets cannot be replaced; create a new version tag."; exit 1; }
        done
        echo "$tag already has these assets; nothing was uploaded."
        ;;
    *) echo "::error::GitHub returned an invalid draft state for $tag: $draft"; exit 1 ;;
esac
gh release edit "$tag" --draft=false --prerelease="$prerelease" --latest="$latest"

state=$(gh release view "$tag" --json isDraft,isPrerelease --jq '"draft=\(.isDraft) prerelease=\(.isPrerelease)"')
if [ "$state" != "draft=false prerelease=$prerelease" ]; then
    echo "::error::$tag is $state, expected draft=false prerelease=$prerelease."
    exit 1
fi
after=$(gh api "repos/$GH_REPO/releases/latest" --jq .tag_name 2>/dev/null) || after=none
if [ "$latest" = true ]; then expected=$tag; else expected=$before; fi
if [ "$after" != "$expected" ]; then
    echo "::error::The Latest release is $after after publishing $tag, expected $expected."
    exit 1
fi
echo "Published $tag ($state). The Latest release, which install.sh downloads by default, is $after."

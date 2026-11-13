#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Publishes the GitHub release for a version tag with the files in a directory, then checks the result.
# The directory holds the CLI tarballs, the app's disk image Koidrive-<version>-arm64.dmg and SHA256SUMS,
# which lists each of them; the script refuses to publish anything else.
# The release becomes Latest only when no published release that can be Latest (not a draft and not a
# prerelease) has a higher version, so a rerun or an older tag leaves Latest where it is.
# Usage: publish-release.sh <tag> <directory>
# Environment:
#   GH_REPO                     owner/name of the repository
#   PRERELEASE                  true for an -alpha.N or -rc.N tag, false for vX.Y.Z
#   PRERELEASE_TAGS_PUBLISH_AS  latest: an -alpha.N or -rc.N tag is published as a normal release;
#                               prerelease: it is published as a GitHub prerelease
set -euo pipefail

tag=${1:?usage: publish-release.sh <tag> <directory>}
files=${2:?usage: publish-release.sh <tag> <directory>}
: "${GH_REPO:?is not set}" "${PRERELEASE:?is not set}" "${PRERELEASE_TAGS_PUBLISH_AS:?is not set}"

# version_key prints a key for vX.Y.Z[-alpha.N|-rc.N] that sorts as text in version order, with
# alpha before rc before the release itself. It fails for any other tag.
version_key() {
    [[ $1 =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)(-(alpha|rc)\.([0-9]+))?$ ]] || return 1
    local stage=2
    case ${BASH_REMATCH[5]} in alpha) stage=0 ;; rc) stage=1 ;; esac
    printf '%010d.%010d.%010d.%d.%010d\n' "$((10#${BASH_REMATCH[1]}))" "$((10#${BASH_REMATCH[2]}))" \
        "$((10#${BASH_REMATCH[3]}))" "$stage" "$((10#${BASH_REMATCH[6]:-0}))"
}

own=$(version_key "$tag") || { echo "::error::$tag is not a vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N tag."; exit 1; }

dmg="Koidrive-${tag#v}-arm64.dmg"
sums="$files/SHA256SUMS"
[ -f "$files/$dmg" ] || { echo "::error::$files has no $dmg, and a release needs the signed app."; exit 1; }
[ -f "$sums" ] || { echo "::error::$files has no SHA256SUMS."; exit 1; }
assets=0
for path in "$files"/*; do
    name=${path##*/}
    [ "$name" != SHA256SUMS ] || continue
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

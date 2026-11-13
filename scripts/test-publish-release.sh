#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests scripts/publish-release.sh against a stub gh that keeps releases and their uploaded file names
# in a directory and follows GitHub's rules for Latest: --latest=true moves it, --latest=false leaves it,
# and a draft or a prerelease is never Latest. The release list goes through the script's own jq filter.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
db=$GH_STUB_DIR
case "$1 $2" in
    "api --paginate")
        [ -z "${GH_STUB_FAIL_LIST:-}" ] || { echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1; }
        [ "$3 $4" = "repos/$GH_REPO/releases --jq" ] || { echo "stub gh: unexpected $*" >&2; exit 2; }
        for f in "$db"/release-*; do
            [ -e "$f" ] || continue
            read -r draft prerelease < "$f"
            jq -n --arg tag "${f#"$db"/release-}" --argjson draft "$draft" --argjson prerelease "$prerelease" \
                '{tag_name: $tag, draft: $draft, prerelease: $prerelease}'
        done | jq -s . | jq -r "$5"
        ;;
    "api repos/$GH_REPO/releases/latest")
        [ -s "$db/latest" ] || { echo 'gh: Not Found (HTTP 404)' >&2; exit 1; }
        cat "$db/latest"
        ;;
    "release view")
        [ -e "$db/release-$3" ] || exit 1
        if [ "$#" -gt 3 ]; then
            read -r draft prerelease < "$db/release-$3"
            echo "draft=$draft prerelease=$prerelease"
        fi
        ;;
    "release create") echo "true false" > "$db/release-$3" ;;
    "release upload")
        for arg in "${@:4}"; do
            [ "$arg" = --clobber ] || basename "$arg"
        done >> "$db/assets-$3"
        ;;
    "release edit")
        tag=$3
        read -r draft prerelease < "$db/release-$tag"
        latest=
        for arg in "${@:4}"; do
            case $arg in
                --draft=*) draft=${arg#--draft=} ;;
                --prerelease=*) prerelease=${arg#--prerelease=} ;;
                --latest=*) latest=${arg#--latest=} ;;
                *) echo "stub gh: unexpected $arg" >&2; exit 2 ;;
            esac
        done
        [ -n "$latest" ] || { echo 'stub gh: publish without --latest=true|false' >&2; exit 2; }
        echo "$draft $prerelease" > "$db/release-$tag"
        if [ "$latest" = true ] && [ "$draft $prerelease" = "false false" ]; then echo "$tag" > "$db/latest"; fi
        ;;
    *) echo "stub gh: unexpected $*" >&2; exit 2 ;;
esac
EOF
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH" GH_REPO=koidrive/koidrive GH_STUB_DIR="$tmp/db" PRERELEASE_TAGS_PUBLISH_AS=latest
passed=0

stage() {
    rm -rf "$tmp/dist"
    mkdir "$tmp/dist"
    echo cli > "$tmp/dist/koidrive-x86_64-unknown-linux-musl.tar.gz"
    echo cli > "$tmp/dist/koidrive-aarch64-apple-darwin.tar.gz"
    echo app > "$tmp/dist/Koidrive-${1#v}-arm64.dmg"
    (cd "$tmp/dist" && sha256sum koidrive-*.tar.gz Koidrive-*.dmg > SHA256SUMS)
}
publish_staged() {
    local prerelease=false
    if [[ $1 == *-* ]]; then prerelease=true; fi
    PRERELEASE=$prerelease bash "$root/scripts/publish-release.sh" "$1" "$tmp/dist"
}
publish() {
    stage "$1"
    publish_staged "$1"
}
pass() {
    echo "PASS $1"
    passed=$((passed + 1))
}
expect_latest() {
    local actual
    actual=$(cat "$tmp/db/latest" 2>/dev/null || echo none)
    [ "$actual" = "$1" ] || { echo "FAIL $2: Latest is $actual, expected $1" >&2; exit 1; }
    pass "$2"
}
fresh() {
    rm -rf "$tmp/db"
    mkdir -p "$tmp/db"
}
expect_refused() {
    local tag=$1 message=$2 description=$3 output
    if output=$(publish_staged "$tag" 2>&1); then
        echo "FAIL $description: the publish succeeded" >&2
        exit 1
    fi
    grep '^::error::' <<< "$output" | grep -qF "$message" || { echo "FAIL $description: expected an error with '$message' in:" >&2; echo "$output" >&2; exit 1; }
    [ ! -e "$tmp/db/release-$tag" ] || { echo "FAIL $description: the release $tag was created" >&2; exit 1; }
    pass "$description"
}

fresh
publish v0.1.0-alpha.2
[ "$(sort "$tmp/db/assets-v0.1.0-alpha.2")" = "$(printf '%s\n' Koidrive-0.1.0-alpha.2-arm64.dmg SHA256SUMS koidrive-aarch64-apple-darwin.tar.gz koidrive-x86_64-unknown-linux-musl.tar.gz | sort)" ] \
    || { echo 'FAIL the release does not have the tarballs, the dmg and SHA256SUMS' >&2; cat "$tmp/db/assets-v0.1.0-alpha.2" >&2; exit 1; }
pass 'a prerelease tag attaches the tarballs, its dmg and SHA256SUMS'

fresh
stage v0.2.0
rm "$tmp/dist/Koidrive-0.2.0-arm64.dmg"
(cd "$tmp/dist" && sha256sum koidrive-*.tar.gz > SHA256SUMS)
expect_refused v0.2.0 'has no Koidrive-0.2.0-arm64.dmg' 'a release without the dmg is refused'
stage v0.2.0
mv "$tmp/dist/Koidrive-0.2.0-arm64.dmg" "$tmp/dist/Koidrive-0.2.0-arm64-unsigned.dmg"
(cd "$tmp/dist" && sha256sum koidrive-*.tar.gz Koidrive-*.dmg > SHA256SUMS)
expect_refused v0.2.0 'has no Koidrive-0.2.0-arm64.dmg' 'an unsigned dmg alone is refused'
stage v0.2.0
echo app > "$tmp/dist/Koidrive-0.2.0-arm64-unsigned.dmg"
(cd "$tmp/dist" && sha256sum koidrive-*.tar.gz Koidrive-*.dmg > SHA256SUMS)
expect_refused v0.2.0 'has Koidrive-0.2.0-arm64-unsigned.dmg' 'an unsigned dmg next to the signed one is refused'
stage v0.2.0-alpha.1
expect_refused v0.2.0 'has no Koidrive-0.2.0-arm64.dmg' 'a dmg of another version is refused'
stage v0.2.0
grep -v dmg "$tmp/dist/SHA256SUMS" > "$tmp/sums" && mv "$tmp/sums" "$tmp/dist/SHA256SUMS"
expect_refused v0.2.0 'SHA256SUMS lists Koidrive-0.2.0-arm64.dmg 0 times' 'a dmg missing from SHA256SUMS is refused'
stage v0.2.0
(cd "$tmp/dist" && sha256sum koidrive-x86_64-unknown-linux-musl.tar.gz | sed 's/musl/gnu/') >> "$tmp/dist/SHA256SUMS"
expect_refused v0.2.0 'SHA256SUMS has 4 entries' 'SHA256SUMS entries for files that are not published are refused'
stage v0.2.0
echo changed > "$tmp/dist/Koidrive-0.2.0-arm64.dmg"
expect_refused v0.2.0 'does not match its SHA256SUMS entry' 'a dmg that does not match SHA256SUMS is refused'
stage v0.2.0
rm "$tmp/dist/SHA256SUMS"
expect_refused v0.2.0 'has no SHA256SUMS' 'a release without SHA256SUMS is refused'

fresh
publish v0.1.0-alpha.1
expect_latest v0.1.0-alpha.1 'the first alpha becomes Latest'
publish v0.1.0-alpha.1
expect_latest v0.1.0-alpha.1 'a rerun of the highest tag keeps Latest'
publish v0.1.0-alpha.2
expect_latest v0.1.0-alpha.2 'a higher alpha moves Latest'
publish v0.1.0-rc.1
expect_latest v0.1.0-rc.1 'an rc moves Latest past the alphas'
publish v0.1.0
expect_latest v0.1.0 'the release moves Latest past its rc'
publish v0.1.0-alpha.2
expect_latest v0.1.0 'an alpha rerun after the release leaves Latest'
publish v0.1.0-rc.1
expect_latest v0.1.0 'a rerun of an older tag leaves Latest'
if GH_STUB_FAIL_LIST=1 publish v0.1.0-rc.1; then
    echo 'FAIL the publish succeeded although the release list failed' >&2
    exit 1
fi
expect_latest v0.1.0 'a failed release list stops the publish and leaves Latest'
publish v0.1.0-alpha.3
expect_latest v0.1.0 'an older tag pushed later leaves Latest'
[ "$(cat "$tmp/db/release-v0.1.0-alpha.3")" = "false false" ] || { echo 'FAIL the older tag is not published' >&2; exit 1; }
pass 'the older tag is still published'

fresh
publish v0.10.0
publish v0.9.0
expect_latest v0.10.0 'versions compare as numbers'

fresh
publish v0.1.0
PRERELEASE_TAGS_PUBLISH_AS=prerelease publish v0.2.0-alpha.1
expect_latest v0.1.0 'a prerelease-mode alpha leaves Latest'
[ "$(cat "$tmp/db/release-v0.2.0-alpha.1")" = "false true" ] || { echo 'FAIL the alpha is not a prerelease' >&2; exit 1; }
pass 'a prerelease-mode alpha is a GitHub prerelease'
publish v0.1.1
expect_latest v0.1.1 'a GitHub prerelease does not hold back Latest'

echo "Passed $passed release publishing tests"

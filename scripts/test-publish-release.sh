#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests scripts/publish-release.sh against a stub gh that keeps releases in a directory and
# follows GitHub's rules for Latest: --latest=true moves it, --latest=false leaves it, and a draft
# or a prerelease is never Latest.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/dist"
touch "$tmp/dist/SHA256SUMS"
cat > "$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
db=$GH_STUB_DIR
case "$1 $2" in
    "api --paginate")
        for f in "$db"/release-*; do
            [ -e "$f" ] || continue
            read -r draft prerelease < "$f"
            if [ "$draft $prerelease" = "false false" ]; then printf '%s\n' "${f#"$db"/release-}"; fi
        done
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
    "release upload") ;;
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

publish() {
    local prerelease=false
    if [[ $1 == *-* ]]; then prerelease=true; fi
    PRERELEASE=$prerelease bash "$root/scripts/publish-release.sh" "$1" "$tmp/dist"
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

fresh
publish v0.1.0-alpha.1
expect_latest v0.1.0-alpha.1 'the first alpha becomes Latest'
publish v0.1.0-alpha.1
expect_latest v0.1.0-alpha.1 'a rerun of the highest tag keeps Latest'
publish v0.1.0-alpha.2
publish v0.1.0-rc.1
publish v0.1.0
expect_latest v0.1.0 'alpha, rc and release move Latest in version order'
publish v0.1.0-rc.1
expect_latest v0.1.0 'a rerun of an older tag leaves Latest'
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

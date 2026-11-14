#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
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
            case "$5" in
                isDraft) echo "$draft" ;;
                isDraft,isPrerelease) echo "draft=$draft prerelease=$prerelease" ;;
                *) echo "stub gh: unexpected $*" >&2; exit 2 ;;
            esac
        fi
        ;;
    "release create") echo "true false" > "$db/release-$3" ;;
    "release upload")
        mkdir -p "$db/files-$3"
        for arg in "${@:4}"; do
            if [ "$arg" != --clobber ]; then
                basename "$arg"
                cp "$arg" "$db/files-$3/"
            fi
        done >> "$db/assets-$3"
        ;;
    "release download")
        [ "$4" = --dir ] || { echo "stub gh: unexpected $*" >&2; exit 2; }
        cp "$db/files-$3/"* "$5/"
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
ssh-keygen -q -t ed25519 -N '' -C koidrive-release -f "$tmp/release-key"
ssh-keygen -q -t ed25519 -N '' -C other -f "$tmp/other-key"
printf 'koidrive namespaces="koidrive-release" %s\n' "$(cut -d' ' -f1,2 "$tmp/release-key.pub")" > "$tmp/allowed-signers"
RELEASE_SIGNING_KEY=$(cat "$tmp/release-key")
unset PRERELEASE PRERELEASE_TAGS_PUBLISH_AS prerelease mode
export PATH="$tmp/bin:$PATH" GH_REPO=koidrive/koidrive GH_STUB_DIR="$tmp/db"
export RELEASE_ALLOWED_SIGNERS="$tmp/allowed-signers" RELEASE_SIGNING_KEY
passed=0

stage() {
    rm -rf "$tmp/dist"
    mkdir "$tmp/dist"
    echo cli > "$tmp/dist/koidrive-x86_64-unknown-linux-musl.tar.gz"
    echo cli > "$tmp/dist/koidrive-aarch64-apple-darwin.tar.gz"
    echo app > "$tmp/dist/Koidrive-${1#v}-arm64.dmg"
    (cd "$tmp/dist" && sha256sum koidrive-*.tar.gz Koidrive-*.dmg > SHA256SUMS)
    sign
}
sign() {
    bash "$root/scripts/sign-checksums.sh" "$tmp/dist" > /dev/null
}
sign_with() {
    rm -f "$tmp/dist/SHA256SUMS.sig"
    ssh-keygen -q -Y sign -f "$1" -n "$2" "$tmp/dist/SHA256SUMS"
}
publish_staged() {
    local derived=false
    if [[ $1 == *-* ]]; then derived=true; fi
    PRERELEASE=${prerelease:-$derived} PRERELEASE_TAGS_PUBLISH_AS=${mode:-normal-release} bash "$root/scripts/publish-release.sh" "$1" "$tmp/dist"
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
[ "$(sort "$tmp/db/assets-v0.1.0-alpha.2")" = "$(printf '%s\n' Koidrive-0.1.0-alpha.2-arm64.dmg SHA256SUMS SHA256SUMS.sig koidrive-aarch64-apple-darwin.tar.gz koidrive-x86_64-unknown-linux-musl.tar.gz | sort)" ] \
    || { echo 'FAIL the release does not have the tarballs, the dmg, SHA256SUMS and its signature' >&2; cat "$tmp/db/assets-v0.1.0-alpha.2" >&2; exit 1; }
pass 'a prerelease tag attaches the tarballs, its dmg, SHA256SUMS and its signature'

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
sed -i.bak 's/^[0-9a-f]*  Koidrive-0.2.0-arm64.dmg$/invalid  Koidrive-0.2.0-arm64.dmg/' "$tmp/dist/SHA256SUMS" && rm "$tmp/dist/SHA256SUMS.bak"
expect_refused v0.2.0 'does not match its SHA256SUMS entry' 'a malformed SHA256SUMS entry is refused'
stage v0.2.0
rm "$tmp/dist/SHA256SUMS"
expect_refused v0.2.0 'has no SHA256SUMS' 'a release without SHA256SUMS is refused'
stage v0.2.0
rm "$tmp/dist/SHA256SUMS.sig"
expect_refused v0.2.0 'has no SHA256SUMS.sig' 'a release without SHA256SUMS.sig is refused'
stage v0.2.0
sign_with "$tmp/other-key" koidrive-release
expect_refused v0.2.0 'is not a signature of SHA256SUMS by the release key' 'SHA256SUMS signed by another key is refused'
stage v0.2.0
sign_with "$tmp/release-key" file
expect_refused v0.2.0 'is not a signature of SHA256SUMS by the release key' 'a release-key signature in another namespace is refused'
stage v0.2.0
printf 'other bytes\n' > "$tmp/other-sums"
ssh-keygen -q -Y sign -f "$tmp/release-key" -n koidrive-release "$tmp/other-sums"
cp "$tmp/other-sums.sig" "$tmp/dist/SHA256SUMS.sig"
expect_refused v0.2.0 'is not a signature of SHA256SUMS by the release key' 'a release-key signature of other bytes is refused'
if output=$(RELEASE_SIGNING_KEY='' bash "$root/scripts/sign-checksums.sh" "$tmp/dist" 2>&1); then
    echo 'FAIL signing succeeded without RELEASE_SIGNING_KEY' >&2
    exit 1
fi
grep -qF '::error::Signing SHA256SUMS needs the secret RELEASE_SIGNING_KEY in the release-signing environment.' <<< "$output"
pass 'signing without the secret names the secret and its environment'
if output=$(RELEASE_SIGNING_KEY=$(cat "$tmp/other-key") bash "$root/scripts/sign-checksums.sh" "$tmp/dist" 2>&1); then
    echo 'FAIL signing succeeded with a key other than the pinned one' >&2
    exit 1
fi
grep -qF 'RELEASE_SIGNING_KEY does not match the release key pinned in' <<< "$output"
pass 'a signing key other than the pinned one stops the release'
pinned="$root/.github/release-allowed-signers"
if [ "$(grep -c . "$pinned" 2>/dev/null)" != 1 ] || ! grep -Eqx 'koidrive namespaces="koidrive-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]{48}' "$pinned"; then
    echo 'FAIL .github/release-allowed-signers must hold one line: koidrive namespaces="koidrive-release" ssh-ed25519 <release public key>' >&2
    exit 1
fi
pass 'the release public key is pinned'

fresh
publish v0.1.0-alpha.1
expect_latest v0.1.0-alpha.1 'the first alpha becomes Latest'
publish v0.1.0-alpha.1
expect_latest v0.1.0-alpha.1 'a rerun of the highest tag keeps Latest'
[ "$(wc -l < "$tmp/db/assets-v0.1.0-alpha.1" | tr -d ' ')" = 5 ] || { echo 'FAIL an identical rerun uploaded assets again' >&2; exit 1; }
pass 'an identical rerun leaves published assets untouched'
echo 'signed again with a different timestamp' > "$tmp/dist/koidrive-aarch64-apple-darwin.tar.gz"
(cd "$tmp/dist" && sha256sum koidrive-*.tar.gz Koidrive-*.dmg > SHA256SUMS)
sign
if output=$(publish_staged v0.1.0-alpha.1 2>&1); then
    echo 'FAIL a rerun replaced a published release with different bytes' >&2
    exit 1
fi
grep -qF 'Published assets cannot be replaced; create a new version tag.' <<< "$output"
[ "$(cat "$tmp/db/files-v0.1.0-alpha.1/koidrive-aarch64-apple-darwin.tar.gz")" = cli ] || { echo 'FAIL a refused rerun changed the published tarball' >&2; exit 1; }
pass 'a rebuilt signed tarball is refused before any published asset changes'
stage v0.1.0-alpha.1
echo extra > "$tmp/db/files-v0.1.0-alpha.1/extra.txt"
if output=$(publish_staged v0.1.0-alpha.1 2>&1); then
    echo 'FAIL a rerun ignored an extra published asset' >&2
    exit 1
fi
grep -qF 'has a published asset extra.txt that is absent from this build' <<< "$output"
pass 'a rerun with an extra published asset is refused'
rm "$tmp/db/files-v0.1.0-alpha.1/extra.txt"
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
mode=prerelease publish v0.2.0-alpha.1
expect_latest v0.1.0 'a prerelease-mode alpha leaves Latest'
[ "$(cat "$tmp/db/release-v0.2.0-alpha.1")" = "false true" ] || { echo 'FAIL the alpha is not a prerelease' >&2; exit 1; }
pass 'a prerelease-mode alpha is a GitHub prerelease'
publish v0.1.1
expect_latest v0.1.1 'a GitHub prerelease does not hold back Latest'

fresh
stage v0.2.0-alpha.1
mode=prerelase expect_refused v0.2.0-alpha.1 'PRERELEASE_TAGS_PUBLISH_AS is prerelase, expected normal-release or prerelease' 'a misspelled prerelease mode is refused'
expect_latest none 'a misspelled prerelease mode leaves Latest'
prerelease=false expect_refused v0.2.0-alpha.1 'PRERELEASE is false, but v0.2.0-alpha.1 means true' 'a PRERELEASE that disagrees with the tag is refused'
stage v0.2.0
prerelease=True expect_refused v0.2.0 'PRERELEASE is True, but v0.2.0 means false' 'a PRERELEASE other than true or false is refused'

fresh
(
    export PRERELEASE=false PRERELEASE_TAGS_PUBLISH_AS=prerelase
    publish v0.3.0-alpha.1
)
expect_latest v0.3.0-alpha.1 'an exported PRERELEASE or mode does not reach the publish'

for tag in v01.0.0 v0.01.0 v0.1.00 v0.1.0-alpha.01; do
    stage "$tag"
    expect_refused "$tag" "$tag is not a vX.Y.Z" "a tag with a leading zero, $tag, is refused"
done
workflow_tag_check=$(grep -F "[[ \"\$TAG\" =~" "$root/.github/workflows/release-cli.yml")
[ -n "$workflow_tag_check" ] || { echo 'FAIL release-cli.yml has no tag check' >&2; exit 1; }
for tag in v01.0.0 v0.01.0 v0.1.00 v0.1.0-alpha.01 v0.1 v0.1.0-beta.1; do
    if TAG=$tag bash -c "$workflow_tag_check" > /dev/null 2>&1; then echo "FAIL the release workflow accepts $tag" >&2; exit 1; fi
done
for tag in v0.0.0 v0.1.0 v10.20.30 v0.1.0-alpha.4 v1.0.0-rc.0; do
    TAG=$tag bash -c "$workflow_tag_check" > /dev/null || { echo "FAIL the release workflow refuses $tag" >&2; exit 1; }
done
pass 'the release workflow accepts only tags without leading zeros'

echo "Passed $passed release publishing tests"

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/images" "$tmp/tap/Casks"
cat > "$tmp/bin/brew" <<'EOF'
#!/usr/bin/env bash
case $1 in
    tap|list|tap-new|untap) ;;
    --prefix) printf '%s/prefix\n' "$CASK_FIXTURE_TEST_ROOT" ;;
    --repository) printf '%s/tap\n' "$CASK_FIXTURE_TEST_ROOT" ;;
    *) printf 'unexpected brew command: %s\n' "$*" >&2; exit 91 ;;
esac
EOF
cat > "$tmp/bin/uuidgen" <<'EOF'
#!/usr/bin/env bash
printf '00000000-0000-0000-0000-000000000000\n'
EOF
cat > "$tmp/bin/hdiutil" <<'EOF'
#!/usr/bin/env bash
if [ "$CASK_FIXTURE_TEST_STAGE" = checksum ]; then
    printf 'fixture image\n' > "${!#}"
    exit 0
fi
if [ "$CASK_FIXTURE_TEST_STAGE" = native ]; then
    case " $* " in *' -quiet '*) ;; *) printf 'native image creation detail\n' >&2 ;; esac
fi
exit 73
EOF
cat > "$tmp/bin/shasum" <<'EOF'
#!/usr/bin/env bash
printf 'checksum failure detail\n' >&2
exit 75
EOF
chmod +x "$tmp/bin/"*
export PATH="$tmp/bin:$PATH" CASK_FIXTURE_TEST_ROOT=$tmp HOMEBREW_TEMP=$tmp/images

for stage in silent native checksum; do
    status=0
    CASK_FIXTURE_TEST_STAGE=$stage bash "$root/scripts/test-homebrew-cask-install.sh" > "$tmp/output" 2>&1 || status=$?
    expected=73
    [ "$stage" != checksum ] || expected=75
    if [ "$status" -ne "$expected" ] || ! grep -qF "status=$expected" "$tmp/output" || ! grep -qF 'version=0.1.0-alpha.2' "$tmp/output" || ! grep -Eq 'test-homebrew-cask-install.sh:[0-9]+' "$tmp/output"; then
        cat "$tmp/output" >&2
        echo "FAIL $stage failure lacks command, line, version or original status $expected (observed $status)" >&2
        exit 1
    fi
    case $stage in
        silent) grep -qF 'command=hdiutil create' "$tmp/output" ;;
        native) grep -qF 'native image creation detail' "$tmp/output" ;;
        checksum) grep -qF 'pipeline=75 0' "$tmp/output" && grep -qF 'checksum failure detail' "$tmp/output" ;;
    esac || { cat "$tmp/output" >&2; echo "FAIL $stage failure lost its diagnostic detail" >&2; exit 1; }
    echo "PASS cask fixture reports $stage failure and preserves exit status $expected"
done
echo 'Passed 3 Homebrew cask failure diagnostic tests'

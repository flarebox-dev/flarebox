#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

scripts=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
touch "$tmp/Koidrive.dmg"
cat > "$tmp/bin/hdiutil" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
    attach)
        while [ "$1" != -mountpoint ]; do shift; done
        app=$2/Koidrive.app
        mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers"
        touch "$app/Contents/MacOS/Koidrive" "$app/Contents/Info.plist"
        chmod +x "$app/Contents/MacOS/Koidrive"
        ln -s /Applications "$2/Applications"
        cat > "$app/Contents/Helpers/koi" <<'HELPER'
#!/bin/sh
touch "$VERIFY_FIXTURE_HELPER_CALLED"
printf 'koi 0.1.0-alpha.2\n'
HELPER
        chmod +x "$app/Contents/Helpers/koi"
        ;;
    detach) rm -rf "$2/Koidrive.app" "$2/Applications" ;;
    verify) ;;
    *) exit 2 ;;
esac
STUB
cat > "$tmp/bin/plutil" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
    -p) printf '"KoidriveVersion" => "0.1.0-alpha.2"\n' ;;
    -extract)
        case "$2" in
            KoidriveVersion) echo 0.1.0-alpha.2 ;;
            CFBundleShortVersionString) echo 0.1.0 ;;
            *) exit 2 ;;
        esac
        ;;
    *) exit 2 ;;
esac
STUB
cat > "$tmp/bin/codesign" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
    *--entitlements*) echo '<plist><dict/></plist>' ;;
    *--display*)
        echo 'Identifier=dev.koidrive.Koidrive'
        echo 'CodeDirectory v=20500 size=1 flags=0x10000(runtime)'
        if [ "$VERIFY_FIXTURE_MODE" = unsigned ]; then
            echo 'Signature=adhoc'
        else
            echo 'Authority=Developer ID Application: Fixture'
            echo 'Timestamp=Nov 13, 2026'
        fi
        ;;
    *--verify*) ;;
    *) exit 2 ;;
esac
STUB
cat > "$tmp/bin/xcrun" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[ "$1 $2" = 'stapler validate' ] || exit 2
case "$3" in *.app) target=app ;; *.dmg) target=dmg ;; *) exit 2 ;; esac
[ "$VERIFY_FIXTURE_CASE" != "$target-unstapled" ]
STUB
cat > "$tmp/bin/spctl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "${!#}" in *.app) target=app ;; *.dmg) target=dmg ;; *) exit 2 ;; esac
[ "$VERIFY_FIXTURE_CASE" != "$target-rejected" ] || exit 1
if [ "$VERIFY_FIXTURE_CASE" = "$target-unnotarized" ]; then
    echo 'source=Developer ID'
else
    echo 'source=Notarized Developer ID'
fi
STUB
cat > "$tmp/bin/xattr" <<'STUB'
#!/usr/bin/env bash
if [ "$VERIFY_FIXTURE_CASE" = unsigned-quarantined ]; then
    echo 'Koidrive.app: com.apple.quarantine: 0083;fixture'
fi
STUB
chmod +x "$tmp/bin/"*
passed=0
for entry in \
    'valid|signed|' \
    'valid-static|signed-static|' \
    'unsigned|unsigned|' \
    'app-unstapled|signed|The app has no valid stapled notarization ticket.' \
    'dmg-unstapled|signed|The disk image has no valid stapled notarization ticket.' \
    'dmg-unstapled-static|signed-static|The disk image has no valid stapled notarization ticket.' \
    'app-rejected|signed|Gatekeeper rejects the app.' \
    'dmg-rejected|signed|Gatekeeper rejects the disk image.' \
    'app-unnotarized|signed|Gatekeeper accepts the app, but not as notarized Developer ID software.' \
    'dmg-unnotarized|signed|Gatekeeper accepts the disk image, but not as notarized Developer ID software.' \
    'dmg-unnotarized-static|signed-static|Gatekeeper accepts the disk image, but not as notarized Developer ID software.' \
    'unsigned-quarantined|unsigned|Unsigned verification requires a locally built, unquarantined disk image and app.'; do
    IFS='|' read -r name mode error <<< "$entry"
    rm -f "$tmp/helper-called"
    status=0
    PATH="$tmp/bin:$PATH" VERSION=0.1.0-alpha.2 VERIFY_FIXTURE_CASE=${name%-static} VERIFY_FIXTURE_MODE=$mode \
        VERIFY_FIXTURE_HELPER_CALLED=$tmp/helper-called \
        bash "$scripts/verify-release.sh" "$tmp/Koidrive.dmg" "$mode" > "$tmp/output" 2>&1 || status=$?
    if [ "$name" = valid-static ]; then
        if [ "$status" -ne 0 ] || [ -e "$tmp/helper-called" ]; then
            cat "$tmp/output" >&2
            echo 'FAIL static verification did not pass without running the helper' >&2
            exit 1
        fi
    elif [ -z "$error" ]; then
        if [ "$status" -ne 0 ] || [ ! -e "$tmp/helper-called" ]; then
            cat "$tmp/output" >&2
            echo "FAIL $name did not verify and run the helper" >&2
            exit 1
        fi
    elif [ "$status" -eq 0 ] || ! grep -qF "$error" "$tmp/output" || [ -e "$tmp/helper-called" ]; then
        cat "$tmp/output" >&2
        echo "FAIL $name was not rejected before executing the helper" >&2
        exit 1
    fi
    echo "PASS $name"
    passed=$((passed + 1))
done
echo "Passed $passed release verification tests"

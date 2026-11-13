#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

source_app=${1:?usage: test-login-item-lifecycle.sh <Koidrive.app>}
attributes=$(xattr -lr "$source_app")
if grep -q 'com.apple.quarantine:' <<< "$attributes"; then
    echo 'FAIL native lifecycle checks require a locally built, unquarantined app' >&2
    exit 1
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
app=$tmp/KoidriveUninstallFixture.app
ditto "$source_app" "$app"
/usr/libexec/PlistBuddy -c 'Set CFBundleIdentifier dev.koidrive.KoidriveUninstallFixture' "$app/Contents/Info.plist"
codesign --force --deep --sign - --identifier dev.koidrive.KoidriveUninstallFixture "$app"
codesign --verify --deep --strict "$app"
attributes=$(xattr -lr "$app")
if grep -q 'com.apple.quarantine:' <<< "$attributes"; then
    echo 'FAIL the native lifecycle fixture must not be quarantined' >&2
    exit 1
fi
mkdir "$tmp/home"
HOME=$tmp/home XDG_CONFIG_HOME=$tmp/config XDG_STATE_HOME=$tmp/state XDG_RUNTIME_DIR=$tmp/runtime python3 - "$app" <<'PY'
import os
import pathlib
import subprocess
import sys

app = pathlib.Path(sys.argv[1])
binary = app / "Contents/MacOS/Koidrive"
for command in ("--unregister-login-item", "--unregister-login-item"):
    result = subprocess.run(
        [str(binary), command],
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert result.returncode == 0, result.stderr
    assert not result.stdout, result.stdout
    assert not result.stderr, result.stderr
    assert not (pathlib.Path(os.environ["HOME"]) / "Library/Application Support/Koidrive").exists()
    assert not pathlib.Path(os.environ["XDG_STATE_HOME"]).exists()
    print(f"PASS {command} exits without UI for an unregistered fixture")
print("Passed 2 native login-item lifecycle checks")
PY

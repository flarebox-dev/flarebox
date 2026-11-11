#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs the koidrive-driver sync tests, two drivers in one process, against the Worker in server/ under wrangler dev
# with fresh local state. Needs the server's dependencies (pnpm install in server/). Extra arguments go to the test
# binary.

set -euo pipefail
set -m

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
work="$(mktemp -d)"
port="${KOIDRIVE_TEST_PORT:-$(node -e 'const s = require("net").createServer().listen(0, "127.0.0.1", () => { console.log(s.address().port); s.close(); })')}"
log="$work/wrangler.log"
wrangler="$root/server/node_modules/.bin/wrangler"

if [ ! -x "$wrangler" ]; then
  echo "error: $wrangler is missing; run 'pnpm install' in server/ first." >&2
  exit 1
fi

cleanup() {
  status=$?
  if [ -n "${server_pid:-}" ]; then
    kill -TERM -- "-$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "--- wrangler dev output ($log) ---" >&2
    tail -n 200 "$log" >&2 || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# set -m puts the server in its own process group, so cleanup stops workerd with it.
(cd "$root/server" && WRANGLER_SEND_METRICS=false exec "$wrangler" dev --ip 127.0.0.1 --port "$port" \
  --persist-to "$work/state" --show-interactive-dev-session=false) >"$log" 2>&1 &
server_pid=$!

for _ in $(seq 1 120); do
  if grep -q "Ready on" "$log"; then
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "error: wrangler dev exited before it was ready." >&2
    exit 1
  fi
  sleep 0.5
done
if ! grep -q "Ready on" "$log"; then
  echo "error: wrangler dev was not ready on port $port after 60 seconds." >&2
  exit 1
fi

cd "$root"
KOIDRIVE_TEST_SERVER="http://127.0.0.1:$port" KOIDRIVE_TEST_SERVER_LOG="$log" \
  cargo test -p koidrive-driver --locked --test sync -- --ignored "$@"

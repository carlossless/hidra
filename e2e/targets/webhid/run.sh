#!/usr/bin/env bash
# End-to-end WebHID conformance run: starts the uhid fixture, grants it to the
# test origin via Chrome's WebHidAllowDevicesForUrls policy, then drives hidra's
# WebHID backend through Chromium/Playwright.
#
# Needs root (uhid + the system policy dir). Run:  sudo -E ./e2e/webhid/run.sh
# Expects the wasm harness (pkg/) and node_modules already built (see README).
set -uo pipefail

WEB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=8099
POLICY_DIR=/etc/chromium/policies/managed
POLICY="$POLICY_DIR/hidra-webhid.json"

: "${CHROMIUM:=$(command -v chromium || command -v google-chrome || true)}"
: "${NODE:=node}"
: "${XVFB_RUN:=xvfb-run}"
[ -n "$CHROMIUM" ] || { echo "set CHROMIUM to a chromium/chrome binary"; exit 2; }

# Build the fixture crate unless a caller pre-built it.
FIXTURE="$WEB/fixture/target/debug/webhid_uhid_fixture"
[ -x "$FIXTURE" ] || ( cd "$WEB/fixture" && cargo build )

cleanup() {
  [ -n "${FPID:-}" ] && kill "$FPID" 2>/dev/null
  rm -f "$POLICY"
}
trap cleanup EXIT

# Pre-grant vid 0x1209 / pid 0x000c to the test origin so no chooser appears.
mkdir -p "$POLICY_DIR"
cat > "$POLICY" <<JSON
{
  "WebHidAllowDevicesForUrls": [
    { "devices": [{ "vendor_id": 4617, "product_id": 12 }], "urls": ["http://localhost:$PORT"] }
  ]
}
JSON

FIXLOG="$(mktemp)"
"$FIXTURE" > "$FIXLOG" 2>&1 &
FPID=$!
for _ in $(seq 1 30); do grep -q READY "$FIXLOG" 2>/dev/null && break; sleep 0.2; done
grep -q READY "$FIXLOG" || { echo "fixture never became READY"; cat "$FIXLOG"; exit 3; }

cd "$WEB"
CHROMIUM="$CHROMIUM" HOME="${HOME:-/root}" "$XVFB_RUN" -a "$NODE" webhid_run.mjs
RC=$?

echo "--- fixture (device side) saw ---"
grep -E 'OUTPUT|SET_REPORT' "$FIXLOG" || echo "(no output/set-report captured)"
rm -f "$FIXLOG"
exit $RC

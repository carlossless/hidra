#!/usr/bin/env bash
# End-to-end WebUSB conformance run: brings up the vendor-class FunctionFS
# gadget, grants it to the test origin via Chrome's WebUsbAllowDevicesForUrls
# policy, then drives hidra's WebUSB backend through Chromium/Playwright.
#
# Needs root (configfs/dummy_hcd + the system policy dir).
# Run:  sudo -E ./e2e/targets/webusb/run.sh
# Expects the wasm harness (pkg/) and node_modules already built (see README).
set -uo pipefail

WEB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=8098
POLICY_DIR=/etc/chromium/policies/managed
POLICY="$POLICY_DIR/hidra-webusb.json"

: "${CHROMIUM:=$(command -v chromium || command -v google-chrome || true)}"
: "${NODE:=node}"
: "${XVFB_RUN:=xvfb-run}"
[ -n "$CHROMIUM" ] || { echo "set CHROMIUM to a chromium/chrome binary"; exit 2; }

FIXTURE="$WEB/fixture/target/debug/webusb_ffs_fixture"
[ -x "$FIXTURE" ] || ( cd "$WEB/fixture" && cargo build )

cleanup() {
  [ -n "${FPID:-}" ] && kill "$FPID" 2>/dev/null
  rm -f "$POLICY"
}
trap cleanup EXIT

# Pre-grant vid 0x1209 / pid 0x000c to the test origin so no chooser appears.
# Unlike WebHID this is what makes the run non-interactive at all: WebUSB's
# requestDevice() always opens a chooser, so the harness reads device_list()
# instead, which only sees devices already granted.
mkdir -p "$POLICY_DIR"
cat > "$POLICY" <<JSON
{
  "WebUsbAllowDevicesForUrls": [
    { "devices": [{ "vendor_id": 4617, "product_id": 12 }], "urls": ["http://localhost:$PORT"] }
  ]
}
JSON

FIXLOG="$(mktemp)"
"$FIXTURE" > "$FIXLOG" 2>&1 &
FPID=$!
for _ in $(seq 1 50); do grep -q READY "$FIXLOG" 2>/dev/null && break; sleep 0.2; done
grep -q READY "$FIXLOG" || { echo "fixture never became READY"; cat "$FIXLOG"; exit 3; }

cd "$WEB"
CHROMIUM="$CHROMIUM" HOME="${HOME:-/root}" "$XVFB_RUN" -a "$NODE" webusb_run.mjs
RC=$?

echo "--- fixture (device side) saw ---"
grep -E 'OUTPUT|SET_REPORT|GET_REPORT|GET_DESCRIPTOR|STALL' "$FIXLOG" || echo "(nothing captured)"
rm -f "$FIXLOG"
exit $RC

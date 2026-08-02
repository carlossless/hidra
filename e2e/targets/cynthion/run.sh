#!/usr/bin/env bash
# Run the full conformance matrix on Linux against a real USB HID device emulated
# by a Cynthion running Facedancer: for each report-ID mode (unnumbered, numbered)
# (re)start device.py with the matching descriptor and run both host backends
# (hidraw, nusb). For Windows/macOS, USB-pass the device into the VM and point
# HIDRA_CYNTHION_CTRL at this host (see README).
#
# The Facedancer/Moondancer link wedges (LIBUSB_ERROR_TIMEOUT) if device.py is
# restarted without a full USB reset, so between modes we power-cycle the
# Cynthion's hub ports with uhubctl (best effort; override with
# HIDRA_CYNTHION_UHUBCTL, e.g. "-l 9-1.4.4 -p 1,4").
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E="$(cd "$DIR/.." && pwd)"
PYENV='python3.withPackages(ps: with ps; [ facedancer cynthion ])'
CTRL="${HIDRA_CYNTHION_CTRL:-127.0.0.1:9999}"
UHUBCTL_LOC="${HIDRA_CYNTHION_UHUBCTL:-}"
LOG="$(mktemp)"
FPID=""

stop_device() { [ -n "$FPID" ] && kill "$FPID" 2>/dev/null; FPID=""; sleep 2; }
cleanup() { stop_device; rm -f "$LOG"; }
trap cleanup EXIT

power_cycle() {
  [ -n "$UHUBCTL_LOC" ] || return 0
  echo "--- power-cycling Cynthion ($UHUBCTL_LOC) ---"
  sudo nix-shell -p uhubctl --run "uhubctl $UHUBCTL_LOC -a cycle -d 3" >/dev/null 2>&1 || true
  sleep 8
}

start_device() {
  local numbered="$1"
  power_cycle
  HIDRA_CYNTHION_NUMBERED="$([ "$numbered" = 1 ] && echo 1 || true)" \
    nix-shell -p "$PYENV" --run "python3 '$DIR/device.py'" >"$LOG" 2>&1 &
  FPID=$!
  for _ in $(seq 1 30); do
    lsusb 2>/dev/null | grep -q '1209:000c' && grep -q listening "$LOG" && break
    sleep 1
  done
  lsusb 2>/dev/null | grep -q '1209:000c' || { echo "device never enumerated"; cat "$LOG"; exit 3; }
}

# Resolve the test binary for a feature set (builds it first).
bin_for() {
  ( cd "$E2E" && cargo test -p cynthion "$@" --no-run ) >/dev/null 2>&1
  cd "$E2E" && cargo test -p cynthion "$@" --no-run --message-format=json 2>/dev/null \
    | grep -o '"executable":"[^"]*cynthion-[0-9a-f]*"' | grep -v unittest | head -1 \
    | sed 's/.*"\(\/[^"]*\)"/\1/'
}

HIDRAW_BIN="$(bin_for)"
NUSB_BIN="$(bin_for --features nusb)"

for numbered in 0 1; do
  mode="$([ "$numbered" = 1 ] && echo numbered || echo unnumbered)"
  export HIDRA_CYNTHION_NUMBERED="$([ "$numbered" = 1 ] && echo 1 || true)"

  echo "===== $mode : hidraw ====="
  start_device "$numbered"
  sudo -E env "PATH=$PATH" HIDRA_CYNTHION_REQUIRED=1 "HIDRA_CYNTHION_CTRL=$CTRL" \
    "${HIDRA_CYNTHION_NUMBERED:+HIDRA_CYNTHION_NUMBERED=1}" \
    "$HIDRAW_BIN" --test-threads=1 --nocapture
  stop_device

  echo "===== $mode : nusb ====="
  start_device "$numbered"
  sudo -E env "PATH=$PATH" HIDRA_CYNTHION_REQUIRED=1 "HIDRA_CYNTHION_CTRL=$CTRL" \
    "${HIDRA_CYNTHION_NUMBERED:+HIDRA_CYNTHION_NUMBERED=1}" \
    "$NUSB_BIN" --test-threads=1 --nocapture
  stop_device
done

echo "ALL cynthion Linux runs passed (unnumbered + numbered, hidraw + nusb)."

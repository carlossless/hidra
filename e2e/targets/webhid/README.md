# WebHID backend conformance test

End-to-end test of hidra's **WebHID** backend, running in **Chrome on Linux**
driven by **Playwright** — the browser analog of the native `e2e/*` test crates.

Two crates live here: the wasm harness (this directory) and the native uhid
`fixture/`. They are separate cargo workspaces so the wasm build's `.cargo/config`
(which pins the wasm target) doesn't cascade into the native fixture.

## How it works

1. `fixture/` is a native crate creating a virtual HID device via `uhid` with the
   shared conformance identity (`1209:000c`, product `hidra-conformance`), streams
   a known input report,
   answers GET_REPORT (feature), and logs any output / set-feature reports it
   receives — so the device side can confirm `write()`/`send_feature_report()`
   actually arrived.
2. `src/lib.rs` is a `wasm-bindgen` harness (`run_webhid_test`) that drives
   hidra's WebHID backend against that device: `get_devices` → `open` → device
   info → collections → `read` → `get_feature_report` → `write` →
   `send_feature_report`, asserting the payloads.
3. `webhid_run.mjs` serves `index.html` + the wasm on a fixed port, launches
   Chromium (headed, under Xvfb), and reads the harness result.
4. Chrome's device permission is pre-granted with no chooser via the
   `WebHidAllowDevicesForUrls` enterprise policy (`run.sh` writes it to
   `/etc/chromium/policies/managed/` and removes it afterwards). The CDP
   `DeviceAccess` chooser-automation path is unreliable with this Chromium, so
   the policy is used instead.

## Running

Needs root (uhid + the system policy dir) and these tools: `wasm-pack`,
`wasm-bindgen-cli`, a Chromium/Chrome, `nodejs`, `xvfb-run`. On NixOS:

```sh
# from e2e/webhid/ :
# 1. build the wasm harness
nix shell nixpkgs#wasm-pack nixpkgs#wasm-bindgen-cli \
  -c wasm-pack build --target web --dev

# 2. install the Playwright npm package (browser download skipped)
PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 nix shell nixpkgs#nodejs -c npm install

# 3. run everything (run.sh builds the fixture crate itself)
CHROMIUM=$(nix build nixpkgs#chromium --no-link --print-out-paths)/bin/chromium \
NODE=$(nix build nixpkgs#nodejs --no-link --print-out-paths)/bin/node \
XVFB_RUN=$(nix build nixpkgs#xvfb-run --no-link --print-out-paths)/bin/xvfb-run \
  sudo -E ./run.sh
```

Or just `nix run .#test-vm` (with `hidra.autorun`) to run this plus the uhid and
nusb suites in a reproducible NixOS VM — see `../nix/test-vm.nix`.

A successful run prints `WEBHID_RESULT_OK PASS: …` and exits 0.

# WebUSB backend conformance test

End-to-end test of hidra's **WebUSB** backend, running in **Chromium on Linux**
driven by **Playwright** — the WebUSB counterpart to [`../webhid`](../webhid).

The device under test has to declare a **vendor-specific interface class**:
Blink refuses `claimInterface` on the protected classes, HID among them, so a
`usb_f_hid` gadget like the one `linux-nusb` uses would be rejected outright.
That rules out the stock HID function and calls for FunctionFS, where userspace
supplies the descriptors verbatim.

Two crates, as separate cargo workspaces so the wasm build's `.cargo/config`
does not cascade into the native fixture:

1. **`fixture/`** — a native crate that stands up a `dummy_hcd` + configfs
   **FunctionFS** gadget with the shared conformance identity (`1209:000c`),
   one vendor-class (0xFF) interface and two interrupt endpoints. It services
   the interface's control requests by hand (`FUNCTIONFS_ALL_CTRL_RECIP`):
   `GET_DESCRIPTOR(Report)`, `GET_REPORT`, `SET_REPORT`, stalling the rest. It
   streams a known input report on EP1 IN and logs whatever arrives on EP2 OUT.
2. **`src/lib.rs`** — a `wasm-bindgen` harness (`run_webusb_test`) driving
   hidra's WebUSB backend against it: `device_list` → `open` → device info →
   report descriptor → `get_feature_report` → `send_feature_report` → `write` →
   `read`, asserting payloads.
3. **`webusb_run.mjs`** — serves `index.html` + the wasm on a fixed port,
   launches Chromium (headed, under Xvfb), reads the harness result.

## Device shapes

[`run.sh`](run.sh) runs the harness three times, rebuilding the gadget between
each, because the degenerate shapes are the ones with their own code paths:

| variant | gadget | what it pins down |
|---------|--------|-------------------|
| `full` | 2 interrupt endpoints, report descriptor | the ordinary path |
| `control-only` | `bNumEndpoints` 0 | every report goes over the control pipe: `write` falls back to `SET_REPORT(Output)`, `read` must refuse instead of hanging, and `get_input_report` remains the way to poll |
| `no-report-descriptor` | endpoints, `GET_DESCRIPTOR(Report)` stalls | `report_descriptor` reports `Unsupported` rather than handing back an empty buffer |

`control-only` is not hypothetical: it is the shape of the Sinowealth ISP
bootloaders, which sinowisp drives entirely through feature reports.

## Why `device_list` and not the chooser

`navigator.usb.requestDevice()` always opens a picker and needs a user gesture,
so it cannot be driven unattended. The `WebUsbAllowDevicesForUrls` enterprise
policy ([`run.sh`](run.sh) writes it to `/etc/chromium/policies/managed/` and
removes it afterwards) pre-grants the fixture to the test origin, after which it
appears in `navigator.usb.getDevices()` — hidra's `webusb::Hidra::device_list`
— with no interaction at all.

## Running

Needs root (configfs, `dummy_hcd`, the system policy dir) and: `wasm-pack`,
`wasm-bindgen-cli`, a Chromium/Chrome, `nodejs`, `xvfb-run`. Kernel modules
`dummy_hcd`, `libcomposite` and `usb_f_fs` must be available. From this
directory:

```sh
# 1. build the wasm harness
nix shell nixpkgs#wasm-pack nixpkgs#wasm-bindgen-cli -c wasm-pack build --target web --dev

# 2. install the Playwright npm package (browser download skipped)
PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 nix shell nixpkgs#nodejs -c npm install

# 3. run everything (run.sh builds the fixture crate itself)
sudo -E ./run.sh
```

The whole thing runs unattended in the Linux test VM — see
[`../../platform/linux`](../../platform/linux/README.md).

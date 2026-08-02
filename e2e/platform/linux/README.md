# Linux testing VM

Covers all three Linux paths against software virtual devices:

- **hidraw** backend against a kernel `uhid` virtual HID device,
- **nusb** backend against a `dummy_hcd` + configfs `g_hid` virtual USB gadget,
- **WebHID** (wasm32) backend, reading a `uhid` device through headless Chromium.

A NixOS VM is the reference environment because everything below can be
expressed declaratively — it is codified in [`test-vm.nix`](test-vm.nix)
and buildable with `nix run .#test-vm`. Any Linux distribution works if you
provide the same kernel modules, toolchain, and privileges.

## Kernel modules

The tests create their virtual devices through these modules — load them (and
make sure the running kernel actually ships them; cloud/CI kernels often omit
the USB-gadget ones):

```
uhid            # hidraw path: /dev/uhid virtual HID device
dummy_hcd       # nusb path: virtual USB host controller
libcomposite    # nusb path: configfs USB gadget framework
usb_f_hid       # nusb path: HID gadget function
configfs        # nusb path: gadget configuration filesystem
```

`dummy_hcd` is the usual missing one — many distro/cloud kernels build no
USB-gadget support at all. The nusb path only runs on a kernel that ships it.

## Privileges

The test process must run as **root**: it opens `/dev/uhid`, writes the gadget
under `/sys/kernel/config`, and opens the resulting `/dev/hidrawN` node and raw
USB device.

## uhid / nusb build gotcha

hidra's `nusb` feature *switches* the Linux backend (raw USB instead of
hidraw). Build and run the uhid test and the nusb test in **separate** cargo
invocations — compiling both in one invocation unifies the feature and the
uhid test ends up on the USB backend.

## Toolchain (native)

- Rust stable with a C toolchain (`gcc`) and `pkg-config`.
- `libudev` development files — nusb enumerates through libudev on Linux.
  (On NixOS: `pkg-config` + `udev` in the shell's build inputs.)

## Toolchain (WebHID / wasm32)

- Rust stable with the `wasm32-unknown-unknown` target.
- `wasm-pack`, plus a `wasm-bindgen` CLI whose version **exactly matches** the
  `wasm-bindgen` crate hidra depends on (currently `0.2.126`). A mismatched CLI
  produces broken glue. Distro packages are frequently too old — install the
  pinned version explicitly, e.g. `cargo install wasm-bindgen-cli --version 0.2.126`.
- Node.js (for the Playwright driver) and `npm` (install with
  `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1`; the browser comes from the OS below).
- Chromium and `xvfb` (or `xvfb-run`).

### WebHID browser configuration

WebHID normally requires a user gesture to pick a device. For an unattended
test, grant access with an enterprise policy instead of automating the chooser
(chooser automation via CDP does not work on recent Chromium):

Write `/etc/chromium/policies/managed/hidra-webhid.json`:

```json
{
  "WebHidAllowDevicesForUrls": [
    {
      "devices": [ { "vendor_id": 4617, "product_id": 12 } ],
      "urls": [ "http://localhost:8099" ]
    }
  ]
}
```

- `vendor_id` / `product_id` are **decimal** (4617 = 0x1209, 12 = 0x000c) and
  must match the virtual device's IDs; `urls` must match the origin the test
  page is served from.
- Chromium must run **headed** under Xvfb (`headless:false`); the old headless
  shell cannot access HID.
- When Chromium runs as root (because the test needs root), add `--no-sandbox`.
  The root-owned `/dev/hidrawN` node is then accessible.

## NixOS host note

On a NixOS *host* building the wasm artifacts, Rust ≥1.90's bundled `rust-lld`
may fail to run; wrapper scripts around `~/.rustup/.../rust-lld` are needed and
must be re-applied after every `rustup update`. This affects the build host, not
the VM guest.

## Running

With the modules loaded and the toolchain present, from the `e2e/` workspace:

```sh
sudo -E cargo test -p linux-hidraw    # hidraw backend via uhid
sudo -E cargo test -p linux-nusb    # nusb backend via dummy_hcd + g_hid
cd webhid && ./run.sh               # WebHID via Playwright/Xvfb/Chromium
```

Keep `linux-hidraw` and `linux-nusb` in **separate** cargo invocations (see the
gotcha above).

# Linux testing VM

Covers all three Linux paths against software virtual devices: **hidraw** (kernel
`uhid`), **nusb** (`dummy_hcd` + configfs `g_hid` gadget), and **WebHID** (wasm32,
a `uhid` device read through headless Chromium).

Codified in [`test-vm.nix`](test-vm.nix): `nix run .#test-vm` (interactive, log in
as root, `run-hidra-tests`) or headless CI (auto-run + poweroff, results on the
serial console) via the `hidra.autorun` kernel param — see the file header. Any
distro works given the same modules, toolchain, and privileges.

## Kernel modules

Load these, and make sure the running kernel actually ships them — cloud/CI
kernels often omit the USB-gadget ones (`dummy_hcd` is the usual missing one):

```
uhid                                       # hidraw path: /dev/uhid
dummy_hcd libcomposite usb_f_hid configfs  # nusb path: virtual USB gadget
```

## Privileges

Root: the tests open `/dev/uhid`, write the gadget under `/sys/kernel/config`, and
open the resulting `/dev/hidrawN` node and raw USB device.

## Toolchain

- **Native:** Rust stable, `gcc`, `pkg-config`, and `libudev` dev files (nusb
  enumerates through libudev; on NixOS `pkg-config` + `udev` in the shell inputs).
- **WebHID / wasm32:** the `wasm32-unknown-unknown` target, `wasm-pack`, Node.js +
  `npm` (install with `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1`), Chromium, and `xvfb`.
  The `wasm-bindgen` CLI version must **exactly match** the `wasm-bindgen` crate
  hidra depends on (currently **`0.2.126`** — distro packages are frequently too
  old; `cargo install wasm-bindgen-cli --version 0.2.126`). A mismatched CLI emits
  broken glue.

WebHID access is pre-granted with no chooser via a `WebHidAllowDevicesForUrls`
enterprise policy, written by [`run.sh`](../../targets/webhid/run.sh) to
`/etc/chromium/policies/managed/`. Its `vendor_id`/`product_id` are **decimal**
and must match the virtual device (`4617`/`12` = `0x1209`/`0x000c`); the `urls`
origin must match the served page. Chromium must run **headed** under Xvfb (the
old headless shell cannot access HID); add `--no-sandbox` when it runs as root.

## Gotchas

- **uhid vs nusb build:** the `nusb` feature *switches* the Linux backend, so build
  and run `linux-hidraw` and `linux-nusb` in **separate** cargo invocations —
  compiling both together unifies the feature and the uhid test ends up on the USB
  backend.
- **NixOS build host:** Rust ≥1.90's bundled `rust-lld` may fail to run when
  building the wasm artifacts; wrapper scripts around `~/.rustup/.../rust-lld` are
  needed and must be re-applied after every `rustup update`. This affects the
  build host, not the VM guest.

## Running

```sh
sudo -E cargo test -p linux-hidraw       # hidraw backend via uhid
sudo -E cargo test -p linux-nusb         # nusb backend via dummy_hcd + g_hid
cd ../../targets/webhid && ./run.sh      # WebHID via Playwright/Xvfb/Chromium
```

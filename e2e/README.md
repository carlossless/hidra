# hidra end-to-end / conformance tests

A separate workspace so hidra's own manifest stays free of the heavy,
platform-specific harness deps (`block2`, `libloading`, `core-foundation-sys`).
Each target crate depends on hidra by path and pulls only what its platform needs.

Every backend is exercised against a **software virtual HID device** (no physical
hardware): each platform creates the device with its own OS facility, then hidra
enumerates, reads, writes, and feature-reports against it. Creating a virtual
device needs elevated privileges — and, on macOS/Windows, non-default OS security
settings — so the suites run in dedicated VMs. Per-OS setup is under
[`platform/`](platform).

## Layout

```
e2e/
  shared/conformance/   the shared suite: run_conformance, VirtualDevice, Caps
  targets/              one crate per backend; stands up a device, then run_conformance
  platform/             reproducible per-OS VM setup (linux/macos/windows)
```

## Targets

| target | platform | virtual device | privilege / setup |
|--------|----------|----------------|-------------------|
| `linux-hidraw` | Linux | kernel `uhid` (`Backend::Native`) | root; also a descriptor-variety test |
| `linux-nusb` | Linux | `dummy_hcd` + configfs `g_hid` (`Backend::Nusb`) | root; needs USB-gadget kernel modules |
| `macos` | macOS | `IOHIDUserDevice` | root + SIP/AMFI off + signed entitlement — see [`platform/macos`](platform/macos/README.md) |
| `windows` | Windows | WinUHid driver | test-signed driver — see [`platform/windows`](platform/windows/README.md) |
| `webhid` | Linux + Chromium | `uhid` via Playwright | root; own sub-tree (wasm harness + native fixture) — see [`targets/webhid`](targets/webhid/README.md) |
| `webusb` | Linux + Chromium | vendor-class FunctionFS gadget via Playwright | root; the interface must not be HID-class, so `usb_f_hid` will not do — see [`targets/webusb`](targets/webusb/README.md) |
| `cynthion` | all | real USB via Cynthion + Facedancer | emulated *real* hardware, not virtual, on both backends — see [`targets/cynthion`](targets/cynthion/README.md) |

Which hidra backend a suite drives is [`Caps::backend`](shared/conformance/src/lib.rs),
picked at run time, so the whole workspace builds at once and one binary can
cover both backends (`cynthion` does, via `HIDRA_BACKEND`).

## Running

Run **per crate**: each needs its own privileges and virtual-device setup.

```sh
sudo -E cargo test -p linux-hidraw    # Linux, needs /dev/uhid
sudo -E cargo test -p linux-nusb      # Linux, needs dummy_hcd
cargo test -p macos                   # macOS: sign + run as root (platform/macos)
cargo test -p windows                 # Windows: needs the WinUHid driver (platform/windows)
cd targets/webhid && ./run.sh         # WebHID via Playwright/Xvfb/Chromium
cd targets/webusb && ./run.sh         # WebUSB via Playwright/Xvfb/Chromium
./targets/cynthion/run.sh             # real USB via Cynthion (targets/cynthion)
```

The Linux crates need `libudev` (nusb enumerates through it); `nix develop` from
the repo root wires up `pkg-config` + `udev`.

Everything on Linux (uhid + nusb + WebHID) also runs headlessly and reproducibly
in the flake's NixOS VM: `nix run .#test-vm` — see
[`platform/linux`](platform/linux/README.md).

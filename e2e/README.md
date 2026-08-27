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
| `linux-hidraw` | Linux | kernel `uhid` (hidraw backend) | root; also a descriptor-variety test |
| `linux-nusb` | Linux | `dummy_hcd` + configfs `g_hid` (nusb backend) | root; needs USB-gadget kernel modules |
| `macos` | macOS | `IOHIDUserDevice` | root + SIP/AMFI off + signed entitlement — see [`platform/macos`](platform/macos/README.md) |
| `windows` | Windows | WinUHid driver | test-signed driver — see [`platform/windows`](platform/windows/README.md) |
| `webhid` | Linux + Chromium | `uhid` via Playwright | root; own sub-tree (wasm harness + native fixture) — see [`targets/webhid`](targets/webhid/README.md) |
| `cynthion` | all | real USB via Cynthion + Facedancer | emulated *real* hardware, not virtual — see [`targets/cynthion`](targets/cynthion/README.md) |

## Running

Run **per crate**, never the whole workspace at once: hidra's `nusb` feature
switches the Linux backend, so building `linux-hidraw` and `linux-nusb` together
unifies the feature and breaks the hidraw test.

```sh
sudo -E cargo test -p linux-hidraw    # Linux, needs /dev/uhid
sudo -E cargo test -p linux-nusb      # Linux, needs dummy_hcd
cargo test -p macos                   # macOS: sign + run as root (platform/macos)
cargo test -p windows                 # Windows: needs the WinUHid driver (platform/windows)
cd targets/webhid && ./run.sh         # WebHID via Playwright/Xvfb/Chromium
./targets/cynthion/run.sh             # real USB via Cynthion (targets/cynthion)
```

The Linux crates need `libudev` (nusb enumerates through it); `nix develop` from
the repo root wires up `pkg-config` + `udev`.

Everything on Linux (uhid + nusb + WebHID) also runs headlessly and reproducibly
in the flake's NixOS VM: `nix run .#test-vm` — see
[`platform/linux`](platform/linux/README.md).

# hidra end-to-end / conformance tests

These test crates live in their own workspace so the hidra library's manifest
stays free of the heavy, platform-specific harness dependencies (`block2`,
`libloading`, `core-foundation-sys`). Each crate depends on hidra by path and
pulls only what its platform needs.

## Layout

```
e2e/
  shared/conformance/   the shared suite: run_conformance, VirtualDevice, Caps
  targets/              one crate per backend; each stands up a device, then
                        calls run_conformance
  platform/             reproducible VM setup per OS (linux/macos/windows)
```

Each target `-p <name>` maps to `targets/<name>`:

| target | platform | virtual device | notes |
|--------|----------|----------------|-------|
| `linux-hidraw` | Linux | `uhid` (hidraw backend) | also a descriptor-variety test |
| `linux-nusb` | Linux | `dummy_hcd` + `g_hid` (nusb backend) | |
| `macos` | macOS | `IOHIDUserDevice` | needs SIP-off + entitlement (see `platform/macos`) |
| `windows` | Windows | WinUHid | |
| `cynthion` | all | — (real USB via Cynthion/Facedancer) | emulated *real* hardware, not virtual |
| `webhid` | Linux + Chrome | `uhid` via Playwright | its own sub-tree (wasm harness + native fixture) |

## Running

Run **per crate** — never the whole workspace at once, because hidra's `nusb`
feature switches the Linux backend, so building `linux-hidraw` and `linux-nusb`
together would unify it and break the hidraw test:

```sh
cargo test -p linux-hidraw    # Linux, under sudo (needs /dev/uhid)
cargo test -p linux-nusb    # Linux, under sudo (needs dummy_hcd)
cargo test -p macos         # macOS, sign + run as root (see targets/macos)
cargo test -p windows       # Windows (needs the WinUHid driver)
```

The Linux crates need `libudev` (nusb enumerates through it); use `nix develop`
from the repo root (its devShell wires up `pkg-config` + `udev`).

Everything (uhid + nusb + WebHID) also runs headlessly and reproducibly in the
flake's NixOS VM: `nix run .#test-vm` — see `platform/linux/test-vm.nix`.

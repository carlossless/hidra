# Testing VMs for hidra's virtual-HID integration tests

hidra's per-platform backends are exercised end to end against a **software
virtual HID device** — no physical hardware. Each platform creates the virtual
device with its own OS facility, then hidra enumerates, reads, writes, and
feature-reports against it exactly as it would a real device.

| Platform | Backend under test | Virtual-device facility | Notes |
|----------|--------------------|-------------------------|-------|
| Linux    | `hidraw` (default) | kernel `uhid`           | plain root, no extra provisioning |
| Linux    | `nusb` (feature)   | `dummy_hcd` + configfs `g_hid` gadget | virtual USB HID |
| Linux    | WebHID (wasm32)    | `uhid` device read through Chromium | headless Chromium + wasm |
| macOS    | IOHIDManager       | `IOHIDUserDevice`       | entitlement-gated, SIP/AMFI off, run as root |
| Windows  | `hid.dll`          | WinUHid driver          | test-signed kernel driver |

Because creating each virtual device requires elevated privileges and, on macOS
and Windows, non-default OS security settings, the tests are run inside
dedicated VMs rather than on a workstation. Each VM's setup is documented
separately:

- [`linux/`](linux/README.md) — a NixOS VM covering all three Linux paths (uhid, nusb, WebHID).
- [`macos/`](macos/README.md) — an OSX-KVM macOS VM for the `IOHIDUserDevice` path.
- [`windows/`](windows/README.md) — a QuickEMU Windows 11 VM with WinUHid.

Each document lists **only what the VM must have** to run the virtual tests:
kernel modules / drivers, toolchain, code-signing, and the privilege level the
test process needs. The tests themselves live in the [`e2e/targets/`](../targets)
workspace (`linux-hidraw`, `linux-nusb`, `macos`, `windows`, `webhid`) — see
[`../README.md`](../README.md) for how to invoke them.

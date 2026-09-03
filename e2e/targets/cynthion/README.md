# Conformance against real USB HID hardware (Cynthion + Facedancer)

This crate runs the shared conformance suite against a **real USB HID device**
emulated by a [Cynthion](https://greatscottgadgets.com/cynthion/) running
[Facedancer](https://github.com/greatscottgadgets/facedancer), the most
authentic target available, since the host under test sees genuine USB hardware
enumerated by its real HID stack (not a software virtual device).

## Pieces

- **`device.py`**, the Facedancer device: emulates VID `0x1209` / PID `0x000c`,
  product `hidra-conformance`, the standard 8-byte input/output/feature vendor
  report descriptor (byte-identical to `conformance::make_descriptor(false)`).
  It also runs a small **TCP control server** so the device side can be driven
  remotely:
  `inject <hex>` (input report), `prime <hex>` (GET_REPORT payload),
  `output?` / `setfeature?` (last received), `reset`.
- **`tests/cynthion.rs`**, a `VirtualDevice` that drives the device over that
  control channel (`HIDRA_CYNTHION_CTRL`, default `127.0.0.1:9999`) while hidra
  talks to the real USB device, then runs `run_conformance`.

## Topology

Facedancer runs on a *control host* (connected to the Cynthion's control port);
the Cynthion's *target* port presents the emulated device to the *host under
test*. The two can be the same machine (target port looped back) or different
(target → a VM via USB passthrough). The Rust test connects to the control
server over TCP, so it works either way.

```
Linux (same host):   cynthion target port ── this host ── hidra (native/nusb)
                     control server on 127.0.0.1:9999

VM (Windows/macOS):  cynthion target ── control host ══USB passthrough══> VM ── hidra
                     control server reachable from the VM (e.g. 10.0.2.2:9999)
```

## Coverage

The suite runs in both report-ID modes. Verified passing on all three OSes:

| host backend           | unnumbered | numbered |
| ---------------------- | :--------: | :------: |
| Linux hidraw           |     ✓      |    ✓     |
| Linux nusb (pure-Rust) |     ✓      |    ✓     |
| Windows                |     ✓      |    ✓     |
| macOS                  |     ✓      |    ✓     |

Every run also hits the shared suite's **concurrency** stress (the `Send + Sync`
handle hammered by writes + get_feature from several threads), **odd/oversized
input robustness** (no panic), and, over real USB, **disconnect** (Facedancer
drops the device; hidra's pending read resolves `Disconnected`). The Windows run
additionally shows **`set_write_timeout` actually firing**: a 1ms timeout makes a
real-USB write time out. The Windows/macOS runs also exercise the platform-only
client methods against real hardware: `container_id()` + `set_write_timeout()`
on Windows, `set_open_exclusive()`/`open_exclusive()` on macOS.

Two real-USB caveats: disconnect + `set_write_timeout`-firing **wedge the
Facedancer emulation afterward**, so each Windows/macOS run needs a fresh
`device.py`; and the **nusb** backend over the real Cynthion is flaky (nusb's
usbhid detach/claim wedges Moondancer), the stable real-USB path is hidraw.

`feature`/`input_get` GET_REPORT are skipped on the Linux `g_hid` gadget, and
strings on WinUHid, both virtual-device limitations, not real-Cynthion ones.

**Numbered mode.** Set `HIDRA_CYNTHION_NUMBERED=1` on *both* `device.py` and the
test (a real device's descriptor is fixed at enumeration, so each mode is a
separate run with a re-started `device.py`). `run.sh` drives the whole matrix.

## Running (Linux)

```sh
sudo -E ./run.sh                      # unnumbered + numbered, native + nusb
HIDRA_CYNTHION_UHUBCTL="-l 9-1.4.4 -p 1,4" sudo -E ./run.sh   # + auto power-cycle
```

## Running (Windows / macOS)

On the control host (Linux), start `device.py` (see `run.sh` for the nix env;
add `HIDRA_CYNTHION_NUMBERED=1` for the numbered pass), then USB-pass the
emulated `1209:000c` device into the VM and, in the VM, run
`cargo test -p cynthion` with `HIDRA_CYNTHION_CTRL=10.0.2.2:9999`, an optional
`HIDRA_BACKEND` (`native`, the default, or `nusb`)
(the host as seen from QEMU user-mode networking) and a matching
`HIDRA_CYNTHION_NUMBERED`. Both VMs read the device through their real OS HID
stack; no virtual-device entitlement or WinUHid is needed (those are for the
*virtual*-device tests).

**USB passthrough gotcha, speed detection.** QEMU's `usb-host` must present the
device to the guest at its true **full speed (12 Mb/s)**. Two things break this:
- **QEMU < 11 misdetects it as low-speed (1.5 Mb/s)** over the Cynthion's nested
  loopback hub chain. macOS/Windows then reject the device (a 64-byte `ep0` is
  illegal for low speed). Use QEMU ≥ 11.
- **libvirt-launched QEMU misdetects it even on 11**, while the *same* QEMU
  binary launched directly (QuickEMU / a plain `qemu-system` command) detects
  12 Mb/s. Cause not fully root-caused, but the fix is: pass the device through a
  **directly-launched** QEMU, not through libvirt.

**Windows** (QuickEMU): add `usb_devices=("1209:000c")` to the `.conf`, make the
usbfs node writable (`sudo chmod 666 /dev/bus/usb/BBB/DDD`), launch, then build +
run in an MSVC env (see `build_win.bat` / `run_win_cyn.bat` in a provisioned
guest: EWDK ISO mounted at `D:`, `SetupBuildEnv.cmd amd64`, LIB/INCLUDE set). If
the device doesn't appear, `pnputil /restart-device` the xHCI controller.

**macOS** (OSX-KVM): the libvirt domain misdetects the speed, so boot the *same*
disks via a **direct** `qemu-system` command instead (see
`OSX-KVM/OpenCore-Boot-cynthion.sh`, QEMU 11, `usb-host` on a dedicated
`qemu-xhci`, hostfwd `:2223→22`). Then run the test binary **as root**
(`IOHIDDeviceOpen` returns `kIOReturnNotPrivileged`, `0xe00002c1`, otherwise).

To power-cycle a wedged Cynthion (Facedancer `LIBUSB_ERROR_TIMEOUT`), use
`uhubctl` on its hub ports rather than repeated `apollo` resets.

A key finding this hardware surfaced: for a **real** USB HID device, Linux's
hidraw returns `get_feature_report`/`get_input_report` **with** the leading
`0x00` report-ID byte (like Windows/macOS), the "bare body" seen with the
`uhid` software device is a uhid-emulation artifact, not real USB behavior.
Because every real backend prepends the report-number byte, the conformance
harness asserts the `[report-number, body]` framing unconditionally (there is
no per-platform opt-out cap); the `uhid` device.py responder was updated to
prepend it too, matching what real hardware does.

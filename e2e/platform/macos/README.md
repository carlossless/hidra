# macOS testing VM

Validates the **IOHIDManager** backend against a real `IOHIDUserDevice`
virtual HID device. Creating that virtual device uses the restricted
`com.apple.developer.hid.virtual.device` entitlement, which the kernel only
honors with AMFI relaxed — so this runs in a VM with SIP and AMFI disabled, not
on a stock machine.

Verified on macOS Sequoia 15.7.7 and Tahoe 26.5.2 (x86_64 VMs) and natively on
macOS 26.5.1 **Apple Silicon (arm64)**.

**Apple Silicon** needs two things, and each is useless without the other:

1. **Permissive Security**, set in Recovery (Startup Security Utility). Check
   with `sudo bputil -d` → `Security Mode: Permissive`, and make sure
   `Boot Args Filtering Status: Disabled` (otherwise boot-args are ignored).
2. `sudo nvram boot-args=amfi_get_out_of_my_way=0x1`, then reboot. Confirm the
   *running* kernel took it: `sysctl -n kern.bootargs`.

On **Intel / VMs**, use `csr-active-config = 0x0FFF` plus the same
`amfi_get_out_of_my_way=1` boot-arg (see below).

> **If the signed binary dies with `Killed: 9`**, AMFI is still enforcing —
> the code is fine. Confirm with:
>
> ```sh
> sudo log show --last 3m --predicate 'process == "kernel" AND eventMessage CONTAINS "AMFI"'
> # AMFI: '<bin>' is adhoc signed.
> # AMFI: bailing out because of restricted entitlements.
> ```
>
> Isolate it in seconds — build `int main(){return 42;}` and run it as root
> three ways: unsigned, ad-hoc signed, and ad-hoc signed **+ entitlement**.
> Only the last one dying means the security config, not your code, is at
> fault. On Intel/VMs that's a partial SIP (`csr-active-config != 0x0FFF`); on
> Apple Silicon it's a missing Permissive Security or boot-arg.
>
> Beware a **false pass**: if the entitlements plist is missing (e.g. it lived
> in `/tmp`, which is wiped on reboot), `codesign` fails and the binary runs
> *unentitled* and succeeds. Always check
> `codesign -d --entitlements - <bin> | grep -i hid` before trusting a pass.

## VM configuration gotchas

- **CPU model** must be `Skylake-Client` (drop `-hle,-rtm`) for Sequoia. Older
  models such as `Penryn` panic on boot (no AVX2).
- **CPU topology** must be `sockets=1 cores=2 threads=2`. A bare `<vcpu>4</vcpu>`
  becomes 4 sockets and macOS spins forever at boot.
- **Video** must be `vmware-svga`; virtio/plain VGA give "display output not active".
- The host kernel needs `kvm.ignore_msrs=1`.
- Use **VNC, not SPICE** for the console (macOS has no SPICE agent, so SPICE
  gives no input passthrough).

## Disabling SIP / AMFI

SIP and AMFI cannot be changed from inside a running macOS — with NVRAM
Protections on, `sudo nvram csr-active-config=…` fails with
`(iokit/common) not permitted`. Edit the OpenCore `EFI/OC/config.plist`
**offline**, with the VM shut down (back the image up first):

```sh
sudo modprobe nbd max_part=8
sudo qemu-nbd --connect=/dev/nbd0 /path/to/OpenCore.qcow2
sudo mount /dev/nbd0p1 /mnt/oc     # the EFI partition holds EFI/OC/config.plist
```

In the plist, under `NVRAM`, GUID `7C436110-AB2A-4BBB-A880-FE41995C9F82`:

- `Add` → `csr-active-config` = `FF0F0000` (0x0FFF little-endian; **data**, not string),
- `Add` → `boot-args` contains `amfi_get_out_of_my_way=1`,
- **`Delete` → append `csr-active-config`.** This is the step people miss:
  OpenCore's `Add` only applies when the variable is *absent*, so without a
  `Delete` entry a stale value already in NVRAM survives every boot and your
  edit appears to do nothing.
- ensure `WriteFlash` is `true`.

Then `sync`, `umount`, `sudo qemu-nbd --disconnect /dev/nbd0`, and boot. Verify
in the guest: `nvram csr-active-config` → `%ff%0f%00%00`, and `csrutil status`
should report *Kext Signing* and *NVRAM Protections* as `disabled`.

## Code-signing the test binary

Creating a virtual HID device requires the binary to carry the virtual-device
entitlement ([`hid-virtual-device.entitlements`](hid-virtual-device.entitlements):
a single `com.apple.developer.hid.virtual.device` = `true`). Ad-hoc signing is
sufficient on an AMFI-off system.

Build the test binary (`cargo test -p macos --no-run` from `e2e/`), sign it,
then run it as root:

```sh
codesign -s - --entitlements e2e/platform/macos/hid-virtual-device.entitlements --force ./the-test-binary
sudo ./the-test-binary --test-threads=1
```

The flake also builds and ad-hoc-signs this binary reproducibly:
`nix build .#macos-virtual-signed`.

- **Root is required** even just to *open* a HID device (`IOHIDDeviceOpen`
  returns `kIOReturnNotPrivileged`, `0xe00002c1`, otherwise). Reading a device
  needs only root; *creating* a virtual device additionally needs the signed
  entitlement above.
- `IOHIDUserDevice` GET/SET report handlers are only serviced through the
  dispatch-queue lifecycle (`SetDispatchQueue` + `Activate`, teardown via
  `Cancel`); run-loop scheduling alone leaves the handlers silent.

## Passing through real USB hardware (optional)

If you instead pass a *real* USB HID device into this VM, launch QEMU
**directly** rather than through libvirt: libvirt-launched QEMU misdetects the
device speed (full-speed 12 Mb/s shows as low-speed 1.5 Mb/s), which macOS
rejects. Use QEMU ≥ 11 with a `usb-host` device on a dedicated `qemu-xhci`, and
run the test as root (no entitlement needed to *read* a device).

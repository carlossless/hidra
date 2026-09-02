# Windows testing VM

Validates the **Windows HID backend** (`hid.dll` + SetupAPI) against a
userland-created virtual HID device via [WinUHid](https://github.com/cgutman/WinUHid)
— a Windows analog of Linux `uhid`. The test loads `WinUHid.dll` at runtime,
creates the virtual device, services its input/output/feature events on a thread,
and hidra enumerates and drives it.

The reference environment is codified in Nix with
[wfvm](https://git.m-labs.hk/m-labs/wfvm), which builds a reproducible, fully
automated Windows 11 install under QEMU/KVM. The build, its COW-layer structure,
and the pinned downloads (VC++ RT, rustup, VS Build Tools, WDK, WinUHid rev + all
sha256s) live in [`test-vm.nix`](test-vm.nix); the provisioning gotchas are
documented inline in [`provision/winuhid.ps1`](provision/winuhid.ps1) and
[`provision/toolchain.ps1`](provision/toolchain.ps1). It deliberately avoids the
~12 GB EWDK ISO (MSVC comes from VS Build Tools, the driver bits from the WDK), so
**the Windows 11 ISO is the only ISO you need**.

## Prerequisites

- **`x86_64-linux` host with `/dev/kvm`** and a Nix that exposes the `kvm` system
  feature (wfvm builds the image by running QEMU inside the build).
- **The Windows 11 ISO added to the store** (an unfree `requireFile`, pinned in
  `test-vm.nix` — currently `Win11_25H2_EnglishInternational_x64_v2.iso`):
  ```sh
  nix-store --add-fixed sha256 Win11_25H2_EnglishInternational_x64_v2.iso
  ```
  Override `windowsImage` (name + sha256) and `locale` for a different ISO — the
  locale **must** match the ISO language or Windows Setup stalls at the language
  picker.
- **`export NIXPKGS_ALLOW_UNFREE=1`** — required to evaluate the ISO derivation.
  Flakes only read it under `--impure`, so every command below passes that.
- **`--option sandbox relaxed`** for anything at/above the toolchain layer: that
  stage runs `__noChroot` (needs the network to download VS Build Tools + the WDK,
  so it needs a trusted user). The base and WinUHid stages run fully sandboxed.

## Build & run

```sh
export NIXPKGS_ALLOW_UNFREE=1
nix build --impure .#windows-test-vm-base                        # smoke-test the wfvm install (offline, sandboxed)
nix build --impure --option sandbox relaxed .#windows-test-vm    # full image (toolchain boot needs the network)
nix run   --impure --option sandbox relaxed .#windows-test-vm-run # boot snapshot, run the conformance test over SSH
```

Iterating the driver build is cheap: the toolchain layer is cached, so only the
last, offline WinUHid stage re-runs.

## Why WinUHid (not a hand-rolled driver)

A from-scratch KMDF VHF driver installs and loads but builds no HID collection,
because a hand-written INF with `HKR,,"LowerFilters",…,"vhf"` is insufficient.
WinUHid's INF uses `Needs=vhfservice.NT` (the inbox VHF include), the correct
wiring.

## Manual reference / troubleshooting

The automation above encodes these steps; keep them for debugging a failed
provisioning boot or driving a VM by hand (e.g. a QuickEMU Win11 guest).

### Toolchain

- **EWDK ISO** (Enterprise WDK; matches the target OS build, e.g. 26100), mounted
  as a drive. Lighter alternative: `Microsoft.Windows.WDK.x64` NuGet + VS Build
  Tools.
- Rust **MSVC** toolchain (the GNU toolchain needs an external `dlltool`).
- Enable test signing: `bcdedit /set testsigning on` (reboot).

### Building and installing WinUHid

1. Enter the build environment: `call <EWDK>\BuildEnv\SetupBuildEnv.cmd`.
2. Build the driver and the loader library, e.g.:
   ```
   msbuild "WinUHid Driver\WinUHid Driver.vcxproj" /p:Configuration=Release /p:Platform=x64 /p:WindowsTargetPlatformVersion=10.0.26100.0
   ```
   plus the `WinUHid` (lib) target. Skip the RootDevCA/UnitTests projects (they
   pull NuGet/vcpkg).
3. Sign and install: create a self-signed CodeSigning cert, `certutil -f -addstore
   Root` and `-addstore TrustedPublisher`, `signtool sign` the `.dll`, `inf2cat`,
   then `signtool sign` **only** the `.cat` (never re-touch the `.dll` after
   cataloging — it breaks the catalog's hash and Windows silently rejects the
   package), then `devcon install WinUHidDriver.inf "Root\WinUHid"` (`devcon` lives
   under the WDK `Tools\` dir, not `bin\`). The device appears as "WinUHid Virtual
   HID Enumerator" (`ROOT\SYSTEM\xxxx`, Class = System).

### Building and running the hidra test

`cargo test -p windows` from the [`e2e/`](../../..) workspace, in an **x64 MSVC**
environment.

- `SetupBuildEnv.cmd amd64` provides the tools and `INCLUDE` but leaves `LIB`
  short — also set:
  ```
  set LIB=<Kits>\Lib\10.0.26100.0\um\x64;<Kits>\Lib\10.0.26100.0\ucrt\x64;<VC>\lib\x64;%LIB%
  ```
  (`vcvars64.bat` fails here because the EWDK SDK is not registry-registered.)
- The runtime needs the **VC++ redistributable**
  (`aka.ms/vs/17/release/vc_redist.x64.exe`): a fresh Windows 11 lacks
  `vcruntime140.dll`, which both the MSVC test binary and `WinUHid.dll` need.
- Run the test in the **same** MSVC environment it was built in (the build scripts
  key off `VCINSTALLDIR`; a different env re-triggers build scripts).

### Manual VM provisioning (QuickEMU, pre-wfvm)

- QuickEMU Windows 11 Pro; enable OpenSSH in the guest. (wfvm enables OpenSSH
  automatically — user `wfvm`, host port 2022.)
- The default disk (~16 GB) is too small: stop the VM, `qemu-img resize disk.qcow2
  96G`, relaunch, `Resize-Partition` C: to max. (wfvm sizes to 96 GB up front.)

### Passing through real USB hardware (optional)

QuickEMU's directly-launched QEMU 11 detects a real device's full-speed correctly,
so a real USB HID device can be passed through with `usb_devices=("VVVV:PPPP")` in
the `.conf` and read through the guest's normal HID stack (no WinUHid needed to
read a real device).

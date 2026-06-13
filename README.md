# hidra

A pure-Rust HID library, with a complete blocking HID API, a WebHID backend
for WebAssembly, and standalone HID report-descriptor primitives.

No C library is linked. Every backend talks to the operating system directly:

| Platform | Backend | Notes |
|----------|---------|-------|
| Linux | `hidraw` device nodes, sysfs enumeration | no libudev dependency |
| Windows | `hid.dll` + SetupAPI (via `windows-sys` declarations) | |
| macOS | IOHIDManager (direct framework FFI) | |
| any native OS | raw USB transfers via [nusb](https://docs.rs/nusb) | optional `nusb` feature, a pure-Rust USB transport |
| WebAssembly | [WebHID](https://wicg.github.io/webhid/) via `web-sys` | async API in `hidra::webhid` |

## Quick start

```rust
let api = hidra::HidApi::new()?;
for dev in api.device_list() {
    println!("{:04x}:{:04x} {}", dev.vendor_id(), dev.product_id(),
             dev.product_string().unwrap_or("<unnamed>"));
}

let device = api.open(0x046d, 0xc216)?;
device.write(&[0x00, 0x01, 0x02])?;        // report ID 0 + payload
let mut buf = [0u8; 64];
let len = device.read_timeout(&mut buf, 1000)?;
```

See `examples/` for runnable versions (`cargo run --example enumerate`).

## Async

Input reads, the one HID operation that actually waits, are also available
as runtime-agnostic futures on every backend, in the same spirit as
[nusb](https://docs.rs/nusb): plain `Waker` wake-ups backed by OS readiness
(a `poll(2)` reactor on Linux, overlapped-event waits on Windows, the
IOHIDManager callback queue on macOS, nusb itself for the `nusb` feature). No
tokio/async-std dependency; the futures run under any executor.

```rust,ignore
let len = device.read_async(&mut buf).await?;   // never 0; resolves per report
```

- `read_async` never returns `Ok(0)`; for timeouts use your runtime's
  combinator (e.g. `tokio::time::timeout`). On unplug it fails with
  `HidError::Disconnected`.
- Futures are cancel-safe: dropping one never loses a report; pending input
  stays queued for the next read.
- Writes and feature reports remain blocking by design: they are synchronous
  kernel calls on every OS (there is no async primitive for them); they
  complete quickly and hidapi treats them identically. On wasm32 everything
  is async via `hidra::webhid`.

`cargo run --example read_async` shows the futures driven by a 20-line
hand-rolled executor.

## API

Buffer and report-ID conventions follow hidapi's (`data[0]` is the report ID
for writes and feature reports, etc.). Equivalents for common hidapi
functions:

| hidapi | hidra |
|--------|-------|
| `hid_init` / `hid_exit` | `HidApi::new()` / drop (no global state) |
| `hid_enumerate` / `hid_free_enumeration` | `HidApi::device_list()`, `HidApi::enumerate(vid, pid)` |
| `hid_open` | `HidApi::open(vid, pid)`, `HidApi::open_serial(vid, pid, serial)` |
| `hid_open_path` | `HidApi::open_path(path)` |
| `hid_close` | drop the `HidDevice` |
| `hid_write` | `HidDevice::write` |
| `hid_read` / `hid_read_timeout` | `HidDevice::read` / `read_timeout` |
| `hid_set_nonblocking` | `HidDevice::set_blocking_mode` (inverted, no double negative) |
| `hid_send_feature_report` / `hid_get_feature_report` | `send_feature_report` / `get_feature_report` |
| `hid_get_input_report` | `HidDevice::get_input_report` |
| `hid_get_manufacturer_string` / `..._product_string` / `..._serial_number_string` | `get_manufacturer_string` / `get_product_string` / `get_serial_number_string` |
| `hid_get_indexed_string` | `get_indexed_string` (hidraw backend: unsupported, like hidapi; `nusb` backend: supported) |
| `hid_get_report_descriptor` | `get_report_descriptor` (+ `report_descriptor()` / `parsed_report_descriptor()` conveniences) |
| `hid_get_device_info` | `HidDevice::get_device_info` |
| `hid_error` | typed `HidError` on every `Result` |
| `hid_version` / `hid_version_str` | `hidra::version()` / `version_str()` |
| `hid_darwin_set_open_exclusive` / `..._get_...` | `HidApi::set_open_exclusive` / `open_exclusive` (macOS) |
| `hid_winapi_get_container_id` | `HidDevice::container_id()` (Windows) |
| `hid_winapi_set_write_timeout` | `HidDevice::set_write_timeout()` (Windows) |
| `hid_libusb_wrap_sys_device` | not applicable, use the `nusb` feature backend |

## USB backend (`nusb` feature)

The `nusb` feature provides a USB-transport backend built on
[nusb](https://docs.rs/nusb): pure Rust, no libusb.

```toml
hidra = { version = "0.1", features = ["nusb"] }
```

```rust
let api = hidra::usb::UsbHidApi::new()?;
let device = api.open(0x16c0, 0x27dd, None)?;
```

`UsbHidDevice` has the same method surface as `HidDevice`. Use it when you
need raw USB access (kernel-driver detach on Linux, indexed string
descriptors) instead of the OS HID stack. Like hidapi-libusb it claims the
interface away from the OS driver and needs appropriate permissions (udev
rules on Linux).

## WebHID (wasm32)

On `wasm32-unknown-unknown` the `hidra::webhid` module provides an async API
over WebHID:

```rust,ignore
let api = hidra::webhid::WebHidApi::new()?;
// must be called from a user gesture:
let devices = api.request_device(&[
    hidra::webhid::DeviceFilter::new().vendor_id(0x046d),
]).await?;
let device = &devices[0];
device.open().await?;
device.write(&[0x00, 0x01, 0x02]).await?;

let mut reports = device.start_reading();
let report = reports.read().await?;
```

WebHID requires a secure context, browser support (Chromium-based browsers),
and a user gesture for `request_device`. Because browsers expose parsed
collections rather than descriptor bytes, `WebHidDevice::report_descriptor()`
*reconstructs* a descriptor from the collection data, it parses back to the
same reports/usages even though it is not byte-identical to the original.

Builds need the `web_sys_unstable_apis` cfg until WebHID stabilizes in
web-sys; this repository's `.cargo/config.toml` shows the required rustflags.

## Report descriptor primitives

`hidra::descriptor` works on every target, independent of any device:

- `Items`, zero-copy lexer over raw descriptor items
- `ReportDescriptor::parse`, collection tree + every report with field
  offsets, usages, logical ranges, and computed sizes
- `DescriptorBuilder`, emit descriptor bytes (used for the WebHID and
  Windows reconstructions, handy for tests and emulated devices)

`hidra::report_info` adds WebHID-style `CollectionInfo`/`ReportInfo` types
and `reconstruct_descriptor`, also usable outside the browser.

```rust
use hidra::descriptor::{ReportDescriptor, ReportKind};

let parsed = ReportDescriptor::parse(&bytes)?;
println!("max input report: {} bytes", parsed.max_report_size(ReportKind::Input));
for report in &parsed.reports {
    println!("{:?} id={:?}: {} fields", report.kind, report.report_id, report.fields.len());
}
```

## Nix

The repo is a flake. `nix develop` drops you into a shell with the Rust
toolchain; `nix build` compiles the crate with the `nusb` feature, runs the
test suite, and installs the example binaries; `nix flake check` adds a
rustfmt check on top.

```sh
nix build                                  # current system
nix build .#checks.aarch64-darwin.build    # test-build for another system
                                           # (dispatched to remote builders)
```

## Platform notes

- **Linux**: opening `/dev/hidraw*` requires permissions, typically a udev
  rule like `KERNEL=="hidraw*", ATTRS{idVendor}=="046d", TAG+="uaccess"`.
  `get_input_report` needs Linux ≥ 5.11.
- **Windows**: the OS never exposes raw report descriptors;
  `get_report_descriptor` reconstructs one from preparsed data (as hidapi
  does), using only documented HidP APIs.
- **macOS**: keyboards/mice are claimed by the system; reading them requires
  Input Monitoring permission.

## License

Licensed under the [MIT license](LICENSE-MIT).

# hidra

A pure-Rust HID library with a unified async API (with blocking `.wait()` on
native targets, like nusb), a WebHID backend for WebAssembly, and standalone
HID report-descriptor primitives.

No C library is linked. One `Hidra` / `HidDevice` regardless of backend:

| Platform | Backend | Notes |
|----------|---------|-------|
| Linux | `hidraw` device nodes, sysfs enumeration | no libudev dependency |
| Windows | `hid.dll` + SetupAPI (via `windows-sys` declarations) | |
| macOS | IOHIDManager (direct framework FFI) | |
| any platform [nusb](https://docs.rs/nusb) supports | raw USB transfers via nusb | optional `nusb` feature, swaps in a pure-Rust USB transport |
| WebAssembly | [WebHID](https://wicg.github.io/webhid/) via `web-sys` | same `Hidra`/`HidDevice`, await-only |

## Quick start

Every I/O method returns a future. On native, bring [`MaybeFuture`] into scope
and call `.wait()` to run it blocking:

```rust
use hidra::MaybeFuture;

let api = hidra::Hidra::new()?;
for dev in api.device_list() {
    println!("{:04x}:{:04x} {}", dev.vendor_id(), dev.product_id(),
             dev.product_string().unwrap_or("<unnamed>"));
}

let device = api.open(0x046d, 0xc216).wait()?;
device.write(&[0x00, 0x01, 0x02]).wait()?;        // report ID 0 + payload
let mut buf = [0u8; 64];
let len = device.read(&mut buf).wait()?;          // one input report
```

See `examples/` for runnable versions (`cargo run --example enumerate`).

## Async and blocking

Following nusb's design, every `Hidra` / `HidDevice` method returns an
`impl Future`. Drive it either way:

- `.await` it in any async runtime (the futures are runtime-agnostic: plain
  `Waker` wake-ups, no tokio/async-std dependency).
- `.wait()` it to block the current thread (a tiny built-in executor). This
  is the `MaybeFuture` extension trait, available on native targets only;
  `wasm32` cannot block, so there you must `.await`.

```rust,ignore
let len = device.read(&mut buf).await?;          // async
let len = device.read(&mut buf).wait()?;         // blocking (native)
```

Input reads genuinely wait on the OS (a `poll(2)` reactor on Linux,
overlapped-event waits on Windows, the IOHIDManager callback queue on macOS,
nusb's own I/O with the `nusb` feature, `inputreport` events on WebHID).
`read` resolves with exactly one input report (never empty); for a timeout
use your runtime's combinator (e.g. `tokio::time::timeout`). On unplug it
fails with `HidError::Disconnected`, and the read future is cancel-safe:
dropping it never loses a report. Writes and feature reports complete
promptly; their futures simply run the synchronous OS call when polled.

[`MaybeFuture`]: https://docs.rs/hidra/latest/hidra/trait.MaybeFuture.html

## API

Buffer and report-ID conventions follow hidapi's (`data[0]` is the report ID
for writes and feature reports, etc.); every method below returns a future
(`.await` or, on native, `.wait()`). Equivalents for common hidapi functions:

| hidapi | hidra |
|--------|-------|
| `hid_init` / `hid_exit` | `Hidra::new()` / drop (no global state) |
| `hid_enumerate` / `hid_free_enumeration` | `Hidra::device_list()`, `Hidra::enumerate(vid, pid)` |
| `hid_open` | `Hidra::open(vid, pid)`, `Hidra::open_serial(vid, pid, serial)` |
| `hid_open_path` | `Hidra::open_path(path)` |
| `hid_close` | drop the `HidDevice` |
| `hid_write` | `HidDevice::write` |
| `hid_read` / `hid_read_timeout` | `HidDevice::read` (one report; use a runtime timeout combinator for deadlines) |
| `hid_send_feature_report` / `hid_get_feature_report` | `send_feature_report` / `get_feature_report` |
| `hid_get_input_report` | `HidDevice::get_input_report` (native) |
| `hid_get_manufacturer_string` / `..._product_string` / `..._serial_number_string` | `get_manufacturer_string` / `get_product_string` / `get_serial_number_string` |
| `hid_get_indexed_string` | `get_indexed_string` (hidraw backend: unsupported, like hidapi; `nusb` backend: supported) |
| `hid_get_report_descriptor` | `get_report_descriptor` (+ `report_descriptor()` / `parsed_report_descriptor()` conveniences) |
| `hid_get_device_info` | `HidDevice::get_device_info` |
| `hid_error` | typed `HidError` on every `Result` |
| `hid_version` / `hid_version_str` | `hidra::version()` / `version_str()` |
| `hid_darwin_set_open_exclusive` / `..._get_...` | `Hidra::set_open_exclusive` / `open_exclusive` (macOS) |
| `hid_winapi_get_container_id` | `HidDevice::container_id()` (Windows) |
| `hid_winapi_set_write_timeout` | `HidDevice::set_write_timeout()` (Windows) |
| `hid_libusb_wrap_sys_device` | not applicable, enable the `nusb` feature |

`hid_set_nonblocking` has no equivalent: `read` is always the async "next
report" operation, so blocking versus non-blocking is just `.wait()` versus
`.await` plus your own timeout.

## USB backend (`nusb` feature)

Enabling the `nusb` feature swaps the per-OS native backend for a pure-Rust
USB transport built on [nusb](https://docs.rs/nusb) (no libusb), behind the
same `Hidra` / `HidDevice` (there is no separate type). It runs on whatever
platforms nusb itself supports:

```toml
hidra = { version = "0.1", features = ["nusb"] }
```

Use it for raw USB access (kernel-driver detach on Linux, indexed string
descriptors) instead of the OS HID stack. Like hidapi-libusb it claims the
interface away from the OS driver and needs appropriate permissions (udev
rules on Linux).

It is especially useful when a USB device is not exposed as a HID device by
the OS (so the native HID backend can't see it) but the device's interface is
still a HID one. For example, an interface with no endpoints is ignored by
the Linux `usbhid` driver and never appears as a `hidraw` device; the nusb
backend talks to it over raw USB regardless.

## WebHID (wasm32)

On `wasm32-unknown-unknown` the same `hidra::Hidra` / `hidra::HidDevice` types
are backed by WebHID. There is no blocking mode, so always `.await` their
futures (no `.wait()`), and discovery is WebHID-shaped:

```rust,ignore
let api = hidra::Hidra::new()?;
// must be called from a user gesture:
let devices = api.request_device(&[
    hidra::DeviceFilter::new().vendor_id(0x046d),
]).await?;
let device = &devices[0];
device.open().await?;
device.write(&[0x00, 0x01, 0x02]).await?;

let mut buf = [0u8; 64];
let len = device.read(&mut buf).await?;
```

WebHID requires a secure context, browser support (Chromium-based browsers),
and a user gesture for `request_device`. Because browsers expose parsed
collections rather than descriptor bytes, `HidDevice::report_descriptor()`
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

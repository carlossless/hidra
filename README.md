# hidra

[![CI](https://github.com/carlossless/hidra/actions/workflows/ci.yml/badge.svg)](https://github.com/carlossless/hidra/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hidra.svg)](https://crates.io/crates/hidra)
[![docs.rs](https://docs.rs/hidra/badge.svg)](https://docs.rs/hidra)

A pure-Rust HID library with a unified async API (with blocking `.wait()` on
native targets, like nusb), WebHID and WebUSB backends for WebAssembly, and
standalone HID report-descriptor primitives.

No C library is linked. One `Hidra` / `HidDevice` regardless of backend:

| Platform | `Backend::Native` | Notes |
|----------|-------------------|-------|
| Linux | `hidraw` device nodes, sysfs enumeration | no libudev dependency |
| Windows | `hid.dll` + SetupAPI (via `windows-sys` declarations) | |
| macOS | IOHIDManager (direct framework FFI) | |
| Browsers | [WebHID](https://wicg.github.io/webhid/) via `web-sys` | same `Hidra`/`HidDevice`, await-only; no `Backend` |
| Browsers | [WebUSB](https://wicg.github.io/webusb/) via nusb | `hidra::webusb`, for devices WebHID will not expose |

`Backend::Nusb` adds raw USB transfers via [nusb](https://docs.rs/nusb) on
Linux, macOS and Windows; on `wasm32` the same feature adds the WebUSB backend
— see [Backends](#backends).

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

## Backends

Native builds carry two backends, and `Backend` picks between them **at run
time**, per `Hidra` instance:

| `Backend` | Talks to | Available when |
|-----------|----------|----------------|
| `Native` (default) | the OS HID stack: hidraw, `hid.dll`, IOHIDManager | Linux, Windows, macOS |
| `Nusb` | raw USB interrupt/control transfers, bypassing the OS HID stack | the `nusb` feature is on, and the target is Linux, macOS or Windows |

```rust
use hidra::{Backend, Hidra};

// Prefer the OS HID stack; fall back to raw USB where it has no node for the
// device, or refuses access to it.
let api = match Hidra::builder().backend(Backend::Native).build() {
    Ok(api) => api,
    Err(_) => Hidra::builder().backend(Backend::Nusb).build()?,
};
```

`Backend` implements `Display` and `FromStr`, so it can come straight from a
flag or an environment variable — `HIDRA_BACKEND=nusb cargo run --features nusb
--example enumerate`. Ask `Backend::is_available()` (or list
`Backend::available()`) before selecting; a backend this build does not have
returns `HidError::Unsupported` instead of falling back silently.

Pick `Nusb` when the OS HID stack has no node for a device, restricts access to
it, or has to be taken out of the way; `get_indexed_string` also needs it.
Note that it sees **USB devices only**, opening one **claims the whole USB
interface** away from the OS driver until the handle is dropped, and it needs
raw-USB permissions (udev rules for `/dev/bus/usb` on Linux, a WinUSB-compatible
driver on Windows). The per-OS extensions (`Hidra::set_open_exclusive` on macOS,
`HidDevice::container_id` / `set_write_timeout` on Windows) belong to `Native`
and report `Unsupported` under `Nusb`.

The `nusb` feature is additive: it compiles the second backend in beside the
first rather than replacing it, so enabling it anywhere in a dependency graph
cannot change which backend another crate ends up using.

### WebUSB

On `wasm32` the `nusb` feature adds [`hidra::webusb`], a second browser backend.
It is not an alternative to WebHID but a complement: WebHID only surfaces
interfaces the host recognises as HID, while Blink refuses `claimInterface` on
the [protected classes](https://groups.google.com/a/chromium.org/g/blink-dev/c/LZXocaeCwDw),
HID among them. The two reach disjoint sets of devices:

| interface class | WebHID | WebUSB |
|-----------------|--------|--------|
| HID (0x03) | yes | no — `SecurityError` |
| vendor-specific (0xFF) | no — invisible | yes |

So a device that declares a vendor-specific class, yet still speaks the HID
protocol on the wire, is reachable only through `hidra::webusb`. It has its own
`Hidra` / `HidDevice` (browser APIs are permission-gated and chooser-driven, so
there is no enumerate or open-by-vid-pid), and unlike the native `nusb` backend
it does not filter for HID-class interfaces.

```rust,ignore
use hidra::webusb::{DeviceSelector, Hidra};

let api = Hidra::new()?;
let device = api
    .request_device(&[DeviceSelector::all().with_vid_pid(0x0603, 0x1020)])
    .await?
    .ok_or("no device selected")?;
let handle = device.open(None).await?;   // None: first interface
handle.send_feature_report(&report).await?;
```

[`hidra::webusb`]: https://docs.rs/hidra/latest/wasm32-unknown-unknown/hidra/webusb/index.html

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
nusb's own I/O on `Backend::Nusb`, `inputreport` events on WebHID).
`read` resolves with exactly one input report (never empty); for a timeout
use your runtime's combinator (e.g. `tokio::time::timeout`). On unplug it
fails with `HidError::Disconnected`, and the read future is cancel-safe:
dropping it never loses a report. Writes and feature reports complete
promptly; their futures simply run the synchronous OS call when polled.

[`MaybeFuture`]: https://docs.rs/hidra/latest/hidra/trait.MaybeFuture.html

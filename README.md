# hidra

[![CI](https://github.com/carlossless/hidra/actions/workflows/ci.yml/badge.svg)](https://github.com/carlossless/hidra/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hidra.svg)](https://crates.io/crates/hidra)
[![docs.rs](https://docs.rs/hidra/badge.svg)](https://docs.rs/hidra)

A pure-Rust HID library: one async API across Linux, Windows, macOS and the
browser, with blocking `.wait()` on native targets, plus standalone HID
report-descriptor primitives.

| Platform | `Backend::Native` | Notes |
|----------|-------------------|-------|
| Linux | `hidraw` device nodes, sysfs enumeration | no libudev dependency |
| Windows | `hid.dll` + SetupAPI (via `windows-sys` declarations) | |
| macOS | IOHIDManager (direct framework FFI) | |
| Browsers | [WebHID](https://wicg.github.io/webhid/) via `web-sys` | same types, await-only; no `Backend` |

`Backend::Nusb` adds raw USB transfers via [nusb](https://docs.rs/nusb) on the
three native targets — see [Backends](#backends).

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

Native builds carry both, and `Backend` picks between them **at run time**, per
`Hidra` instance:

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

`Backend` implements `Display` and `FromStr` (`HIDRA_BACKEND=nusb`), and
`Backend::is_available()` reports whether this build has one — an absent backend
errors rather than falling back silently.

Reach for `Nusb` when the OS HID stack has no node for a device, restricts it,
or must be taken out of the way; `get_indexed_string` needs it too. It sees
**USB devices only**, **claims the whole interface** while open, and needs
raw-USB permissions (udev rules on Linux, a WinUSB-compatible driver on
Windows). `Native`-only extensions (`set_open_exclusive`, `container_id`,
`set_write_timeout`) report `Unsupported` under it. The feature is additive, so
enabling it never changes which backend another crate gets.

## Async and blocking

Following nusb's design, those futures drive either way:

- `.await` in any async runtime — runtime-agnostic (plain `Waker` wake-ups, no
  tokio/async-std dependency).
- `.wait()` to block the current thread, via the `MaybeFuture` extension trait.
  Native only; `wasm32` cannot block, so there you must `.await`.

```rust,ignore
let len = device.read(&mut buf).await?;          // async
let len = device.read(&mut buf).wait()?;         // blocking (native)
```

Reads genuinely wait on the OS (`poll(2)` on Linux, overlapped events on
Windows, the IOHIDManager queue on macOS, nusb's I/O under `Nusb`,
`inputreport` on WebHID). `read` resolves with exactly one report, never empty,
and is cancel-safe: dropping the future never loses one. On unplug it fails
with `HidError::Disconnected`; for a timeout use your runtime's combinator.
Writes and feature reports run the synchronous OS call when polled.

[`MaybeFuture`]: https://docs.rs/hidra/latest/hidra/trait.MaybeFuture.html

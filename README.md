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

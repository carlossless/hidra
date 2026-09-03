//! Backend traits and backend selection.
//!
//! Every native backend implements [`HidBackend`] and [`HidDeviceBackend`].
//! Which one a [`crate::Hidra`] talks to is chosen at run time rather than at
//! compile time: [`dispatch`] holds one enum variant per backend compiled into
//! the build, and the `native` module in `lib.rs` is written against that enum
//! and the traits alone.
//!
//! Backends are compiled in additively. The per-OS backend for the target is
//! always available; the `nusb` feature *adds* the USB-transport backend
//! beside it instead of replacing it, so enabling the feature anywhere in a
//! dependency graph cannot silently change another crate's backend.

#[cfg(not(target_arch = "wasm32"))]
use core::future::Future;

#[cfg(not(target_arch = "wasm32"))]
use crate::{DeviceInfo, HidError, HidResult};

/// Enumerating and opening HID devices through one implementation.
///
/// `Send + Sync` is required rather than incidental: [`crate::Hidra`] is
/// documented as usable from any thread, and the bound makes that a compile
/// error to break instead of a doc comment to disbelieve.
#[cfg(not(target_arch = "wasm32"))]
pub trait HidBackend: Sized + Send + Sync {
    /// The open-device handle this backend produces.
    type Device: HidDeviceBackend;

    /// Initialize the backend.
    fn new() -> HidResult<Self>;

    /// List connected devices. A `vendor_id` or `product_id` of 0 is a
    /// wildcard. Devices with several top-level collections yield one entry
    /// per collection, like hidapi.
    fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>>;

    /// Open a device by its platform path, as reported by [`DeviceInfo::path`].
    fn open_path(&self, path: &str) -> HidResult<Self::Device>;

    /// Open the first device matching `vendor_id`/`product_id` and, when
    /// given, `serial`.
    ///
    /// Every backend resolves this the same way, so it is provided here;
    /// a backend with a cheaper lookup may still override it.
    fn open(
        &self,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> HidResult<Self::Device> {
        let info = self
            .enumerate(vendor_id, product_id)?
            .into_iter()
            .find(|info| match serial {
                Some(serial) => info.serial_number.as_deref() == Some(serial),
                None => true,
            })
            .ok_or(HidError::DeviceNotFound)?;
        self.open_path(&info.path)
    }
}

/// One open HID device.
///
/// Buffer conventions are hidapi's, and identical across backends:
///
/// * `write` / `send_feature_report`: `data[0]` is the report ID; use 0 when
///   the device has no numbered reports. The ID byte counts toward the
///   returned length.
/// * `get_feature_report` / `get_input_report`: `buf[0]` must contain the
///   report ID on entry; on return the buffer starts with that ID.
#[cfg(not(target_arch = "wasm32"))]
pub trait HidDeviceBackend: Send + Sync {
    /// The future [`read_async`](Self::read_async) returns.
    ///
    /// Named rather than `impl Future` so a caller holding several backends
    /// can name it; `Unpin` lets an enum over them project to a variant
    /// without unsafe. Every backend's read future is a plain struct borrowing
    /// the device and the caller's buffer, so the bound costs nothing.
    type Read<'a>: Future<Output = HidResult<usize>> + Send + Unpin + 'a
    where
        Self: 'a;

    /// Send an output report.
    fn write(&self, data: &[u8]) -> HidResult<usize>;

    /// Resolve with one input report copied into `buf`, prefixed with its
    /// report ID only when the device uses numbered reports.
    ///
    /// Never resolves with `Ok(0)`, and fails with [`HidError::Disconnected`]
    /// once the device is gone and any queued reports have drained.
    /// Implementations must be cancel-safe: dropping the future may not lose
    /// an already-delivered report, it stays queued for the next read.
    /// Wake-ups are runtime-agnostic (raw `Waker`s, no executor assumed).
    fn read_async<'a>(&'a self, buf: &'a mut [u8]) -> Self::Read<'a>;

    /// Send a feature report.
    fn send_feature_report(&self, data: &[u8]) -> HidResult<()>;

    /// Read a feature report.
    fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize>;

    /// Read an input report synchronously.
    fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize>;

    /// Manufacturer string, if the device reports one.
    fn get_manufacturer_string(&self) -> HidResult<Option<String>>;

    /// Product string, if the device reports one.
    fn get_product_string(&self) -> HidResult<Option<String>>;

    /// Serial number string, if the device reports one.
    fn get_serial_number_string(&self) -> HidResult<Option<String>>;

    /// A string from the device's string descriptor table. Backends with no
    /// raw USB access return [`HidError::Unsupported`].
    fn get_indexed_string(&self, index: u32) -> HidResult<Option<String>>;

    /// Raw report descriptor; returns the number of bytes written to `buf`.
    fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize>;

    /// Enumeration-style metadata for this open device.
    fn get_device_info(&self) -> HidResult<DeviceInfo>;
}

/// Strip the leading report-ID byte when it is 0.
///
/// hidapi's convention is that `data[0]` is always the report ID, but a device
/// without numbered reports never sees that byte on the wire: a 0 is a
/// placeholder the transport drops, while a nonzero ID is transmitted as the
/// first payload byte. The USB and `IOKit` backends both need this; hidraw
/// does not, because the kernel consumes the byte itself.
#[cfg(any(
    target_os = "macos",
    all(feature = "nusb", any(target_os = "linux", target_os = "windows"))
))]
pub(crate) fn payload_after_report_id(data: &[u8]) -> &[u8] {
    if data.first() == Some(&0) {
        &data[1..]
    } else {
        data
    }
}

// Shared by the backends whose input reports arrive on a producer thread
// (macOS and nusb); see the module docs for why Windows and hidraw do not.
#[cfg(any(
    target_os = "macos",
    all(feature = "nusb", any(target_os = "linux", target_os = "windows"))
))]
pub(crate) mod queue;

// The WebHID backend on wasm. It does not implement the traits above (WebHID
// is async and permission-gated, so the `web` module in lib.rs drives it
// directly), but it belongs here as a backend.
#[cfg(target_arch = "wasm32")]
pub(crate) mod webhid;

#[cfg(all(
    feature = "nusb",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub(crate) mod nusb;

#[cfg(target_os = "linux")]
pub(crate) mod reactor;

#[cfg(target_os = "linux")]
pub(crate) mod hidraw;

#[cfg(target_os = "windows")]
pub(crate) mod windows;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

// The per-OS backend for this target. A target without one has no `Native`,
// so selecting it is a name resolution error rather than a runtime one.
#[cfg(target_os = "linux")]
pub use hidraw::{HidrawApi as Native, HidrawDevice as NativeDevice};
#[cfg(target_os = "macos")]
pub use macos::{MacApi as Native, MacDevice as NativeDevice};
#[cfg(target_os = "windows")]
pub use windows::{WinApi as Native, WinDevice as NativeDevice};

#[cfg(all(
    feature = "nusb",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub use nusb::{NusbApi as Nusb, NusbDevice};

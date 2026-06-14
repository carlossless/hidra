//! # hidra
//!
//! A pure-Rust HID library.
//!
//! hidra talks to HID devices through native OS interfaces, no C library is
//! linked:
//!
//! | Platform | Backend |
//! |----------|---------|
//! | Linux    | `hidraw` device nodes + sysfs enumeration |
//! | Windows  | `hid.dll` / SetupAPI via `windows-sys` declarations |
//! | macOS    | IOHIDManager via direct framework FFI |
//! | Any (feature `nusb`) | USB interrupt/control transfers via [nusb] |
//! | WebAssembly | [WebHID](https://wicg.github.io/webhid/) via `web-sys` (async, in `webhid`) |
//!
//! Following nusb's model, every [`HidApi`] / [`HidDevice`] I/O method returns
//! an `impl Future`. Bring [`MaybeFuture`] into scope to drive it blocking with
//! `.wait()` on native targets, or `.await` it under any async runtime (no
//! executor dependency, wake-ups are plain `Waker`s like nusb). On `wasm32` the
//! `webhid` module provides an async equivalent, and [`descriptor`] offers
//! report-descriptor primitives that work everywhere.
//!
//! ```no_run
//! # #[cfg(not(target_arch = "wasm32"))] fn demo() -> hidra::HidResult<()> {
//! use hidra::MaybeFuture;
//! let api = hidra::HidApi::new()?;
//! for dev in api.device_list() {
//!     println!("{:04x}:{:04x} {}", dev.vendor_id(), dev.product_id(),
//!              dev.product_string().unwrap_or("<unnamed>"));
//! }
//! let device = api.open(0x046d, 0xc216).wait()?;
//! device.write(&[0x00, 0x01, 0x02]).wait()?; // report ID 0 + payload
//! let mut buf = [0u8; 64];
//! let len = device.read(&mut buf).wait()?;
//! # let _ = len; Ok(()) }
//! ```
//!
//! [nusb]: https://docs.rs/nusb

pub mod descriptor;
mod device_info;
mod error;
pub mod report_info;

pub use device_info::{BusType, DeviceInfo};
pub use error::{HidError, HidResult};

#[cfg(not(target_arch = "wasm32"))]
mod backend;

#[cfg(not(target_arch = "wasm32"))]
mod maybe_future;
#[cfg(not(target_arch = "wasm32"))]
pub use maybe_future::MaybeFuture;

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod test_util;

#[cfg(target_arch = "wasm32")]
pub mod webhid;

/// hidra's version, mirroring `hid_version()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

/// Library version (`hid_version` equivalent).
pub const fn version() -> ApiVersion {
    // Parsed from CARGO_PKG_VERSION_* at compile time.
    const fn parse(s: &str) -> u16 {
        let bytes = s.as_bytes();
        let mut v = 0u16;
        let mut i = 0;
        while i < bytes.len() {
            v = v * 10 + (bytes[i] - b'0') as u16;
            i += 1;
        }
        v
    }
    ApiVersion {
        major: parse(env!("CARGO_PKG_VERSION_MAJOR")),
        minor: parse(env!("CARGO_PKG_VERSION_MINOR")),
        patch: parse(env!("CARGO_PKG_VERSION_PATCH")),
    }
}

/// Library version string (`hid_version_str` equivalent).
pub const fn version_str() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{HidApi, HidDevice};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use core::future::Future;

    use crate::backend::{PlatformApi, PlatformDevice};
    use crate::{DeviceInfo, HidResult};

    /// Entry point to the library; owns backend state and the cached device
    /// list (`hid_init` / `hid_enumerate` equivalents).
    ///
    /// Unlike hidapi there is no global state: create as many instances as
    /// you like, from any thread.
    pub struct HidApi {
        backend: PlatformApi,
        device_list: Vec<DeviceInfo>,
    }

    impl HidApi {
        /// Initialize the backend and enumerate all connected HID devices.
        pub fn new() -> HidResult<Self> {
            let mut api = Self::new_without_enumerate()?;
            api.refresh_devices()?;
            Ok(api)
        }

        /// Initialize the backend without enumerating (cheaper when you only
        /// need [`open_path`](Self::open_path)).
        pub fn new_without_enumerate() -> HidResult<Self> {
            Ok(HidApi {
                backend: PlatformApi::new()?,
                device_list: Vec::new(),
            })
        }

        /// Re-enumerate connected devices, refreshing
        /// [`device_list`](Self::device_list).
        pub fn refresh_devices(&mut self) -> HidResult<()> {
            self.device_list = self.backend.enumerate(0, 0)?;
            Ok(())
        }

        /// The cached device list from the last enumeration.
        pub fn device_list(&self) -> impl Iterator<Item = &DeviceInfo> {
            self.device_list.iter()
        }

        /// Enumerate devices matching `vendor_id`/`product_id` directly from
        /// the OS (`hid_enumerate(vid, pid)` equivalent; `0` is a wildcard).
        /// Does not touch the cached list.
        pub fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
            self.backend.enumerate(vendor_id, product_id)
        }

        /// Open the first device matching `vendor_id`/`product_id`
        /// (`hid_open` with a null serial).
        pub fn open(
            &self,
            vendor_id: u16,
            product_id: u16,
        ) -> impl Future<Output = HidResult<HidDevice>> + '_ {
            crate::maybe_future::Blocking::new(move || {
                Ok(HidDevice {
                    backend: self.backend.open(vendor_id, product_id, None)?,
                })
            })
        }

        /// Open the device matching `vendor_id`/`product_id` and serial
        /// number (`hid_open` equivalent).
        pub fn open_serial<'a>(
            &'a self,
            vendor_id: u16,
            product_id: u16,
            serial_number: &'a str,
        ) -> impl Future<Output = HidResult<HidDevice>> + 'a {
            crate::maybe_future::Blocking::new(move || {
                Ok(HidDevice {
                    backend: self
                        .backend
                        .open(vendor_id, product_id, Some(serial_number))?,
                })
            })
        }

        /// Open a device by platform path (`hid_open_path` equivalent). Use
        /// the paths reported by [`DeviceInfo::path`].
        pub fn open_path<'a>(
            &'a self,
            path: &'a str,
        ) -> impl Future<Output = HidResult<HidDevice>> + 'a {
            crate::maybe_future::Blocking::new(move || {
                Ok(HidDevice {
                    backend: self.backend.open_path(path)?,
                })
            })
        }
    }

    /// macOS-specific options (`hid_darwin_*` equivalents).
    #[cfg(all(target_os = "macos", not(feature = "nusb")))]
    impl HidApi {
        /// Whether subsequently opened devices are seized exclusively
        /// (`hid_darwin_set_open_exclusive`). Defaults to shared, matching
        /// hidapi >= 0.12.
        pub fn set_open_exclusive(&self, exclusive: bool) {
            self.backend.set_open_exclusive(exclusive);
        }

        /// Current exclusivity setting (`hid_darwin_get_open_exclusive`).
        pub fn open_exclusive(&self) -> bool {
            self.backend.open_exclusive()
        }
    }

    /// An open HID device (`hid_device` equivalent). Closed on drop.
    ///
    /// All methods take `&self`; the handle is `Send + Sync` and may be
    /// shared across threads, like hidapi handles.
    pub struct HidDevice {
        backend: PlatformDevice,
    }

    impl HidDevice {
        /// Send an output report (`hid_write`).
        ///
        /// `data[0]` must be the report ID (0 when the device has no
        /// numbered reports); the first byte is consumed accordingly and
        /// counts toward the returned length.
        ///
        /// Writes are synchronous kernel calls on every platform (there is no
        /// async OS primitive for them), so the returned future completes on
        /// first poll; it is exposed as a future only so blocking and async
        /// callers share one API.
        pub fn write<'a>(&'a self, data: &'a [u8]) -> impl Future<Output = HidResult<usize>> + 'a {
            crate::maybe_future::Blocking::new(move || self.backend.write(data))
        }

        /// Read one input report asynchronously (hidra's async `hid_read`).
        ///
        /// Resolves once a report has been copied into `buf`, returning its
        /// length, never `Ok(0)`; use your runtime's timeout combinator
        /// (e.g. `tokio::time::timeout`) to bound the wait. Reports are
        /// prefixed with their report ID only when the device uses numbered
        /// reports. Fails with
        /// [`HidError::Disconnected`](crate::HidError::Disconnected) when the
        /// device is removed.
        ///
        /// The future is runtime-agnostic (plain `Waker` wake-ups, like nusb,
        /// works under tokio, async-std, smol or a hand-rolled executor) and
        /// cancel-safe: dropping it never loses a report; pending input stays
        /// queued for the next read. Drive it blocking with
        /// [`MaybeFuture::wait`](crate::MaybeFuture::wait).
        ///
        /// Only input reads are asynchronous: writes and feature reports are
        /// synchronous kernel calls on every platform, so those futures
        /// complete on first poll.
        pub fn read<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            self.backend.read_async(buf)
        }

        /// Send a feature report (`hid_send_feature_report`). `data[0]` is
        /// the report ID, 0 if unnumbered.
        pub fn send_feature_report<'a>(
            &'a self,
            data: &'a [u8],
        ) -> impl Future<Output = HidResult<()>> + 'a {
            crate::maybe_future::Blocking::new(move || self.backend.send_feature_report(data))
        }

        /// Get a feature report (`hid_get_feature_report`). Set `buf[0]` to
        /// the report ID before calling; returns the report (ID included)
        /// and its length.
        pub fn get_feature_report<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            crate::maybe_future::Blocking::new(move || self.backend.get_feature_report(buf))
        }

        /// Get an input report synchronously (`hid_get_input_report`). Same
        /// buffer convention as [`get_feature_report`](Self::get_feature_report).
        pub fn get_input_report<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            crate::maybe_future::Blocking::new(move || self.backend.get_input_report(buf))
        }

        /// Manufacturer string (`hid_get_manufacturer_string`).
        pub fn get_manufacturer_string(
            &self,
        ) -> impl Future<Output = HidResult<Option<String>>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.get_manufacturer_string())
        }

        /// Product string (`hid_get_product_string`).
        pub fn get_product_string(&self) -> impl Future<Output = HidResult<Option<String>>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.get_product_string())
        }

        /// Serial number string (`hid_get_serial_number_string`).
        pub fn get_serial_number_string(
            &self,
        ) -> impl Future<Output = HidResult<Option<String>>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.get_serial_number_string())
        }

        /// A string from the device's string descriptor table
        /// (`hid_get_indexed_string`). Only meaningful for USB devices.
        pub fn get_indexed_string(
            &self,
            index: u32,
        ) -> impl Future<Output = HidResult<Option<String>>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.get_indexed_string(index))
        }

        /// Raw report descriptor (`hid_get_report_descriptor`). Returns the
        /// number of bytes written into `buf`; 4096 bytes is always enough.
        pub fn get_report_descriptor<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            crate::maybe_future::Blocking::new(move || self.backend.get_report_descriptor(buf))
        }

        /// Raw report descriptor as a vector (convenience over
        /// [`get_report_descriptor`](Self::get_report_descriptor)).
        pub fn report_descriptor(&self) -> impl Future<Output = HidResult<Vec<u8>>> + '_ {
            crate::maybe_future::Blocking::new(move || {
                let mut buf = vec![0u8; crate::MAX_REPORT_DESCRIPTOR_SIZE];
                let len = self.backend.get_report_descriptor(&mut buf)?;
                buf.truncate(len);
                Ok(buf)
            })
        }

        /// Parsed report descriptor (hidra extension built on
        /// [`crate::descriptor`]).
        pub async fn parsed_report_descriptor(
            &self,
        ) -> HidResult<crate::descriptor::ReportDescriptor> {
            let bytes = self.report_descriptor().await?;
            crate::descriptor::ReportDescriptor::parse(&bytes)
        }

        /// Metadata for this open device (`hid_get_device_info`).
        pub fn get_device_info(&self) -> impl Future<Output = HidResult<DeviceInfo>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.get_device_info())
        }
    }

    /// Windows-specific extensions (`hid_winapi_*` equivalents).
    #[cfg(all(target_os = "windows", not(feature = "nusb")))]
    impl HidDevice {
        /// The container ID GUID grouping this interface with its siblings
        /// (`hid_winapi_get_container_id`), as 16 little-endian GUID bytes.
        pub fn container_id(&self) -> impl Future<Output = HidResult<[u8; 16]>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.container_id())
        }

        /// Set the timeout for `write` in milliseconds
        /// (`hid_winapi_set_write_timeout`). Defaults to 1000 ms.
        pub fn set_write_timeout(&self, timeout_ms: u32) {
            self.backend.set_write_timeout(timeout_ms)
        }
    }
}

/// Largest report descriptor a HID device can have
/// (`HID_API_MAX_REPORT_DESCRIPTOR_SIZE`).
pub const MAX_REPORT_DESCRIPTOR_SIZE: usize = 4096;

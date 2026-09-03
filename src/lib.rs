//! # hidra
//!
//! A pure-Rust HID library.
//!
//! hidra talks to HID devices through native OS interfaces, no C library is
//! linked:
//!
#![cfg_attr(
    not(target_arch = "wasm32"),
    doc = "| Target | [`Backend::Native`] | [`Backend::Nusb`] (feature `nusb`) |
|--------|---------------------|------------------------------------|
| Linux   | `hidraw` device nodes + sysfs enumeration | USB interrupt/control transfers via [nusb] |
| Windows | `hid.dll` / `SetupAPI` via `windows-sys` declarations | as above |
| macOS   | `IOHIDManager` via direct framework FFI | as above |
"
)]
#![cfg_attr(
    target_arch = "wasm32",
    doc = "| Target | Native | `nusb` (feature `nusb`) |
|--------|--------|-------------------------|
| Linux   | `hidraw` device nodes + sysfs enumeration | USB interrupt/control transfers via [nusb] |
| Windows | `hid.dll` / `SetupAPI` via `windows-sys` declarations | as above |
| macOS   | `IOHIDManager` via direct framework FFI | as above |
"
)]
//!
//! On WebAssembly the backend is [`WebHID`](https://wicg.github.io/webhid/) via
//! `web-sys`, and the `Backend` selector does not exist.
//!
#![cfg_attr(
    target_arch = "wasm32",
    doc = "Devices the browser refuses to expose over `WebHID`, because their interface
declares a vendor-specific class rather than HID, are reachable through
[`webusb`] instead.
"
)]
#![cfg_attr(
    not(target_arch = "wasm32"),
    doc = "The two native backends coexist in one build: [`Backend`] selects between
them per [`Hidra`] instance, at run time, so a program can fall back from
one to the other, or drive two devices through different backends at once.
[`Hidra::new`] uses [`Backend::default`] (native wherever there is one);
[`Hidra::builder`] picks.
"
)]
//!
//! ```no_run
//! # #[cfg(all(not(target_arch = "wasm32"), feature = "nusb"))] fn demo() -> hidra::HidResult<()> {
//! use hidra::{Backend, Hidra};
//! // Prefer the OS HID stack; fall back to raw USB when it has no node for
//! // the device (or refuses access to it).
//! let api = match Hidra::builder().backend(Backend::Native).build() {
//!     Ok(api) => api,
//!     Err(_) => Hidra::builder().backend(Backend::Nusb).build()?,
//! };
//! # let _ = api; Ok(()) }
//! ```
//!
//! Following nusb's model, every [`Hidra`] / [`HidDevice`] I/O method returns
//! an `impl Future`. On native targets bring `MaybeFuture` into scope to drive
//! it blocking with `.wait()`, or `.await` it under any async runtime (no
//! executor dependency, wake-ups are plain `Waker`s like nusb).
//!
//! On `wasm32` the same [`Hidra`] / [`HidDevice`] types are backed by `WebHID`;
//! there is no blocking mode, so always `.await` their futures (no `.wait()`).
//! Discovery is WebHID-shaped: `Hidra::request_device` shows the browser's
//! device chooser (filtered with `DeviceFilter`) and `Hidra::get_devices`
//! lists previously granted devices. [`descriptor`] offers report-descriptor
//! primitives that work everywhere.
//!
//! ```no_run
//! # #[cfg(not(target_arch = "wasm32"))] fn demo() -> hidra::HidResult<()> {
//! use hidra::MaybeFuture;
//! let api = hidra::Hidra::new()?;
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

// Houses every backend: the per-OS native ones and nusb (non-wasm), and the
// WebHID backend (wasm). Its internals are individually cfg-gated.
mod backend;

#[cfg(not(target_arch = "wasm32"))]
mod maybe_future;
#[cfg(not(target_arch = "wasm32"))]
pub use maybe_future::MaybeFuture;

/// WebHID-only public surface: the device filter for `Hidra::request_device`,
/// the listener handle returned by the event hooks, and the buffered input
/// report stream from `HidDevice::start_reading`.
#[cfg(target_arch = "wasm32")]
pub use backend::webhid::{DeviceFilter, EventListenerHandle, InputReportStream};

/// hidra's version, mirroring `hid_version()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiVersion {
    /// Major version component.
    pub major: u16,
    /// Minor version component.
    pub minor: u16,
    /// Patch version component.
    pub patch: u16,
}

/// Library version (`hid_version` equivalent).
pub const fn version() -> ApiVersion {
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
pub use backend::Backend;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{HidDevice, Hidra, HidraBuilder};

#[cfg(target_arch = "wasm32")]
pub use web::{HidDevice, Hidra};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use core::future::Future;

    use crate::backend::dispatch::{DynApi, DynDevice};
    use crate::backend::Backend;
    use crate::{DeviceInfo, HidResult};

    /// Entry point to the library; owns backend state and the cached device
    /// list.
    ///
    /// There is no global state: create as many instances as
    /// you like, from any thread, each on whichever [`Backend`] you choose.
    pub struct Hidra {
        backend: DynApi,
        device_list: Vec<DeviceInfo>,
    }

    // The platform backends hold raw OS handles that have no useful `Debug`;
    // report the selected backend and the cached device count instead, which
    // is what a caller inspecting a `Hidra` actually wants to see.
    impl core::fmt::Debug for Hidra {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Hidra")
                .field("backend", &self.backend())
                .field("devices", &self.device_list.len())
                .finish_non_exhaustive()
        }
    }

    /// Configures a [`Hidra`]: which [`Backend`] to talk to, and whether to
    /// enumerate up front.
    ///
    /// ```no_run
    /// # fn demo() -> hidra::HidResult<()> {
    /// use hidra::{Backend, Hidra};
    /// let api = Hidra::builder().backend(Backend::Native).build()?;
    /// # let _ = api; Ok(()) }
    /// ```
    #[derive(Debug, Clone)]
    pub struct HidraBuilder {
        backend: Backend,
        enumerate_on_build: bool,
    }

    impl Default for HidraBuilder {
        fn default() -> Self {
            HidraBuilder {
                backend: Backend::default(),
                enumerate_on_build: true,
            }
        }
    }

    impl HidraBuilder {
        /// Select the backend. Defaults to [`Backend::default`].
        #[must_use]
        pub fn backend(mut self, backend: Backend) -> Self {
            self.backend = backend;
            self
        }

        /// Whether [`build`](Self::build) enumerates connected devices.
        /// Defaults to `true`; pass `false` when you only need
        /// [`Hidra::open_path`].
        #[must_use]
        pub fn enumerate_on_build(mut self, enumerate: bool) -> Self {
            self.enumerate_on_build = enumerate;
            self
        }

        /// Initialize the selected backend.
        ///
        /// Fails with [`HidError::Unsupported`](crate::HidError::Unsupported)
        /// when this build has no such backend (see
        /// [`Backend::is_available`]).
        pub fn build(self) -> HidResult<Hidra> {
            let mut api = Hidra {
                backend: DynApi::new(self.backend)?,
                device_list: Vec::new(),
            };
            if self.enumerate_on_build {
                api.refresh_devices()?;
            }
            Ok(api)
        }
    }

    impl Hidra {
        /// Initialize the default backend and enumerate all connected HID
        /// devices.
        pub fn new() -> HidResult<Self> {
            Self::builder().build()
        }

        /// Initialize the default backend without enumerating (cheaper when
        /// you only need [`open_path`](Self::open_path)).
        #[deprecated(
            since = "0.0.4",
            note = "use Hidra::builder().enumerate_on_build(false).build()"
        )]
        pub fn new_without_enumerate() -> HidResult<Self> {
            Self::builder().enumerate_on_build(false).build()
        }

        /// Start configuring a `Hidra`, to select a [`Backend`] other than
        /// [`Backend::default`].
        #[must_use]
        pub fn builder() -> HidraBuilder {
            HidraBuilder::default()
        }

        /// The backend this instance is talking to.
        #[must_use]
        pub fn backend(&self) -> Backend {
            self.backend.backend()
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

    /// macOS-specific options, on [`Backend::Native`].
    ///
    /// Both fail with
    /// [`HidError::Unsupported`](crate::HidError::Unsupported) under
    /// [`Backend::Nusb`], which claims the USB interface outright and has no
    /// shared mode to configure.
    #[cfg(target_os = "macos")]
    impl Hidra {
        /// Whether subsequently opened devices are seized exclusively
        /// (`hid_darwin_set_open_exclusive`). Defaults to shared
        /// (non-exclusive) access.
        pub fn set_open_exclusive(&self, exclusive: bool) -> HidResult<()> {
            self.backend.set_open_exclusive(exclusive)
        }

        /// Current exclusivity setting.
        pub fn open_exclusive(&self) -> HidResult<bool> {
            self.backend.open_exclusive()
        }
    }

    /// An open HID device. Closed on drop.
    ///
    /// All methods take `&self`; the handle is `Send + Sync` and may be
    /// shared across threads.
    pub struct HidDevice {
        backend: DynDevice,
    }

    impl core::fmt::Debug for HidDevice {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("HidDevice").finish_non_exhaustive()
        }
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

    /// Windows-specific extensions (`hid_winapi_*` equivalents), on
    /// [`Backend::Native`].
    ///
    /// Both fail with
    /// [`HidError::Unsupported`](crate::HidError::Unsupported) under
    /// [`Backend::Nusb`], which goes nowhere near the Windows HID stack these
    /// come from.
    #[cfg(target_os = "windows")]
    impl HidDevice {
        /// The container ID GUID grouping this interface with its siblings
        /// (`hid_winapi_get_container_id`), as 16 little-endian GUID bytes.
        pub fn container_id(&self) -> impl Future<Output = HidResult<[u8; 16]>> + '_ {
            crate::maybe_future::Blocking::new(move || self.backend.container_id())
        }

        /// Set the timeout for `write` in milliseconds
        /// (`hid_winapi_set_write_timeout`). Defaults to 1000 ms.
        pub fn set_write_timeout(&self, timeout_ms: u32) -> HidResult<()> {
            self.backend.set_write_timeout(timeout_ms)
        }
    }
}

/// Largest report descriptor a HID device can have
/// (`HID_API_MAX_REPORT_DESCRIPTOR_SIZE`).
pub const MAX_REPORT_DESCRIPTOR_SIZE: usize = 4096;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    /// `Hidra` and `HidDevice` are documented as shareable across threads.
    /// The `HidBackend`/`HidDeviceBackend` bounds make that hold for whichever
    /// backend is selected; this pins it for the public wrappers too.
    #[test]
    fn public_handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::Hidra>();
        assert_send_sync::<super::HidDevice>();
        assert_send_sync::<super::HidError>();
        assert_send_sync::<super::DeviceInfo>();
    }

    /// A backend name survives `Display` -> `FromStr`, so a value from a
    /// config file or `--backend` flag round-trips.
    #[test]
    fn backend_names_round_trip() {
        for backend in [super::Backend::Native, super::Backend::Nusb] {
            assert_eq!(
                backend.to_string().parse::<super::Backend>().unwrap(),
                backend
            );
        }
        assert_eq!(
            " NUSB ".parse::<super::Backend>().unwrap(),
            super::Backend::Nusb
        );
        assert!("hidraw".parse::<super::Backend>().is_err());
    }

    /// Selecting a backend this build does not have is a clean `Unsupported`,
    /// not a panic or a silent fall back to the other one.
    #[test]
    fn unavailable_backend_is_rejected() {
        for backend in [super::Backend::Native, super::Backend::Nusb] {
            if backend.is_available() {
                continue;
            }
            let err = super::Hidra::builder()
                .backend(backend)
                .build()
                .unwrap_err();
            assert!(matches!(err, super::HidError::Unsupported { .. }), "{err}");
        }
    }

    /// The default is the first backend this build actually has (nothing at
    /// all only on a target with no per-OS backend and no `nusb` feature).
    #[test]
    fn default_backend_is_the_first_available_one() {
        let mut available = super::Backend::available();
        if let Some(first) = available.next() {
            assert_eq!(first, super::Backend::default());
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use core::cell::RefCell;
    use core::future::Future;

    use crate::backend::webhid::{
        DeviceFilter, EventListenerHandle, InputReportStream, WebHidApi, WebHidDevice,
    };
    use crate::report_info::CollectionInfo;
    use crate::{DeviceInfo, HidResult};

    /// Entry point to the library, backed by `WebHID` (`navigator.hid`).
    ///
    /// Discovery is WebHID-shaped rather than native-HID-shaped: the browser only
    /// ever exposes devices the user has granted access to, so there is no
    /// enumerate / open-by-vid-pid. Use [`request_device`](Self::request_device)
    /// to show the permission chooser and [`get_devices`](Self::get_devices) to
    /// list previously granted devices.
    pub struct Hidra {
        backend: WebHidApi,
    }

    impl core::fmt::Debug for Hidra {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Hidra").finish_non_exhaustive()
        }
    }

    // These I/O methods return `impl Future` to mirror the native `Hidra` /
    // `HidDevice` signatures exactly (native backs them with `Blocking`, not an
    // async block), so the `manual_async_fn` suggestion does not apply.
    #[allow(clippy::manual_async_fn)]
    impl Hidra {
        /// Bind to `window.navigator.hid`.
        ///
        /// Fails with [`HidError::Initialization`](crate::HidError::Initialization)
        /// when `WebHID` is unavailable (no window, a non-secure context, or a
        /// browser without `WebHID` support).
        pub fn new() -> HidResult<Self> {
            Ok(Hidra {
                backend: WebHidApi::new()?,
            })
        }

        /// Ask the user to grant access to devices matching `filters`
        /// (`navigator.hid.requestDevice`). An empty filter list matches every
        /// device.
        ///
        /// Shows the browser's device chooser and resolves with every device
        /// the user granted (an empty `Vec` when the chooser was dismissed).
        /// **Must be called from within a user gesture** (e.g. a click event
        /// handler), otherwise the browser rejects the request.
        pub fn request_device<'a>(
            &'a self,
            filters: &'a [DeviceFilter],
        ) -> impl Future<Output = HidResult<Vec<HidDevice>>> + 'a {
            async move {
                let devices = self.backend.request_device(filters).await?;
                Ok(devices.into_iter().map(HidDevice::new).collect())
            }
        }

        /// Devices the user has already granted this origin access to
        /// (`navigator.hid.getDevices`). Needs no user gesture.
        pub fn get_devices(&self) -> impl Future<Output = HidResult<Vec<HidDevice>>> + '_ {
            async move {
                let devices = self.backend.get_devices().await?;
                Ok(devices.into_iter().map(HidDevice::new).collect())
            }
        }

        /// Invoke `f` whenever a granted device is plugged in (the `connect`
        /// event). Drop the returned handle to unregister.
        pub fn on_connect(&self, mut f: impl FnMut(HidDevice) + 'static) -> EventListenerHandle {
            self.backend.on_connect(move |dev| f(HidDevice::new(dev)))
        }

        /// Invoke `f` whenever a granted device is unplugged (the `disconnect`
        /// event). Drop the returned handle to unregister.
        pub fn on_disconnect(&self, mut f: impl FnMut(HidDevice) + 'static) -> EventListenerHandle {
            self.backend
                .on_disconnect(move |dev| f(HidDevice::new(dev)))
        }

        /// The underlying `navigator.hid` object (`WebHID` escape hatch).
        pub fn raw(&self) -> &web_sys::Hid {
            self.backend.raw()
        }
    }

    /// An HID device exposed by the browser, backed by `WebHID`.
    ///
    /// Unlike a native HID library the handle exists before the device is opened, so
    /// call [`open`](Self::open) before transferring reports.
    pub struct HidDevice {
        backend: WebHidDevice,
        /// Lazily started on the first [`read`](Self::read); reused thereafter.
        stream: RefCell<Option<InputReportStream>>,
    }

    impl core::fmt::Debug for HidDevice {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("HidDevice")
                .field("opened", &self.backend.opened())
                .finish_non_exhaustive()
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl HidDevice {
        fn new(backend: WebHidDevice) -> Self {
            HidDevice {
                backend,
                stream: RefCell::new(None),
            }
        }

        // --- shared methods (signatures match native) ----------------------

        /// Send an output report (`hid_write`).
        ///
        /// `data[0]` must be the report ID (0 when the device has no numbered
        /// reports); the first byte is consumed accordingly and counts toward
        /// the returned length.
        pub fn write<'a>(&'a self, data: &'a [u8]) -> impl Future<Output = HidResult<usize>> + 'a {
            self.backend.write(data)
        }

        /// Read one input report asynchronously (hidra's async `hid_read`).
        ///
        /// Resolves once a report has been copied into `buf`, returning its
        /// length. Reports are prefixed with their report ID only when the
        /// device uses numbered reports, matching native.
        ///
        /// Backed by a single [`InputReportStream`] lazily started on the first
        /// call (so reports are queued from that point on); subsequent reads
        /// reuse it and drain the queue in order.
        pub fn read<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            async move {
                if self.stream.borrow().is_none() {
                    *self.stream.borrow_mut() = Some(self.backend.start_reading());
                }
                // `next_report` clones the stream's shared queue handle, so the
                // RefCell borrow is released before awaiting.
                let read = {
                    let guard = self.stream.borrow();
                    let stream = guard.as_ref().expect("stream started above");
                    stream.next_report()
                };
                let report = read.await?;
                let len = report.len().min(buf.len());
                buf[..len].copy_from_slice(&report[..len]);
                Ok(len)
            }
        }

        /// Send a feature report (`hid_send_feature_report`). `data[0]` is the
        /// report ID, 0 if unnumbered.
        pub fn send_feature_report<'a>(
            &'a self,
            data: &'a [u8],
        ) -> impl Future<Output = HidResult<()>> + 'a {
            self.backend.send_feature_report(data)
        }

        /// Get a feature report (`hid_get_feature_report`). Set `buf[0]` to the
        /// report ID before calling; returns the report (ID included) and its
        /// length.
        pub fn get_feature_report<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            async move {
                let report_id =
                    buf.first()
                        .copied()
                        .ok_or_else(|| crate::HidError::InvalidData {
                            message: "get_feature_report requires at least the report ID byte"
                                .into(),
                        })?;
                let report = self.backend.get_feature_report(report_id).await?;
                let len = report.len().min(buf.len());
                buf[..len].copy_from_slice(&report[..len]);
                Ok(len)
            }
        }

        /// Raw report descriptor (`hid_get_report_descriptor`). Returns the
        /// number of bytes written into `buf`; 4096 bytes is always enough.
        pub fn get_report_descriptor<'a>(
            &'a self,
            buf: &'a mut [u8],
        ) -> impl Future<Output = HidResult<usize>> + 'a {
            async move {
                let descriptor = self.backend.report_descriptor()?;
                let len = descriptor.len().min(buf.len());
                buf[..len].copy_from_slice(&descriptor[..len]);
                Ok(len)
            }
        }

        /// Raw report descriptor as a vector (convenience over
        /// [`get_report_descriptor`](Self::get_report_descriptor)).
        pub fn report_descriptor(&self) -> impl Future<Output = HidResult<Vec<u8>>> + '_ {
            async move { self.backend.report_descriptor() }
        }

        /// Parsed report descriptor (hidra extension built on
        /// [`crate::descriptor`]).
        pub async fn parsed_report_descriptor(
            &self,
        ) -> HidResult<crate::descriptor::ReportDescriptor> {
            self.backend.parsed_report_descriptor()
        }

        /// Product string (`hid_get_product_string`).
        pub fn get_product_string(&self) -> impl Future<Output = HidResult<Option<String>>> + '_ {
            async move { Ok(self.backend.product_name()) }
        }

        /// Metadata for this open device (`hid_get_device_info`).
        pub fn get_device_info(&self) -> impl Future<Output = HidResult<DeviceInfo>> + '_ {
            async move { Ok(self.backend.device_info()) }
        }

        // --- WebHID-specific extras ----------------------------------------

        /// Open the device for I/O (`HIDDevice.open`). Required before any
        /// report transfer.
        pub async fn open(&self) -> HidResult<()> {
            self.backend.open().await
        }

        /// Close the device (`HIDDevice.close`). The permission grant is kept,
        /// reopen with [`open`](Self::open).
        pub async fn close(&self) -> HidResult<()> {
            // Drop any input stream so a reopen starts a fresh listener.
            *self.stream.borrow_mut() = None;
            self.backend.close().await
        }

        /// Whether the device is currently open (`HIDDevice.opened`).
        pub fn opened(&self) -> bool {
            self.backend.opened()
        }

        /// Revoke the user's permission grant for this device
        /// (`HIDDevice.forget`).
        pub async fn forget(&self) -> HidResult<()> {
            self.backend.forget().await
        }

        /// Invoke `f` with `(report_id, payload)` for every incoming input
        /// report (the `inputreport` event). Drop the returned handle to
        /// unregister.
        pub fn on_input_report(&self, f: impl FnMut(u8, Vec<u8>) + 'static) -> EventListenerHandle {
            self.backend.on_input_report(f)
        }

        /// Start an independent buffered input-report stream. Most callers
        /// should use [`read`](Self::read) instead; this is exposed for the
        /// `WebHID` streaming idiom.
        pub fn start_reading(&self) -> InputReportStream {
            self.backend.start_reading()
        }

        /// The collection tree the browser parsed from the device's report
        /// descriptor (`HIDDevice.collections`).
        pub fn collections(&self) -> Vec<CollectionInfo> {
            self.backend.collections()
        }

        /// The underlying `HIDDevice` object (`WebHID` escape hatch).
        pub fn raw(&self) -> &web_sys::HidDevice {
            self.backend.raw()
        }
    }
}

/// `WebUSB` entry point, for devices the browser will not expose over `WebHID`.
///
/// `WebHID` only surfaces interfaces the host recognises as HID. A device whose
/// interface declares a vendor-specific class never appears there, but is still
/// reachable over `WebUSB`, and typically still speaks the HID protocol on the
/// wire. This module drives those devices, mapping the same report calls onto
/// the control and interrupt transfers the native `nusb` backend uses.
///
/// Blink refuses `claimInterface` on the protected classes, HID (0x03) among
/// them, so [`open`](HidDevice::open) fails for an interface the browser
/// considers HID. That is the division of labour: `WebHID` for those, this for
/// the rest.
#[cfg(all(target_arch = "wasm32", feature = "nusb"))]
pub mod webusb {
    use crate::backend::webusb::{WebUsbApi, WebUsbDevice};
    use crate::descriptor::ReportDescriptor;
    use crate::{DeviceInfo, HidResult, MAX_REPORT_DESCRIPTOR_SIZE};

    pub use nusb::DeviceSelector;

    /// Entry point to the library, backed by `WebUSB` (`navigator.usb`).
    ///
    /// Discovery is browser-shaped, like [`crate::Hidra`] on wasm: only devices
    /// the user has granted are visible, so there is no open-by-vid-pid. Call
    /// [`request_device`](Self::request_device) to show the chooser and
    /// [`device_list`](Self::device_list) for devices already granted.
    pub struct Hidra {
        backend: WebUsbApi,
    }

    impl core::fmt::Debug for Hidra {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Hidra").finish_non_exhaustive()
        }
    }

    impl Hidra {
        /// Bind to `window.navigator.usb`.
        pub fn new() -> HidResult<Self> {
            Ok(Hidra {
                backend: WebUsbApi::new()?,
            })
        }

        /// Show the browser's device chooser, narrowed by `selectors`.
        ///
        /// An empty slice offers every device. Resolves with `None` when the
        /// user dismisses the chooser without picking one. Must be called from
        /// a user gesture, or the browser rejects it.
        pub async fn request_device(
            &self,
            selectors: &[DeviceSelector],
        ) -> HidResult<Option<Device>> {
            Ok(self
                .backend
                .request_device(selectors)
                .await?
                .map(|info| Device { info }))
        }

        /// Devices the user has already granted access to.
        ///
        /// Unlike the chooser, this needs no user gesture, so it is the way in
        /// for a device granted ahead of time (a `WebUsbAllowDevicesForUrls`
        /// policy, or an earlier `request_device` the browser remembered).
        pub async fn device_list(&self) -> HidResult<Vec<Device>> {
            Ok(self
                .backend
                .device_list()
                .await?
                .into_iter()
                .map(|info| Device { info })
                .collect())
        }
    }

    /// A granted device, not yet claimed. Call [`open`](Self::open) to claim an
    /// interface and get a [`HidDevice`].
    #[derive(Debug)]
    pub struct Device {
        info: nusb::DeviceInfo,
    }

    impl Device {
        /// Claim an interface and start talking to it.
        ///
        /// `interface_number` selects the interface; `None` takes the first the
        /// device exposes. Fails when the browser refuses the claim, which it
        /// does for the protected classes.
        pub async fn open(&self, interface_number: Option<u8>) -> HidResult<HidDevice> {
            Ok(HidDevice {
                backend: WebUsbDevice::open(&self.info, interface_number).await?,
            })
        }

        /// Vendor ID.
        pub fn vendor_id(&self) -> u16 {
            self.info.vendor_id()
        }

        /// Product ID.
        pub fn product_id(&self) -> u16 {
            self.info.product_id()
        }

        /// Serial number string, if the device reports one.
        pub fn serial_number(&self) -> Option<&str> {
            self.info.serial_number()
        }

        /// Product string, if the device reports one.
        pub fn product_string(&self) -> Option<&str> {
            self.info.product_string()
        }
    }

    /// One open device, claimed on a single interface.
    pub struct HidDevice {
        backend: WebUsbDevice,
    }

    impl core::fmt::Debug for HidDevice {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("HidDevice").finish_non_exhaustive()
        }
    }

    impl HidDevice {
        /// Send an output report.
        ///
        /// `data[0]` must be the report ID (0 when the device has no numbered
        /// reports). Goes out on the interrupt OUT endpoint when the interface
        /// has one, otherwise as `SET_REPORT(Output)`.
        pub async fn write(&self, data: &[u8]) -> HidResult<usize> {
            self.backend.write(data).await
        }

        /// Resolve with one input report from the interrupt IN endpoint.
        ///
        /// Fails with [`HidError::Unsupported`](crate::HidError::Unsupported)
        /// on an interface without one; use
        /// [`get_input_report`](Self::get_input_report) there.
        pub async fn read(&self, buf: &mut [u8]) -> HidResult<usize> {
            self.backend.read(buf).await
        }

        /// Send a feature report via `SET_REPORT(Feature)`. `data[0]` is the
        /// report ID.
        pub async fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
            self.backend.send_feature_report(data).await
        }

        /// Read a feature report via `GET_REPORT(Feature)`. Set `buf[0]` to the
        /// report ID before calling.
        pub async fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
            self.backend.get_feature_report(buf).await
        }

        /// Read an input report via `GET_REPORT(Input)`, without waiting for the
        /// device to send one.
        pub async fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
            self.backend.get_input_report(buf).await
        }

        /// Raw report descriptor.
        ///
        /// Fails with [`HidError::Unsupported`](crate::HidError::Unsupported)
        /// on a vendor-class interface, which has no HID class descriptor to
        /// read; that is the usual case here.
        pub fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
            self.backend.get_report_descriptor(buf)
        }

        /// The report descriptor as an owned buffer.
        pub fn report_descriptor(&self) -> HidResult<Vec<u8>> {
            let mut buf = vec![0u8; MAX_REPORT_DESCRIPTOR_SIZE];
            let len = self.get_report_descriptor(&mut buf)?;
            buf.truncate(len);
            Ok(buf)
        }

        /// The report descriptor, parsed.
        pub fn parsed_report_descriptor(&self) -> HidResult<ReportDescriptor> {
            ReportDescriptor::parse(&self.report_descriptor()?)
        }

        /// Enumeration-style metadata for this interface.
        pub fn get_device_info(&self) -> HidResult<DeviceInfo> {
            Ok(self.backend.device_info())
        }

        /// The underlying `nusb` interface, for transfers outside the HID
        /// mapping.
        pub fn raw(&self) -> &nusb::Interface {
            self.backend.raw()
        }
    }
}

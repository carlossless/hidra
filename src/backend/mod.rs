//! Platform backend selection.
//!
//! Each native backend exposes two types with an identical inherent-method
//! surface (a compile-time "trait"):
//!
//! ```text
//! pub(crate) struct PlatformApi;
//! impl PlatformApi {
//!     pub fn new() -> HidResult<Self>;
//!     /// vendor_id/product_id of 0 act as wildcards.
//!     pub fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>>;
//!     pub fn open(&self, vendor_id: u16, product_id: u16, serial: Option<&str>)
//!         -> HidResult<PlatformDevice>;
//!     pub fn open_path(&self, path: &str) -> HidResult<PlatformDevice>;
//! }
//!
//! pub(crate) struct PlatformDevice;
//! impl PlatformDevice {
//!     pub fn write(&self, data: &[u8]) -> HidResult<usize>;
//!     pub fn read(&self, buf: &mut [u8]) -> HidResult<usize>;
//!     pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> HidResult<usize>;
//!     /// Returns a Send future resolving with one input report; never 0.
//!     pub fn read_async<'a>(&'a self, buf: &'a mut [u8])
//!         -> impl Future<Output = HidResult<usize>> + Send + 'a;
//!     pub fn set_blocking_mode(&self, blocking: bool) -> HidResult<()>;
//!     pub fn send_feature_report(&self, data: &[u8]) -> HidResult<()>;
//!     pub fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize>;
//!     pub fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize>;
//!     pub fn get_manufacturer_string(&self) -> HidResult<Option<String>>;
//!     pub fn get_product_string(&self) -> HidResult<Option<String>>;
//!     pub fn get_serial_number_string(&self) -> HidResult<Option<String>>;
//!     pub fn get_indexed_string(&self, index: u32) -> HidResult<Option<String>>;
//!     pub fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize>;
//!     pub fn get_device_info(&self) -> HidResult<DeviceInfo>;
//! }
//! ```
//!
//! Semantics shared by all backends (hidapi parity):
//!
//! * `write` / `send_feature_report`: `data[0]` is the report ID; use 0 when
//!   the device has no numbered reports. The ID byte counts toward the
//!   returned length.
//! * `read` / `read_timeout`: input reports are prefixed with their report ID
//!   only when the device uses numbered reports. `timeout_ms < 0` blocks
//!   forever, `0` polls.
//! * `get_feature_report` / `get_input_report`: `buf[0]` must contain the
//!   report ID on entry; on return the buffer starts with that ID.
//! * In non-blocking mode, `read` returns `Ok(0)` when no report is queued.
//! * `read_async` ignores the blocking mode, never resolves with `Ok(0)`,
//!   fails with `HidError::Disconnected` on removal, and must be
//!   cancel-safe: dropping the future may not lose an already-delivered
//!   report (it stays queued for the next read). Wake-ups are
//!   runtime-agnostic (raw `Waker`s, no executor assumed).

// The WebHID backend on wasm. Unlike the native backends below it does not
// implement the PlatformApi/PlatformDevice contract documented above (the
// `web` module in lib.rs drives it directly), but it belongs here as a backend.
#[cfg(target_arch = "wasm32")]
pub(crate) mod webhid;

// With the `nusb` feature the USB-transport backend replaces the per-OS native
// backends on every platform; otherwise the native backend for the target OS
// is selected.
#[cfg(all(feature = "nusb", not(target_arch = "wasm32")))]
pub(crate) mod nusb;
#[cfg(all(feature = "nusb", not(target_arch = "wasm32")))]
pub(crate) use nusb::{NusbApi as PlatformApi, NusbDevice as PlatformDevice};

#[cfg(all(not(feature = "nusb"), target_os = "linux"))]
pub(crate) mod reactor;

#[cfg(all(not(feature = "nusb"), target_os = "linux"))]
pub(crate) mod hidraw;
#[cfg(all(not(feature = "nusb"), target_os = "linux"))]
pub(crate) use hidraw::{HidrawApi as PlatformApi, HidrawDevice as PlatformDevice};

#[cfg(all(not(feature = "nusb"), target_os = "windows"))]
pub(crate) mod windows;
#[cfg(all(not(feature = "nusb"), target_os = "windows"))]
pub(crate) use windows::{WinApi as PlatformApi, WinDevice as PlatformDevice};

#[cfg(all(not(feature = "nusb"), target_os = "macos"))]
pub(crate) mod macos;
#[cfg(all(not(feature = "nusb"), target_os = "macos"))]
pub(crate) use macos::{MacApi as PlatformApi, MacDevice as PlatformDevice};

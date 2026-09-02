//! Run-time backend selection.
//!
//! [`DynApi`] / [`DynDevice`] hold one variant per [`Backend`], and forward
//! every [`HidBackend`] / [`HidDeviceBackend`] method to whichever the caller
//! picked. Dispatch is a single enum branch per call, and the read future
//! ([`DynRead`]) is an enum too, so no operation allocates or boxes.
//!
//! Both variants always exist. A backend that is not part of this build is
//! typed [`super::unsupported::Unsupported`], which is uninhabited: its arms
//! compile but can never be reached, and [`Backend::is_available`] refuses the
//! selection before the variant would be constructed.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::str::FromStr;
use core::task::{Context, Poll};

use super::{HidBackend, HidDeviceBackend};
use crate::{DeviceInfo, HidError, HidResult};

#[cfg(any(target_os = "linux", target_os = "android"))]
use super::hidraw::{HidrawApi as NativeApi, HidrawDevice as NativeDevice};
#[cfg(target_os = "macos")]
use super::macos::{MacApi as NativeApi, MacDevice as NativeDevice};
#[cfg(target_os = "windows")]
use super::windows::{WinApi as NativeApi, WinDevice as NativeDevice};

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
)))]
use super::unsupported::Unsupported as NativeApi;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
)))]
use super::unsupported::Unsupported as NativeDevice;

#[cfg(all(
    feature = "nusb",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
use super::nusb::{NusbApi, NusbDevice};

#[cfg(not(all(
    feature = "nusb",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
)))]
use super::unsupported::Unsupported as NusbApi;
#[cfg(not(all(
    feature = "nusb",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
)))]
use super::unsupported::Unsupported as NusbDevice;

/// Which implementation a [`Hidra`](crate::Hidra) talks to.
///
/// Pick one with [`Hidra::builder`](crate::Hidra::builder); [`Hidra::new`](crate::Hidra::new)
/// uses [`Backend::default`]. The choice is per-instance, so a program may hold
/// one [`Hidra`](crate::Hidra) per backend at the same time.
///
/// Which variants a given build can actually construct depends on the target
/// and on the `nusb` feature; ask [`is_available`](Self::is_available) rather
/// than assuming, and expect [`HidError::Unsupported`] from a selection that
/// is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// The operating system's own HID stack: `hidraw` on Linux and Android,
    /// `hid.dll` and `SetupAPI` on Windows, `IOHIDManager` on macOS.
    ///
    /// Available on those targets. Devices are shared with the OS, no driver
    /// is displaced, and non-USB transports (Bluetooth, I2C, ...) work.
    Native,
    /// Raw USB interrupt and control transfers via [nusb], bypassing the OS
    /// HID stack.
    ///
    /// Available with the `nusb` feature on Linux, macOS and Windows —
    /// not on Android, where nusb offers no enumeration (a device arrives
    /// there as a file descriptor from the Java USB Host API).
    ///
    /// Sees USB devices only, and opening one claims the whole USB interface
    /// away from the OS driver until the handle is dropped, so it needs
    /// raw-USB permissions. Use it where the OS HID stack has no node for a
    /// device, restricts access to it, or must be taken out of the way.
    ///
    /// [nusb]: https://docs.rs/nusb
    Nusb,
}

impl Backend {
    /// Whether this build can use this backend.
    ///
    /// False for [`Native`](Self::Native) on a target with no per-OS backend,
    /// and for [`Nusb`](Self::Nusb) without the `nusb` feature.
    #[must_use]
    pub const fn is_available(self) -> bool {
        match self {
            Backend::Native => cfg!(any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "windows"
            )),
            Backend::Nusb => cfg!(all(
                feature = "nusb",
                any(
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "windows"
                )
            )),
        }
    }

    /// Every backend this build can use, in preference order.
    pub fn available() -> impl Iterator<Item = Backend> {
        [Backend::Native, Backend::Nusb]
            .into_iter()
            .filter(|backend| backend.is_available())
    }

    /// The name [`Display`](fmt::Display) and [`FromStr`] use: `native` or `nusb`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Native => "native",
            Backend::Nusb => "nusb",
        }
    }

    fn unavailable(self) -> HidError {
        let message = match self {
            Backend::Native => {
                "no native HID backend for this target; select Backend::Nusb (needs hidra's \
                 `nusb` feature)"
            }
            Backend::Nusb => {
                "the nusb backend needs hidra's `nusb` feature, on Linux, macOS or Windows"
            }
        };
        HidError::Unsupported {
            message: message.into(),
        }
    }
}

/// The first [`available`](Backend::available) backend: [`Native`](Backend::Native)
/// wherever there is one, else [`Nusb`](Backend::Nusb).
impl Default for Backend {
    fn default() -> Self {
        if Backend::Native.is_available() {
            Backend::Native
        } else {
            Backend::Nusb
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses `native` and `nusb`, case-insensitively, so a backend can come from
/// a command line or an environment variable.
impl FromStr for Backend {
    type Err = HidError;

    fn from_str(s: &str) -> HidResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "native" => Ok(Backend::Native),
            "nusb" => Ok(Backend::Nusb),
            other => Err(HidError::InvalidData {
                message: format!("unknown backend {other:?}, expected \"native\" or \"nusb\""),
            }),
        }
    }
}

/// The selected [`HidBackend`].
pub(crate) enum DynApi {
    Native(NativeApi),
    Nusb(NusbApi),
}

/// A device opened through [`DynApi`], on that same backend.
// Handle sizes differ a lot per backend (a `WinDevice` carries its overlapped
// I/O state inline); boxing to even them out would cost an allocation per open
// device, for a handle that is created once and lives long.
#[allow(clippy::large_enum_variant)]
pub(crate) enum DynDevice {
    Native(NativeDevice),
    Nusb(NusbDevice),
}

/// The future [`DynDevice::read_async`] returns.
pub(crate) enum DynRead<'a> {
    Native(<NativeDevice as HidDeviceBackend>::Read<'a>),
    Nusb(<NusbDevice as HidDeviceBackend>::Read<'a>),
}

macro_rules! forward {
    ($self:expr, $inner:ident => $call:expr) => {
        match $self {
            DynApi::Native($inner) => $call,
            DynApi::Nusb($inner) => $call,
        }
    };
}

macro_rules! forward_device {
    ($self:expr, $inner:ident => $call:expr) => {
        match $self {
            DynDevice::Native($inner) => $call,
            DynDevice::Nusb($inner) => $call,
        }
    };
}

impl DynApi {
    pub(crate) fn new(backend: Backend) -> HidResult<Self> {
        if !backend.is_available() {
            return Err(backend.unavailable());
        }
        match backend {
            Backend::Native => Ok(DynApi::Native(NativeApi::new()?)),
            Backend::Nusb => Ok(DynApi::Nusb(NusbApi::new()?)),
        }
    }

    pub(crate) fn backend(&self) -> Backend {
        match self {
            DynApi::Native(_) => Backend::Native,
            DynApi::Nusb(_) => Backend::Nusb,
        }
    }

    pub(crate) fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
        forward!(self, api => api.enumerate(vendor_id, product_id))
    }

    pub(crate) fn open_path(&self, path: &str) -> HidResult<DynDevice> {
        match self {
            DynApi::Native(api) => api.open_path(path).map(DynDevice::Native),
            DynApi::Nusb(api) => api.open_path(path).map(DynDevice::Nusb),
        }
    }

    pub(crate) fn open(
        &self,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> HidResult<DynDevice> {
        match self {
            DynApi::Native(api) => api
                .open(vendor_id, product_id, serial)
                .map(DynDevice::Native),
            DynApi::Nusb(api) => api.open(vendor_id, product_id, serial).map(DynDevice::Nusb),
        }
    }
}

impl DynDevice {
    pub(crate) fn write(&self, data: &[u8]) -> HidResult<usize> {
        forward_device!(self, dev => dev.write(data))
    }

    // The arm for an absent backend builds a `DynRead` variant out of an
    // uninhabited future, which reads as unreachable code because it is.
    #[allow(unreachable_code)]
    pub(crate) fn read_async<'a>(&'a self, buf: &'a mut [u8]) -> DynRead<'a> {
        match self {
            DynDevice::Native(dev) => DynRead::Native(dev.read_async(buf)),
            DynDevice::Nusb(dev) => DynRead::Nusb(dev.read_async(buf)),
        }
    }

    pub(crate) fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        forward_device!(self, dev => dev.send_feature_report(data))
    }

    pub(crate) fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        forward_device!(self, dev => dev.get_feature_report(buf))
    }

    pub(crate) fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        forward_device!(self, dev => dev.get_input_report(buf))
    }

    pub(crate) fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        forward_device!(self, dev => dev.get_manufacturer_string())
    }

    pub(crate) fn get_product_string(&self) -> HidResult<Option<String>> {
        forward_device!(self, dev => dev.get_product_string())
    }

    pub(crate) fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        forward_device!(self, dev => dev.get_serial_number_string())
    }

    pub(crate) fn get_indexed_string(&self, index: u32) -> HidResult<Option<String>> {
        forward_device!(self, dev => dev.get_indexed_string(index))
    }

    pub(crate) fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
        forward_device!(self, dev => dev.get_report_descriptor(buf))
    }

    pub(crate) fn get_device_info(&self) -> HidResult<DeviceInfo> {
        forward_device!(self, dev => dev.get_device_info())
    }
}

impl Future for DynRead<'_> {
    type Output = HidResult<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            DynRead::Native(read) => Pin::new(read).poll(cx),
            DynRead::Nusb(read) => Pin::new(read).poll(cx),
        }
    }
}

/// The error a backend-specific extension gets when another backend is selected.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn native_only(what: &str) -> HidError {
    HidError::Unsupported {
        message: format!("{what} is specific to the native backend"),
    }
}

#[cfg(target_os = "macos")]
impl DynApi {
    pub(crate) fn set_open_exclusive(&self, exclusive: bool) -> HidResult<()> {
        match self {
            DynApi::Native(api) => {
                api.set_open_exclusive(exclusive);
                Ok(())
            }
            DynApi::Nusb(_) => Err(native_only("set_open_exclusive")),
        }
    }

    pub(crate) fn open_exclusive(&self) -> HidResult<bool> {
        match self {
            DynApi::Native(api) => Ok(api.open_exclusive()),
            DynApi::Nusb(_) => Err(native_only("open_exclusive")),
        }
    }
}

#[cfg(target_os = "windows")]
impl DynDevice {
    pub(crate) fn container_id(&self) -> HidResult<[u8; 16]> {
        match self {
            DynDevice::Native(dev) => dev.container_id(),
            DynDevice::Nusb(_) => Err(native_only("container_id")),
        }
    }

    pub(crate) fn set_write_timeout(&self, timeout_ms: u32) -> HidResult<()> {
        match self {
            DynDevice::Native(dev) => {
                dev.set_write_timeout(timeout_ms);
                Ok(())
            }
            DynDevice::Nusb(_) => Err(native_only("set_write_timeout")),
        }
    }
}

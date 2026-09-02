//! Device enumeration metadata.

use core::fmt;

/// The underlying transport a HID device is attached through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum BusType {
    /// The transport could not be determined.
    #[default]
    Unknown,
    /// USB.
    Usb,
    /// Bluetooth or Bluetooth LE.
    Bluetooth,
    /// I2C.
    I2c,
    /// SPI.
    Spi,
}

impl fmt::Display for BusType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            BusType::Unknown => "Unknown",
            BusType::Usb => "USB",
            BusType::Bluetooth => "Bluetooth",
            BusType::I2c => "I2C",
            BusType::Spi => "SPI",
        };
        f.write_str(name)
    }
}

/// Information about a connected HID device, as returned by enumeration.
///
/// All strings are UTF-8; the backends convert from each platform's native
/// string encoding.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub(crate) path: String,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) serial_number: Option<String>,
    pub(crate) release_number: u16,
    pub(crate) manufacturer_string: Option<String>,
    pub(crate) product_string: Option<String>,
    pub(crate) usage_page: u16,
    pub(crate) usage: u16,
    pub(crate) interface_number: i32,
    pub(crate) bus_type: BusType,
}

impl DeviceInfo {
    /// Platform-specific device path, usable with `Hidra::open_path`.
    ///
    /// On Linux this is a `/dev/hidrawN` node, on Windows a device interface
    /// path, on macOS an `IORegistry` entry path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// USB vendor ID.
    pub fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    /// USB product ID.
    pub fn product_id(&self) -> u16 {
        self.product_id
    }

    /// Device serial number, if the device reports one.
    pub fn serial_number(&self) -> Option<&str> {
        self.serial_number.as_deref()
    }

    /// Device release number in binary-coded decimal (`bcdDevice`).
    pub fn release_number(&self) -> u16 {
        self.release_number
    }

    /// Manufacturer string, if the device reports one.
    pub fn manufacturer_string(&self) -> Option<&str> {
        self.manufacturer_string.as_deref()
    }

    /// Product string, if the device reports one.
    pub fn product_string(&self) -> Option<&str> {
        self.product_string.as_deref()
    }

    /// Usage page of the top-level collection this device node represents.
    pub fn usage_page(&self) -> u16 {
        self.usage_page
    }

    /// Usage of the top-level collection this device node represents.
    pub fn usage(&self) -> u16 {
        self.usage
    }

    /// USB interface number, or `-1` when not applicable.
    pub fn interface_number(&self) -> i32 {
        self.interface_number
    }

    /// The transport this device is attached through.
    pub fn bus_type(&self) -> BusType {
        self.bus_type
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl DeviceInfo {
    /// Whether this entry passes an enumeration filter, where a `vendor_id`
    /// or `product_id` of 0 is a wildcard (hidapi's `hid_enumerate` rule).
    pub(crate) fn matches(&self, vendor_id: u16, product_id: u16) -> bool {
        (vendor_id == 0 || self.vendor_id == vendor_id)
            && (product_id == 0 || self.product_id == product_id)
    }

    /// Fan this entry out to one copy per top-level usage pair, which is how
    /// hidapi reports a device whose report descriptor declares several
    /// application collections. With no usages (an unreadable descriptor) the
    /// entry is returned once, with `usage_page`/`usage` left at 0.
    ///
    /// The Windows backend has no use for this: its driver already exposes
    /// each top-level collection as a separate device interface path.
    #[cfg(any(
        feature = "nusb",
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    ))]
    pub(crate) fn per_usage(self, usages: &[(u16, u16)]) -> Vec<DeviceInfo> {
        let Some((last, rest)) = usages.split_last() else {
            return vec![self];
        };
        let mut out = Vec::with_capacity(usages.len());
        for &(usage_page, usage) in rest {
            out.push(DeviceInfo {
                usage_page,
                usage,
                ..self.clone()
            });
        }
        out.push(DeviceInfo {
            usage_page: last.0,
            usage: last.1,
            ..self
        });
        out
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn info(vendor_id: u16, product_id: u16) -> DeviceInfo {
        DeviceInfo {
            vendor_id,
            product_id,
            path: "p".into(),
            ..Default::default()
        }
    }

    #[test]
    fn zero_matches_any_id() {
        let dev = info(0x046d, 0xc216);
        assert!(dev.matches(0, 0));
        assert!(dev.matches(0x046d, 0));
        assert!(dev.matches(0, 0xc216));
        assert!(dev.matches(0x046d, 0xc216));
        assert!(!dev.matches(0x046e, 0));
        assert!(!dev.matches(0, 0xc217));
    }

    #[cfg(any(
        feature = "nusb",
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    ))]
    #[test]
    fn no_usages_yields_the_entry_unchanged() {
        let out = info(1, 2).per_usage(&[]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].usage_page(), out[0].usage()), (0, 0));
    }

    #[cfg(any(
        feature = "nusb",
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    ))]
    #[test]
    fn one_entry_per_usage_pair_sharing_the_rest() {
        let out = info(1, 2).per_usage(&[(0x01, 0x06), (0x0c, 0x01), (0xff00, 0x01)]);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter()
                .map(|d| (d.usage_page(), d.usage()))
                .collect::<Vec<_>>(),
            vec![(0x01, 0x06), (0x0c, 0x01), (0xff00, 0x01)]
        );
        assert!(out.iter().all(|d| d.vendor_id() == 1 && d.path() == "p"));
    }
}

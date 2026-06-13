//! Device enumeration metadata, mirroring hidapi's `hid_device_info`.

use core::fmt;

/// The underlying transport a HID device is attached through.
///
/// Mirrors hidapi's `hid_bus_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum BusType {
    #[default]
    Unknown,
    Usb,
    Bluetooth,
    I2c,
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
/// Mirrors hidapi's `hid_device_info`. All strings are UTF-8; hidapi's
/// `wchar_t` strings are converted by the backends.
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
    /// Platform-specific device path, usable with `HidApi::open_path`.
    ///
    /// On Linux this is a `/dev/hidrawN` node, on Windows a device interface
    /// path, on macOS an IORegistry entry path.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    pub fn product_id(&self) -> u16 {
        self.product_id
    }

    pub fn serial_number(&self) -> Option<&str> {
        self.serial_number.as_deref()
    }

    /// Device release number in binary-coded decimal (`bcdDevice`).
    pub fn release_number(&self) -> u16 {
        self.release_number
    }

    pub fn manufacturer_string(&self) -> Option<&str> {
        self.manufacturer_string.as_deref()
    }

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

    pub fn bus_type(&self) -> BusType {
        self.bus_type
    }
}

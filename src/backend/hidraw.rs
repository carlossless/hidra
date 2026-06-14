//! Linux backend: `/dev/hidraw*` device nodes, enumerated through sysfs.
//!
//! Unlike hidapi's hidraw backend this does not link libudev; enumeration
//! reads `/sys/class/hidraw` directly, which is what libudev does underneath.

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::descriptor::ReportDescriptor;
use crate::error::{HidError, HidResult};
use crate::{BusType, DeviceInfo, MAX_REPORT_DESCRIPTOR_SIZE};

// --- ioctl plumbing ---------------------------------------------------------
//
// The hidraw ioctls are not exported by the libc crate, so the _IOC encoding
// is replicated here (linux/ioctl.h).

#[cfg(not(any(
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc64",
)))]
mod ioc {
    pub const SIZEBITS: u32 = 14; // dir takes the remaining 2 bits
    pub const WRITE: u32 = 1;
    pub const READ: u32 = 2;
}

#[cfg(any(
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc64",
))]
mod ioc {
    pub const SIZEBITS: u32 = 13; // dir takes the remaining 3 bits
    pub const READ: u32 = 2;
    pub const WRITE: u32 = 4;
}

const NRBITS: u32 = 8;
const TYPEBITS: u32 = 8;
const NRSHIFT: u32 = 0;
const TYPESHIFT: u32 = NRSHIFT + NRBITS;
const SIZESHIFT: u32 = TYPESHIFT + TYPEBITS;
const DIRSHIFT: u32 = SIZESHIFT + ioc::SIZEBITS;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((dir << DIRSHIFT)
        | ((ty as u32) << TYPESHIFT)
        | ((nr as u32) << NRSHIFT)
        | ((size as u32) << SIZESHIFT)) as libc::c_ulong
}

const HIDRAW_MAGIC: u8 = b'H';

const fn hidiocgrdescsize() -> libc::c_ulong {
    ioc(
        ioc::READ,
        HIDRAW_MAGIC,
        0x01,
        core::mem::size_of::<libc::c_int>(),
    )
}

#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; MAX_REPORT_DESCRIPTOR_SIZE],
}

const fn hidiocgrdesc() -> libc::c_ulong {
    ioc(
        ioc::READ,
        HIDRAW_MAGIC,
        0x02,
        core::mem::size_of::<HidrawReportDescriptor>(),
    )
}

const fn hidiocsfeature(len: usize) -> libc::c_ulong {
    ioc(ioc::WRITE | ioc::READ, HIDRAW_MAGIC, 0x06, len)
}

const fn hidiocgfeature(len: usize) -> libc::c_ulong {
    ioc(ioc::WRITE | ioc::READ, HIDRAW_MAGIC, 0x07, len)
}

/// `HIDIOCGINPUT`, available since Linux 5.11.
const fn hidiocginput(len: usize) -> libc::c_ulong {
    ioc(ioc::WRITE | ioc::READ, HIDRAW_MAGIC, 0x0A, len)
}

// --- sysfs enumeration -------------------------------------------------------

/// Bus numbers from `linux/input.h`.
fn bus_type_from_id(bus: u32) -> BusType {
    match bus {
        0x03 => BusType::Usb,
        0x05 => BusType::Bluetooth,
        0x18 => BusType::I2c,
        0x1C => BusType::Spi,
        _ => BusType::Unknown,
    }
}

/// Parsed `uevent` of a HID device sysfs node.
#[derive(Debug, Default, PartialEq)]
struct Uevent {
    bus: u32,
    vendor_id: u16,
    product_id: u16,
    name: Option<String>,
    serial: Option<String>,
}

fn parse_uevent(content: &str) -> Option<Uevent> {
    let mut ev = Uevent::default();
    let mut have_id = false;
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "HID_ID" => {
                // e.g. 0003:0000046D:0000C216
                let mut parts = value.split(':');
                let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
                let vid = u32::from_str_radix(parts.next()?, 16).ok()?;
                let pid = u32::from_str_radix(parts.next()?, 16).ok()?;
                ev.bus = bus;
                ev.vendor_id = vid as u16;
                ev.product_id = pid as u16;
                have_id = true;
            }
            "HID_NAME" if !value.is_empty() => ev.name = Some(value.to_string()),
            "HID_UNIQ" if !value.is_empty() => ev.serial = Some(value.to_string()),
            _ => {}
        }
    }
    have_id.then_some(ev)
}

fn read_sysfs_string(dir: &Path, file: &str) -> Option<String> {
    let s = fs::read_to_string(dir.join(file)).ok()?;
    let s = s.trim_end_matches('\n');
    (!s.is_empty()).then(|| s.to_string())
}

fn read_sysfs_hex(dir: &Path, file: &str) -> Option<u32> {
    let s = read_sysfs_string(dir, file)?;
    u32::from_str_radix(s.trim(), 16).ok()
}

/// USB metadata gathered by walking up from the HID device directory.
#[derive(Debug, Default)]
struct UsbInfo {
    manufacturer: Option<String>,
    product: Option<String>,
    serial: Option<String>,
    release_number: u16,
    interface_number: i32,
}

fn collect_usb_info(hid_dev_dir: &Path) -> Option<UsbInfo> {
    let mut info = UsbInfo {
        interface_number: -1,
        ..Default::default()
    };
    let mut dir = hid_dev_dir.canonicalize().ok()?;
    for _ in 0..8 {
        if info.interface_number < 0 {
            if let Some(n) = read_sysfs_hex(&dir, "bInterfaceNumber") {
                info.interface_number = n as i32;
            }
        }
        if dir.join("idVendor").exists() && dir.join("idProduct").exists() {
            info.manufacturer = read_sysfs_string(&dir, "manufacturer");
            info.product = read_sysfs_string(&dir, "product");
            info.serial = read_sysfs_string(&dir, "serial");
            info.release_number = read_sysfs_hex(&dir, "bcdDevice").unwrap_or(0) as u16;
            return Some(info);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Build the `DeviceInfo` entries for one hidraw node. Devices with several
/// top-level collections produce one entry per collection, like hidapi.
fn device_infos(hid_dev_dir: &Path, dev_path: &str) -> Option<Vec<DeviceInfo>> {
    let uevent = fs::read_to_string(hid_dev_dir.join("uevent")).ok()?;
    let uevent = parse_uevent(&uevent)?;

    let bus_type = bus_type_from_id(uevent.bus);
    let mut info = DeviceInfo {
        path: dev_path.to_string(),
        vendor_id: uevent.vendor_id,
        product_id: uevent.product_id,
        bus_type,
        interface_number: -1,
        ..Default::default()
    };

    match bus_type {
        BusType::Usb => {
            if let Some(usb) = collect_usb_info(hid_dev_dir) {
                info.manufacturer_string = usb.manufacturer;
                info.product_string = usb.product;
                info.serial_number = usb.serial;
                info.release_number = usb.release_number;
                info.interface_number = usb.interface_number;
            } else {
                info.product_string = uevent.name;
                info.serial_number = uevent.serial;
            }
        }
        _ => {
            // Bluetooth, I2C, SPI and virtual devices carry their identity in
            // the HID uevent, matching hidapi's behavior.
            info.product_string = uevent.name;
            info.serial_number = uevent.serial;
        }
    }

    let usages = fs::read(hid_dev_dir.join("report_descriptor"))
        .ok()
        .and_then(|d| ReportDescriptor::parse(&d).ok())
        .map(|d| d.top_level_usages())
        .unwrap_or_default();

    let mut entries = Vec::with_capacity(usages.len().max(1));
    match usages.as_slice() {
        [] => entries.push(info),
        [first @ .., last] => {
            for &(page, usage) in first {
                let mut e = info.clone();
                e.usage_page = page;
                e.usage = usage;
                entries.push(e);
            }
            info.usage_page = last.0;
            info.usage = last.1;
            entries.push(info);
        }
    }
    Some(entries)
}

// --- backend API -------------------------------------------------------------

pub(crate) struct HidrawApi;

impl HidrawApi {
    pub fn new() -> HidResult<Self> {
        Ok(HidrawApi)
    }

    pub fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
        let mut result = Vec::new();
        let entries = match fs::read_dir("/sys/class/hidraw") {
            Ok(entries) => entries,
            // No HID support compiled in / nothing ever plugged: empty list.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(e) => return Err(HidError::io("reading /sys/class/hidraw", e)),
        };
        let mut nodes: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        nodes.sort();
        for node in nodes {
            let hid_dev_dir = PathBuf::from("/sys/class/hidraw")
                .join(&node)
                .join("device");
            let dev_path = format!("/dev/{node}");
            let Some(infos) = device_infos(&hid_dev_dir, &dev_path) else {
                continue;
            };
            for info in infos {
                let vid_ok = vendor_id == 0 || info.vendor_id == vendor_id;
                let pid_ok = product_id == 0 || info.product_id == product_id;
                if vid_ok && pid_ok {
                    result.push(info);
                }
            }
        }
        Ok(result)
    }

    pub fn open(
        &self,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> HidResult<HidrawDevice> {
        let candidates = self.enumerate(vendor_id, product_id)?;
        let info = candidates
            .into_iter()
            .find(|info| match serial {
                Some(s) => info.serial_number.as_deref() == Some(s),
                None => true,
            })
            .ok_or(HidError::DeviceNotFound)?;
        self.open_path(&info.path)
    }

    pub fn open_path(&self, path: &str) -> HidResult<HidrawDevice> {
        HidrawDevice::open(path)
    }
}

// --- device handle ------------------------------------------------------------

pub(crate) struct HidrawDevice {
    fd: OwnedFd,
    // Part of the backend contract; the wrapper now reads input via
    // `read_async`, so the blocking-mode state is unused on this path.
    #[allow(dead_code)]
    blocking: AtomicBool,
    /// Whether the device's report descriptor declares numbered reports.
    /// hidraw expects `write()` data to omit the leading 0 byte for devices
    /// without report IDs.
    numbered_reports: bool,
    /// Canonical sysfs HID device directory, for metadata lookups.
    sysfs_hid_dir: PathBuf,
    /// `/dev/hidrawN`.
    dev_path: String,
}

impl HidrawDevice {
    fn open(path: &str) -> HidResult<Self> {
        let c_path = std::ffi::CString::new(path).map_err(|_| HidError::InvalidData {
            message: "path contains NUL".into(),
        })?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(match err.kind() {
                std::io::ErrorKind::NotFound => HidError::DeviceNotFound,
                _ => HidError::OpenFailed {
                    message: format!("{path}: {err}"),
                },
            });
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let sysfs_hid_dir = sysfs_dir_for_fd(fd.as_raw_fd())?;
        let dev = HidrawDevice {
            fd,
            blocking: AtomicBool::new(true),
            numbered_reports: false,
            sysfs_hid_dir,
            dev_path: path.to_string(),
        };

        // hidapi probes this once at open time too.
        let mut desc_buf = [0u8; MAX_REPORT_DESCRIPTOR_SIZE];
        let numbered = dev
            .get_report_descriptor(&mut desc_buf)
            .ok()
            .and_then(|len| ReportDescriptor::parse(&desc_buf[..len]).ok())
            .map(|d| d.uses_report_ids())
            .unwrap_or(false);

        Ok(HidrawDevice {
            numbered_reports: numbered,
            ..dev
        })
    }

    fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn write(&self, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "write data must contain a report ID byte".into(),
            });
        }
        // For devices without numbered reports, hidraw wants the bare
        // payload; the caller passed a leading 0 per the hidapi convention.
        let (payload, skipped) = if !self.numbered_reports && data[0] == 0 && data.len() > 1 {
            (&data[1..], 1)
        } else {
            (data, 0)
        };
        let res = loop {
            let r = unsafe { libc::write(self.raw_fd(), payload.as_ptr().cast(), payload.len()) };
            if r >= 0 {
                break r as usize;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(HidError::io("hidraw write", err));
        };
        Ok(res + skipped)
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn read(&self, buf: &mut [u8]) -> HidResult<usize> {
        let timeout = if self.blocking.load(Ordering::Relaxed) {
            -1
        } else {
            0
        };
        self.read_timeout(buf, timeout)
    }

    /// Read one input report without ever returning `Ok(0)`: resolves when a
    /// report arrives, fails with [`HidError::Disconnected`] when the device
    /// goes away. Wake-ups come from the crate's [`reactor`](super::reactor).
    pub fn read_async<'a>(&'a self, buf: &'a mut [u8]) -> ReadAsync<'a> {
        ReadAsync { dev: self, buf }
    }

    /// Non-blocking read attempt: `Ok(None)` when no report is queued.
    fn try_read_now(&self, buf: &mut [u8]) -> HidResult<Option<usize>> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            });
        }
        match self.read_timeout(buf, 0)? {
            0 => Ok(None),
            n => Ok(Some(n)),
        }
    }

    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            });
        }
        let mut pollfd = libc::pollfd {
            fd: self.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let res = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if res < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(HidError::io("poll on hidraw device", err));
            }
            if res == 0 {
                return Ok(0); // timeout
            }
            break;
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(HidError::Disconnected);
        }
        let res = unsafe { libc::read(self.raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if res < 0 {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINPROGRESS) => Ok(0),
                Some(libc::EIO) | Some(libc::ENODEV) => Err(HidError::Disconnected),
                _ => Err(HidError::io("hidraw read", err)),
            };
        }
        Ok(res as usize)
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn set_blocking_mode(&self, blocking: bool) -> HidResult<()> {
        self.blocking.store(blocking, Ordering::Relaxed);
        Ok(())
    }

    pub fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "feature report must contain a report ID byte".into(),
            });
        }
        let res = unsafe {
            libc::ioctl(
                self.raw_fd(),
                hidiocsfeature(data.len()) as _,
                data.as_ptr(),
            )
        };
        if res < 0 {
            return Err(HidError::last_os_error("HIDIOCSFEATURE"));
        }
        Ok(())
    }

    pub fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "buffer must contain a report ID byte".into(),
            });
        }
        let res = unsafe {
            libc::ioctl(
                self.raw_fd(),
                hidiocgfeature(buf.len()) as _,
                buf.as_mut_ptr(),
            )
        };
        if res < 0 {
            return Err(HidError::last_os_error("HIDIOCGFEATURE"));
        }
        Ok(res as usize)
    }

    pub fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "buffer must contain a report ID byte".into(),
            });
        }
        let res = unsafe {
            libc::ioctl(
                self.raw_fd(),
                hidiocginput(buf.len()) as _,
                buf.as_mut_ptr(),
            )
        };
        if res < 0 {
            let err = std::io::Error::last_os_error();
            return Err(match err.raw_os_error() {
                // Kernel < 5.11.
                Some(libc::EINVAL) | Some(libc::ENOTTY) => HidError::Unsupported {
                    message: "HIDIOCGINPUT requires Linux 5.11+".into(),
                },
                _ => HidError::io("HIDIOCGINPUT", err),
            });
        }
        Ok(res as usize)
    }

    pub fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        Ok(self.get_device_info()?.manufacturer_string)
    }

    pub fn get_product_string(&self) -> HidResult<Option<String>> {
        Ok(self.get_device_info()?.product_string)
    }

    pub fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        Ok(self.get_device_info()?.serial_number)
    }

    pub fn get_indexed_string(&self, _index: u32) -> HidResult<Option<String>> {
        // Same as hidapi's hidraw backend: not available without raw USB
        // access. The `nusb` feature backend supports it.
        Err(HidError::Unsupported {
            message: "indexed strings are not available via hidraw; use the usb backend".into(),
        })
    }

    pub fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
        let mut size: libc::c_int = 0;
        let res = unsafe { libc::ioctl(self.raw_fd(), hidiocgrdescsize() as _, &mut size) };
        if res < 0 {
            return Err(HidError::last_os_error("HIDIOCGRDESCSIZE"));
        }
        let mut desc = HidrawReportDescriptor {
            size: size as u32,
            value: [0; MAX_REPORT_DESCRIPTOR_SIZE],
        };
        let res = unsafe { libc::ioctl(self.raw_fd(), hidiocgrdesc() as _, &mut desc) };
        if res < 0 {
            return Err(HidError::last_os_error("HIDIOCGRDESC"));
        }
        let len = (desc.size as usize).min(buf.len());
        buf[..len].copy_from_slice(&desc.value[..len]);
        Ok(len)
    }

    pub fn get_device_info(&self) -> HidResult<DeviceInfo> {
        let mut infos = device_infos(&self.sysfs_hid_dir, &self.dev_path)
            .ok_or_else(|| HidError::backend("failed to read device metadata from sysfs"))?;
        // For multi-collection devices hidapi returns the first entry.
        Ok(infos.remove(0))
    }
}

/// Future returned by [`HidrawDevice::read_async`].
///
/// Cancel-safe: dropping it before completion leaves any pending report in
/// the kernel's hidraw queue (the read syscall only happens when the fd is
/// already readable).
pub(crate) struct ReadAsync<'a> {
    dev: &'a HidrawDevice,
    buf: &'a mut [u8],
}

impl std::future::Future for ReadAsync<'_> {
    type Output = HidResult<usize>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        match this.dev.try_read_now(this.buf) {
            Ok(Some(n)) => std::task::Poll::Ready(Ok(n)),
            Ok(None) => {
                // The reactor polls level-triggered, so a report arriving
                // between the read attempt above and this registration is
                // still observed on the loop's next iteration, no re-check
                // needed.
                super::reactor::Reactor::global().register(this.dev.raw_fd(), cx.waker());
                std::task::Poll::Pending
            }
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
}

/// Resolve the sysfs HID device directory for an open hidraw fd via
/// `/sys/dev/char/<major>:<minor>/device`.
fn sysfs_dir_for_fd(fd: RawFd) -> HidResult<PathBuf> {
    let mut stat = unsafe { core::mem::zeroed::<libc::stat>() };
    let res = unsafe { libc::fstat(fd, &mut stat) };
    if res < 0 {
        return Err(HidError::last_os_error("fstat on hidraw device"));
    }
    let major = libc::major(stat.st_rdev);
    let minor = libc::minor(stat.st_rdev);
    let class_dir = PathBuf::from(format!("/sys/dev/char/{major}:{minor}"));
    class_dir
        .join("device")
        .canonicalize()
        .map_err(|e| HidError::io("resolving sysfs device directory", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usb_uevent() {
        let content = "DRIVER=hid-generic\n\
                       HID_ID=0003:0000046D:0000C216\n\
                       HID_NAME=Logitech Logitech Dual Action\n\
                       HID_PHYS=usb-0000:00:14.0-2/input0\n\
                       HID_UNIQ=\n\
                       MODALIAS=hid:b0003g0001v0000046Dp0000C216\n";
        let ev = parse_uevent(content).unwrap();
        assert_eq!(ev.bus, 3);
        assert_eq!(ev.vendor_id, 0x046D);
        assert_eq!(ev.product_id, 0xC216);
        assert_eq!(ev.name.as_deref(), Some("Logitech Logitech Dual Action"));
        assert_eq!(ev.serial, None);
        assert_eq!(bus_type_from_id(ev.bus), BusType::Usb);
    }

    #[test]
    fn parses_bluetooth_uevent() {
        let content = "HID_ID=0005:0000054C:00000268\n\
                       HID_NAME=PLAYSTATION(R)3 Controller\n\
                       HID_UNIQ=00:19:c1:5c:f5:11\n";
        let ev = parse_uevent(content).unwrap();
        assert_eq!(bus_type_from_id(ev.bus), BusType::Bluetooth);
        assert_eq!(ev.serial.as_deref(), Some("00:19:c1:5c:f5:11"));
    }

    #[test]
    fn rejects_uevent_without_hid_id() {
        assert_eq!(parse_uevent("DRIVER=hid-generic\n"), None);
    }

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        // Values taken from a C program using <linux/hidraw.h> on x86_64.
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(hidiocgrdescsize(), 0x80044801);
            assert_eq!(hidiocgrdesc(), 0x90044802);
            assert_eq!(hidiocsfeature(8), 0xC0084806);
            assert_eq!(hidiocgfeature(8), 0xC0084807);
        }
    }

    #[test]
    fn enumeration_does_not_panic() {
        // The machine may or may not have HID devices; either way this must
        // return cleanly.
        let api = HidrawApi::new().unwrap();
        let devices = api.enumerate(0, 0).unwrap();
        for d in &devices {
            assert!(d.path().starts_with("/dev/hidraw"));
        }
    }
}

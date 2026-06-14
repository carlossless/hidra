//! macOS backend: IOHIDManager / IOHIDDevice through hand-written IOKit FFI.
//!
//! Mirrors hidapi's `mac/hid.c`:
//!
//! * Device paths use the modern hidapi form `DevSrvsID:<registry-entry-id>`.
//! * Each open device runs a dedicated read thread pumping a private
//!   CFRunLoop mode; input reports land in a bounded queue (30 entries,
//!   oldest dropped first).
//! * Exclusive open (`hid_darwin_set_open_exclusive`) maps to
//!   `kIOHIDOptionsTypeSeizeDevice`; the default is shared, matching
//!   hidapi >= 0.12.
//!
//! CoreFoundation declarations come from `core-foundation-sys` (which links
//! the framework); the IOKit symbols below link `IOKit.framework` directly.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::Waker;
use std::thread;
use std::time::{Duration, Instant};

use core_foundation_sys::array::{
    CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex, CFArrayRef,
};
use core_foundation_sys::base::{
    kCFAllocatorDefault, CFAllocatorRef, CFGetTypeID, CFIndex, CFRelease, CFTypeRef,
};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef};
use core_foundation_sys::dictionary::{
    CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef, CFMutableDictionaryRef,
};
use core_foundation_sys::number::{
    kCFNumberSInt32Type, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef,
};
use core_foundation_sys::runloop::{
    kCFRunLoopDefaultMode, kCFRunLoopRunFinished, kCFRunLoopRunStopped, CFRunLoopGetCurrent,
    CFRunLoopGetMain, CFRunLoopRef, CFRunLoopRunInMode, CFRunLoopStop, CFRunLoopWakeUp,
};
use core_foundation_sys::set::{CFSetGetCount, CFSetGetValues, CFSetRef};
use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringGetCString, CFStringGetCStringPtr,
    CFStringGetLength, CFStringGetMaximumSizeForEncoding, CFStringGetTypeID, CFStringRef,
};

use crate::error::{HidError, HidResult};
use crate::{BusType, DeviceInfo};

// --- IOKit FFI ----------------------------------------------------------------
//
// Hand-written declarations for the small IOHIDManager/IOHIDDevice/IORegistry
// surface this backend needs (IOKit/hid/IOHIDLib.h, IOKit/IOKitLib.h).

#[allow(non_camel_case_types)]
type io_object_t = u32; // mach_port_t
#[allow(non_camel_case_types)]
type io_service_t = io_object_t;
type IOOptionBits = u32;
type IOReturn = i32; // kern_return_t
type IOHIDManagerRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type IOHIDReportType = u32;

/// `IOHIDReportCallback` from IOHIDDevice.h.
type IOHIDReportCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    report_type: IOHIDReportType,
    report_id: u32,
    report: *mut u8,
    report_length: CFIndex,
);

/// `IOHIDCallback` from IOHIDDevice.h (used for removal notification).
type IOHIDCallback =
    unsafe extern "C" fn(context: *mut c_void, result: IOReturn, sender: *mut c_void);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDManagerCreate(allocator: CFAllocatorRef, options: IOOptionBits) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFDictionaryRef);
    fn IOHIDManagerCopyDevices(manager: IOHIDManagerRef) -> CFSetRef;

    fn IOHIDDeviceCreate(allocator: CFAllocatorRef, service: io_service_t) -> IOHIDDeviceRef;
    fn IOHIDDeviceOpen(device: IOHIDDeviceRef, options: IOOptionBits) -> IOReturn;
    fn IOHIDDeviceClose(device: IOHIDDeviceRef, options: IOOptionBits) -> IOReturn;
    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;
    fn IOHIDDeviceSetReport(
        device: IOHIDDeviceRef,
        report_type: IOHIDReportType,
        report_id: CFIndex,
        report: *const u8,
        report_length: CFIndex,
    ) -> IOReturn;
    fn IOHIDDeviceGetReport(
        device: IOHIDDeviceRef,
        report_type: IOHIDReportType,
        report_id: CFIndex,
        report: *mut u8,
        report_length: *mut CFIndex,
    ) -> IOReturn;
    fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        report_length: CFIndex,
        callback: Option<IOHIDReportCallback>,
        context: *mut c_void,
    );
    fn IOHIDDeviceRegisterRemovalCallback(
        device: IOHIDDeviceRef,
        callback: Option<IOHIDCallback>,
        context: *mut c_void,
    );
    fn IOHIDDeviceScheduleWithRunLoop(
        device: IOHIDDeviceRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn IOHIDDeviceUnscheduleFromRunLoop(
        device: IOHIDDeviceRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn IOHIDDeviceGetService(device: IOHIDDeviceRef) -> io_service_t;

    fn IORegistryEntryGetRegistryEntryID(entry: io_object_t, entry_id: *mut u64) -> IOReturn;
    fn IORegistryEntryIDMatching(entry_id: u64) -> CFMutableDictionaryRef;
    /// Search a registry entry (and, with the parent-iterate option, its
    /// ancestors) for a property. Returns a +1 CF reference or null.
    fn IORegistryEntrySearchCFProperty(
        entry: io_service_t,
        plane: *const c_char,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    /// Consumes one reference of `matching`.
    fn IOServiceGetMatchingService(
        main_port: io_object_t,
        matching: CFDictionaryRef,
    ) -> io_service_t;
    fn IOObjectRelease(object: io_object_t) -> IOReturn;
}

/// `kIOReturnSuccess` / `KERN_SUCCESS`.
const IO_RETURN_SUCCESS: IOReturn = 0;
/// `kIORegistryIterateRecursively | kIORegistryIterateParents`, search the
/// entry and walk up its ancestors.
const IO_REGISTRY_ITERATE_PARENTS_RECURSIVELY: u32 = 0x1 | 0x2;
/// `kIOMasterPortDefault` / `kIOMainPortDefault`: 0 means "use the default".
const IO_MASTER_PORT_DEFAULT: io_object_t = 0;
/// `kIOHIDOptionsTypeNone` (shared open).
const IOHID_OPTIONS_TYPE_NONE: IOOptionBits = 0;
/// `kIOHIDOptionsTypeSeizeDevice` (exclusive open).
const IOHID_OPTIONS_TYPE_SEIZE_DEVICE: IOOptionBits = 1;
/// `kIOHIDManagerOptionNone`.
const IOHID_MANAGER_OPTION_NONE: IOOptionBits = 0;
/// `kIOHIDReportTypeInput`.
const IOHID_REPORT_TYPE_INPUT: IOHIDReportType = 0;
/// `kIOHIDReportTypeOutput`.
const IOHID_REPORT_TYPE_OUTPUT: IOHIDReportType = 1;
/// `kIOHIDReportTypeFeature`.
const IOHID_REPORT_TYPE_FEATURE: IOHIDReportType = 2;

// IOHIDDevice property keys (IOKit/hid/IOHIDKeys.h).
const KEY_VENDOR_ID: &str = "VendorID"; // kIOHIDVendorIDKey
const KEY_PRODUCT_ID: &str = "ProductID"; // kIOHIDProductIDKey
const KEY_SERIAL_NUMBER: &str = "SerialNumber"; // kIOHIDSerialNumberKey
const KEY_MANUFACTURER: &str = "Manufacturer"; // kIOHIDManufacturerKey
const KEY_PRODUCT: &str = "Product"; // kIOHIDProductKey
const KEY_VERSION_NUMBER: &str = "VersionNumber"; // kIOHIDVersionNumberKey
const KEY_PRIMARY_USAGE_PAGE: &str = "PrimaryUsagePage"; // kIOHIDPrimaryUsagePageKey
const KEY_PRIMARY_USAGE: &str = "PrimaryUsage"; // kIOHIDPrimaryUsageKey
const KEY_DEVICE_USAGE_PAIRS: &str = "DeviceUsagePairs"; // kIOHIDDeviceUsagePairsKey
const KEY_DEVICE_USAGE_PAGE: &str = "DeviceUsagePage"; // kIOHIDDeviceUsagePageKey
const KEY_DEVICE_USAGE: &str = "DeviceUsage"; // kIOHIDDeviceUsageKey
const KEY_TRANSPORT: &str = "Transport"; // kIOHIDTransportKey
const KEY_MAX_INPUT_REPORT_SIZE: &str = "MaxInputReportSize"; // kIOHIDMaxInputReportSizeKey
const KEY_REPORT_DESCRIPTOR: &str = "ReportDescriptor"; // kIOHIDReportDescriptorKey

/// Device path prefix, identical to modern hidapi (`DevSrvsID:%llu`).
const PATH_PREFIX: &str = "DevSrvsID:";

/// Unread input reports kept per device before the oldest is dropped,
/// matching hidapi's queue cap.
const MAX_QUEUED_REPORTS: usize = 30;

// --- pure helpers --------------------------------------------------------------

/// Parse a `DevSrvsID:<decimal id>` path into the IORegistry entry ID.
fn parse_dev_srvs_id(path: &str) -> Option<u64> {
    path.strip_prefix(PATH_PREFIX)?.parse().ok()
}

/// Format an IORegistry entry ID as a `DevSrvsID:` path.
fn format_dev_srvs_id(entry_id: u64) -> String {
    format!("{PATH_PREFIX}{entry_id}")
}

/// Map a `kIOHIDTransportKey` value onto [`BusType`].
///
/// IOHIDKeys.h spells Bluetooth LE as `BluetoothLowEnergy`; some stacks
/// report it with spaces, so both are accepted.
fn bus_type_from_transport(transport: &str) -> BusType {
    match transport {
        "USB" => BusType::Usb,
        "Bluetooth" | "BluetoothLowEnergy" | "Bluetooth Low Energy" => BusType::Bluetooth,
        "I2C" => BusType::I2c,
        "SPI" => BusType::Spi,
        _ => BusType::Unknown,
    }
}

// --- CoreFoundation helpers -----------------------------------------------------

/// Create a CFString from a Rust string. The caller releases it.
unsafe fn cfstr(s: &str) -> CFStringRef {
    let c = CString::new(s).expect("CF key contains NUL");
    CFStringCreateWithCString(kCFAllocatorDefault, c.as_ptr(), kCFStringEncodingUTF8)
}

/// Convert a CF object to `String` if it is a CFString.
unsafe fn cfstring_to_string(value: CFTypeRef) -> Option<String> {
    if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
        return None;
    }
    let s = value as CFStringRef;
    // Fast path: CF may expose its internal buffer directly.
    let ptr = CFStringGetCStringPtr(s, kCFStringEncodingUTF8);
    if !ptr.is_null() {
        return Some(CStr::from_ptr(ptr).to_string_lossy().into_owned());
    }
    let max = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), kCFStringEncodingUTF8) + 1;
    let mut buf = vec![0u8; max as usize];
    if CFStringGetCString(
        s,
        buf.as_mut_ptr() as *mut c_char,
        max,
        kCFStringEncodingUTF8,
    ) == 0
    {
        return None;
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(nul);
    String::from_utf8(buf).ok()
}

/// Convert a CF object to `i32` if it is a CFNumber.
unsafe fn cfnumber_to_i32(value: CFTypeRef) -> Option<i32> {
    if value.is_null() || CFGetTypeID(value) != CFNumberGetTypeID() {
        return None;
    }
    let mut out: i32 = 0;
    CFNumberGetValue(
        value as CFNumberRef,
        kCFNumberSInt32Type,
        &mut out as *mut i32 as *mut c_void,
    )
    .then_some(out)
}

/// Fetch a device property. Per the CF "Get rule" the returned reference is
/// not owned and must not be released.
unsafe fn device_property(device: IOHIDDeviceRef, key: &str) -> CFTypeRef {
    let key = cfstr(key);
    let value = IOHIDDeviceGetProperty(device, key);
    CFRelease(key as CFTypeRef);
    value
}

unsafe fn string_property(device: IOHIDDeviceRef, key: &str) -> Option<String> {
    cfstring_to_string(device_property(device, key))
}

unsafe fn i32_property(device: IOHIDDeviceRef, key: &str) -> Option<i32> {
    cfnumber_to_i32(device_property(device, key))
}

/// IORegistry entry ID of the device's backing service.
unsafe fn registry_entry_id(device: IOHIDDeviceRef) -> Option<u64> {
    // IOHIDDeviceGetService does not transfer ownership; nothing to release.
    let service = IOHIDDeviceGetService(device);
    if service == 0 {
        return None;
    }
    let mut entry_id = 0u64;
    (IORegistryEntryGetRegistryEntryID(service, &mut entry_id) == IO_RETURN_SUCCESS)
        .then_some(entry_id)
}

/// Build the `DeviceInfo` entries for one IOHIDDevice: the primary usage pair
/// first, then one entry per additional pair in `kIOHIDDeviceUsagePairsKey`,
/// exactly like hidapi.
/// The USB `bInterfaceNumber` of a HID device, found by searching up the
/// IOService plane to the owning `IOUSBHostInterface`. Returns `-1` for
/// devices with no USB interface ancestor (Bluetooth, etc.), matching what
/// hidapi reports on macOS.
unsafe fn usb_interface_number(device: IOHIDDeviceRef) -> i32 {
    let service = IOHIDDeviceGetService(device);
    if service == 0 {
        return -1;
    }
    // `kIOServicePlane` is the C string "IOService".
    let plane = b"IOService\0";
    let key = cfstr("bInterfaceNumber");
    let value = IORegistryEntrySearchCFProperty(
        service,
        plane.as_ptr() as *const c_char,
        key,
        kCFAllocatorDefault,
        IO_REGISTRY_ITERATE_PARENTS_RECURSIVELY,
    );
    CFRelease(key as CFTypeRef);
    if value.is_null() {
        return -1;
    }
    let n = cfnumber_to_i32(value).unwrap_or(-1);
    CFRelease(value);
    n
}

unsafe fn device_infos(device: IOHIDDeviceRef) -> Vec<DeviceInfo> {
    let info = DeviceInfo {
        // hidapi falls back to an empty path when the registry ID is
        // unavailable.
        path: registry_entry_id(device)
            .map(format_dev_srvs_id)
            .unwrap_or_default(),
        vendor_id: i32_property(device, KEY_VENDOR_ID).unwrap_or(0) as u16,
        product_id: i32_property(device, KEY_PRODUCT_ID).unwrap_or(0) as u16,
        serial_number: string_property(device, KEY_SERIAL_NUMBER),
        release_number: i32_property(device, KEY_VERSION_NUMBER).unwrap_or(0) as u16,
        manufacturer_string: string_property(device, KEY_MANUFACTURER),
        product_string: string_property(device, KEY_PRODUCT),
        usage_page: i32_property(device, KEY_PRIMARY_USAGE_PAGE).unwrap_or(0) as u16,
        usage: i32_property(device, KEY_PRIMARY_USAGE).unwrap_or(0) as u16,
        // Found by walking up to the owning USB interface; -1 for non-USB
        // transports, matching hidapi on macOS.
        interface_number: usb_interface_number(device),
        bus_type: string_property(device, KEY_TRANSPORT)
            .as_deref()
            .map(bus_type_from_transport)
            .unwrap_or(BusType::Unknown),
    };

    let mut entries = vec![info.clone()];

    let pairs = device_property(device, KEY_DEVICE_USAGE_PAIRS);
    if !pairs.is_null() && CFGetTypeID(pairs) == CFArrayGetTypeID() {
        let pairs = pairs as CFArrayRef;
        let page_key = cfstr(KEY_DEVICE_USAGE_PAGE);
        let usage_key = cfstr(KEY_DEVICE_USAGE);
        for i in 0..CFArrayGetCount(pairs) {
            let dict = CFArrayGetValueAtIndex(pairs, i) as CFTypeRef;
            if dict.is_null() || CFGetTypeID(dict) != CFDictionaryGetTypeID() {
                continue;
            }
            let dict = dict as CFDictionaryRef;
            let page = cfnumber_to_i32(CFDictionaryGetValue(dict, page_key as *const c_void));
            let usage = cfnumber_to_i32(CFDictionaryGetValue(dict, usage_key as *const c_void));
            let (Some(page), Some(usage)) = (page, usage) else {
                continue;
            };
            let (page, usage) = (page as u16, usage as u16);
            if page == info.usage_page && usage == info.usage {
                continue; // primary pair, already added
            }
            let mut entry = info.clone();
            entry.usage_page = page;
            entry.usage = usage;
            entries.push(entry);
        }
        CFRelease(page_key as CFTypeRef);
        CFRelease(usage_key as CFTypeRef);
    }

    entries
}

// --- backend API -----------------------------------------------------------------

pub(crate) struct MacApi {
    /// Whether `open`/`open_path` seize the device
    /// (`hid_darwin_set_open_exclusive`). Defaults to shared.
    open_exclusive: AtomicBool,
}

// MacApi holds only an AtomicBool, so it is `Send + Sync` automatically; each
// `enumerate` call creates and releases its own IOHIDManager.

impl MacApi {
    pub fn new() -> HidResult<Self> {
        Ok(MacApi {
            open_exclusive: AtomicBool::new(false),
        })
    }

    /// `hid_darwin_set_open_exclusive` equivalent.
    pub fn set_open_exclusive(&self, exclusive: bool) {
        self.open_exclusive.store(exclusive, Ordering::Relaxed);
    }

    /// `hid_darwin_get_open_exclusive` equivalent.
    pub fn open_exclusive(&self) -> bool {
        self.open_exclusive.load(Ordering::Relaxed)
    }

    pub fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
        let mut result = Vec::new();
        // SAFETY: the copied set owns one reference to each IOHIDDeviceRef;
        // the refs are only used before the set is released.
        unsafe {
            // A fresh manager per call, like hidapi's `hid_enumerate`. Reusing
            // a long-lived manager returns a stale device set: devices that
            // appear after it was created (e.g. a keyboard re-enumerating into
            // its bootloader) never show up in `CopyDevices`.
            let manager = IOHIDManagerCreate(kCFAllocatorDefault, IOHID_MANAGER_OPTION_NONE);
            if manager.is_null() {
                return Err(HidError::Initialization {
                    message: "IOHIDManagerCreate returned NULL".into(),
                });
            }
            // NULL matching dictionary = match every HID device.
            IOHIDManagerSetDeviceMatching(manager, std::ptr::null());
            let set = IOHIDManagerCopyDevices(manager);
            if set.is_null() {
                // No HID devices present (hidapi reports this as an empty
                // enumeration too).
                CFRelease(manager as CFTypeRef);
                return Ok(result);
            }
            let count = CFSetGetCount(set) as usize;
            let mut devices: Vec<*const c_void> = vec![std::ptr::null(); count];
            CFSetGetValues(set, devices.as_mut_ptr());
            for device in devices {
                if device.is_null() {
                    continue;
                }
                for info in device_infos(device as IOHIDDeviceRef) {
                    let vid_ok = vendor_id == 0 || info.vendor_id == vendor_id;
                    let pid_ok = product_id == 0 || info.product_id == product_id;
                    if vid_ok && pid_ok {
                        result.push(info);
                    }
                }
            }
            CFRelease(set as CFTypeRef);
            CFRelease(manager as CFTypeRef);
        }
        Ok(result)
    }

    pub fn open(
        &self,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> HidResult<MacDevice> {
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

    pub fn open_path(&self, path: &str) -> HidResult<MacDevice> {
        let entry_id = parse_dev_srvs_id(path).ok_or_else(|| HidError::InvalidData {
            message: format!("not a macOS device path (expected \"DevSrvsID:<id>\"): {path}"),
        })?;
        let options = if self.open_exclusive() {
            IOHID_OPTIONS_TYPE_SEIZE_DEVICE
        } else {
            IOHID_OPTIONS_TYPE_NONE
        };
        MacDevice::open(entry_id, options)
    }
}

// --- device handle -----------------------------------------------------------------

/// State shared between the device handle, the read thread, and the IOKit
/// callbacks.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cond: Condvar,
}

#[derive(Default)]
struct State {
    /// Queued input reports, oldest first.
    reports: VecDeque<Vec<u8>>,
    /// Set by the removal callback (or when the run loop dies unexpectedly).
    disconnected: bool,
    /// Set by `Drop` to make the read thread exit.
    shutdown: bool,
    /// CFRunLoopRef of the read thread (as usize), 0 until the thread is up.
    run_loop: usize,
    /// The read thread finished scheduling the device.
    ready: bool,
    /// Wakers of pending `read_async` futures; drained (and woken) by the
    /// IOKit callbacks whenever a report arrives or the device goes away.
    wakers: Vec<Waker>,
}

impl Shared {
    /// Lock the state, recovering from poisoning: none of the critical
    /// sections can leave the queue in an inconsistent state.
    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `IOHIDReportCallback`: runs on the read thread's run loop; queues a copy
/// of the report and wakes readers, dropping the oldest entry beyond the cap.
unsafe extern "C" fn input_report_callback(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    _report_type: IOHIDReportType,
    _report_id: u32,
    report: *mut u8,
    report_length: CFIndex,
) {
    if report.is_null() || report_length <= 0 {
        return;
    }
    // SAFETY: context is the Shared pointer registered in MacDevice::open and
    // stays valid until the callback is unregistered (MacDevice::drop).
    let shared = &*(context as *const Shared);
    let data = std::slice::from_raw_parts(report, report_length as usize).to_vec();
    let wakers = {
        let mut state = shared.lock_state();
        state.reports.push_back(data);
        if state.reports.len() > MAX_QUEUED_REPORTS {
            state.reports.pop_front();
        }
        shared.cond.notify_all();
        std::mem::take(&mut state.wakers)
    };
    // Wake outside the lock so no executor code ever runs while it is held.
    for waker in wakers {
        waker.wake();
    }
}

/// `IOHIDCallback` for device removal: flags the disconnect, wakes readers
/// and stops the read thread's run loop, like hidapi.
unsafe extern "C" fn removal_callback(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
) {
    // SAFETY: see input_report_callback.
    let shared = &*(context as *const Shared);
    let wakers = {
        let mut state = shared.lock_state();
        state.disconnected = true;
        shared.cond.notify_all();
        if state.run_loop != 0 {
            CFRunLoopStop(state.run_loop as CFRunLoopRef);
        }
        std::mem::take(&mut state.wakers)
    };
    // Wake outside the lock so no executor code ever runs while it is held.
    for waker in wakers {
        waker.wake();
    }
}

/// Read-thread body: schedules the device on this thread's run loop and pumps
/// the private mode until shutdown or disconnection (hidapi's `read_thread`).
fn read_thread(device: usize, mode: usize, shared: Arc<Shared>) {
    let device = device as IOHIDDeviceRef;
    let mode = mode as CFStringRef;
    // SAFETY: the device and mode refs outlive the thread (Drop joins it
    // before releasing them); run loop calls target this thread's loop.
    unsafe {
        let run_loop = CFRunLoopGetCurrent();
        IOHIDDeviceScheduleWithRunLoop(device, run_loop, mode);
        {
            let mut state = shared.lock_state();
            state.run_loop = run_loop as usize;
            state.ready = true;
            shared.cond.notify_all();
        }
        loop {
            {
                let state = shared.lock_state();
                if state.shutdown || state.disconnected {
                    break;
                }
            }
            // hidapi pumps 1000-second slices of its private mode; the loop
            // is interrupted by CFRunLoopStop (close/removal) or by Drop
            // unscheduling the device, which empties the mode (-> Finished).
            let code = CFRunLoopRunInMode(mode, 1000.0, 0);
            if code == kCFRunLoopRunFinished || code == kCFRunLoopRunStopped {
                let mut state = shared.lock_state();
                if !state.shutdown {
                    // The run loop died without an orderly close: treat it as
                    // a disconnect, as hidapi does.
                    state.disconnected = true;
                }
                break;
            }
        }
        // Wake any threads blocked in read()/read_timeout() and any pending
        // read_async futures (the run loop may have died without the removal
        // callback firing).
        let wakers = {
            let mut state = shared.lock_state();
            shared.cond.notify_all();
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

pub(crate) struct MacDevice {
    device: IOHIDDeviceRef,
    /// Options the device was opened with; IOHIDDeviceClose wants them back.
    open_options: IOOptionBits,
    /// Private run loop mode (hidapi's `HIDAPI_%p`), so input reports are not
    /// dispatched by unrelated default-mode run loop activity.
    run_loop_mode: CFStringRef,
    /// Buffer IOKit writes incoming reports into. Kept as a raw allocation
    /// because IOKit holds the pointer while the callback is registered; it
    /// is reboxed and freed in Drop after the read thread is joined.
    input_buf: *mut u8,
    input_buf_len: usize,
    shared: Arc<Shared>,
    read_thread: Option<thread::JoinHandle<()>>,
    // Part of the backend contract; the wrapper now reads input via
    // `read_async`, so the blocking-mode state is unused on this path.
    #[allow(dead_code)]
    blocking: AtomicBool,
}

// SAFETY: IOHIDDeviceRef supports concurrent use for the calls made here,
// SetReport/GetReport/GetProperty from user threads while the read thread
// pumps the run loop, which is exactly hidapi's threading model. All mutable
// Rust-side state (report queue, flags) is behind a Mutex; the input buffer
// is only touched by IOKit on the read thread.
unsafe impl Send for MacDevice {}
unsafe impl Sync for MacDevice {}

impl MacDevice {
    fn open(entry_id: u64, options: IOOptionBits) -> HidResult<Self> {
        // SAFETY: each FFI call is checked before its result is used; on
        // every error path all acquired references are released.
        unsafe {
            let matching = IORegistryEntryIDMatching(entry_id);
            if matching.is_null() {
                return Err(HidError::backend("IORegistryEntryIDMatching failed"));
            }
            // Consumes the matching dictionary reference.
            let service = IOServiceGetMatchingService(IO_MASTER_PORT_DEFAULT, matching);
            if service == 0 {
                return Err(HidError::DeviceNotFound);
            }
            let device = IOHIDDeviceCreate(kCFAllocatorDefault, service);
            IOObjectRelease(service);
            if device.is_null() {
                return Err(HidError::backend("IOHIDDeviceCreate failed"));
            }
            let ret = IOHIDDeviceOpen(device, options);
            if ret != IO_RETURN_SUCCESS {
                CFRelease(device as CFTypeRef);
                return Err(HidError::OpenFailed {
                    message: format!("IOHIDDeviceOpen failed: IOReturn 0x{:08x}", ret as u32),
                });
            }

            // Buffer for the input report callback, sized like hidapi.
            let input_buf_len = i32_property(device, KEY_MAX_INPUT_REPORT_SIZE)
                .filter(|&len| len > 0)
                .unwrap_or(64) as usize;
            let input_buf = Box::into_raw(vec![0u8; input_buf_len].into_boxed_slice()) as *mut u8;

            // Private per-device run loop mode, like hidapi's "HIDAPI_%p".
            let mode_name =
                CString::new(format!("HIDAPI_{device:p}")).expect("no NUL in pointer format");
            let run_loop_mode = CFStringCreateWithCString(
                kCFAllocatorDefault,
                mode_name.as_ptr(),
                kCFStringEncodingUTF8,
            );

            let shared = Arc::new(Shared::default());
            // Valid for the callbacks' whole lifetime: Drop unregisters them
            // before `shared` is dropped.
            let context = Arc::as_ptr(&shared) as *mut c_void;
            IOHIDDeviceRegisterInputReportCallback(
                device,
                input_buf,
                input_buf_len as CFIndex,
                Some(input_report_callback),
                context,
            );
            IOHIDDeviceRegisterRemovalCallback(device, Some(removal_callback), context);

            let thread = thread::Builder::new().name("hidra-hid-read".into()).spawn({
                let shared = Arc::clone(&shared);
                let device = device as usize;
                let mode = run_loop_mode as usize;
                move || read_thread(device, mode, shared)
            });
            let thread = match thread {
                Ok(thread) => thread,
                Err(err) => {
                    IOHIDDeviceRegisterInputReportCallback(
                        device,
                        input_buf,
                        input_buf_len as CFIndex,
                        None,
                        context,
                    );
                    IOHIDDeviceRegisterRemovalCallback(device, None, context);
                    IOHIDDeviceClose(device, options);
                    CFRelease(device as CFTypeRef);
                    CFRelease(run_loop_mode as CFTypeRef);
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        input_buf,
                        input_buf_len,
                    )));
                    return Err(HidError::io("spawning HID read thread", err));
                }
            };

            // Wait for the read thread to schedule the device (hidapi's
            // barrier).
            {
                let mut state = shared.lock_state();
                while !state.ready {
                    state = shared
                        .cond
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }

            Ok(MacDevice {
                device,
                open_options: options,
                run_loop_mode,
                input_buf,
                input_buf_len,
                shared,
                read_thread: Some(thread),
                blocking: AtomicBool::new(true),
            })
        }
    }

    fn disconnected(&self) -> bool {
        self.shared.lock_state().disconnected
    }

    /// Common implementation of `write` and `send_feature_report`
    /// (hidapi's `set_report`): `data[0]` is the report ID and is not sent
    /// as payload when 0, but still counts toward the returned length.
    fn set_report(&self, report_type: IOHIDReportType, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "report data must contain a report ID byte".into(),
            });
        }
        if self.disconnected() {
            return Err(HidError::Disconnected);
        }
        let report_id = data[0];
        let payload = if report_id == 0 { &data[1..] } else { data };
        // SAFETY: payload outlives the synchronous call.
        let ret = unsafe {
            IOHIDDeviceSetReport(
                self.device,
                report_type,
                report_id as CFIndex,
                payload.as_ptr(),
                payload.len() as CFIndex,
            )
        };
        if ret != IO_RETURN_SUCCESS {
            return Err(HidError::backend(format!(
                "IOHIDDeviceSetReport failed: IOReturn 0x{:08x}",
                ret as u32
            )));
        }
        Ok(data.len())
    }

    /// Common implementation of `get_feature_report` and `get_input_report`
    /// (hidapi's `get_report`): `buf[0]` holds the report ID; for ID 0 the
    /// report body is read after it and the ID byte counts toward the length.
    fn get_report(&self, report_type: IOHIDReportType, buf: &mut [u8]) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "buffer must contain a report ID byte".into(),
            });
        }
        if self.disconnected() {
            return Err(HidError::Disconnected);
        }
        let report_id = buf[0];
        let body = if report_id == 0 {
            &mut buf[1..]
        } else {
            &mut *buf
        };
        let mut len = body.len() as CFIndex;
        // SAFETY: body outlives the synchronous call; len is in/out.
        let ret = unsafe {
            IOHIDDeviceGetReport(
                self.device,
                report_type,
                report_id as CFIndex,
                body.as_mut_ptr(),
                &mut len,
            )
        };
        if ret != IO_RETURN_SUCCESS {
            return Err(HidError::backend(format!(
                "IOHIDDeviceGetReport failed: IOReturn 0x{:08x}",
                ret as u32
            )));
        }
        let mut returned = len as usize;
        if report_id == 0 {
            returned += 1; // the untouched ID byte in buf[0]
        }
        Ok(returned)
    }

    pub fn write(&self, data: &[u8]) -> HidResult<usize> {
        self.set_report(IOHID_REPORT_TYPE_OUTPUT, data)
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

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            });
        }
        let deadline =
            (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
        let mut state = self.shared.lock_state();
        loop {
            if let Some(report) = state.reports.pop_front() {
                let len = report.len().min(buf.len());
                buf[..len].copy_from_slice(&report[..len]);
                return Ok(len);
            }
            // Queue drained: a disconnect now means no data will ever come.
            if state.disconnected {
                return Err(HidError::Disconnected);
            }
            if state.shutdown {
                return Err(HidError::backend("device read thread is shut down"));
            }
            state = match deadline {
                // timeout_ms == 0: poll.
                None if timeout_ms == 0 => return Ok(0),
                // timeout_ms < 0: block until data or disconnect.
                None => self
                    .shared
                    .cond
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(0);
                    }
                    self.shared
                        .cond
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// Read one input report without ever returning `Ok(0)`: resolves when a
    /// report is popped from the queue, fails with [`HidError::Disconnected`]
    /// once the device goes away and the queue is drained. Wake-ups come from
    /// the IOKit callbacks on the read thread (raw [`Waker`]s, no executor
    /// assumed).
    pub fn read_async<'a>(&'a self, buf: &'a mut [u8]) -> ReadAsync<'a> {
        ReadAsync { dev: self, buf }
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn set_blocking_mode(&self, blocking: bool) -> HidResult<()> {
        self.blocking.store(blocking, Ordering::Relaxed);
        Ok(())
    }

    pub fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        self.set_report(IOHID_REPORT_TYPE_FEATURE, data).map(|_| ())
    }

    pub fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(IOHID_REPORT_TYPE_FEATURE, buf)
    }

    pub fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(IOHID_REPORT_TYPE_INPUT, buf)
    }

    pub fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        // SAFETY: self.device is open for the lifetime of self.
        Ok(unsafe { string_property(self.device, KEY_MANUFACTURER) })
    }

    pub fn get_product_string(&self) -> HidResult<Option<String>> {
        // SAFETY: self.device is open for the lifetime of self.
        Ok(unsafe { string_property(self.device, KEY_PRODUCT) })
    }

    pub fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        // SAFETY: self.device is open for the lifetime of self.
        Ok(unsafe { string_property(self.device, KEY_SERIAL_NUMBER) })
    }

    pub fn get_indexed_string(&self, _index: u32) -> HidResult<Option<String>> {
        // Same as hidapi's macOS backend: USB string descriptor tables are
        // not reachable through IOHIDDevice. The `nusb` feature backend
        // supports it.
        Err(HidError::Unsupported {
            message: "indexed strings are not available via IOHIDDevice; use the usb backend"
                .into(),
        })
    }

    pub fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
        // SAFETY: the property reference follows the Get rule (not owned) and
        // its bytes are copied out before any other CF call.
        unsafe {
            let value = device_property(self.device, KEY_REPORT_DESCRIPTOR);
            if value.is_null() || CFGetTypeID(value) != CFDataGetTypeID() {
                return Err(HidError::backend(
                    "failed to read the ReportDescriptor property",
                ));
            }
            let data = value as CFDataRef;
            let len = (CFDataGetLength(data) as usize).min(buf.len());
            std::ptr::copy_nonoverlapping(CFDataGetBytePtr(data), buf.as_mut_ptr(), len);
            Ok(len)
        }
    }

    pub fn get_device_info(&self) -> HidResult<DeviceInfo> {
        // SAFETY: self.device is open for the lifetime of self.
        let mut infos = unsafe { device_infos(self.device) };
        // device_infos always yields the primary-usage entry first, which is
        // what hidapi returns for an open handle.
        Ok(infos.remove(0))
    }
}

impl Drop for MacDevice {
    fn drop(&mut self) {
        // SAFETY: mirrors hidapi's hid_close teardown order. The callbacks
        // are unregistered before `shared` (their context) can go away, the
        // read thread is joined before the device/mode refs are released, and
        // the input buffer is freed only after IOKit can no longer write it.
        unsafe {
            let (disconnected, run_loop) = {
                let state = self.shared.lock_state();
                (state.disconnected, state.run_loop)
            };
            let context = Arc::as_ptr(&self.shared) as *mut c_void;
            if !disconnected {
                // Disconnect the callbacks and move the device off the read
                // thread's run loop, like hidapi.
                IOHIDDeviceRegisterInputReportCallback(
                    self.device,
                    self.input_buf,
                    self.input_buf_len as CFIndex,
                    None,
                    context,
                );
                IOHIDDeviceRegisterRemovalCallback(self.device, None, context);
                if run_loop != 0 {
                    IOHIDDeviceUnscheduleFromRunLoop(
                        self.device,
                        run_loop as CFRunLoopRef,
                        self.run_loop_mode,
                    );
                    IOHIDDeviceScheduleWithRunLoop(
                        self.device,
                        CFRunLoopGetMain(),
                        kCFRunLoopDefaultMode,
                    );
                }
            }
            {
                let mut state = self.shared.lock_state();
                state.shutdown = true;
                self.shared.cond.notify_all();
            }
            if run_loop != 0 {
                CFRunLoopStop(run_loop as CFRunLoopRef);
                CFRunLoopWakeUp(run_loop as CFRunLoopRef);
            }
            if let Some(thread) = self.read_thread.take() {
                let _ = thread.join();
            }
            if !disconnected {
                IOHIDDeviceClose(self.device, self.open_options);
            }
            CFRelease(self.device as CFTypeRef);
            CFRelease(self.run_loop_mode as CFTypeRef);
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.input_buf,
                self.input_buf_len,
            )));
        }
    }
}

/// Future returned by [`MacDevice::read_async`].
///
/// Cancel-safe: a report is only popped from the queue inside `poll`, so
/// dropping the future before completion loses nothing, the report stays
/// queued for the next read. A waker left behind by a dropped future causes
/// at most one spurious wake-up; the callbacks drain the whole waker list on
/// every wake, so stale entries never accumulate.
pub(crate) struct ReadAsync<'a> {
    dev: &'a MacDevice,
    buf: &'a mut [u8],
}

impl std::future::Future for ReadAsync<'_> {
    type Output = HidResult<usize>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        if this.buf.is_empty() {
            return std::task::Poll::Ready(Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            }));
        }
        let mut state = this.dev.shared.lock_state();
        if let Some(report) = state.reports.pop_front() {
            let len = report.len().min(this.buf.len());
            this.buf[..len].copy_from_slice(&report[..len]);
            return std::task::Poll::Ready(Ok(len));
        }
        // Queue drained: a disconnect now means no data will ever come.
        if state.disconnected {
            return std::task::Poll::Ready(Err(HidError::Disconnected));
        }
        if state.shutdown {
            return std::task::Poll::Ready(Err(HidError::backend(
                "device read thread is shut down",
            )));
        }
        // Registering under the same lock the callbacks take closes the gap
        // between the checks above and the registration: no report or
        // disconnect can slip in unobserved.
        if !state.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            state.wakers.push(cx.waker().clone());
        }
        std::task::Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dev_srvs_id_paths() {
        assert_eq!(parse_dev_srvs_id("DevSrvsID:4295123456"), Some(4295123456));
        assert_eq!(parse_dev_srvs_id("DevSrvsID:0"), Some(0));
        assert_eq!(parse_dev_srvs_id("DevSrvsID:"), None);
        assert_eq!(parse_dev_srvs_id("DevSrvsID:abc"), None);
        assert_eq!(parse_dev_srvs_id("DevSrvsID:-3"), None);
        assert_eq!(parse_dev_srvs_id("/dev/hidraw0"), None);
        assert_eq!(parse_dev_srvs_id(""), None);
    }

    #[test]
    fn formats_and_reparses_paths() {
        assert_eq!(format_dev_srvs_id(42), "DevSrvsID:42");
        // The largest registry entry ID hidapi accounts for (20 digits).
        let path = format_dev_srvs_id(u64::MAX);
        assert_eq!(path, "DevSrvsID:18446744073709551615");
        assert_eq!(parse_dev_srvs_id(&path), Some(u64::MAX));
    }

    #[test]
    fn maps_transport_strings() {
        assert_eq!(bus_type_from_transport("USB"), BusType::Usb);
        assert_eq!(bus_type_from_transport("Bluetooth"), BusType::Bluetooth);
        assert_eq!(
            bus_type_from_transport("BluetoothLowEnergy"),
            BusType::Bluetooth
        );
        assert_eq!(
            bus_type_from_transport("Bluetooth Low Energy"),
            BusType::Bluetooth
        );
        assert_eq!(bus_type_from_transport("I2C"), BusType::I2c);
        assert_eq!(bus_type_from_transport("SPI"), BusType::Spi);
        assert_eq!(bus_type_from_transport("AirPlay"), BusType::Unknown);
        assert_eq!(bus_type_from_transport(""), BusType::Unknown);
    }
}

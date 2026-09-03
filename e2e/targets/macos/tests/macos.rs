//! End-to-end test of the macOS IOHIDManager backend against a virtual HID
//! device created via IOKit's `IOHIDUserDevice`. macOS only.
//!
//! `IOHIDUserDeviceCreate` is gated by the restricted
//! `com.apple.developer.hid.virtual.device` entitlement (enforced by AMFI, not
//! just SIP), so root alone returns NULL. To run it on a machine you control:
//!
//!   1. Disable SIP and relax AMFI via boot-args:
//!      `csr-active-config` = permissive (e.g. 0x0FFF) and
//!      `boot-args` = `amfi_get_out_of_my_way=0x1`.
//!   2. Build the test binary: `cargo test -p macos --no-run`.
//!   3. Ad-hoc sign it *with the entitlement embedded*:
//!      `codesign -s - --entitlements hid-virtual-device.entitlements --force <test-bin>`
//!   4. Run as root: `sudo <test-bin> --test-threads=1`.
//!
//! Self-skips when creation returns NULL, unless `HIDRA_MACOS_REQUIRED=1`.
#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_foundation_sys::base::{kCFAllocatorDefault, CFIndex, CFRelease, CFTypeRef};
use core_foundation_sys::data::CFDataCreate;
use core_foundation_sys::dictionary::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
    CFDictionaryRef,
};
use core_foundation_sys::number::{kCFNumberSInt32Type, CFNumberCreate};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringRef};

use block2::{Block, RcBlock};

use conformance::{
    make_descriptor, run_conformance, Caps, VirtualDevice, MANUFACTURER, PRODUCT, SERIAL, TEST_PID,
    TEST_VID,
};

#[repr(C)]
struct OpaqueUserDevice {
    _private: [u8; 0],
}
type IOHIDUserDeviceRef = *const OpaqueUserDevice;

type IOReturn = i32;
type IOHIDReportType = u32;
const IO_RETURN_SUCCESS: IOReturn = 0;
const IOHID_REPORT_TYPE_OUTPUT: IOHIDReportType = 1;
const IOHID_REPORT_TYPE_FEATURE: IOHIDReportType = 2;

// GET block fills `report` and writes the actual byte count back to `*report_length`.
type GetBlock = Block<dyn Fn(IOHIDReportType, u32, *mut u8, *mut CFIndex) -> IOReturn>;
type SetBlock = Block<dyn Fn(IOHIDReportType, u32, *const u8, CFIndex) -> IOReturn>;
type VoidBlock = Block<dyn Fn()>;
type DispatchQueue = *mut c_void;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDUserDeviceCreate(
        allocator: CFTypeRef,
        properties: CFDictionaryRef,
    ) -> IOHIDUserDeviceRef;
    fn IOHIDUserDeviceHandleReport(
        device: IOHIDUserDeviceRef,
        report: *const u8,
        length: CFIndex,
    ) -> i32;
    fn IOHIDUserDeviceRegisterGetReportBlock(device: IOHIDUserDeviceRef, block: &GetBlock);
    fn IOHIDUserDeviceRegisterSetReportBlock(device: IOHIDUserDeviceRef, block: &SetBlock);
    // Block callbacks are serviced via the dispatch-queue lifecycle (run-loop
    // scheduling does not wire them up), so SET/GET report requests need this.
    fn IOHIDUserDeviceSetDispatchQueue(device: IOHIDUserDeviceRef, queue: DispatchQueue);
    fn IOHIDUserDeviceSetCancelHandler(device: IOHIDUserDeviceRef, handler: &VoidBlock);
    fn IOHIDUserDeviceActivate(device: IOHIDUserDeviceRef);
    fn IOHIDUserDeviceCancel(device: IOHIDUserDeviceRef);
}

extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
}

fn cfstr(s: &str) -> CFStringRef {
    let c = CString::new(s).unwrap();
    unsafe { CFStringCreateWithCString(kCFAllocatorDefault, c.as_ptr(), kCFStringEncodingUTF8) }
}
fn cfnum(n: i32) -> CFTypeRef {
    unsafe {
        CFNumberCreate(
            kCFAllocatorDefault,
            kCFNumberSInt32Type,
            &n as *const i32 as *const c_void,
        ) as CFTypeRef
    }
}

/// State shared between the report callbacks (dispatch-queue thread) and the
/// `VirtualDevice` hooks (test thread).
#[derive(Default)]
struct MacShared {
    primed: Mutex<Vec<u8>>, // wire form returned for GET feature/input
    last_output: Mutex<Option<Vec<u8>>>,
    last_set_feature: Mutex<Option<Vec<u8>>>,
}

fn make_get_block(
    shared: Arc<MacShared>,
) -> RcBlock<dyn Fn(IOHIDReportType, u32, *mut u8, *mut CFIndex) -> IOReturn> {
    RcBlock::new(
        move |_ty: IOHIDReportType,
              _report_id: u32,
              report: *mut u8,
              report_length: *mut CFIndex|
              -> IOReturn {
            let wire = shared.primed.lock().unwrap().clone();
            unsafe {
                let cap = *report_length as usize;
                let n = wire.len().min(cap);
                std::ptr::copy_nonoverlapping(wire.as_ptr(), report, n);
                *report_length = n as CFIndex;
            }
            IO_RETURN_SUCCESS
        },
    )
}

fn make_set_block(
    shared: Arc<MacShared>,
) -> RcBlock<dyn Fn(IOHIDReportType, u32, *const u8, CFIndex) -> IOReturn> {
    RcBlock::new(
        move |ty: IOHIDReportType,
              _report_id: u32,
              report: *const u8,
              report_length: CFIndex|
              -> IOReturn {
            let data =
                unsafe { std::slice::from_raw_parts(report, report_length as usize) }.to_vec();
            match ty {
                IOHID_REPORT_TYPE_OUTPUT => *shared.last_output.lock().unwrap() = Some(data),
                IOHID_REPORT_TYPE_FEATURE => *shared.last_set_feature.lock().unwrap() = Some(data),
                _ => {}
            }
            IO_RETURN_SUCCESS
        },
    )
}

fn create_user_device(report_desc: &[u8]) -> IOHIDUserDeviceRef {
    unsafe {
        let k_desc = cfstr("ReportDescriptor");
        let k_vid = cfstr("VendorID");
        let k_pid = cfstr("ProductID");
        let k_product = cfstr("Product");
        let k_serial = cfstr("SerialNumber");
        let k_manuf = cfstr("Manufacturer");

        let v_desc = CFDataCreate(
            kCFAllocatorDefault,
            report_desc.as_ptr(),
            report_desc.len() as CFIndex,
        ) as CFTypeRef;
        let v_vid = cfnum(TEST_VID as i32);
        let v_pid = cfnum(TEST_PID as i32);
        let v_product = cfstr(PRODUCT) as CFTypeRef;
        let v_serial = cfstr(SERIAL) as CFTypeRef;
        let v_manuf = cfstr(MANUFACTURER) as CFTypeRef;

        let keys = [
            k_desc as CFTypeRef,
            k_vid as CFTypeRef,
            k_pid as CFTypeRef,
            k_product as CFTypeRef,
            k_serial as CFTypeRef,
            k_manuf as CFTypeRef,
        ];
        let values = [v_desc, v_vid, v_pid, v_product, v_serial, v_manuf];

        let props = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as CFIndex,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );

        let dev = IOHIDUserDeviceCreate(kCFAllocatorDefault, props);

        CFRelease(props as CFTypeRef);
        for k in keys {
            CFRelease(k);
        }
        for v in values {
            CFRelease(v);
        }
        dev
    }
}

struct MacDevice {
    dev: IOHIDUserDeviceRef,
    shared: Arc<MacShared>,
    cancel_rx: std::sync::mpsc::Receiver<()>,
    // Keep the registered blocks + queue alive for the device's lifetime.
    _get_block: RcBlock<dyn Fn(IOHIDReportType, u32, *mut u8, *mut CFIndex) -> IOReturn>,
    _set_block: RcBlock<dyn Fn(IOHIDReportType, u32, *const u8, CFIndex) -> IOReturn>,
    _cancel_block: RcBlock<dyn Fn()>,
    _queue: DispatchQueue,
    /// Guards against `disconnect()` and `Drop` double-cancelling/releasing the device.
    torn_down: AtomicBool,
}

impl MacDevice {
    /// Cancel, wait for the cancel handler to fire, then release — IOKit's
    /// required teardown order — exactly once.
    fn teardown(&self) {
        if self.torn_down.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe { IOHIDUserDeviceCancel(self.dev) };
        self.cancel_rx.recv_timeout(Duration::from_secs(2)).ok();
        unsafe { CFRelease(self.dev as CFTypeRef) };
    }
}

impl MacDevice {
    fn create(numbered: bool) -> Option<Self> {
        let rd = make_descriptor(numbered);
        let dev = create_user_device(&rd);
        if dev.is_null() {
            return None;
        }
        let shared = Arc::new(MacShared::default());
        let get_block = make_get_block(Arc::clone(&shared));
        let set_block = make_set_block(Arc::clone(&shared));
        let (tx, cancel_rx) = std::sync::mpsc::channel::<()>();
        let cancel_block = RcBlock::new(move || {
            tx.send(()).ok();
        });

        let queue = unsafe {
            IOHIDUserDeviceRegisterGetReportBlock(dev, &get_block);
            IOHIDUserDeviceRegisterSetReportBlock(dev, &set_block);
            let queue = dispatch_queue_create(
                b"com.hidra.vhid\0".as_ptr() as *const c_char,
                std::ptr::null(),
            );
            IOHIDUserDeviceSetDispatchQueue(dev, queue);
            IOHIDUserDeviceSetCancelHandler(dev, &cancel_block);
            IOHIDUserDeviceActivate(dev);
            queue
        };

        Some(MacDevice {
            dev,
            shared,
            cancel_rx,
            _get_block: get_block,
            _set_block: set_block,
            _cancel_block: cancel_block,
            _queue: queue,
            torn_down: AtomicBool::new(false),
        })
    }
}

impl VirtualDevice for MacDevice {
    fn inject_input(&self, wire: &[u8]) {
        unsafe { IOHIDUserDeviceHandleReport(self.dev, wire.as_ptr(), wire.len() as CFIndex) };
    }
    fn prime_get(&self, wire: &[u8]) {
        *self.shared.primed.lock().unwrap() = wire.to_vec();
    }
    fn last_output(&self) -> Option<Vec<u8>> {
        self.shared.last_output.lock().unwrap().clone()
    }
    fn last_set_feature(&self) -> Option<Vec<u8>> {
        self.shared.last_set_feature.lock().unwrap().clone()
    }
    fn disconnect(&self) {
        // Removing the device fires hidra's IOHIDManager removal callback,
        // waking the pending read as `Disconnected`.
        self.teardown();
    }
}

impl Drop for MacDevice {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn macos_caps() -> Caps {
    Caps {
        numbered: true,
        strings: true,
        manufacturer: true,
        exact_descriptor: true,
        feature: true,
        input_get: true,
        disconnect: true,
        indexed_string_unsupported: false,
        usage_at_enumerate: true,
        // IOHIDUserDevice reports no USB bus/interface and no bcdDevice.
        bus_type: conformance::BusType::Unknown,
        release_number: 0x0000,
        interface_number: -1,
        backend: conformance::Backend::Native,
    }
}

fn skip_or_panic(msg: &str) -> bool {
    if std::env::var("HIDRA_MACOS_REQUIRED").is_ok() {
        panic!("HIDRA_MACOS_REQUIRED set but {msg}");
    }
    eprintln!("SKIP: {msg}");
    true
}

/// Wait until the test VID/PID no longer enumerates, so the next variant starts
/// from a clean registry.
fn wait_gone() {
    let Ok(api) = hidra::Hidra::new() else { return };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if api
            .enumerate(TEST_VID, TEST_PID)
            .map(|v| v.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn macos_conformance() {
    let caps = macos_caps();
    for numbered in [false, true] {
        match MacDevice::create(numbered) {
            Some(dev) => {
                run_conformance::<hidra::Native>(numbered, &caps, &dev);
                drop(dev);
                wait_gone();
            }
            None => {
                if skip_or_panic("IOHIDUserDeviceCreate returned NULL (need root + SIP off)") {
                    return;
                }
            }
        }
    }
    eprintln!("PASS: macos virtual-device full-API conformance test");
}

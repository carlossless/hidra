//! End-to-end test of the Windows HID backend against a virtual HID device
//! created from user space via cgutman's WinUHid framework. Windows only.
//!
//! Requires:
//!   - the WinUHid driver installed (Root\WinUHid, test-signed) and
//!   - WinUHid.dll at DEFAULT_DLL below (override via HIDRA_WINUHID_DLL).
//! Self-skips if the DLL/driver isn't present, unless HIDRA_WINDOWS_REQUIRED=1.
#![cfg(target_os = "windows")]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use libloading::Library;

use conformance::{make_descriptor, run_conformance, Caps, VirtualDevice, TEST_PID, TEST_VID};

const DEFAULT_DLL: &str = r"C:\WinUHid\build\Release\x64\WinUHid.dll";

// WINUHID_EVENT_TYPE bits.
const EV_GET_FEATURE: u32 = 0x1;
const EV_SET_FEATURE: u32 = 0x2;
const EV_WRITE_REPORT: u32 = 0x4;

// WinUHid structs are #pragma pack(1).
#[repr(C, packed)]
struct WinUHidConfig {
    supported_events: u32,
    vendor_id: u16,
    product_id: u16,
    version: u16,
    report_desc_len: u16,
    report_desc: *const u8,
    container_id: [u8; 16],
    instance_id: *const u16,
    hardware_ids: *const u16,
    read_report_period_us: u32,
}

#[repr(C, packed)]
struct WinUHidEvent {
    ev_type: u32,
    request_id: u32,
    report_id: u8,
    data_length: u32, // union common leading field; Data[] follows for writes
}

type PDev = *mut c_void;

type FnCreate = unsafe extern "system" fn(*const WinUHidConfig) -> PDev;
type FnStart = unsafe extern "system" fn(PDev, *const c_void, *mut c_void) -> i32;
type FnSubmit = unsafe extern "system" fn(PDev, *const u8, u32) -> i32;
type FnPoll = unsafe extern "system" fn(PDev, u32) -> *const WinUHidEvent;
type FnCompleteRead = unsafe extern "system" fn(PDev, *const WinUHidEvent, *const u8, u32);
type FnCompleteWrite = unsafe extern "system" fn(PDev, *const WinUHidEvent, i32);
type FnStop = unsafe extern "system" fn(PDev);
type FnDestroy = unsafe extern "system" fn(PDev);

/// Resolved WinUHid.dll entry points; the fn pointers stay valid only while
/// `_lib` keeps the DLL loaded.
#[derive(Clone)]
struct WinUHid {
    create: FnCreate,
    start: FnStart,
    submit: FnSubmit,
    poll: FnPoll,
    complete_read: FnCompleteRead,
    complete_write: FnCompleteWrite,
    stop: FnStop,
    destroy: FnDestroy,
    _lib: Arc<Library>,
}

impl WinUHid {
    unsafe fn load(path: &str) -> Result<Self, libloading::Error> {
        let lib = Library::new(path)?;
        let create = *lib.get::<FnCreate>(b"WinUHidCreateDevice\0")?;
        let start = *lib.get::<FnStart>(b"WinUHidStartDevice\0")?;
        let submit = *lib.get::<FnSubmit>(b"WinUHidSubmitInputReport\0")?;
        let poll = *lib.get::<FnPoll>(b"WinUHidPollEvent\0")?;
        let complete_read = *lib.get::<FnCompleteRead>(b"WinUHidCompleteReadEvent\0")?;
        let complete_write = *lib.get::<FnCompleteWrite>(b"WinUHidCompleteWriteEvent\0")?;
        let stop = *lib.get::<FnStop>(b"WinUHidStopDevice\0")?;
        let destroy = *lib.get::<FnDestroy>(b"WinUHidDestroyDevice\0")?;
        Ok(WinUHid {
            create,
            start,
            submit,
            poll,
            complete_read,
            complete_write,
            stop,
            destroy,
            _lib: Arc::new(lib),
        })
    }
}

/// Shared device-side state, updated by the servicer thread / read by the trait.
#[derive(Default)]
struct Shared {
    primed: Mutex<Vec<u8>>, // report body returned for GET_FEATURE
    last_output: Mutex<Option<Vec<u8>>>,
    last_set: Mutex<Option<Vec<u8>>>,
    /// When set, the servicer stops completing requests, so a write stalls and
    /// its `set_write_timeout` fires.
    output_paused: AtomicBool,
}

/// The device handle isn't Send, but the DLL keeps it valid; wrap it so the
/// servicer thread may use it.
#[derive(Clone, Copy)]
struct SendDev(PDev);
unsafe impl Send for SendDev {}
unsafe impl Sync for SendDev {}

/// A WinUHid-backed virtual device; a background thread services GET_FEATURE
/// (answered with the primed body), SET_FEATURE and WRITE_REPORT (recorded) events.
struct WinUHidDevice {
    api: WinUHid,
    dev: SendDev,
    shared: Arc<Shared>,
    stop_flag: Arc<AtomicBool>,
    servicer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Guards against `disconnect()` and `Drop` double-destroying the device.
    torn_down: AtomicBool,
}

impl WinUHidDevice {
    /// Stop the servicer thread, then stop + destroy the device, exactly once.
    fn teardown(&self) {
        if self.torn_down.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.servicer.lock().unwrap().take() {
            h.join().ok();
        }
        unsafe {
            (self.api.stop)(self.dev.0);
            (self.api.destroy)(self.dev.0);
        }
    }
}

impl WinUHidDevice {
    fn create(api: &WinUHid, numbered: bool) -> Option<Self> {
        let rd = make_descriptor(numbered);
        let cfg = WinUHidConfig {
            supported_events: EV_GET_FEATURE | EV_SET_FEATURE | EV_WRITE_REPORT,
            vendor_id: TEST_VID,
            product_id: TEST_PID,
            version: 0x0001,
            report_desc_len: rd.len() as u16,
            report_desc: rd.as_ptr(),
            container_id: [0u8; 16],
            instance_id: core::ptr::null(),
            hardware_ids: core::ptr::null(),
            read_report_period_us: 0,
        };
        let dev = unsafe { (api.create)(&cfg) };
        if dev.is_null() {
            return None;
        }
        if unsafe { (api.start)(dev, core::ptr::null(), core::ptr::null_mut()) } == 0 {
            unsafe { (api.destroy)(dev) };
            return None;
        }

        let shared = Arc::new(Shared::default());
        let stop_flag = Arc::new(AtomicBool::new(false));
        let dev_s = SendDev(dev);

        let (api_t, shared_t, stop_t) = (api.clone(), Arc::clone(&shared), Arc::clone(&stop_flag));
        let servicer = std::thread::spawn(move || {
            let dev = dev_s;
            while !stop_t.load(Ordering::Relaxed) {
                if shared_t.output_paused.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                let ev = unsafe { (api_t.poll)(dev.0, 100) };
                if ev.is_null() {
                    continue;
                }
                let ty = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*ev).ev_type)) };
                match ty {
                    EV_GET_FEATURE => {
                        let body = shared_t.primed.lock().unwrap().clone();
                        unsafe {
                            (api_t.complete_read)(dev.0, ev, body.as_ptr(), body.len() as u32)
                        };
                    }
                    EV_SET_FEATURE | EV_WRITE_REPORT => {
                        let dlen = unsafe {
                            core::ptr::read_unaligned(core::ptr::addr_of!((*ev).data_length))
                        } as usize;
                        let data = unsafe { (ev as *const u8).add(13) }; // Data[] (packed)
                        let mut buf = vec![0u8; dlen];
                        unsafe { core::ptr::copy_nonoverlapping(data, buf.as_mut_ptr(), dlen) };
                        if ty == EV_WRITE_REPORT {
                            *shared_t.last_output.lock().unwrap() = Some(buf);
                        } else {
                            *shared_t.last_set.lock().unwrap() = Some(buf);
                        }
                        unsafe { (api_t.complete_write)(dev.0, ev, 1) };
                    }
                    _ => {}
                }
            }
        });

        Some(WinUHidDevice {
            api: api.clone(),
            dev: dev_s,
            shared,
            stop_flag,
            servicer: Mutex::new(Some(servicer)),
            torn_down: AtomicBool::new(false),
        })
    }
}

impl VirtualDevice for WinUHidDevice {
    fn inject_input(&self, wire: &[u8]) {
        unsafe { (self.api.submit)(self.dev.0, wire.as_ptr(), wire.len() as u32) };
    }
    fn prime_get(&self, wire: &[u8]) {
        // Answer GET_FEATURE with the report exactly as it appears on the wire:
        // body only when unnumbered, [report-ID | body] when numbered. The
        // framework does not synthesize the report-ID byte, so a numbered
        // report must carry it.
        *self.shared.primed.lock().unwrap() = wire.to_vec();
    }
    fn last_output(&self) -> Option<Vec<u8>> {
        self.shared.last_output.lock().unwrap().clone()
    }
    fn last_set_feature(&self) -> Option<Vec<u8>> {
        self.shared.last_set.lock().unwrap().clone()
    }
    fn disconnect(&self) {
        // Destroying the device makes hidra's next read fail with
        // ERROR_DEVICE_NOT_CONNECTED, surfaced as `Disconnected`.
        self.teardown();
    }
    fn set_output_paused(&self, paused: bool) {
        self.shared.output_paused.store(paused, Ordering::Relaxed);
    }
}

impl Drop for WinUHidDevice {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// No strings (the config has no string fields), the descriptor is reconstructed
/// (not byte-exact), and there is no GET_INPUT event.
fn windows_caps() -> Caps {
    Caps {
        numbered: true,
        strings: false,
        manufacturer: false,
        exact_descriptor: false,
        feature: true,
        input_get: false,
        disconnect: true,
        indexed_string_unsupported: false,
        usage_at_enumerate: true,
        // WinUHid is a Root-enumerated device: no USB bus/interface, version 0x0001.
        bus_type: conformance::BusType::Unknown,
        release_number: 0x0001,
        interface_number: -1,
        backend: conformance::Backend::Native,
    }
}

fn skip_or_panic(msg: &str) -> bool {
    if std::env::var("HIDRA_WINDOWS_REQUIRED").is_ok() {
        panic!("HIDRA_WINDOWS_REQUIRED set but {msg}");
    }
    eprintln!("SKIP: {msg}");
    true
}

fn load_or_skip() -> Option<WinUHid> {
    let dll = std::env::var("HIDRA_WINUHID_DLL").unwrap_or_else(|_| DEFAULT_DLL.to_string());
    if !std::path::Path::new(&dll).exists() {
        skip_or_panic(&format!("WinUHid.dll not found at {dll}"));
        return None;
    }
    match unsafe { WinUHid::load(&dll) } {
        Ok(api) => Some(api),
        Err(e) => {
            skip_or_panic(&format!("failed to load WinUHid.dll: {e}"));
            None
        }
    }
}

#[test]
fn windows_virtual_conformance() {
    let Some(api) = load_or_skip() else { return };
    let caps = windows_caps();
    for numbered in [false, true] {
        match WinUHidDevice::create(&api, numbered) {
            Some(dev) => {
                run_conformance::<hidra::Native>(numbered, &caps, &dev);
                drop(dev);
            }
            None => {
                if skip_or_panic("WinUHidCreateDevice failed (driver installed?)") {
                    return;
                }
            }
        }
    }
    eprintln!("PASS: windows WinUHid full-API conformance test");
}

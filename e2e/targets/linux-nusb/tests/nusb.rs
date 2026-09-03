//! Conformance suite (the `conformance` crate) on hidra's `nusb`
//! USB-transport backend, against a virtual USB HID device built from
//! `dummy_hcd` + a configfs `g_hid` gadget. Needs root (module loading,
//! configfs, raw USB access); self-skips otherwise unless `HIDRA_NUSB_REQUIRED=1`
//! (CI) forces a failure. Run: `sudo -E $(rustup which cargo) test -p linux-nusb`.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use conformance::{
    make_descriptor, run_conformance, Caps, VirtualDevice, MANUFACTURER, PRODUCT, SERIAL, TEST_PID,
    TEST_VID,
};

const GADGET: &str = "/sys/kernel/config/usb_gadget/hidra";
const O_NONBLOCK: i32 = 0o4000;

fn w(path: &str, val: &str) -> std::io::Result<()> {
    fs::write(path, val)
}

fn teardown() {
    if !Path::new(GADGET).exists() {
        return;
    }
    let _ = w(&format!("{GADGET}/UDC"), "\n");
    let _ = fs::remove_file(format!("{GADGET}/configs/c.1/hid.usb0"));
    let _ = fs::remove_dir(format!("{GADGET}/configs/c.1/strings/0x409"));
    let _ = fs::remove_dir(format!("{GADGET}/configs/c.1"));
    let _ = fs::remove_dir(format!("{GADGET}/functions/hid.usb0"));
    let _ = fs::remove_dir(format!("{GADGET}/strings/0x409"));
    let _ = fs::remove_dir(GADGET);
}

fn setup_gadget(report_desc: &[u8], report_len: usize) {
    for m in ["dummy_hcd", "libcomposite", "usb_f_hid"] {
        let _ = Command::new("modprobe").arg(m).status();
    }
    teardown(); // clean any leftover from a previous run

    fs::create_dir_all(format!("{GADGET}/strings/0x409")).expect("mkdir gadget");
    w(&format!("{GADGET}/idVendor"), &format!("{TEST_VID:#06x}\n")).unwrap();
    w(
        &format!("{GADGET}/idProduct"),
        &format!("{TEST_PID:#06x}\n"),
    )
    .unwrap();
    w(&format!("{GADGET}/bcdDevice"), "0x0100\n").unwrap();
    w(&format!("{GADGET}/bcdUSB"), "0x0200\n").unwrap();
    w(
        &format!("{GADGET}/strings/0x409/manufacturer"),
        MANUFACTURER,
    )
    .unwrap();
    w(&format!("{GADGET}/strings/0x409/product"), PRODUCT).unwrap();
    w(&format!("{GADGET}/strings/0x409/serialnumber"), SERIAL).unwrap();

    fs::create_dir_all(format!("{GADGET}/functions/hid.usb0")).expect("mkdir hid function");
    w(&format!("{GADGET}/functions/hid.usb0/protocol"), "0\n").unwrap();
    w(&format!("{GADGET}/functions/hid.usb0/subclass"), "0\n").unwrap();
    w(
        &format!("{GADGET}/functions/hid.usb0/report_length"),
        &format!("{report_len}\n"),
    )
    .unwrap();
    fs::write(
        format!("{GADGET}/functions/hid.usb0/report_desc"),
        report_desc,
    )
    .expect("write report_desc");

    fs::create_dir_all(format!("{GADGET}/configs/c.1/strings/0x409")).expect("mkdir config");
    w(
        &format!("{GADGET}/configs/c.1/strings/0x409/configuration"),
        "hidra\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        format!("{GADGET}/functions/hid.usb0"),
        format!("{GADGET}/configs/c.1/hid.usb0"),
    )
    .expect("link function");

    let udc = fs::read_dir("/sys/class/udc")
        .expect("read /sys/class/udc")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .expect("no UDC available (dummy_hcd not loaded?)");
    w(&format!("{GADGET}/UDC"), &format!("{udc}\n")).expect("bind UDC");
}

fn open_hidg() -> std::fs::File {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open("/dev/hidg0")
        {
            return f;
        }
        assert!(Instant::now() < deadline, "/dev/hidg0 never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Driven via `/dev/hidg0`: writes send input reports to the host; a background
/// thread reads output reports (SET_REPORT / interrupt OUT).
struct NusbGadget {
    write_fd: Mutex<std::fs::File>,
    last_output: Arc<Mutex<Option<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl NusbGadget {
    fn create(numbered: bool) -> Self {
        let rd = make_descriptor(numbered);
        let report_len = if numbered { 9 } else { 8 };
        setup_gadget(&rd, report_len);
        let hidg = open_hidg();

        let write_fd = hidg.try_clone().expect("clone hidg fd");
        let last_output = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (out_t, stop_t) = (Arc::clone(&last_output), Arc::clone(&stop));
        let reader = std::thread::spawn(move || {
            let mut fd = hidg;
            let mut buf = [0u8; 64];
            while !stop_t.load(Ordering::Relaxed) {
                match fd.read(&mut buf) {
                    Ok(n) if n > 0 => *out_t.lock().unwrap() = Some(buf[..n].to_vec()),
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        NusbGadget {
            write_fd: Mutex::new(write_fd),
            last_output,
            stop,
            reader: Some(reader),
        }
    }
}

impl VirtualDevice for NusbGadget {
    fn inject_input(&self, wire: &[u8]) {
        // Nonblocking write may EAGAIN if the host isn't draining interrupt IN;
        // the conformance body re-injects.
        let _ = self.write_fd.lock().unwrap().write(wire);
    }
    fn prime_get(&self, _wire: &[u8]) {
        // g_hid cannot service GET_REPORT, so feature/input-get are disabled in
        // caps and this is never consulted.
    }
    fn last_output(&self) -> Option<Vec<u8>> {
        self.last_output.lock().unwrap().clone()
    }
    fn last_set_feature(&self) -> Option<Vec<u8>> {
        None
    }
    fn disconnect(&self) {
        let _ = w(&format!("{GADGET}/UDC"), "\n");
    }
}

impl Drop for NusbGadget {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader.take() {
            h.join().ok();
        }
        teardown();
    }
}

fn nusb_caps() -> Caps {
    Caps {
        numbered: true,
        strings: true,
        manufacturer: true,
        exact_descriptor: true,
        feature: false,
        input_get: false,
        // Unbinding the UDC surfaces removal only because the conformance body
        // drains the buffered interrupt-IN reports before requiring `Disconnected`.
        disconnect: true,
        indexed_string_unsupported: false,
        // nusb enumeration is non-invasive: usage is only known after open.
        usage_at_enumerate: false,
        // Real USB gadget: bcdDevice 0x0100, interface 0.
        bus_type: conformance::BusType::Usb,
        release_number: 0x0100,
        interface_number: 0,
        backend: conformance::Backend::Nusb,
    }
}

fn have_root() -> bool {
    for m in ["dummy_hcd", "libcomposite", "usb_f_hid"] {
        let _ = Command::new("modprobe").arg(m).status();
    }
    let probe = "/sys/kernel/config/usb_gadget/.hidra_probe";
    match fs::create_dir(probe) {
        Ok(()) => {
            let _ = fs::remove_dir(probe);
            true
        }
        Err(_) => false,
    }
}

#[test]
fn nusb_conformance() {
    if !have_root() {
        if std::env::var("HIDRA_NUSB_REQUIRED").is_ok() {
            panic!("HIDRA_NUSB_REQUIRED set but cannot set up USB gadget (need root + dummy_hcd)");
        }
        eprintln!("SKIP: cannot set up USB gadget (need root + dummy_hcd); run under sudo");
        return;
    }

    let caps = nusb_caps();
    for numbered in [false, true] {
        let dev = NusbGadget::create(numbered);
        run_conformance::<hidra::Nusb>(numbered, &caps, &dev);
        drop(dev);
    }
    eprintln!("PASS: nusb gadget full-API conformance test");
}

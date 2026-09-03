//! hidra's `nusb` backend against a HID device with **no endpoints at all**.
//!
//! HID 1.11 §4.4 mandates an interrupt IN endpoint, but control-only devices
//! that declare `bNumEndpoints` 0 exist in the wild and are perfectly usable
//! through GET_REPORT/SET_REPORT on the control pipe. The Sinowealth ISP
//! bootloaders are exactly that shape, and the backend's fallback for them was
//! going untested: `g_hid` always creates an interrupt IN endpoint, so the
//! `nusb` conformance test cannot produce this device.
//!
//! FunctionFS can, because userspace supplies the descriptors verbatim, at the
//! price of servicing the interface's control requests by hand.
//!
//! Needs root; self-skips otherwise unless `HIDRA_NUSB_REQUIRED=1` forces a
//! failure. Run: `sudo -E $(rustup which cargo) test -p linux-nusb`.
#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use conformance::{
    make_descriptor, MANUFACTURER, PRODUCT, RID_FEATURE, SERIAL, TEST_PID, TEST_VID,
};
use hidra::{Backend, Hidra, MaybeFuture};

const GADGET: &str = "/sys/kernel/config/usb_gadget/hidra_ctrl";
const FFS_INSTANCE: &str = "hidractl";
const FFS_MOUNT: &str = "/dev/ffs-hidractl";

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
const FUNCTIONFS_HAS_HS_DESC: u32 = 2;
const FUNCTIONFS_ALL_CTRL_RECIP: u32 = 1 << 6;

const FFS_SETUP: u8 = 4;

const HID_GET_REPORT: u8 = 0x01;
const HID_SET_REPORT: u8 = 0x09;
const GET_DESCRIPTOR: u8 = 0x06;
const DESCRIPTOR_TYPE_HID_REPORT: u8 = 0x22;

const FEAT_PAYLOAD: [u8; 8] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];
const OUT_PAYLOAD: [u8; 8] = [0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7];

fn w(path: &str, val: &str) -> std::io::Result<()> {
    fs::write(path, val)
}

/// Interface descriptor only: HID class, and deliberately no endpoints.
fn function_descriptors(report_desc_len: usize) -> Vec<u8> {
    let mut d = vec![9, 0x04, 0, 0, 0, 0x03, 0x00, 0x00, 1];
    // The HID class descriptor still declares the report descriptor, which is
    // how the backend learns its length before asking for it.
    let len = report_desc_len as u16;
    d.extend_from_slice(&[
        9,
        0x21,
        0x11,
        0x01,
        0x00,
        0x01,
        DESCRIPTOR_TYPE_HID_REPORT,
        (len & 0xff) as u8,
        (len >> 8) as u8,
    ]);
    d
}

fn descriptor_blob(report_desc_len: usize) -> Vec<u8> {
    let descs = function_descriptors(report_desc_len);
    let flags = FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC | FUNCTIONFS_ALL_CTRL_RECIP;
    let length = 12 + 4 + 4 + descs.len() * 2;

    let mut blob = Vec::with_capacity(length);
    blob.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    blob.extend_from_slice(&(length as u32).to_le_bytes());
    blob.extend_from_slice(&flags.to_le_bytes());
    blob.extend_from_slice(&2u32.to_le_bytes()); // fs: interface + HID descriptor
    blob.extend_from_slice(&2u32.to_le_bytes()); // hs: same
    blob.extend_from_slice(&descs);
    blob.extend_from_slice(&descs);
    blob
}

fn strings_blob() -> Vec<u8> {
    let text = CString::new("hidra control-only").unwrap();
    let length = 16 + 2 + text.as_bytes_with_nul().len();

    let mut blob = Vec::with_capacity(length);
    blob.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    blob.extend_from_slice(&(length as u32).to_le_bytes());
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&0x0409u16.to_le_bytes());
    blob.extend_from_slice(text.as_bytes_with_nul());
    blob
}

fn unmount_ffs() {
    if let Ok(path) = CString::new(FFS_MOUNT) {
        // SAFETY: a NUL-terminated path; failing (not mounted) is fine here.
        unsafe { libc::umount(path.as_ptr()) };
    }
}

fn teardown() {
    if !Path::new(GADGET).exists() {
        return;
    }
    let _ = w(&format!("{GADGET}/UDC"), "\n");
    let _ = fs::remove_file(format!("{GADGET}/configs/c.1/ffs.{FFS_INSTANCE}"));
    let _ = fs::remove_dir(format!("{GADGET}/configs/c.1/strings/0x409"));
    let _ = fs::remove_dir(format!("{GADGET}/configs/c.1"));
    unmount_ffs();
    let _ = fs::remove_dir(format!("{GADGET}/functions/ffs.{FFS_INSTANCE}"));
    let _ = fs::remove_dir(format!("{GADGET}/strings/0x409"));
    let _ = fs::remove_dir(GADGET);
}

fn mount_ffs() {
    fs::create_dir_all(FFS_MOUNT).expect("mkdir ffs mount point");
    let src = CString::new(FFS_INSTANCE).unwrap();
    let target = CString::new(FFS_MOUNT).unwrap();
    let fstype = CString::new("functionfs").unwrap();
    // SAFETY: three NUL-terminated strings, valid for the duration of the call.
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    assert_eq!(
        rc,
        0,
        "mount functionfs: {}",
        std::io::Error::last_os_error()
    );
}

fn setup_gadget() {
    for m in ["dummy_hcd", "libcomposite", "usb_f_fs"] {
        let _ = Command::new("modprobe").arg(m).status();
    }
    teardown();

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

    fs::create_dir_all(format!("{GADGET}/functions/ffs.{FFS_INSTANCE}")).expect("mkdir ffs");
    fs::create_dir_all(format!("{GADGET}/configs/c.1/strings/0x409")).expect("mkdir config");
    w(
        &format!("{GADGET}/configs/c.1/strings/0x409/configuration"),
        "hidra\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        format!("{GADGET}/functions/ffs.{FFS_INSTANCE}"),
        format!("{GADGET}/configs/c.1/ffs.{FFS_INSTANCE}"),
    )
    .expect("link function");
}

fn bind_udc() {
    let udc = fs::read_dir("/sys/class/udc")
        .expect("read /sys/class/udc")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .expect("no UDC available (dummy_hcd not loaded?)");
    w(&format!("{GADGET}/UDC"), &format!("{udc}\n")).expect("bind UDC");
}

/// Answers the control requests the host aims at the interface.
fn ep0_loop(
    mut ep0: File,
    report_desc: Vec<u8>,
    last_set_report: Arc<Mutex<Option<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
) {
    let mut event = [0u8; 12];
    while !stop.load(Ordering::Relaxed) {
        if !matches!(ep0.read(&mut event), Ok(12)) {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if event[8] != FFS_SETUP {
            continue;
        }
        let request_type = event[0];
        let request = event[1];
        let value = u16::from_le_bytes([event[2], event[3]]);
        let length = u16::from_le_bytes([event[6], event[7]]) as usize;
        let to_host = request_type & 0x80 != 0;
        let report_id = (value & 0xff) as u8;

        match (to_host, request) {
            (true, GET_DESCRIPTOR) if value >> 8 == u16::from(DESCRIPTOR_TYPE_HID_REPORT) => {
                let n = report_desc.len().min(length);
                let _ = ep0.write(&report_desc[..n]);
            }
            (true, HID_GET_REPORT) => {
                let mut body = vec![report_id];
                body.extend_from_slice(&FEAT_PAYLOAD);
                let n = body.len().min(length);
                let _ = ep0.write(&body[..n]);
            }
            (false, HID_SET_REPORT) => {
                let mut body = vec![0u8; length];
                let n = ep0.read(&mut body).unwrap_or(0);
                body.truncate(n);
                *last_set_report.lock().unwrap() = Some(body);
            }
            // Anything else is stalled by transferring the other way.
            _ => {
                if to_host {
                    let mut sink = [0u8; 1];
                    let _ = ep0.read(&mut sink);
                } else {
                    let _ = ep0.write(&[]);
                }
            }
        }
    }
}

struct ControlOnlyGadget {
    last_set_report: Arc<Mutex<Option<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
}

impl ControlOnlyGadget {
    fn create(report_desc: Vec<u8>) -> Self {
        setup_gadget();
        mount_ffs();

        let mut ep0 = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{FFS_MOUNT}/ep0"))
            .expect("open ep0");
        ep0.write_all(&descriptor_blob(report_desc.len()))
            .expect("write descriptors");
        ep0.write_all(&strings_blob()).expect("write strings");

        bind_udc();

        let last_set_report = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, stop_t) = (Arc::clone(&last_set_report), Arc::clone(&stop));
        thread::spawn(move || ep0_loop(ep0, report_desc, seen, stop_t));

        ControlOnlyGadget {
            last_set_report,
            stop,
        }
    }
}

impl Drop for ControlOnlyGadget {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        teardown();
    }
}

fn have_root() -> bool {
    for m in ["dummy_hcd", "libcomposite", "usb_f_fs"] {
        let _ = Command::new("modprobe").arg(m).status();
    }
    let probe = "/sys/kernel/config/usb_gadget/.hidra_ctrl_probe";
    match fs::create_dir(probe) {
        Ok(()) => {
            let _ = fs::remove_dir(probe);
            true
        }
        Err(_) => false,
    }
}

#[test]
fn nusb_control_only_device() {
    if !have_root() {
        if std::env::var("HIDRA_NUSB_REQUIRED").is_ok() {
            panic!("HIDRA_NUSB_REQUIRED set but cannot set up USB gadget (need root + dummy_hcd)");
        }
        eprintln!("SKIP: cannot set up USB gadget (need root + dummy_hcd); run under sudo");
        return;
    }

    let report_desc = make_descriptor(true);
    let gadget = ControlOnlyGadget::create(report_desc.clone());

    let api = Hidra::builder()
        .backend(Backend::Nusb)
        .build()
        .expect("open nusb backend");

    // Enumeration must still find it: no endpoints does not mean no device.
    let deadline = Instant::now() + Duration::from_secs(5);
    let info = loop {
        let found = api
            .enumerate(TEST_VID, TEST_PID)
            .expect("enumerate")
            .into_iter()
            .next();
        if let Some(info) = found {
            break info;
        }
        assert!(
            Instant::now() < deadline,
            "control-only device never enumerated"
        );
        thread::sleep(Duration::from_millis(100));
    };
    eprintln!(
        "enumerated {:04x}:{:04x}",
        info.vendor_id(),
        info.product_id()
    );

    let dev = api
        .open_path(info.path())
        .wait()
        .expect("open control-only device");

    let got = dev.report_descriptor().wait().expect("report descriptor");
    assert_eq!(got, report_desc, "report descriptor mismatch");
    eprintln!(
        "report descriptor: {} bytes over the control pipe",
        got.len()
    );

    // Feature reports are the whole API on a device like this.
    let mut buf = vec![0u8; 1 + FEAT_PAYLOAD.len()];
    buf[0] = RID_FEATURE;
    let n = dev
        .get_feature_report(&mut buf)
        .wait()
        .expect("get_feature_report");
    assert_eq!(
        &buf[..n],
        &[&[RID_FEATURE][..], &FEAT_PAYLOAD[..]].concat()[..n]
    );
    eprintln!("get_feature_report: {n} bytes");

    let mut feature = vec![RID_FEATURE];
    feature.extend_from_slice(&FEAT_PAYLOAD);
    dev.send_feature_report(&feature)
        .wait()
        .expect("send_feature_report");
    assert_eq!(
        gadget.last_set_report.lock().unwrap().clone(),
        Some(feature.clone()),
        "device did not see the feature report"
    );
    eprintln!("send_feature_report: SET_REPORT(Feature) reached the device");

    // With no interrupt OUT endpoint, write has to fall back to
    // SET_REPORT(Output) rather than fail.
    let mut out = vec![RID_FEATURE];
    out.extend_from_slice(&OUT_PAYLOAD);
    dev.write(&out)
        .wait()
        .expect("write via SET_REPORT(Output)");
    assert_eq!(
        gadget.last_set_report.lock().unwrap().clone(),
        Some(out),
        "output report did not reach the device over the control pipe"
    );
    eprintln!("write: fell back to SET_REPORT(Output)");

    // And with no interrupt IN endpoint, read must refuse rather than block
    // forever waiting for a report that can never arrive.
    let started = Instant::now();
    let mut inbuf = [0u8; 64];
    let err = dev
        .read(&mut inbuf)
        .wait()
        .expect_err("read should fail on a device with no interrupt IN endpoint");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "read blocked for {:?} instead of failing",
        started.elapsed()
    );
    assert!(
        err.to_string().contains("no interrupt IN endpoint"),
        "read failed for the wrong reason: {err}"
    );
    eprintln!("read refused as it must: {err}");

    drop(dev);
    drop(gadget);
    eprintln!("PASS: nusb control-only (bNumEndpoints 0) device test");
}

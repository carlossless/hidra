//! A **vendor-class** USB gadget for the WebUSB conformance test.
//!
//! WebUSB cannot claim a HID-class interface, so this cannot use `usb_f_hid`
//! the way the `linux-nusb` fixture does: the interface has to declare class
//! 0xFF. FunctionFS is what allows that — userspace supplies the descriptors
//! verbatim — at the cost of also having to service the endpoints and, with
//! `FUNCTIONFS_ALL_CTRL_RECIP`, the interface's control requests by hand.
//!
//! The device still speaks the HID protocol on the wire (GET_REPORT /
//! SET_REPORT, an interrupt IN stream), which is exactly the shape of device
//! the WebUSB backend exists for.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

const GADGET: &str = "/sys/kernel/config/usb_gadget/hidra_webusb";
const FFS_INSTANCE: &str = "hidra";
const FFS_MOUNT: &str = "/dev/ffs-hidra";

const TEST_VID: u16 = 0x1209;
const TEST_PID: u16 = 0x000c;
const PRODUCT: &str = "hidra-conformance";
const SERIAL: &str = "HIDRA-CONF-01";
const MANUFACTURER: &str = "hidra";

const RID_INPUT: u8 = 0x11;
const RID_FEATURE: u8 = 0x33;
const IN_PAYLOAD: [u8; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
const FEAT_PAYLOAD: [u8; 8] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
const FUNCTIONFS_HAS_HS_DESC: u32 = 2;
const FUNCTIONFS_ALL_CTRL_RECIP: u32 = 1 << 6;

const FFS_BIND: u8 = 0;
const FFS_UNBIND: u8 = 1;
const FFS_ENABLE: u8 = 2;
const FFS_DISABLE: u8 = 3;
const FFS_SETUP: u8 = 4;

const HID_GET_REPORT: u8 = 0x01;
const HID_SET_REPORT: u8 = 0x09;
const GET_DESCRIPTOR: u8 = 0x06;
const DESCRIPTOR_TYPE_HID_REPORT: u8 = 0x22;

/// One interface descriptor plus two interrupt endpoints, FS and HS alike.
fn function_descriptors() -> Vec<u8> {
    let mut d = Vec::new();
    // interface: 2 endpoints, class 0xFF (vendor-specific) — the whole point.
    d.extend_from_slice(&[9, 0x04, 0, 0, 2, 0xFF, 0x00, 0x00, 1]);
    // EP1 IN, interrupt, 64 bytes
    d.extend_from_slice(&[7, 0x05, 0x81, 0x03, 64, 0, 4]);
    // EP2 OUT, interrupt, 64 bytes
    d.extend_from_slice(&[7, 0x05, 0x02, 0x03, 64, 0, 4]);
    d
}

fn descriptor_blob() -> Vec<u8> {
    let descs = function_descriptors();
    let flags = FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC | FUNCTIONFS_ALL_CTRL_RECIP;
    // head (12) + fs_count (4) + hs_count (4) + both descriptor sets
    let length = 12 + 4 + 4 + descs.len() * 2;

    let mut blob = Vec::with_capacity(length);
    blob.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    blob.extend_from_slice(&(length as u32).to_le_bytes());
    blob.extend_from_slice(&flags.to_le_bytes());
    blob.extend_from_slice(&3u32.to_le_bytes()); // fs: interface + 2 endpoints
    blob.extend_from_slice(&3u32.to_le_bytes()); // hs: same
    blob.extend_from_slice(&descs);
    blob.extend_from_slice(&descs);
    blob
}

fn strings_blob() -> Vec<u8> {
    let text = CString::new("hidra webusb conformance").unwrap();
    // head (16) + lang code (2) + the string
    let length = 16 + 2 + text.as_bytes_with_nul().len();

    let mut blob = Vec::with_capacity(length);
    blob.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    blob.extend_from_slice(&(length as u32).to_le_bytes());
    blob.extend_from_slice(&1u32.to_le_bytes()); // one string
    blob.extend_from_slice(&1u32.to_le_bytes()); // one language
    blob.extend_from_slice(&0x0409u16.to_le_bytes());
    blob.extend_from_slice(text.as_bytes_with_nul());
    blob
}

/// The report descriptor served over GET_DESCRIPTOR(Report).
///
/// A vendor-class interface is not obliged to carry one, and hidra reports
/// `Unsupported` when it does not; serving it here exercises the path that
/// does exist rather than leaving it untested.
#[rustfmt::skip]
fn report_descriptor() -> Vec<u8> {
    vec![
        0x06, 0x00, 0xFF, // usage page (vendor-defined 0xFF00)
        0x09, 0x01, //       usage 1
        0xA1, 0x01, //       collection (application)
        0x85, RID_INPUT, //  report id
        0x09, 0x02, //       usage 2
        0x15, 0x00, //       logical min 0
        0x26, 0xFF, 0x00, // logical max 255
        0x75, 0x08, //       report size 8
        0x95, 0x08, //       report count 8
        0x81, 0x02, //       input (data, var, abs)
        0x85, RID_FEATURE, // report id
        0x09, 0x03, //       usage 3
        0x75, 0x08, 0x95, 0x08, 0xB1, 0x02, // feature
        0xC0, //             end collection
    ]
}

fn w(path: &str, value: &str) -> std::io::Result<()> {
    fs::write(path, value)
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

fn unmount_ffs() {
    if let Ok(path) = CString::new(FFS_MOUNT) {
        // SAFETY: a NUL-terminated path; failure (not mounted) is fine here.
        unsafe { libc::umount(path.as_ptr()) };
    }
}

fn mount_ffs() {
    fs::create_dir_all(FFS_MOUNT).expect("mkdir ffs mount point");
    let src = CString::new(FFS_INSTANCE).unwrap();
    let target = CString::new(FFS_MOUNT).unwrap();
    let fstype = CString::new("functionfs").unwrap();
    // SAFETY: all four pointers are NUL-terminated strings valid for the call.
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
        .map(|e| e.file_name().as_bytes().to_vec())
        .map(|n| String::from_utf8_lossy(&n).into_owned())
        .next()
        .expect("no UDC available (dummy_hcd not loaded?)");
    w(&format!("{GADGET}/UDC"), &format!("{udc}\n")).expect("bind UDC");
}

/// Service ep0: the control requests the host aims at our interface.
fn ep0_loop(mut ep0: File) {
    let mut event = [0u8; 12];
    loop {
        match ep0.read(&mut event) {
            Ok(12) => {}
            Ok(_) => continue,
            Err(e) => {
                eprintln!("ep0 read: {e}");
                return;
            }
        }
        // struct usb_functionfs_event: an 8-byte usb_ctrlrequest, then the type.
        let kind = event[8];
        match kind {
            FFS_BIND => println!("BIND"),
            FFS_UNBIND => println!("UNBIND"),
            FFS_ENABLE => println!("ENABLE"),
            FFS_DISABLE => println!("DISABLE"),
            FFS_SETUP => {
                let request_type = event[0];
                let request = event[1];
                let value = u16::from_le_bytes([event[2], event[3]]);
                let length = u16::from_le_bytes([event[6], event[7]]) as usize;
                handle_setup(&mut ep0, request_type, request, value, length);
            }
            _ => {}
        }
    }
}

fn handle_setup(ep0: &mut File, request_type: u8, request: u8, value: u16, length: usize) {
    let to_host = request_type & 0x80 != 0;
    let report_id = (value & 0xFF) as u8;
    let report_type = value >> 8;

    match (to_host, request) {
        // GET_DESCRIPTOR(Report) — standard, interface recipient.
        (true, GET_DESCRIPTOR) if report_type == u16::from(DESCRIPTOR_TYPE_HID_REPORT) => {
            let desc = report_descriptor();
            let n = desc.len().min(length);
            let _ = ep0.write(&desc[..n]);
            println!("GET_DESCRIPTOR(report) {n} bytes");
        }
        (true, HID_GET_REPORT) => {
            let mut body = Vec::with_capacity(1 + FEAT_PAYLOAD.len());
            body.push(report_id);
            body.extend_from_slice(&FEAT_PAYLOAD);
            let n = body.len().min(length);
            let _ = ep0.write(&body[..n]);
            println!("GET_REPORT id={report_id:#04x} {n} bytes");
        }
        (false, HID_SET_REPORT) => {
            let mut body = vec![0u8; length];
            let n = ep0.read(&mut body).unwrap_or(0);
            println!("SET_REPORT id={report_id:#04x} data={:02x?}", &body[..n]);
        }
        _ => {
            // Everything else is stalled by transferring in the wrong direction.
            println!("STALL type={request_type:#04x} req={request:#04x}");
            if to_host {
                let mut sink = [0u8; 1];
                let _ = ep0.read(&mut sink);
            } else {
                let _ = ep0.write(&[]);
            }
        }
    }
}

fn main() {
    setup_gadget();
    mount_ffs();

    let mut ep0 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{FFS_MOUNT}/ep0"))
        .expect("open ep0");
    ep0.write_all(&descriptor_blob())
        .expect("write descriptors");
    ep0.write_all(&strings_blob()).expect("write strings");

    bind_udc();

    // The endpoint files only appear once the descriptors are accepted.
    let ep1 = OpenOptions::new()
        .write(true)
        .open(format!("{FFS_MOUNT}/ep1"))
        .expect("open ep1");
    let ep2 = OpenOptions::new()
        .read(true)
        .open(format!("{FFS_MOUNT}/ep2"))
        .expect("open ep2");

    thread::spawn(move || ep0_loop(ep0));

    // Interrupt IN: keep an input report available for the harness to read.
    thread::spawn(move || {
        let mut ep1 = ep1;
        let mut report = Vec::with_capacity(1 + IN_PAYLOAD.len());
        report.push(RID_INPUT);
        report.extend_from_slice(&IN_PAYLOAD);
        loop {
            if ep1.write(&report).is_err() {
                thread::sleep(Duration::from_millis(100));
            }
        }
    });

    // Interrupt OUT: log whatever the harness writes.
    thread::spawn(move || {
        let mut ep2 = ep2;
        let mut buf = [0u8; 64];
        loop {
            match ep2.read(&mut buf) {
                Ok(n) if n > 0 => println!("OUTPUT {:02x?}", &buf[..n]),
                Ok(_) => {}
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
    });

    println!("READY");
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

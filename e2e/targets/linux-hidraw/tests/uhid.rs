//! Conformance suite (the `conformance` crate) on the Linux `hidraw` backend,
//! against virtual HID devices created via the kernel's `uhid` interface.
//! Needs write access to `/dev/uhid` (root); self-skips otherwise unless
//! `HIDRA_HIDRAW_REQUIRED=1` (CI) forces a failure.
//! Run: `sudo -E $(rustup which cargo) test -p linux-hidraw`.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hidra::descriptor::{CollectionKind, DescriptorBuilder, MainFlags};
use hidra::MaybeFuture;

use conformance::{
    make_descriptor, run_conformance, Caps, VirtualDevice, PRODUCT, SERIAL, TEST_PID, TEST_VID,
};

// uhid event types (linux/uhid.h)
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

const EVENT_SIZE: usize = 4380; // >= sizeof(struct uhid_event)
const O_NONBLOCK: i32 = 0o4000;

fn le16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn le32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn rd16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn event_type(buf: &[u8], n: usize) -> Option<u32> {
    (n >= 4).then(|| u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

fn create2_event(name: &str, uniq: &str, bus: u16, vid: u16, pid: u16, rd: &[u8]) -> Vec<u8> {
    let mut e = vec![0u8; EVENT_SIZE];
    le32(&mut e, 0, UHID_CREATE2);
    // name[128]@4, phys[64]@132, uniq[64]@196, rd_size@260, bus@262,
    // vendor@264, product@268, version@272, country@276, rd_data@280
    let nb = name.as_bytes();
    e[4..4 + nb.len().min(127)].copy_from_slice(&nb[..nb.len().min(127)]);
    let ub = uniq.as_bytes();
    e[196..196 + ub.len().min(63)].copy_from_slice(&ub[..ub.len().min(63)]);
    le16(&mut e, 260, rd.len() as u16);
    le16(&mut e, 262, bus);
    le32(&mut e, 264, vid as u32);
    le32(&mut e, 268, pid as u32);
    e[280..280 + rd.len()].copy_from_slice(rd);
    e
}

fn input2_event(data: &[u8]) -> Vec<u8> {
    let mut e = vec![0u8; EVENT_SIZE];
    le32(&mut e, 0, UHID_INPUT2);
    le16(&mut e, 4, data.len() as u16); // size@4, data@6
    e[6..6 + data.len()].copy_from_slice(data);
    e
}

fn simple_event(ty: u32) -> Vec<u8> {
    let mut e = vec![0u8; EVENT_SIZE];
    le32(&mut e, 0, ty);
    e
}

fn read_event(f: &mut std::fs::File, buf: &mut [u8]) -> Option<usize> {
    match f.read(buf) {
        Ok(n) => Some(n),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(e) => panic!("uhid read failed: {e}"),
    }
}

fn wait_for_start(uhid: &mut std::fs::File) {
    let mut buf = vec![0u8; EVENT_SIZE];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match read_event(uhid, &mut buf) {
            Some(n) if event_type(&buf, n) == Some(UHID_START) => return,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("device never reported UHID_START");
}

/// Background thread answers GET_REPORT with the primed payload and records
/// SET_REPORT / OUTPUT payloads.
struct UhidDevice {
    write_fd: Mutex<std::fs::File>,
    stop: Arc<AtomicBool>,
    primed: Arc<Mutex<Vec<u8>>>,
    last_output: Arc<Mutex<Option<Vec<u8>>>>,
    last_set: Arc<Mutex<Option<Vec<u8>>>>,
    responder: Option<std::thread::JoinHandle<()>>,
}

impl UhidDevice {
    fn create(numbered: bool) -> Self {
        let mut uhid = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open("/dev/uhid")
            .expect("open /dev/uhid");
        let rd = make_descriptor(numbered);
        // name -> product string, uniq -> serial string.
        uhid.write_all(&create2_event(
            PRODUCT, SERIAL, 0x03, TEST_VID, TEST_PID, &rd,
        ))
        .expect("write CREATE2");
        wait_for_start(&mut uhid);

        let write_fd = uhid.try_clone().expect("clone uhid fd");
        let stop = Arc::new(AtomicBool::new(false));
        let primed = Arc::new(Mutex::new(Vec::new()));
        let last_output = Arc::new(Mutex::new(None));
        let last_set = Arc::new(Mutex::new(None));

        let (stop_t, primed_t, out_t, set_t) = (
            Arc::clone(&stop),
            Arc::clone(&primed),
            Arc::clone(&last_output),
            Arc::clone(&last_set),
        );
        let responder = std::thread::spawn(move || {
            let mut fd = uhid;
            let mut buf = vec![0u8; EVENT_SIZE];
            while !stop_t.load(Ordering::Relaxed) {
                match read_event(&mut fd, &mut buf) {
                    Some(n) => match event_type(&buf, n) {
                        Some(UHID_GET_REPORT) => {
                            // id@4 (u32), rnum@8. Real USB HID prefixes GET_REPORT
                            // results to hidraw with the report number (byte 0, even
                            // 0 when unnumbered — confirmed against hardware via
                            // Cynthion); emulate: rnum followed by the primed body.
                            let id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                            let rnum = buf[8];
                            let primed = primed_t.lock().unwrap().clone();
                            let body = &primed[primed.len().saturating_sub(8)..];
                            let mut data = vec![rnum];
                            data.extend_from_slice(body);
                            let mut reply = simple_event(UHID_GET_REPORT_REPLY);
                            le32(&mut reply, 4, id);
                            le16(&mut reply, 8, 0); // err
                            le16(&mut reply, 10, data.len() as u16);
                            reply[12..12 + data.len()].copy_from_slice(&data);
                            fd.write_all(&reply).ok();
                        }
                        Some(UHID_SET_REPORT) => {
                            // id@4, rnum@8, rtype@9, size@10, data@12
                            let id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                            let size = rd16(&buf, 10) as usize;
                            *set_t.lock().unwrap() = Some(buf[12..12 + size].to_vec());
                            let mut reply = simple_event(UHID_SET_REPORT_REPLY);
                            le32(&mut reply, 4, id);
                            le16(&mut reply, 8, 0); // err
                            fd.write_all(&reply).ok();
                        }
                        Some(UHID_OUTPUT) => {
                            // data@4, size@4100
                            let size = rd16(&buf, 4100) as usize;
                            *out_t.lock().unwrap() = Some(buf[4..4 + size].to_vec());
                        }
                        _ => {}
                    },
                    None => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });

        UhidDevice {
            write_fd: Mutex::new(write_fd),
            stop,
            primed,
            last_output,
            last_set,
            responder: Some(responder),
        }
    }
}

impl VirtualDevice for UhidDevice {
    fn inject_input(&self, wire: &[u8]) {
        self.write_fd
            .lock()
            .unwrap()
            .write_all(&input2_event(wire))
            .expect("INPUT2");
    }
    fn prime_get(&self, wire: &[u8]) {
        *self.primed.lock().unwrap() = wire.to_vec();
    }
    fn last_output(&self) -> Option<Vec<u8>> {
        self.last_output.lock().unwrap().clone()
    }
    fn last_set_feature(&self) -> Option<Vec<u8>> {
        self.last_set.lock().unwrap().clone()
    }
    fn disconnect(&self) {
        self.write_fd
            .lock()
            .unwrap()
            .write_all(&simple_event(UHID_DESTROY))
            .ok();
    }
}

impl Drop for UhidDevice {
    fn drop(&mut self) {
        // Best-effort: disconnect() may already have destroyed it.
        self.write_fd
            .lock()
            .unwrap()
            .write_all(&simple_event(UHID_DESTROY))
            .ok();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.responder.take() {
            h.join().ok();
        }
    }
}

fn linux_caps() -> Caps {
    Caps {
        numbered: true,
        strings: true,
        manufacturer: false,
        exact_descriptor: true,
        feature: true,
        input_get: true,
        disconnect: true,
        indexed_string_unsupported: true,
        usage_at_enumerate: true,
        // uhid creates a BUS_USB device with no bcdDevice and no USB interface.
        bus_type: conformance::BusType::Usb,
        release_number: 0x0000,
        interface_number: -1,
    }
}

fn should_skip() -> bool {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/uhid")
    {
        Ok(_) => false,
        Err(e) => {
            if std::env::var("HIDRA_HIDRAW_REQUIRED").is_ok() {
                panic!("HIDRA_HIDRAW_REQUIRED set but /dev/uhid unavailable: {e}");
            }
            eprintln!("SKIP: cannot open /dev/uhid ({e}); run under sudo to exercise this test");
            true
        }
    }
}

#[test]
fn uhid_conformance() {
    if should_skip() {
        return;
    }
    let caps = linux_caps();
    for numbered in [false, true] {
        let dev = UhidDevice::create(numbered);
        run_conformance(numbered, &caps, &dev);
        drop(dev);
    }
    eprintln!("PASS: uhid full-API conformance test");
}

#[test]
fn uhid_multi_collection() {
    if should_skip() {
        return;
    }
    let mut uhid = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/uhid")
        .expect("open /dev/uhid");

    let mut b = DescriptorBuilder::new();
    b.usage_page(0xFF00)
        .usage(0x01)
        .collection(CollectionKind::Application)
        .logical_minimum(0)
        .logical_maximum(255)
        .report_size(8)
        .report_count(4)
        .usage(0x10)
        .input(MainFlags::VARIABLE)
        .end_collection()
        .usage(0x02)
        .collection(CollectionKind::Application)
        .report_size(8)
        .report_count(4)
        .usage(0x20)
        .input(MainFlags::VARIABLE)
        .end_collection();
    let rd = b.build();

    // Use a distinct pid so it can't collide with the other test's device.
    let pid = 0x0002u16;
    uhid.write_all(&create2_event(PRODUCT, SERIAL, 0x03, TEST_VID, pid, &rd))
        .expect("write CREATE2");
    wait_for_start(&mut uhid);

    let api = hidra::Hidra::new().unwrap();
    let mut entries = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        entries = api.enumerate(TEST_VID, pid).unwrap();
        if entries.len() >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        entries.len(),
        2,
        "expected one DeviceInfo per top-level collection"
    );
    let usages: Vec<(u16, u16)> = entries
        .iter()
        .map(|d| (d.usage_page(), d.usage()))
        .collect();
    assert!(usages.contains(&(0xFF00, 0x01)), "usages: {usages:?}");
    assert!(usages.contains(&(0xFF00, 0x02)), "usages: {usages:?}");
    assert_eq!(entries[0].path(), entries[1].path(), "same underlying node");

    uhid.write_all(&simple_event(UHID_DESTROY)).ok();
    eprintln!("PASS: uhid multi-collection test");
}

fn check_descriptor(
    pid: u16,
    rd: &[u8],
    check: impl Fn(&hidra::descriptor::ReportDescriptor, &[u8]),
) {
    let mut uhid = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/uhid")
        .expect("open /dev/uhid");
    uhid.write_all(&create2_event(PRODUCT, SERIAL, 0x03, TEST_VID, pid, rd))
        .expect("write CREATE2");
    wait_for_start(&mut uhid);

    let api = hidra::Hidra::new().unwrap();
    // The hidraw node can lag the sysfs entry enumerate() reads, so retry the
    // open too, not just the enumeration.
    let device = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let opened = api
                .enumerate(TEST_VID, pid)
                .unwrap()
                .into_iter()
                .next()
                .and_then(|info| api.open_path(info.path()).wait().ok());
            if let Some(d) = opened {
                break d;
            }
            if Instant::now() >= deadline {
                panic!("device {pid:#06x} did not enumerate and open");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let raw = device
        .report_descriptor()
        .wait()
        .expect("report_descriptor");
    let parsed = device.parsed_report_descriptor().wait().expect("parsed");
    check(&parsed, &raw);
    drop(device);
    uhid.write_all(&simple_event(UHID_DESTROY)).ok();
}

#[test]
fn uhid_descriptor_variety() {
    if should_skip() {
        return;
    }
    use hidra::descriptor::ReportKind;

    let mut b = DescriptorBuilder::new();
    b.usage_page(0xFF00)
        .usage(0x01)
        .collection(CollectionKind::Application)
        .logical_minimum(0)
        .logical_maximum(255)
        .report_size(8)
        .report_count(2)
        .report_id(1)
        .usage(0x02)
        .input(MainFlags::VARIABLE)
        .report_size(16)
        .report_count(1)
        .report_id(2)
        .usage(0x03)
        .feature(MainFlags::VARIABLE)
        .report_size(8)
        .report_count(8)
        .report_id(3)
        .usage(0x04)
        .output(MainFlags::VARIABLE)
        .end_collection();
    let rd = b.build();
    check_descriptor(0x0010, &rd, |desc, raw| {
        assert_eq!(raw, rd.as_slice(), "mixed: descriptor byte round-trip");
        assert!(desc.uses_report_ids(), "mixed: uses report ids");
        let inp = desc
            .reports
            .iter()
            .find(|r| r.kind == ReportKind::Input)
            .expect("input report");
        assert_eq!(inp.report_id, Some(1), "mixed: input report id");
        assert_eq!(inp.size_bytes(), 2, "mixed: input 8x2 bits = 2 bytes");
        let feat = desc
            .reports
            .iter()
            .find(|r| r.kind == ReportKind::Feature)
            .expect("feature report");
        assert_eq!(feat.report_id, Some(2), "mixed: feature report id");
        assert_eq!(feat.size_bytes(), 2, "mixed: feature 16x1 bits = 2 bytes");
        let out = desc
            .reports
            .iter()
            .find(|r| r.kind == ReportKind::Output)
            .expect("output report");
        assert_eq!(out.report_id, Some(3), "mixed: output report id");
        assert_eq!(out.size_bytes(), 8, "mixed: output 8x8 bits = 8 bytes");
    });

    let mut b = DescriptorBuilder::new();
    b.usage_page(0xFF00)
        .usage(0x01)
        .collection(CollectionKind::Application)
        .logical_minimum(0)
        .logical_maximum(255)
        .report_size(8)
        .report_count(1)
        .usage(0x05)
        .input(MainFlags::VARIABLE)
        .report_count(1)
        .input(MainFlags::CONSTANT)
        .report_count(1)
        .usage(0x06)
        .input(MainFlags::VARIABLE)
        .end_collection();
    let rd = b.build();
    check_descriptor(0x0011, &rd, |desc, raw| {
        assert_eq!(raw, rd.as_slice(), "padding: descriptor byte round-trip");
        let inp = desc
            .reports
            .iter()
            .find(|r| r.kind == ReportKind::Input)
            .expect("input report");
        assert_eq!(inp.fields.len(), 3, "padding: constant field preserved");
        assert_eq!(inp.size_bytes(), 3, "padding: 3 fields x 1 byte");
    });

    let mut b = DescriptorBuilder::new();
    b.usage_page(0xFF00)
        .usage(0x01)
        .collection(CollectionKind::Application)
        .logical_minimum(0)
        .logical_maximum(101)
        .report_size(8)
        .report_count(6)
        .usage_minimum(0)
        .usage_maximum(101)
        .input(MainFlags::NONE) // array (no VARIABLE bit)
        .end_collection();
    let rd = b.build();
    check_descriptor(0x0012, &rd, |desc, raw| {
        assert_eq!(raw, rd.as_slice(), "array: descriptor byte round-trip");
        let inp = desc
            .reports
            .iter()
            .find(|r| r.kind == ReportKind::Input)
            .expect("input report");
        assert_eq!(inp.size_bytes(), 6, "array: 8x6 bits = 6 bytes");
        assert_eq!(inp.fields[0].report_count, 6, "array: 6 elements");
    });

    eprintln!("PASS: uhid descriptor-variety test");
}

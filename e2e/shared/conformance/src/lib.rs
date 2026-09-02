//! Shared cross-platform HID conformance harness (the `conformance` crate).
//!
//! Each platform test crate creates its own virtual HID device with the same
//! descriptor/VID/PID/strings, implements the [`VirtualDevice`] hooks against its
//! native mechanism, and calls [`run_conformance`]. Everything below the
//! [`VirtualDevice`] boundary is identical across platforms, so a pass proves the
//! public API behaves the same on every backend. Genuine per-platform limits are
//! [`Caps`] flags; anything a run can't exercise is logged, never silently skipped.
#![allow(dead_code)]

use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use hidra::descriptor::{CollectionKind, DescriptorBuilder, MainFlags, ReportKind};
pub use hidra::{Backend, BusType};
use hidra::{HidError, MaybeFuture};

pub const TEST_VID: u16 = 0x1209;
pub const TEST_PID: u16 = 0x000c;
pub const PRODUCT: &str = "hidra-conformance";
pub const SERIAL: &str = "HIDRA-CONF-01";
pub const MANUFACTURER: &str = "hidra";

pub const RID_INPUT: u8 = 0x11;
pub const RID_OUTPUT: u8 = 0x22;
pub const RID_FEATURE: u8 = 0x33;

// Distinct per direction so a mix-up (e.g. an output read back as a feature) is caught.
pub const IN_PAYLOAD: [u8; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
pub const OUT_PAYLOAD: [u8; 8] = [0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7];
pub const OUT_PAYLOAD2: [u8; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];
pub const FEAT_PAYLOAD: [u8; 8] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];
pub const FEAT_PAYLOAD2: [u8; 8] = [0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7];

/// What a given platform's virtual device can exercise. Uniform parts always run;
/// anything gated `false` here is logged as skipped-with-reason, not dropped.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub numbered: bool,
    pub strings: bool,
    /// uhid has no USB parent, so it exposes product/serial but not manufacturer.
    pub manufacturer: bool,
    /// `report_descriptor()` returns our exact bytes, vs a semantically-equivalent
    /// reconstruction.
    pub exact_descriptor: bool,
    pub feature: bool,
    pub input_get: bool,
    pub disconnect: bool,
    /// hidraw has no USB string-descriptor access, so `get_indexed_string` must
    /// return `Unsupported`.
    pub indexed_string_unsupported: bool,
    /// `enumerate()` populates `usage_page`/`usage`. False for the USB-transport
    /// backend, which stays non-invasive and learns the usage only on open.
    pub usage_at_enumerate: bool,
    pub bus_type: BusType,
    /// Expected `bcdDevice`, which each virtual-device mechanism sets differently.
    pub release_number: u16,
    /// Expected interface number; -1 where there is no USB interface (e.g. uhid).
    pub interface_number: i32,
    /// Which hidra backend to run the suite against. Selected at run time, so
    /// one test binary can cover both.
    pub backend: Backend,
}

// Two former caps are now invariants: every backend supports write() output
// reports and returns get_feature/get_input as [report-number, body] (0x00 when
// unnumbered) — verified against real USB hardware via a Cynthion on both the
// hidraw and nusb backends.

impl Caps {
    pub fn full() -> Self {
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
            bus_type: BusType::Usb,
            release_number: 0x0100,
            interface_number: 0,
            backend: Backend::Native,
        }
    }
}

/// Device-side hooks the platform implements. The mechanism differs per OS; the
/// observable semantics must not. `wire` bytes include the report-ID prefix when
/// numbered.
pub trait VirtualDevice {
    /// Make a subsequent hidra `read()` return `wire`. The body polls with a
    /// deadline, so re-injecting periodically is fine.
    fn inject_input(&self, wire: &[u8]);

    /// Prime the payload the device returns for GET feature / GET input requests.
    fn prime_get(&self, wire: &[u8]);

    /// Most recent output report `write()` delivered, as the device saw it.
    fn last_output(&self) -> Option<Vec<u8>>;

    /// Most recent SET-feature report delivered, as the device saw it.
    fn last_set_feature(&self) -> Option<Vec<u8>>;

    /// Tear the device down so a pending `read()` observes disconnection. Only
    /// called when `Caps::disconnect` is set.
    fn disconnect(&self) {}

    /// Pause/resume servicing output reports so a short-`set_write_timeout` write
    /// can be made to time out. Only Windows implements and exercises it.
    fn set_output_paused(&self, _paused: bool) {}
}

/// One vendor-defined application collection (usage page 0xFF00, usage 0x01) with
/// 8-byte Input (0x02), Output (0x03) and Feature (0x04) reports, each tagged with
/// a distinct report ID when `numbered`.
pub fn make_descriptor(numbered: bool) -> Vec<u8> {
    let mut b = DescriptorBuilder::new();
    b.usage_page(0xFF00)
        .usage(0x01)
        .collection(CollectionKind::Application)
        .logical_minimum(0)
        .logical_maximum(255)
        .report_size(8)
        .report_count(8);
    if numbered {
        b.report_id(RID_INPUT);
    }
    b.usage(0x02).input(MainFlags::VARIABLE);
    if numbered {
        b.report_id(RID_OUTPUT);
    }
    b.usage(0x03).output(MainFlags::VARIABLE);
    if numbered {
        b.report_id(RID_FEATURE);
    }
    b.usage(0x04).feature(MainFlags::VARIABLE);
    b.end_collection();
    b.build()
}

fn label(numbered: bool) -> &'static str {
    if numbered {
        "numbered"
    } else {
        "unnumbered"
    }
}

/// Poll a hidra future to completion with a wall-clock deadline (dropping it
/// cancels the operation). Returns `None` on timeout. Between polls it calls
/// `tick` so a platform can re-inject input while a `read()` is pending.
fn poll_deadline<T>(fut: impl Future<Output = T>, secs: u64, mut tick: impl FnMut()) -> Option<T> {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = Box::pin(fut);
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        tick();
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return Some(v),
            Poll::Pending if Instant::now() >= deadline => return None,
            Poll::Pending => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn wire(numbered: bool, id: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 1);
    if numbered {
        v.push(id);
    }
    v.extend_from_slice(payload);
    v
}

/// Assert `[report-number, body]` framing of a get_feature/get_input result: the
/// report ID when numbered, else `0x00` (len ≥ 9, `buf[0] == number`,
/// `buf[1..9] == payload`).
fn assert_get_framing(op: &str, lb: &str, numbered: bool, report_id: u8, buf: &[u8], n: usize) {
    let report_number = if numbered { report_id } else { 0 };
    assert!(
        n >= 9,
        "[{lb}] {op} length {n} < 9 (expected [report-number|body])"
    );
    assert_eq!(
        buf[0], report_number,
        "[{lb}] {op} leading report-number byte"
    );
    assert_eq!(&buf[1..9], &FEAT_PAYLOAD, "[{lb}] {op} payload");
}

/// Run the full suite against one freshly created virtual device. `numbered`
/// selects the descriptor variant; the platform must have created + started it.
pub fn run_conformance(numbered: bool, caps: &Caps, vdev: &dyn VirtualDevice) {
    let lb = label(numbered);
    if numbered {
        assert!(
            caps.numbered,
            "run_conformance(numbered) but Caps::numbered is false"
        );
    }
    let rd = make_descriptor(numbered);

    let api = hidra::Hidra::builder()
        .backend(caps.backend)
        .build()
        .unwrap();
    assert_eq!(api.backend(), caps.backend, "[{lb}] selected backend");
    let info = {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(d) = api
                .enumerate(TEST_VID, TEST_PID)
                .unwrap()
                .into_iter()
                .next()
            {
                break d;
            }
            if Instant::now() >= deadline {
                panic!("[{lb}] device did not enumerate");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    assert_eq!(info.vendor_id(), TEST_VID, "[{lb}] enumerate vid");
    assert_eq!(info.product_id(), TEST_PID, "[{lb}] enumerate pid");
    if caps.usage_at_enumerate {
        assert_eq!(info.usage_page(), 0xFF00, "[{lb}] enumerate usage_page");
        assert_eq!(info.usage(), 0x01, "[{lb}] enumerate usage");
    } else {
        eprintln!(
            "[{lb}] SKIP enumerate usage: this backend reads the report descriptor only on open"
        );
    }
    let path = info.path().to_string();
    assert!(!path.is_empty(), "[{lb}] enumerate path empty");

    // Exercise the alternate constructors as open-and-drop before the long-lived
    // handle below: a USB interface claims once at a time, so nusb can't hold two
    // handles to one device concurrently.
    hidra::Hidra::builder()
        .backend(caps.backend)
        .enumerate_on_build(false)
        .build()
        .unwrap();
    if caps.backend == Backend::default() {
        // The no-argument constructors are the same thing with the default
        // backend; only this run can check they agree.
        assert_eq!(
            hidra::Hidra::new().unwrap().backend(),
            caps.backend,
            "[{lb}] Hidra::new() backend"
        );
        hidra::Hidra::builder()
            .enumerate_on_build(false)
            .build()
            .unwrap();
    }
    let mut api2 = hidra::Hidra::builder()
        .backend(caps.backend)
        .build()
        .unwrap();
    api2.refresh_devices().unwrap();
    assert!(
        api2.device_list()
            .any(|d| d.vendor_id() == TEST_VID && d.product_id() == TEST_PID),
        "[{lb}] device_list did not list the device"
    );
    api.open(TEST_VID, TEST_PID).wait().expect("open(vid,pid)");
    if caps.strings {
        api.open_serial(TEST_VID, TEST_PID, SERIAL)
            .wait()
            .expect("open_serial");
    } else {
        eprintln!("[{lb}] SKIP open_serial: device advertises no serial string");
    }

    // Retry: the node / interface claim may lag enumeration.
    let device = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match api.open_path(&path).wait() {
                Ok(d) => break d,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("[{lb}] open_path failed: {e}"),
            }
        }
    };

    let di = device.get_device_info().wait().unwrap();
    assert_eq!(
        (di.vendor_id(), di.product_id()),
        (TEST_VID, TEST_PID),
        "[{lb}] get_device_info vid/pid"
    );
    assert_eq!(di.usage_page(), 0xFF00, "[{lb}] get_device_info usage_page");
    assert_eq!(di.usage(), 0x01, "[{lb}] get_device_info usage");
    eprintln!(
        "[{lb}] device_info release={:#06x} bus_type={:?} interface_number={}",
        di.release_number(),
        di.bus_type(),
        di.interface_number()
    );
    assert_eq!(
        di.bus_type(),
        caps.bus_type,
        "[{lb}] get_device_info bus_type"
    );
    assert_eq!(
        di.release_number(),
        caps.release_number,
        "[{lb}] get_device_info release_number"
    );
    assert_eq!(
        di.interface_number(),
        caps.interface_number,
        "[{lb}] get_device_info interface_number"
    );

    // Option methods are per-OS in hidra, so they run only where they compile —
    // there, against every device incl. the real Cynthion. They belong to the
    // native backend, and must say so rather than misbehave on the other one.
    #[cfg(target_os = "macos")]
    if caps.backend == Backend::Native {
        let before = api.open_exclusive().unwrap();
        api.set_open_exclusive(!before).unwrap();
        assert_eq!(
            api.open_exclusive().unwrap(),
            !before,
            "[{lb}] open_exclusive() did not reflect set_open_exclusive()"
        );
        api.set_open_exclusive(before).unwrap();
        assert_eq!(
            api.open_exclusive().unwrap(),
            before,
            "[{lb}] open_exclusive() restore"
        );
        eprintln!("[{lb}] checked set_open_exclusive/open_exclusive round-trip");
    } else {
        assert!(
            matches!(api.open_exclusive(), Err(HidError::Unsupported { .. })),
            "[{lb}] open_exclusive() must be Unsupported off the native backend"
        );
        eprintln!("[{lb}] checked open_exclusive() reports Unsupported");
    }
    #[cfg(target_os = "windows")]
    if caps.backend == Backend::Native {
        // container_id() is a 16-byte GUID; its value is device-dependent (may be
        // zeros for a root-enumerated device), so assert only the length.
        let cid = device.container_id().wait().expect("container_id");
        assert_eq!(cid.len(), 16, "[{lb}] container_id must be 16 bytes");
        eprintln!("[{lb}] container_id = {cid:02x?}");
        // Plumb set_write_timeout here; the timeout-firing check runs later.
        device.set_write_timeout(2000).unwrap();
        eprintln!("[{lb}] set_write_timeout(2000) applied");
    } else {
        assert!(
            matches!(
                device.container_id().wait(),
                Err(HidError::Unsupported { .. })
            ),
            "[{lb}] container_id() must be Unsupported off the native backend"
        );
        assert!(
            matches!(
                device.set_write_timeout(2000),
                Err(HidError::Unsupported { .. })
            ),
            "[{lb}] set_write_timeout() must be Unsupported off the native backend"
        );
        eprintln!("[{lb}] checked container_id/set_write_timeout report Unsupported");
    }

    if caps.strings {
        assert_eq!(
            device.get_product_string().wait().unwrap().as_deref(),
            Some(PRODUCT),
            "[{lb}] product string"
        );
        assert_eq!(
            device.get_serial_number_string().wait().unwrap().as_deref(),
            Some(SERIAL),
            "[{lb}] serial string"
        );
    } else {
        eprintln!(
            "[{lb}] SKIP product/serial strings: not exposed by this virtual-device mechanism"
        );
        device.get_product_string().wait().unwrap();
        device.get_serial_number_string().wait().unwrap();
    }
    if caps.manufacturer {
        assert_eq!(
            device.get_manufacturer_string().wait().unwrap().as_deref(),
            Some(MANUFACTURER),
            "[{lb}] manufacturer string"
        );
    } else {
        eprintln!("[{lb}] SKIP manufacturer string: not exposed by this virtual-device mechanism");
        device.get_manufacturer_string().wait().unwrap();
    }

    if caps.indexed_string_unsupported {
        assert!(
            matches!(
                device.get_indexed_string(1).wait(),
                Err(HidError::Unsupported { .. })
            ),
            "[{lb}] get_indexed_string should be Unsupported on this backend"
        );
    }

    let got_rd = device
        .report_descriptor()
        .wait()
        .expect("report_descriptor");
    if caps.exact_descriptor {
        assert_eq!(got_rd, rd, "[{lb}] descriptor byte-exact round-trip");
    } else {
        assert!(!got_rd.is_empty(), "[{lb}] reconstructed descriptor empty");
    }
    let desc = device.parsed_report_descriptor().wait().unwrap();
    assert_eq!(
        desc.uses_report_ids(),
        numbered,
        "[{lb}] parsed uses_report_ids"
    );
    for kind in [ReportKind::Input, ReportKind::Output, ReportKind::Feature] {
        assert!(
            desc.reports.iter().any(|r| r.kind == kind),
            "[{lb}] parsed descriptor missing {kind:?} report"
        );
    }

    // read() convention: an unnumbered report has no leading report-ID byte; a
    // numbered one begins with it.
    let injected = wire(numbered, RID_INPUT, &IN_PAYLOAD);
    let expected_read: &[u8] = &injected;
    let mut rbuf = [0u8; 64];
    let n = poll_deadline(device.read(&mut rbuf), 4, || vdev.inject_input(&injected))
        .unwrap_or_else(|| panic!("[{lb}] read() timed out"))
        .expect("read() error");
    assert_eq!(
        &rbuf[..n],
        expected_read,
        "[{lb}] read() must return the report with the documented report-ID prefix convention"
    );

    let mut small = [0u8; 4];
    let sn = poll_deadline(device.read(&mut small), 4, || vdev.inject_input(&injected))
        .unwrap_or_else(|| panic!("[{lb}] truncated read timed out"))
        .expect("read() error");
    assert_eq!(sn, 4, "[{lb}] truncated read length");
    assert_eq!(&small, &expected_read[..4], "[{lb}] truncated read bytes");

    // write() buffer is [report-ID | data], report-ID 0 when unnumbered.
    {
        let mut out = vec![if numbered { RID_OUTPUT } else { 0 }];
        out.extend_from_slice(&OUT_PAYLOAD);
        let w = device.write(&out).wait().expect("write()");
        assert_eq!(w, out.len(), "[{lb}] write() returns bytes written");
        let deadline = Instant::now() + Duration::from_secs(2);
        let got = loop {
            if let Some(o) = vdev.last_output() {
                if o.ends_with(&OUT_PAYLOAD) {
                    break Some(o);
                }
            }
            if Instant::now() >= deadline {
                break vdev.last_output();
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let got = got.unwrap_or_else(|| panic!("[{lb}] device received no output report"));
        assert!(
            got.ends_with(&OUT_PAYLOAD),
            "[{lb}] output payload must reach the device: {got:02x?}"
        );
    }

    if caps.feature {
        vdev.prime_get(&wire(numbered, RID_FEATURE, &FEAT_PAYLOAD));
        let mut fbuf = [0u8; 64];
        fbuf[0] = if numbered { RID_FEATURE } else { 0 };
        let fnn = device
            .get_feature_report(&mut fbuf)
            .wait()
            .expect("get_feature_report");
        assert_get_framing("get_feature_report", lb, numbered, RID_FEATURE, &fbuf, fnn);

        let mut send = vec![if numbered { RID_FEATURE } else { 0 }];
        send.extend_from_slice(&FEAT_PAYLOAD2);
        device
            .send_feature_report(&send)
            .wait()
            .expect("send_feature_report");
        let deadline = Instant::now() + Duration::from_secs(2);
        let got = loop {
            if let Some(s) = vdev.last_set_feature() {
                if s.ends_with(&FEAT_PAYLOAD2) {
                    break Some(s);
                }
            }
            if Instant::now() >= deadline {
                break vdev.last_set_feature();
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let got = got.unwrap_or_else(|| panic!("[{lb}] device received no set-feature report"));
        assert!(
            got.ends_with(&FEAT_PAYLOAD2),
            "[{lb}] send_feature_report payload must reach the device: {got:02x?}"
        );
    } else {
        eprintln!("[{lb}] SKIP feature reports: not supported by this virtual-device mechanism");
    }

    if caps.input_get {
        vdev.prime_get(&wire(numbered, RID_INPUT, &FEAT_PAYLOAD));
        let mut ibuf = [0u8; 64];
        ibuf[0] = if numbered { RID_INPUT } else { 0 };
        let inn = device
            .get_input_report(&mut ibuf)
            .wait()
            .expect("get_input_report");
        assert_get_framing("get_input_report", lb, numbered, RID_INPUT, &ibuf, inn);
    } else {
        eprintln!("[{lb}] SKIP get_input_report: not supported by this virtual-device mechanism");
    }

    // Concurrency: `HidDevice` is `Send + Sync`. Hammer the shared handle from
    // several threads (writes, plus get_feature where supported) and require every
    // call to succeed.
    {
        const THREADS: usize = 4;
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..THREADS)
                .map(|t| {
                    let device = &device;
                    s.spawn(move || -> Result<(), String> {
                        for _ in 0..5 {
                            let mut out = vec![if numbered { RID_OUTPUT } else { 0 }];
                            out.extend_from_slice(&OUT_PAYLOAD);
                            device
                                .write(&out)
                                .wait()
                                .map_err(|e| format!("thread {t} write: {e}"))?;
                            if caps.feature {
                                let mut fbuf = [0u8; 64];
                                fbuf[0] = if numbered { RID_FEATURE } else { 0 };
                                device
                                    .get_feature_report(&mut fbuf)
                                    .wait()
                                    .map_err(|e| format!("thread {t} get_feature: {e}"))?;
                            }
                        }
                        Ok(())
                    })
                })
                .collect();
            for w in workers {
                w.join()
                    .expect("concurrency worker panicked")
                    .expect("concurrent op failed");
            }
        });
        eprintln!("[{lb}] concurrent access from {THREADS} threads OK");
    }

    // Robustness: odd/oversized/malformed inputs must return cleanly (Ok or Err),
    // never crash or hang; the exact result is backend-defined.
    {
        let _ = device
            .write(&[if numbered { RID_OUTPUT } else { 0 }])
            .wait();

        // Oversized but modest: a huge control transfer can wedge some real USB
        // stacks; the point is that hidra tolerates a too-long buffer.
        let mut big = vec![if numbered { RID_OUTPUT } else { 0 }];
        big.extend_from_slice(&[0x5A; 24]);
        let _ = device.write(&big).wait();

        if caps.feature {
            let mut tiny = [if numbered { RID_FEATURE } else { 0 }];
            let _ = device.get_feature_report(&mut tiny).wait();
        }
        eprintln!("[{lb}] odd/oversized-input robustness OK (no panic)");
    }

    // set_write_timeout firing (Windows). Run late: a fired 1ms write can leave a
    // real device mid-transfer. A 1ms timeout makes a slow real-USB write time
    // out; a buffered virtual-device write still succeeds — accept either.
    #[cfg(target_os = "windows")]
    if caps.backend == Backend::Native {
        let mut out = vec![if numbered { RID_OUTPUT } else { 0 }];
        out.extend_from_slice(&OUT_PAYLOAD);
        device.set_write_timeout(1).unwrap();
        let short = device.write(&out).wait();
        device.set_write_timeout(5000).unwrap();
        device
            .write(&out)
            .wait()
            .expect("write with 5s timeout must succeed");
        device.set_write_timeout(1000).unwrap();
        eprintln!(
            "[{lb}] set_write_timeout: 1ms write -> {}, 5s write ok",
            if short.is_ok() {
                "ok"
            } else {
                "timed out (fired)"
            }
        );
    }

    if caps.disconnect {
        // Fire teardown once, then read until `Disconnected`. Some backends
        // (IOHIDManager, g_hid) first hand back reports buffered before removal;
        // drain those.
        let mut fired = false;
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let mut dbuf = [0u8; 64];
            let r = poll_deadline(device.read(&mut dbuf), 3, || {
                if !fired {
                    fired = true;
                    vdev.disconnect();
                }
            });
            match r {
                Some(Err(HidError::Disconnected)) => break,
                // Buffered report from before removal propagated.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    panic!("[{lb}] read() at disconnect gave an unexpected error: {e:?}")
                }
                None => panic!("[{lb}] read() timed out without observing Disconnected"),
            }
            if Instant::now() >= deadline {
                panic!("[{lb}] never observed Disconnected while draining at disconnect");
            }
        }
    } else {
        eprintln!("[{lb}] SKIP disconnect: pending-read wake-up not asserted on this backend");
    }

    // Error paths last, on a throwaway handle, so a failed open can't perturb the
    // live device: opening an absent VID/PID or a bogus path must fail cleanly.
    let probe = hidra::Hidra::builder()
        .backend(caps.backend)
        .build()
        .unwrap();
    assert!(
        probe.open(0xFFFF, 0xFFFF).wait().is_err(),
        "[{lb}] open() of an absent device should return an error"
    );
    assert!(
        probe.open_path("/nonexistent/hidra/path").wait().is_err(),
        "[{lb}] open_path() of a bogus path should return an error"
    );
    eprintln!("[{lb}] checked error paths (absent open / bogus open_path)");

    eprintln!("PASS: conformance suite ({lb})");
}

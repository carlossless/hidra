//! Conformance suite against a *real* USB HID device emulated by a Cynthion
//! running Facedancer (`device.py`) — the host under test sees genuine USB HID.
//!
//! Run `device.py` on the Cynthion's host, then point `HIDRA_CYNTHION_CTRL` at
//! its control server (default 127.0.0.1:9999; from a VM the host IP, e.g.
//! 10.0.2.2:9999). Self-skips if unreachable unless `HIDRA_CYNTHION_REQUIRED=1`.
//! Needs raw HID access (root).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Mutex;

use conformance::{run_conformance, Backend, Caps, VirtualDevice};

fn ctrl_addr() -> String {
    std::env::var("HIDRA_CYNTHION_CTRL").unwrap_or_else(|_| "127.0.0.1:9999".to_string())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap_or(0))
        .collect()
}

struct CynthionDevice {
    conn: Mutex<(TcpStream, BufReader<TcpStream>)>,
}

impl CynthionDevice {
    fn connect(addr: &str) -> std::io::Result<Self> {
        let w = TcpStream::connect(addr)?;
        // Bound the read so a wedged device.py (Facedancer LIBUSB_ERROR_TIMEOUT
        // under nusb detach/claim churn) fails cleanly instead of hanging.
        w.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
        let r = BufReader::new(w.try_clone()?);
        Ok(CynthionDevice {
            conn: Mutex::new((w, r)),
        })
    }

    fn cmd(&self, line: &str) -> String {
        let mut g = self.conn.lock().unwrap();
        let (w, r) = &mut *g;
        writeln!(w, "{line}").expect("control write");
        w.flush().ok();
        let mut resp = String::new();
        r.read_line(&mut resp).expect("control read");
        resp.trim().to_string()
    }

    fn opt(&self, resp: String) -> Option<Vec<u8>> {
        if resp == "none" || resp.is_empty() {
            None
        } else {
            Some(hex_decode(&resp))
        }
    }
}

impl VirtualDevice for CynthionDevice {
    fn inject_input(&self, wire: &[u8]) {
        self.cmd(&format!("inject {}", hex_encode(wire)));
    }
    fn prime_get(&self, wire: &[u8]) {
        // Device returns these bytes verbatim on GET_REPORT; for unnumbered, wire == body.
        self.cmd(&format!("prime {}", hex_encode(wire)));
    }
    fn last_output(&self) -> Option<Vec<u8>> {
        let r = self.cmd("output?");
        self.opt(r)
    }
    fn last_set_feature(&self) -> Option<Vec<u8>> {
        let r = self.cmd("setfeature?");
        self.opt(r)
    }
    fn disconnect(&self) {
        // Facedancer drops the device; hidra's pending read observes real USB removal.
        self.cmd("disconnect");
    }
}

/// `device.py` must be started with the same `HIDRA_CYNTHION_NUMBERED` setting:
/// a real descriptor is fixed at enumeration, so the two variants are separate runs.
fn numbered() -> bool {
    std::env::var("HIDRA_CYNTHION_NUMBERED").is_ok()
}

/// Which hidra backend to drive the same physical device through. Both are
/// compiled in, so one binary covers the matrix; `run.sh` sets this per run.
fn backend() -> Backend {
    match std::env::var("HIDRA_BACKEND").as_deref() {
        Ok("nusb") => Backend::Nusb,
        Ok("native") | Err(_) => Backend::Native,
        Ok(other) => panic!("HIDRA_BACKEND: no backend named {other:?}"),
    }
}

fn cynthion_caps(numbered: bool, backend: Backend) -> Caps {
    let nusb = backend == Backend::Nusb;
    Caps {
        numbered,
        strings: true,
        manufacturer: true,
        // Windows reconstructs the descriptor; Linux/macOS expose it verbatim.
        exact_descriptor: !cfg!(target_os = "windows"),
        feature: true,
        input_get: true,
        // Not on nusb: a Facedancer disconnect leaves the device half-removed and
        // nusb's claimed-interface read hangs instead of observing removal (nusb's
        // own disconnect path is covered by the g_hid gadget test).
        disconnect: !nusb,
        // Only hidraw lacks USB string-descriptor access; nusb reads them over the control pipe.
        indexed_string_unsupported: cfg!(target_os = "linux") && !nusb,
        // nusb enumeration is non-invasive (usage only known on open); OS HID
        // backends provide the parsed descriptor at enumerate.
        usage_at_enumerate: !nusb,
        bus_type: conformance::BusType::Usb,
        release_number: 0x0100,
        interface_number: 0,
        backend,
    }
}

fn should_skip(reason: &str) -> bool {
    if std::env::var("HIDRA_CYNTHION_REQUIRED").is_ok() {
        panic!("HIDRA_CYNTHION_REQUIRED set but {reason}");
    }
    eprintln!("SKIP: {reason}");
    true
}

#[test]
fn cynthion_conformance() {
    let addr = ctrl_addr();
    let dev = match CynthionDevice::connect(&addr) {
        Ok(d) => d,
        Err(e) => {
            if should_skip(&format!(
                "Facedancer control server unreachable at {addr} ({e}); run device.py"
            )) {
                return;
            }
            unreachable!()
        }
    };
    let numbered = numbered();
    let backend = backend();
    let caps = cynthion_caps(numbered, backend);

    dev.cmd("reset");
    // The backend is a type, so the run-time choice picks the instantiation.
    match caps.backend {
        Backend::Native => run_conformance::<hidra::Native>(numbered, &caps, &dev),
        Backend::Nusb => run_conformance::<hidra::Nusb>(numbered, &caps, &dev),
    }
    eprintln!("PASS: cynthion (real USB HID) conformance test on the {backend} backend");
}

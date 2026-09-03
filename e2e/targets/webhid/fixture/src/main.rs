//! Standalone `uhid` virtual HID device, the fixture Chrome sees via WebHID for
//! the Playwright test. Streams a known input report, answers GET_REPORT with a
//! canned payload, and prints a line per output report so the browser side can
//! prove `write()` reached the device.
//!
//! Linux only, needs root (uhid). Run: `sudo cargo run --example webhid_uhid_fixture`;
//! runs until killed.

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("webhid_uhid_fixture is Linux-only");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    use hidra::descriptor::{CollectionKind, DescriptorBuilder, MainFlags};

    const UHID_START: u32 = 2;
    const UHID_OUTPUT: u32 = 6;
    const UHID_GET_REPORT: u32 = 9;
    const UHID_GET_REPORT_REPLY: u32 = 10;
    const UHID_CREATE2: u32 = 11;
    const UHID_INPUT2: u32 = 12;
    const UHID_SET_REPORT: u32 = 13;
    const UHID_SET_REPORT_REPLY: u32 = 14;

    const EVENT_SIZE: usize = 4380;
    const O_NONBLOCK: i32 = 0o4000;

    const TEST_VID: u16 = 0x1209;
    const TEST_PID: u16 = 0x000c;
    const PRODUCT: &str = "hidra-conformance";
    const SERIAL: &str = "HIDRA-CONF-01";
    const IN_PAYLOAD: [u8; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
    const FEAT_PAYLOAD: [u8; 8] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];

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

    fn descriptor() -> Vec<u8> {
        let mut b = DescriptorBuilder::new();
        b.usage_page(0xFF00)
            .usage(0x01)
            .collection(CollectionKind::Application)
            .logical_minimum(0)
            .logical_maximum(255)
            .report_size(8)
            .report_count(8)
            .usage(0x02)
            .input(MainFlags::VARIABLE)
            .usage(0x03)
            .output(MainFlags::VARIABLE)
            .usage(0x04)
            .feature(MainFlags::VARIABLE)
            .end_collection();
        b.build()
    }

    fn create2_event(rd: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; EVENT_SIZE];
        le32(&mut e, 0, UHID_CREATE2);
        let nb = PRODUCT.as_bytes();
        e[4..4 + nb.len()].copy_from_slice(nb);
        let ub = SERIAL.as_bytes();
        e[196..196 + ub.len()].copy_from_slice(ub);
        le16(&mut e, 260, rd.len() as u16);
        le16(&mut e, 262, 0x03); // bus USB
        le32(&mut e, 264, TEST_VID as u32);
        le32(&mut e, 268, TEST_PID as u32);
        e[280..280 + rd.len()].copy_from_slice(rd);
        e
    }

    fn input2_event(data: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; EVENT_SIZE];
        le32(&mut e, 0, UHID_INPUT2);
        le16(&mut e, 4, data.len() as u16);
        e[6..6 + data.len()].copy_from_slice(data);
        e
    }

    fn simple_event(ty: u32) -> Vec<u8> {
        let mut e = vec![0u8; EVENT_SIZE];
        le32(&mut e, 0, ty);
        e
    }

    pub fn run() {
        let mut uhid = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open("/dev/uhid")
            .expect("open /dev/uhid (need root)");

        let rd = descriptor();
        uhid.write_all(&create2_event(&rd)).expect("CREATE2");

        let mut buf = vec![0u8; EVENT_SIZE];
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match uhid.read(&mut buf) {
                Ok(n) if event_type(&buf, n) == Some(UHID_START) => break,
                _ => {}
            }
            if Instant::now() >= deadline {
                panic!("device never reported START");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("READY {PRODUCT} {TEST_VID:#06x}:{TEST_PID:#06x}");
        let _ = std::io::stdout().flush();

        let mut last_input = Instant::now() - Duration::from_secs(1);
        loop {
            if last_input.elapsed() >= Duration::from_millis(100) {
                uhid.write_all(&input2_event(&IN_PAYLOAD)).ok();
                last_input = Instant::now();
            }
            match uhid.read(&mut buf) {
                Ok(n) => match event_type(&buf, n) {
                    Some(UHID_GET_REPORT) => {
                        let id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        let rnum = buf[8];
                        // Chrome's HID path reads the report as [report-ID | body],
                        // so prepend the report-number byte the descriptor implies.
                        let mut data = vec![rnum];
                        data.extend_from_slice(&FEAT_PAYLOAD);
                        let mut reply = simple_event(UHID_GET_REPORT_REPLY);
                        le32(&mut reply, 4, id);
                        le16(&mut reply, 8, 0);
                        le16(&mut reply, 10, data.len() as u16);
                        reply[12..12 + data.len()].copy_from_slice(&data);
                        uhid.write_all(&reply).ok();
                    }
                    Some(UHID_SET_REPORT) => {
                        let id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        let size = rd16(&buf, 10) as usize;
                        println!("SET_REPORT {:02x?}", &buf[12..12 + size]);
                        let _ = std::io::stdout().flush();
                        let mut reply = simple_event(UHID_SET_REPORT_REPLY);
                        le32(&mut reply, 4, id);
                        le16(&mut reply, 8, 0);
                        uhid.write_all(&reply).ok();
                    }
                    Some(UHID_OUTPUT) => {
                        let size = rd16(&buf, 4100) as usize;
                        println!("OUTPUT {:02x?}", &buf[4..4 + size]);
                        let _ = std::io::stdout().flush();
                    }
                    _ => {}
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("uhid read: {e}"),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

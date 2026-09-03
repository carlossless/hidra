//! WebUSB conformance harness. Drives hidra's WebUSB backend against the
//! `webusb_ffs_fixture` vendor-class gadget; returns a summary string or throws
//! on mismatch.

use hidra::webusb::Hidra;
use wasm_bindgen::prelude::*;

const TEST_VID: u16 = 0x1209;
const TEST_PID: u16 = 0x000c;
const RID_INPUT: u8 = 0x11;
const RID_FEATURE: u8 = 0x33;
const IN_PAYLOAD: [u8; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
const OUT_PAYLOAD: [u8; 8] = [0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7];
const FEAT_PAYLOAD: [u8; 8] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];

fn jerr<E: std::fmt::Display>(e: E) -> JsValue {
    JsValue::from_str(&format!("{e}"))
}

/// `variant` mirrors the fixture: "full", "control-only" or
/// "no-report-descriptor".
#[wasm_bindgen]
pub async fn run_webusb_test(variant: String) -> Result<String, JsValue> {
    let endpoints = variant != "control-only";
    let has_descriptor = variant != "no-report-descriptor";
    let api = Hidra::new().map_err(jerr)?;
    let mut log = Vec::<String>::new();

    // `WebUsbAllowDevicesForUrls` pre-grants the fixture, so it is already in
    // device_list() — no chooser, and so no user gesture needed.
    let device = api
        .device_list()
        .await
        .map_err(jerr)?
        .into_iter()
        .find(|d| (d.vendor_id(), d.product_id()) == (TEST_VID, TEST_PID))
        .ok_or_else(|| JsValue::from_str("fixture device not granted (policy applied?)"))?;
    if (device.vendor_id(), device.product_id()) != (TEST_VID, TEST_PID) {
        return Err(JsValue::from_str(&format!(
            "vid/pid mismatch: {:#06x}:{:#06x}",
            device.vendor_id(),
            device.product_id()
        )));
    }
    log.push(format!(
        "granted device product={:?}",
        device.product_string()
    ));

    // The gadget exposes one interface; None takes it. This is the claim
    // Blink would refuse outright were the interface HID-class.
    let dev = device.open(None).await.map_err(jerr)?;
    log.push("claimed vendor-class interface".into());

    let info = dev.get_device_info().map_err(jerr)?;
    if (info.vendor_id(), info.product_id()) != (TEST_VID, TEST_PID) {
        return Err(JsValue::from_str("device_info vid/pid mismatch"));
    }
    log.push(format!("interface_number={}", info.interface_number()));

    // GET_DESCRIPTOR(Report) over the vendor-class interface. A vendor-class
    // interface owes no HID class descriptor, and hidra has to say so rather
    // than hand back an empty buffer.
    match (has_descriptor, dev.report_descriptor()) {
        (true, Ok(d)) if !d.is_empty() => log.push(format!("report_descriptor={} bytes", d.len())),
        (true, Ok(_)) => return Err(JsValue::from_str("empty report descriptor")),
        (true, Err(e)) => return Err(jerr(e)),
        (false, Err(e)) => log.push(format!("report_descriptor unsupported: {e}")),
        (false, Ok(d)) => {
            return Err(JsValue::from_str(&format!(
                "expected no report descriptor, got {} bytes",
                d.len()
            )))
        }
    }

    // GET_REPORT(Feature): buf[0] carries the report ID on entry.
    let mut buf = vec![0u8; 1 + FEAT_PAYLOAD.len()];
    buf[0] = RID_FEATURE;
    let n = dev.get_feature_report(&mut buf).await.map_err(jerr)?;
    if buf[..n] != [&[RID_FEATURE][..], &FEAT_PAYLOAD[..]].concat()[..n] {
        return Err(JsValue::from_str(&format!(
            "feature report mismatch: {:02x?}",
            &buf[..n]
        )));
    }
    log.push(format!("get_feature_report={n} bytes"));

    // SET_REPORT(Feature).
    let mut out = vec![RID_FEATURE];
    out.extend_from_slice(&FEAT_PAYLOAD);
    dev.send_feature_report(&out).await.map_err(jerr)?;
    log.push("send_feature_report ok".into());

    // An output report goes out on the interrupt OUT endpoint when there is
    // one, and falls back to SET_REPORT(Output) on the control pipe when there
    // is not — the shape the Sinowealth ISP bootloaders take.
    let mut written = vec![RID_INPUT];
    written.extend_from_slice(&OUT_PAYLOAD);
    let n = dev.write(&written).await.map_err(jerr)?;
    log.push(format!("write={n} bytes"));

    if endpoints {
        // Interrupt IN: the fixture streams a known report.
        let mut inbuf = vec![0u8; 64];
        let n = dev.read(&mut inbuf).await.map_err(jerr)?;
        if n == 0 {
            return Err(JsValue::from_str("read returned 0 bytes"));
        }
        let expected = [&[RID_INPUT][..], &IN_PAYLOAD[..]].concat();
        if inbuf[..n] != expected[..] {
            return Err(JsValue::from_str(&format!(
                "input report mismatch: {:02x?}",
                &inbuf[..n]
            )));
        }
        log.push(format!("read={n} bytes"));
    } else {
        // No interrupt IN endpoint: read must refuse rather than hang, and
        // GET_REPORT(Input) stays available as the way to poll.
        let mut inbuf = vec![0u8; 64];
        match dev.read(&mut inbuf).await {
            Ok(n) => {
                return Err(JsValue::from_str(&format!(
                    "read succeeded ({n} bytes) on an interface with no interrupt IN"
                )))
            }
            Err(e) => log.push(format!("read refused: {e}")),
        }

        let mut poll = vec![0u8; 1 + FEAT_PAYLOAD.len()];
        poll[0] = RID_INPUT;
        let n = dev.get_input_report(&mut poll).await.map_err(jerr)?;
        log.push(format!("get_input_report={n} bytes"));
    }

    Ok(format!(
        "PASS: webusb conformance [{variant}] ({})",
        log.join("; ")
    ))
}

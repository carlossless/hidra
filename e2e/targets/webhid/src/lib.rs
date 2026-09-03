//! WebHID conformance harness. Drives hidra's WebHID backend against the
//! `webhid_uhid_fixture` device; returns a summary string or throws on mismatch.

use hidra::Hidra;
use wasm_bindgen::prelude::*;

const IN_PAYLOAD: [u8; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
const FEAT_PAYLOAD: [u8; 8] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];

fn jerr<E: std::fmt::Display>(e: E) -> JsValue {
    JsValue::from_str(&format!("{e}"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[wasm_bindgen]
pub async fn run_webhid_test() -> Result<String, JsValue> {
    let api = Hidra::new().map_err(jerr)?;

    // The `WebHidAllowDevicesForUrls` policy pre-grants our fixture, so it shows
    // up in get_devices() directly, no chooser/user-gesture needed.
    let devices = api.get_devices().await.map_err(jerr)?;
    let dev = devices.into_iter().next().ok_or_else(|| {
        JsValue::from_str("fixture device not in get_devices() (policy applied?)")
    })?;

    let mut log = Vec::<String>::new();

    let product = dev.get_product_string().await.map_err(jerr)?;
    if product.as_deref() != Some("hidra-conformance") {
        return Err(JsValue::from_str(&format!("product mismatch: {product:?}")));
    }
    log.push(format!("product={product:?}"));

    dev.open().await.map_err(jerr)?;

    let di = dev.get_device_info().await.map_err(jerr)?;
    if (di.vendor_id(), di.product_id()) != (0x1209, 0x000c) {
        return Err(JsValue::from_str(&format!(
            "vid/pid mismatch: {:#06x}:{:#06x}",
            di.vendor_id(),
            di.product_id()
        )));
    }
    log.push(format!(
        "vid_pid={:#06x}:{:#06x}",
        di.vendor_id(),
        di.product_id()
    ));

    let cols = dev.collections();
    if cols.is_empty() {
        return Err(JsValue::from_str("no collections exposed"));
    }
    log.push(format!("collections={}", cols.len()));

    // fixture streams IN_PAYLOAD on the interrupt IN pipe.
    let mut buf = [0u8; 64];
    let n = dev.read(&mut buf).await.map_err(jerr)?;
    if !contains(&buf[..n], &IN_PAYLOAD) {
        return Err(JsValue::from_str(&format!(
            "read mismatch: {:02x?}",
            &buf[..n]
        )));
    }
    log.push(format!("read={:02x?}", &buf[..n]));

    // fixture answers GET_REPORT with FEAT_PAYLOAD.
    let mut fbuf = [0u8; 64];
    fbuf[0] = 0;
    let fnn = dev.get_feature_report(&mut fbuf).await.map_err(jerr)?;
    if !contains(&fbuf[..fnn], &FEAT_PAYLOAD) {
        return Err(JsValue::from_str(&format!(
            "feature mismatch: {:02x?}",
            &fbuf[..fnn]
        )));
    }
    log.push(format!("feature={:02x?}", &fbuf[..fnn]));

    // delivery only asserted here; the fixture prints the report on CI stdout.
    let out = [0x00u8, 0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7];
    dev.write(&out).await.map_err(jerr)?;
    log.push("write=ok".into());

    let sf = [0x00u8, 0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7];
    dev.send_feature_report(&sf).await.map_err(jerr)?;
    log.push("send_feature=ok".into());

    // WebHID exposes no raw descriptor; hidra reconstructs one from the browser's
    // parsed collections, must be non-empty.
    let rd = dev.report_descriptor().await.map_err(jerr)?;
    if rd.is_empty() {
        return Err(JsValue::from_str("report_descriptor empty"));
    }
    log.push(format!("report_descriptor_len={}", rd.len()));

    if !dev.opened() {
        return Err(JsValue::from_str("opened() false while open"));
    }

    // Exercise on_input_report and start_reading on one live event: a fresh
    // stream's first read waits for a live `inputreport`, which the listener also gets.
    let captured = std::rc::Rc::new(std::cell::RefCell::new(None::<Vec<u8>>));
    let sink = captured.clone();
    let handle = dev.on_input_report(move |_id, payload| {
        *sink.borrow_mut() = Some(payload);
    });
    let mut stream = dev.start_reading();
    let sr = stream.read().await.map_err(jerr)?;
    drop(stream);
    drop(handle);
    if !contains(&sr, &IN_PAYLOAD) {
        return Err(JsValue::from_str(&format!(
            "start_reading mismatch: {sr:02x?}"
        )));
    }
    log.push(format!("start_reading={:02x?}", sr));
    match captured.borrow().as_ref() {
        Some(p) if contains(p, &IN_PAYLOAD) => log.push("on_input_report=ok".into()),
        other => {
            return Err(JsValue::from_str(&format!(
                "on_input_report mismatch: {other:02x?}"
            )))
        }
    }

    dev.close().await.map_err(jerr)?;
    if dev.opened() {
        return Err(JsValue::from_str("opened() true after close"));
    }
    log.push("close=ok".into());

    // `forget()` is not exercised: its promise never resolves under this Chromium
    // build, hanging the run.

    Ok(format!("PASS: {}", log.join(" | ")))
}

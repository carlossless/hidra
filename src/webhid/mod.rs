//! WebAssembly backend: [WebHID](https://wicg.github.io/webhid/) via `web-sys`.
//!
//! Unlike the native backends this API is inherently async (every WebHID
//! operation returns a Promise) and event-driven (input reports arrive as
//! `inputreport` events instead of being read from a handle), so it does not
//! share the blocking `HidApi`/`HidDevice` surface. Method names and buffer
//! conventions still mirror hidapi where they make sense:
//!
//! * [`WebHidDevice::write`] / [`WebHidDevice::send_feature_report`] take
//!   hidapi-style buffers whose first byte is the report ID (0 when the
//!   device has no numbered reports).
//! * [`WebHidDevice::get_feature_report`] and [`InputReportStream::read`]
//!   return hidapi-style buffers (report-ID prefixed when numbered).
//!
//! WebHID is only available in secure contexts (HTTPS or localhost) on
//! Chromium-based browsers, and device access is permission-gated: the user
//! must pick devices from the chooser shown by
//! [`WebHidApi::request_device`], which in turn may only be called from a
//! user gesture (e.g. a click handler).
//!
//! ```no_run
//! # #[cfg(target_arch = "wasm32")] async fn demo() -> hidra::HidResult<()> {
//! use hidra::webhid::{DeviceFilter, WebHidApi};
//!
//! let api = WebHidApi::new()?;
//! // Must run inside a user gesture:
//! let devices = api.request_device(&[DeviceFilter::new().vendor_id(0x046d)]).await?;
//! let device = &devices[0];
//! device.open().await?;
//! device.write(&[0x00, 0x01, 0x02]).await?; // report ID 0 + payload
//! let mut reports = device.start_reading();
//! let report = reports.read().await?;
//! # Ok(()) }
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::error::{HidError, HidResult};
use crate::{BusType, DeviceInfo};

pub use crate::report_info::{
    reconstruct_descriptor, uses_report_ids, CollectionInfo, ReportInfo, ReportItemInfo, UnitSystem,
};

/// Maximum number of input reports buffered by [`InputReportStream`] before
/// the oldest is dropped.
const INPUT_QUEUE_CAPACITY: usize = 64;

/// Map a thrown `JsValue` to [`HidError::Backend`], extracting the
/// `DOMException` name and message when present.
fn js_err(context: &str, e: JsValue) -> HidError {
    let message = match e.dyn_ref::<web_sys::DomException>() {
        Some(ex) => format!("{context}: {}: {}", ex.name(), ex.message()),
        None => match e.as_string() {
            Some(s) => format!("{context}: {s}"),
            None => format!("{context}: {e:?}"),
        },
    };
    HidError::backend(message)
}

/// Copy the bytes a `DataView` spans into a `Vec`.
fn dataview_to_vec(view: &js_sys::DataView) -> Vec<u8> {
    let buffer = view.buffer();
    let buffer: &JsValue = buffer.as_ref();
    js_sys::Uint8Array::new_with_byte_offset_and_length(
        buffer,
        view.byte_offset() as u32,
        view.byte_length() as u32,
    )
    .to_vec()
}

/// Keeps an event listener (and the Rust closure backing it) alive;
/// removes the listener when dropped.
///
/// Returned by [`WebHidApi::on_connect`], [`WebHidApi::on_disconnect`] and
/// [`WebHidDevice::on_input_report`]. Dropping the handle unregisters the
/// callback; call [`forget`](Self::forget) to leak it and keep the listener
/// installed for the lifetime of the page.
pub struct EventListenerHandle {
    target: web_sys::EventTarget,
    event: &'static str,
    /// `None` only after [`forget`](Self::forget).
    closure: Option<Closure<dyn FnMut(web_sys::Event)>>,
}

impl EventListenerHandle {
    fn add(
        target: &web_sys::EventTarget,
        event: &'static str,
        closure: Closure<dyn FnMut(web_sys::Event)>,
    ) -> Self {
        // addEventListener only throws for malformed listener arguments,
        // which cannot happen with a live Closure-backed Function.
        let _ = target.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref());
        EventListenerHandle {
            target: target.clone(),
            event,
            closure: Some(closure),
        }
    }

    /// Leak the handle, keeping the listener registered for the lifetime of
    /// the page.
    pub fn forget(mut self) {
        if let Some(closure) = self.closure.take() {
            closure.forget();
        }
    }
}

impl Drop for EventListenerHandle {
    fn drop(&mut self) {
        if let Some(closure) = &self.closure {
            let _ = self
                .target
                .remove_event_listener_with_callback(self.event, closure.as_ref().unchecked_ref());
        }
    }
}

/// A device filter for [`WebHidApi::request_device`], mirroring WebHID's
/// `HIDDeviceFilter` dictionary. Unset fields match anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceFilter {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
}

impl DeviceFilter {
    /// A filter matching every device.
    pub fn new() -> Self {
        Self::default()
    }

    /// Match only the given vendor ID.
    pub fn vendor_id(mut self, vendor_id: u16) -> Self {
        self.vendor_id = Some(vendor_id);
        self
    }

    /// Match only the given product ID.
    pub fn product_id(mut self, product_id: u16) -> Self {
        self.product_id = Some(product_id);
        self
    }

    /// Match only top-level collections with the given usage page.
    pub fn usage_page(mut self, usage_page: u16) -> Self {
        self.usage_page = Some(usage_page);
        self
    }

    /// Match only top-level collections with the given usage ID.
    pub fn usage(mut self, usage: u16) -> Self {
        self.usage = Some(usage);
        self
    }

    fn to_js(self) -> web_sys::HidDeviceFilter {
        let filter = web_sys::HidDeviceFilter::new();
        if let Some(vendor_id) = self.vendor_id {
            filter.set_vendor_id(vendor_id.into());
        }
        if let Some(product_id) = self.product_id {
            filter.set_product_id(product_id);
        }
        if let Some(usage_page) = self.usage_page {
            filter.set_usage_page(usage_page);
        }
        if let Some(usage) = self.usage {
            filter.set_usage(usage);
        }
        filter
    }
}

/// Entry point to WebHID (`hid_init` / `hid_enumerate` equivalent, bound to
/// `navigator.hid`).
///
/// Enumeration differs from native hidapi: the browser only ever exposes
/// devices the user has granted access to.
/// [`request_device`](Self::request_device) shows the permission chooser,
/// [`get_devices`](Self::get_devices) lists previously granted devices.
pub struct WebHidApi {
    hid: web_sys::Hid,
}

impl WebHidApi {
    /// Bind to `window.navigator.hid`.
    ///
    /// Fails with [`HidError::Initialization`] when WebHID is unavailable:
    /// no `window` (e.g. a worker without WebHID), a non-secure context, or
    /// a browser without WebHID support.
    pub fn new() -> HidResult<Self> {
        let window = web_sys::window().ok_or_else(|| HidError::Initialization {
            message: "no global window object (WebHID requires a window context)".into(),
        })?;
        let hid = window.navigator().hid();
        let hid_js: &JsValue = hid.as_ref();
        if hid_js.is_undefined() || hid_js.is_null() {
            return Err(HidError::Initialization {
                message: "navigator.hid is unavailable (WebHID requires a secure context \
                          and a browser with WebHID support)"
                    .into(),
            });
        }
        Ok(WebHidApi { hid })
    }

    /// Ask the user to grant access to devices matching `filters`
    /// (`navigator.hid.requestDevice`). An empty filter list matches every
    /// device.
    ///
    /// Shows the browser's device chooser and resolves with every device the
    /// user granted (an empty `Vec` when the chooser was dismissed). **Must
    /// be called from within a user gesture** (e.g. a click event handler),
    /// otherwise the browser rejects the request.
    pub async fn request_device(&self, filters: &[DeviceFilter]) -> HidResult<Vec<WebHidDevice>> {
        let js_filters: Vec<web_sys::HidDeviceFilter> = filters.iter().map(|f| f.to_js()).collect();
        let options = web_sys::HidDeviceRequestOptions::new(&js_filters);
        let devices = JsFuture::from(self.hid.request_device(&options))
            .await
            .map_err(|e| js_err("requestDevice", e))?;
        Ok(devices.iter().map(WebHidDevice::from_raw).collect())
    }

    /// Devices the user has already granted this origin access to
    /// (`navigator.hid.getDevices`). Needs no user gesture.
    pub async fn get_devices(&self) -> HidResult<Vec<WebHidDevice>> {
        let devices = JsFuture::from(self.hid.get_devices())
            .await
            .map_err(|e| js_err("getDevices", e))?;
        Ok(devices.iter().map(WebHidDevice::from_raw).collect())
    }

    /// Invoke `f` whenever a granted device is plugged in (the `connect`
    /// event). Drop the returned handle to unregister.
    pub fn on_connect(&self, f: impl FnMut(WebHidDevice) + 'static) -> EventListenerHandle {
        self.connection_listener("connect", f)
    }

    /// Invoke `f` whenever a granted device is unplugged (the `disconnect`
    /// event). Drop the returned handle to unregister.
    pub fn on_disconnect(&self, f: impl FnMut(WebHidDevice) + 'static) -> EventListenerHandle {
        self.connection_listener("disconnect", f)
    }

    fn connection_listener(
        &self,
        event: &'static str,
        mut f: impl FnMut(WebHidDevice) + 'static,
    ) -> EventListenerHandle {
        let closure = Closure::wrap(Box::new(move |ev: web_sys::Event| {
            let ev: web_sys::HidConnectionEvent = ev.unchecked_into();
            f(WebHidDevice::from_raw(ev.device()));
        }) as Box<dyn FnMut(web_sys::Event)>);
        EventListenerHandle::add(self.hid.as_ref(), event, closure)
    }

    /// The underlying `navigator.hid` object.
    pub fn raw(&self) -> &web_sys::Hid {
        &self.hid
    }
}

/// A HID device exposed by the browser (`hid_device` equivalent, wrapping a
/// `HIDDevice`).
///
/// Obtained from [`WebHidApi::request_device`] / [`WebHidApi::get_devices`];
/// unlike native hidapi the handle exists before the device is opened, call
/// [`open`](Self::open) before transferring reports.
#[derive(Debug, Clone)]
pub struct WebHidDevice {
    device: web_sys::HidDevice,
}

impl WebHidDevice {
    /// Wrap a `web_sys::HidDevice` obtained elsewhere (e.g. from JS glue).
    pub fn from_raw(device: web_sys::HidDevice) -> Self {
        WebHidDevice { device }
    }

    /// The underlying `HIDDevice` object.
    pub fn raw(&self) -> &web_sys::HidDevice {
        &self.device
    }

    /// Open the device for I/O (`hid_open` equivalent; `HIDDevice.open`).
    pub async fn open(&self) -> HidResult<()> {
        JsFuture::from(self.device.open())
            .await
            .map_err(|e| match js_err("open", e) {
                HidError::Backend { message } => HidError::OpenFailed { message },
                other => other,
            })?;
        Ok(())
    }

    /// Close the device (`hid_close` equivalent; `HIDDevice.close`). The
    /// permission grant is kept, reopen with [`open`](Self::open).
    pub async fn close(&self) -> HidResult<()> {
        JsFuture::from(self.device.close())
            .await
            .map_err(|e| js_err("close", e))?;
        Ok(())
    }

    /// Revoke the user's permission grant for this device
    /// (`HIDDevice.forget`; no hidapi equivalent). The device disappears
    /// from [`WebHidApi::get_devices`] until requested again.
    ///
    /// `forget()` is newer than the rest of WebHID (Chromium 100+) and is
    /// called dynamically; browsers without it yield
    /// [`HidError::Unsupported`].
    pub async fn forget(&self) -> HidResult<()> {
        let device_js: &JsValue = self.device.as_ref();
        let method = js_sys::Reflect::get(device_js, &JsValue::from_str("forget"))
            .map_err(|e| js_err("forget", e))?;
        let method: js_sys::Function = method.dyn_into().map_err(|_| HidError::Unsupported {
            message: "HIDDevice.forget() is not supported by this browser".into(),
        })?;
        let promise: js_sys::Promise = method
            .call0(device_js)
            .map_err(|e| js_err("forget", e))?
            .dyn_into()
            .map_err(|_| HidError::backend("forget: did not return a Promise"))?;
        JsFuture::from(promise)
            .await
            .map_err(|e| js_err("forget", e))?;
        Ok(())
    }

    /// Whether the device is currently open (`HIDDevice.opened`).
    pub fn opened(&self) -> bool {
        self.device.opened()
    }

    /// USB-style vendor ID.
    pub fn vendor_id(&self) -> u16 {
        self.device.vendor_id()
    }

    /// USB-style product ID.
    pub fn product_id(&self) -> u16 {
        self.device.product_id()
    }

    /// Product string (`hid_get_product_string` equivalent;
    /// `HIDDevice.productName`). `None` when the browser reports an empty
    /// name.
    pub fn product_name(&self) -> Option<String> {
        let name = self.device.product_name();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    /// Metadata for this device (`hid_get_device_info` equivalent), filled
    /// with everything WebHID exposes:
    ///
    /// * vendor/product ID and product string;
    /// * `usage_page`/`usage` from the first top-level collection;
    /// * a synthetic `path` of the form `webhid:<vid>:<pid>:<product>`
    ///   (WebHID has no stable platform path; the path cannot be used to
    ///   reopen a device);
    /// * `bus_type` is always [`BusType::Unknown`] and `interface_number` is
    ///   `-1` ("not applicable"); serial number, manufacturer string and
    ///   release number are not exposed by WebHID and stay at their
    ///   defaults.
    pub fn device_info(&self) -> DeviceInfo {
        let vendor_id = self.vendor_id();
        let product_id = self.product_id();
        let product = self.product_name();
        let (usage_page, usage) = self
            .collections()
            .first()
            .map(|c| (c.usage_page, c.usage))
            .unwrap_or((0, 0));
        DeviceInfo {
            path: format!(
                "webhid:{:04x}:{:04x}:{}",
                vendor_id,
                product_id,
                product.as_deref().unwrap_or("")
            ),
            vendor_id,
            product_id,
            product_string: product,
            usage_page,
            usage,
            interface_number: -1,
            bus_type: BusType::Unknown,
            ..Default::default()
        }
    }

    /// Send an output report (`hid_write` equivalent; `HIDDevice.sendReport`).
    ///
    /// hidapi buffer convention: `data[0]` is the report ID (0 when the
    /// device has no numbered reports) and is not part of the payload.
    /// Returns `data.len()` on success, like `hid_write`.
    pub async fn write(&self, data: &[u8]) -> HidResult<usize> {
        let (report_id, payload) = data.split_first().ok_or_else(|| HidError::InvalidData {
            message: "write requires at least the report ID byte".into(),
        })?;
        let mut payload = payload.to_vec();
        let promise = self
            .device
            .send_report_with_u8_slice(*report_id, &mut payload)
            .map_err(|e| js_err("sendReport", e))?;
        JsFuture::from(promise)
            .await
            .map_err(|e| js_err("sendReport", e))?;
        Ok(data.len())
    }

    /// Send a feature report (`hid_send_feature_report` equivalent;
    /// `HIDDevice.sendFeatureReport`). `data[0]` is the report ID, 0 if
    /// unnumbered.
    pub async fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        let (report_id, payload) = data.split_first().ok_or_else(|| HidError::InvalidData {
            message: "send_feature_report requires at least the report ID byte".into(),
        })?;
        let mut payload = payload.to_vec();
        let promise = self
            .device
            .send_feature_report_with_u8_slice(*report_id, &mut payload)
            .map_err(|e| js_err("sendFeatureReport", e))?;
        JsFuture::from(promise)
            .await
            .map_err(|e| js_err("sendFeatureReport", e))?;
        Ok(())
    }

    /// Get a feature report (`hid_get_feature_report` equivalent;
    /// `HIDDevice.receiveFeatureReport`).
    ///
    /// Takes the report ID directly instead of hidapi's `buf[0]`-in/out
    /// convention; the returned buffer is prefixed with `report_id` (added
    /// by hidra, not parsed from the browser data) so it matches the buffer
    /// layout `hid_get_feature_report` produces.
    pub async fn get_feature_report(&self, report_id: u8) -> HidResult<Vec<u8>> {
        let view = JsFuture::from(self.device.receive_feature_report(report_id))
            .await
            .map_err(|e| js_err("receiveFeatureReport", e))?;
        let data = dataview_to_vec(&view);
        let mut report = Vec::with_capacity(data.len() + 1);
        report.push(report_id);
        report.extend_from_slice(&data);
        Ok(report)
    }

    /// Invoke `f` with `(report_id, payload)` for every incoming input
    /// report (the `inputreport` event; the event-driven counterpart of
    /// `hid_read`). The payload excludes the report ID; `report_id` is 0 for
    /// unnumbered reports. Drop the returned handle to unregister.
    ///
    /// The device must be [`open`](Self::open)ed to receive reports.
    pub fn on_input_report(&self, mut f: impl FnMut(u8, Vec<u8>) + 'static) -> EventListenerHandle {
        let closure = Closure::wrap(Box::new(move |ev: web_sys::Event| {
            let ev: web_sys::HidInputReportEvent = ev.unchecked_into();
            f(ev.report_id(), dataview_to_vec(&ev.data()));
        }) as Box<dyn FnMut(web_sys::Event)>);
        EventListenerHandle::add(self.device.as_ref(), "inputreport", closure)
    }

    /// Start buffering input reports, returning a stream to read them from
    /// (the closest WebHID gets to `hid_read`).
    ///
    /// Reports are queued from the moment this is called (the device must be
    /// [`open`](Self::open)ed); at most 64 are buffered, after which the
    /// oldest is dropped. Dropping the stream stops listening.
    pub fn start_reading(&self) -> InputReportStream {
        // Match the native read() convention: prefix the report ID byte only
        // when the device declares numbered reports.
        let numbered = uses_report_ids(&self.collections());
        let state = Rc::new(RefCell::new(StreamState::default()));
        let shared = state.clone();
        let closure = Closure::wrap(Box::new(move |ev: web_sys::Event| {
            let ev: web_sys::HidInputReportEvent = ev.unchecked_into();
            let data = dataview_to_vec(&ev.data());
            let report = if numbered {
                let mut buf = Vec::with_capacity(data.len() + 1);
                buf.push(ev.report_id());
                buf.extend_from_slice(&data);
                buf
            } else {
                data
            };
            let mut state = shared.borrow_mut();
            if state.queue.len() >= INPUT_QUEUE_CAPACITY {
                state.queue.pop_front();
            }
            state.queue.push_back(report);
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        let listener = EventListenerHandle::add(self.device.as_ref(), "inputreport", closure);
        InputReportStream {
            state,
            _listener: listener,
        }
    }

    /// The collection tree the browser parsed from the device's report
    /// descriptor (`HIDDevice.collections`), converted to plain Rust types.
    pub fn collections(&self) -> Vec<CollectionInfo> {
        self.device
            .collections()
            .iter()
            .map(|c| convert_collection(&c))
            .collect()
    }

    /// Reconstruct the report descriptor from
    /// [`collections`](Self::collections) (`hid_get_report_descriptor`
    /// equivalent).
    ///
    /// Browsers never expose the raw descriptor bytes, so this re-encodes
    /// the browser-parsed collection data via
    /// [`reconstruct_descriptor`]: report IDs, sizes, flags and usages are
    /// preserved, exact byte layout is not.
    pub fn report_descriptor(&self) -> HidResult<Vec<u8>> {
        Ok(reconstruct_descriptor(&self.collections()))
    }

    /// Parsed report descriptor (hidra extension, matching the native
    /// `HidDevice::parsed_report_descriptor`).
    pub fn parsed_report_descriptor(&self) -> HidResult<crate::descriptor::ReportDescriptor> {
        crate::descriptor::ReportDescriptor::parse(&self.report_descriptor()?)
    }
}

// --- input report stream ------------------------------------------------------

#[derive(Default)]
struct StreamState {
    queue: VecDeque<Vec<u8>>,
    waker: Option<Waker>,
}

/// A buffered reader over `inputreport` events, created by
/// [`WebHidDevice::start_reading`].
///
/// Reports follow the native `read()` convention: each buffer is prefixed
/// with its report ID byte iff the device declares numbered reports. At most
/// 64 reports are queued; when full, the oldest is dropped. Dropping the
/// stream removes the event listener.
pub struct InputReportStream {
    state: Rc<RefCell<StreamState>>,
    _listener: EventListenerHandle,
}

impl InputReportStream {
    /// Wait for the next input report (`hid_read` in blocking mode).
    pub async fn read(&mut self) -> HidResult<Vec<u8>> {
        ReadFuture {
            state: self.state.clone(),
        }
        .await
    }

    /// Pop a buffered report without waiting (`hid_read` in non-blocking
    /// mode; `None` when no report is queued).
    pub fn try_read(&mut self) -> Option<Vec<u8>> {
        self.state.borrow_mut().queue.pop_front()
    }
}

/// Future returned by [`InputReportStream::read`]: resolves when a report is
/// queued, parking the task's [`Waker`] in the shared state meanwhile.
struct ReadFuture {
    state: Rc<RefCell<StreamState>>,
}

impl Future for ReadFuture {
    type Output = HidResult<Vec<u8>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.borrow_mut();
        match state.queue.pop_front() {
            Some(report) => Poll::Ready(Ok(report)),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// --- web-sys -> report_info conversion ----------------------------------------

fn convert_collection(collection: &web_sys::HidCollectionInfo) -> CollectionInfo {
    CollectionInfo {
        usage_page: collection.get_usage_page().unwrap_or(0),
        usage: collection.get_usage().unwrap_or(0),
        collection_type: collection.get_type().unwrap_or(0),
        children: collection
            .get_children()
            .map(|children| children.iter().map(|c| convert_collection(&c)).collect())
            .unwrap_or_default(),
        input_reports: convert_reports(collection.get_input_reports()),
        output_reports: convert_reports(collection.get_output_reports()),
        feature_reports: convert_reports(collection.get_feature_reports()),
    }
}

fn convert_reports(reports: Option<js_sys::Array<web_sys::HidReportInfo>>) -> Vec<ReportInfo> {
    reports
        .map(|reports| {
            reports
                .iter()
                .map(|report| ReportInfo {
                    report_id: report.get_report_id().unwrap_or(0),
                    items: report
                        .get_items()
                        .map(|items| items.iter().map(|i| convert_item(&i)).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn convert_unit_system(system: web_sys::HidUnitSystem) -> UnitSystem {
    match system {
        web_sys::HidUnitSystem::None => UnitSystem::None,
        web_sys::HidUnitSystem::SiLinear => UnitSystem::SiLinear,
        web_sys::HidUnitSystem::SiRotation => UnitSystem::SiRotation,
        web_sys::HidUnitSystem::EnglishLinear => UnitSystem::EnglishLinear,
        web_sys::HidUnitSystem::EnglishRotation => UnitSystem::EnglishRotation,
        web_sys::HidUnitSystem::VendorDefined => UnitSystem::VendorDefined,
        web_sys::HidUnitSystem::Reserved => UnitSystem::Reserved,
        // The wasm_bindgen-generated enum is #[non_exhaustive].
        _ => UnitSystem::Reserved,
    }
}

fn convert_item(item: &web_sys::HidReportItem) -> ReportItemInfo {
    // Absent dictionary members fall back to ReportItemInfo's defaults
    // (an all-zero main item: Data, Variable, Absolute, Linear, Preferred).
    ReportItemInfo {
        usages: item
            .get_usages()
            .map(|usages| usages.iter().map(|u| u.value_of() as u32).collect())
            .unwrap_or_default(),
        usage_minimum: item.get_usage_minimum().unwrap_or(0),
        usage_maximum: item.get_usage_maximum().unwrap_or(0),
        report_size: item.get_report_size().unwrap_or(0),
        report_count: item.get_report_count().unwrap_or(0),
        logical_minimum: item.get_logical_minimum().unwrap_or(0),
        logical_maximum: item.get_logical_maximum().unwrap_or(0),
        physical_minimum: item.get_physical_minimum().unwrap_or(0),
        physical_maximum: item.get_physical_maximum().unwrap_or(0),
        unit_exponent: item.get_unit_exponent().unwrap_or(0),
        unit_system: item
            .get_unit_system()
            .map(convert_unit_system)
            .unwrap_or(UnitSystem::None),
        unit_factor_length_exponent: item.get_unit_factor_length_exponent().unwrap_or(0),
        unit_factor_mass_exponent: item.get_unit_factor_mass_exponent().unwrap_or(0),
        unit_factor_time_exponent: item.get_unit_factor_time_exponent().unwrap_or(0),
        unit_factor_temperature_exponent: item.get_unit_factor_temperature_exponent().unwrap_or(0),
        unit_factor_current_exponent: item.get_unit_factor_current_exponent().unwrap_or(0),
        unit_factor_luminous_intensity_exponent: item
            .get_unit_factor_luminous_intensity_exponent()
            .unwrap_or(0),
        is_absolute: item.get_is_absolute().unwrap_or(true),
        is_array: item.get_is_array().unwrap_or(false),
        is_buffered_bytes: item.get_is_buffered_bytes().unwrap_or(false),
        is_constant: item.get_is_constant().unwrap_or(false),
        is_linear: item.get_is_linear().unwrap_or(true),
        is_range: item.get_is_range().unwrap_or(false),
        is_volatile: item.get_is_volatile().unwrap_or(false),
        has_null: item.get_has_null().unwrap_or(false),
        has_preferred_state: item.get_has_preferred_state().unwrap_or(true),
        wrap: item.get_wrap().unwrap_or(false),
    }
}

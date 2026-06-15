//! USB-transport backend built on [nusb], selected for [`crate::Hidra`] when
//! the `nusb` feature is enabled.
//!
//! Unlike the per-OS native backends, this one talks to devices with raw USB
//! interrupt and control transfers, bypassing the OS HID stack entirely.
//! Prefer it when:
//!
//! * no hidraw node / OS HID driver is available for the device, or the OS
//!   HID stack restricts access;
//! * you need [`NusbDevice::get_indexed_string`], which the hidraw backend
//!   cannot provide;
//! * you want the kernel driver detached from the interface (Linux), e.g. to
//!   take a vendor interface away from `usbhid`.
//!
//! The trade-offs mirror hidapi-libusb exactly: opening a device **claims the
//! whole USB interface, stealing it from the OS driver** until the handle is
//! dropped, and raw USB access needs appropriate permissions, udev rules
//! granting access to the `/dev/bus/usb` node on Linux, a WinUSB-compatible
//! driver bound to the interface on Windows.
//!
//! Device paths use the format `usb:<bus>:<device-address>:<interface>`
//! (e.g. `usb:3:7:1`), where `<bus>` is nusb's bus identifier (the bus number
//! on Linux). Paths are stable for as long as the device stays connected, but
//! are not preserved across replug, like libusb bus addresses.
//!
//! Input reports can be read both blocking ([`NusbDevice::read`] /
//! [`NusbDevice::read_timeout`]) and asynchronously
//! ([`NusbDevice::read_async`]). Writes and feature reports remain blocking,
//! they are control or interrupt OUT transfers that complete quickly.
//!
//! [nusb]: https://docs.rs/nusb

use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroU8;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nusb::descriptors::TransferType;
use nusb::transfer::{
    ControlIn, ControlOut, ControlType, Direction, In, Interrupt, Out, Recipient, TransferError,
};
use nusb::{Endpoint, Interface, MaybeFuture};

use crate::descriptor::{ReportDescriptor, ReportKind};
use crate::error::{HidError, HidResult};
use crate::{BusType, DeviceInfo, MAX_REPORT_DESCRIPTOR_SIZE};

/// `bInterfaceClass` for HID.
const USB_CLASS_HID: u8 = 3;
/// Standard `GET_DESCRIPTOR` request.
const GET_DESCRIPTOR: u8 = 0x06;
/// HID class descriptor type for the report descriptor.
const DESCRIPTOR_TYPE_HID_REPORT: u8 = 0x22;
/// HID class `GET_REPORT` request.
const HID_GET_REPORT: u8 = 0x01;
/// HID class `SET_REPORT` request.
const HID_SET_REPORT: u8 = 0x09;
/// Report types in the high byte of `wValue` (HID 1.11, 7.2.1).
const REPORT_TYPE_INPUT: u16 = 1;
const REPORT_TYPE_OUTPUT: u16 = 2;
const REPORT_TYPE_FEATURE: u16 = 3;
/// hidapi uses a fixed 1000 ms timeout for all control transfers and writes.
const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);
/// How often the reader thread re-checks the shutdown flag while idle.
const READER_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// hidapi drops the oldest queued input report beyond 30.
const MAX_QUEUED_REPORTS: usize = 30;
/// The string-descriptor language hidapi requests.
const US_ENGLISH: u16 = 0x0409;

// --- path handling -----------------------------------------------------------

fn format_path(bus_id: &str, device_address: u8, interface_number: u8) -> String {
    format!("usb:{bus_id}:{device_address}:{interface_number}")
}

/// Parse `usb:<bus>:<device-address>:<interface>`. The bus identifier may
/// itself contain `:` on some platforms, so the address and interface are
/// taken from the right.
fn parse_path(path: &str) -> Option<(&str, u8, u8)> {
    let rest = path.strip_prefix("usb:")?;
    let mut parts = rest.rsplitn(3, ':');
    let interface_number = parts.next()?.parse().ok()?;
    let device_address = parts.next()?.parse().ok()?;
    let bus_id = parts.next()?;
    (!bus_id.is_empty()).then_some((bus_id, device_address, interface_number))
}

// --- helpers ------------------------------------------------------------------

fn transfer_error(operation: &'static str, err: TransferError) -> HidError {
    match err {
        TransferError::Disconnected => HidError::Disconnected,
        TransferError::Cancelled => HidError::backend(format!("{operation}: timed out")),
        e => HidError::backend(format!("{operation}: {e}")),
    }
}

/// Enumeration-style metadata for one HID interface of a device, built from
/// cached descriptors only (no device I/O).
fn device_info(dev: &nusb::DeviceInfo, interface_number: u8) -> DeviceInfo {
    DeviceInfo {
        path: format_path(dev.bus_id(), dev.device_address(), interface_number),
        vendor_id: dev.vendor_id(),
        product_id: dev.product_id(),
        serial_number: dev.serial_number().map(str::to_string),
        release_number: dev.device_version(),
        manufacturer_string: dev.manufacturer_string().map(str::to_string),
        product_string: dev.product_string().map(str::to_string),
        interface_number: i32::from(interface_number),
        bus_type: BusType::Usb,
        ..Default::default()
    }
}

/// Append one `DeviceInfo` per top-level usage pair, like hidapi. With no
/// readable descriptor a single entry with usage 0/0 is emitted.
fn push_usage_entries(mut info: DeviceInfo, usages: &[(u16, u16)], out: &mut Vec<DeviceInfo>) {
    match usages {
        [] => out.push(info),
        [first @ .., last] => {
            for &(page, usage) in first {
                let mut e = info.clone();
                e.usage_page = page;
                e.usage = usage;
                out.push(e);
            }
            info.usage_page = last.0;
            info.usage = last.1;
            out.push(info);
        }
    }
}

/// `bDescriptorType` of the HID class descriptor inside an interface.
const DESCRIPTOR_TYPE_HID: u8 = 0x21;

/// `wDescriptorLength` declared by the HID class descriptor of the given
/// interface's alternate setting 0.
///
/// hidapi-libusb requests exactly this many bytes, and so do we: some
/// devices (seen on a UVC webcam with a vendor HID interface) return
/// unrelated descriptor data past the real report descriptor when the
/// request asks for more.
fn declared_report_descriptor_len<'a, I>(alt_settings: I, interface_number: u8) -> Option<usize>
where
    I: Iterator<Item = nusb::descriptors::InterfaceDescriptor<'a>>,
{
    alt_settings
        .filter(|alt| alt.interface_number() == interface_number && alt.alternate_setting() == 0)
        .flat_map(|alt| alt.descriptors())
        .find(|d| d.descriptor_type() == DESCRIPTOR_TYPE_HID && d.len() >= 9)
        .map(|d| u16::from_le_bytes([d[7], d[8]]) as usize)
}

/// HID report descriptor request, shared by the claimed and unclaimed paths.
fn report_descriptor_request(interface_number: u8, length: usize) -> ControlIn {
    ControlIn {
        control_type: ControlType::Standard,
        recipient: Recipient::Interface,
        request: GET_DESCRIPTOR,
        value: u16::from(DESCRIPTOR_TYPE_HID_REPORT) << 8,
        index: u16::from(interface_number),
        length: length.min(MAX_REPORT_DESCRIPTOR_SIZE) as u16,
    }
}

/// Read the report descriptor through a claimed interface.
fn read_report_descriptor(interface: &Interface) -> Option<Vec<u8>> {
    let interface_number = interface.interface_number();
    let length = declared_report_descriptor_len(interface.descriptors(), interface_number)
        .unwrap_or(MAX_REPORT_DESCRIPTOR_SIZE);
    let mut data = interface
        .control_in(
            report_descriptor_request(interface_number, length),
            CONTROL_TIMEOUT,
        )
        .wait()
        .ok()
        .filter(|d| !d.is_empty())?;
    data.truncate(length);
    Some(data)
}

/// Best-effort report descriptor read during enumeration, without claiming
/// the interface (claiming would detach kernel drivers from every device).
#[cfg(not(target_os = "windows"))]
fn read_report_descriptor_unclaimed(
    device: &nusb::Device,
    interface_number: u8,
) -> Option<Vec<u8>> {
    let length = device
        .active_configuration()
        .ok()
        .and_then(|c| declared_report_descriptor_len(c.interface_alt_settings(), interface_number))
        .unwrap_or(MAX_REPORT_DESCRIPTOR_SIZE);
    let mut data = device
        .control_in(
            report_descriptor_request(interface_number, length),
            CONTROL_TIMEOUT,
        )
        .wait()
        .ok()
        .filter(|d| !d.is_empty())?;
    data.truncate(length);
    Some(data)
}

/// WinUSB only allows control transfers through a claimed interface handle,
/// so enumeration stays non-invasive and reports usage 0/0, like
/// hidapi-libusb built without `INVASIVE_GET_USAGE`.
#[cfg(target_os = "windows")]
fn read_report_descriptor_unclaimed(
    _device: &nusb::Device,
    _interface_number: u8,
) -> Option<Vec<u8>> {
    None
}

/// Size of each interrupt IN transfer: the longest declared input report
/// (wire format, including the report ID byte when used) or at least one
/// packet, rounded up to a multiple of `wMaxPacketSize` as nusb requires.
fn transfer_length(max_input_wire: usize, max_packet_size: usize) -> usize {
    max_input_wire
        .max(max_packet_size)
        .div_ceil(max_packet_size)
        * max_packet_size
}

// --- backend API ---------------------------------------------------------------

/// Entry point for the USB backend; the platform backend behind
/// [`crate::Hidra`] when the `nusb` feature is enabled. See the
/// [module docs](self) for when to prefer it.
pub(crate) struct NusbApi {
    _private: (),
}

impl NusbApi {
    /// Initialize the backend.
    pub fn new() -> HidResult<Self> {
        Ok(NusbApi { _private: () })
    }

    /// Enumerate connected USB HID interfaces. `vendor_id`/`product_id` of 0
    /// act as wildcards.
    ///
    /// Usage page/usage require reading each device's report descriptor,
    /// which needs the device opened; this is attempted best-effort and the
    /// fields stay 0/0 when the device cannot be opened (e.g. missing udev
    /// permissions), matching hidapi-libusb.
    pub fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
        let devices = match nusb::list_devices().wait() {
            Ok(devices) => devices,
            // A missing /sys/bus/usb (containers, build sandboxes) means no
            // USB subsystem: an empty list, like the hidraw backend when
            // /sys/class/hidraw is absent. nusb reports it as Other+ENOENT.
            #[cfg(target_os = "linux")]
            Err(e)
                if e.kind() == nusb::ErrorKind::Other
                    && e.os_error() == Some(libc::ENOENT as u32) =>
            {
                return Ok(Vec::new())
            }
            Err(e) => return Err(HidError::backend(format!("listing USB devices: {e}"))),
        };
        let mut result = Vec::new();
        for dev in devices {
            let vid_ok = vendor_id == 0 || dev.vendor_id() == vendor_id;
            let pid_ok = product_id == 0 || dev.product_id() == product_id;
            if !vid_ok || !pid_ok {
                continue;
            }
            let hid_interfaces: Vec<u8> = dev
                .interfaces()
                .filter(|i| i.class() == USB_CLASS_HID)
                .map(|i| i.interface_number())
                .collect();
            if hid_interfaces.is_empty() {
                continue;
            }
            // Opening does not claim any interface, so this is non-invasive.
            let opened = dev.open().wait().ok();
            for interface_number in hid_interfaces {
                let info = device_info(&dev, interface_number);
                let usages = opened
                    .as_ref()
                    .and_then(|d| read_report_descriptor_unclaimed(d, interface_number))
                    .and_then(|bytes| ReportDescriptor::parse(&bytes).ok())
                    .map(|d| d.top_level_usages())
                    .unwrap_or_default();
                push_usage_entries(info, &usages, &mut result);
            }
        }
        Ok(result)
    }

    /// Open the first interface matching `vendor_id`/`product_id` and,
    /// optionally, the serial number.
    pub fn open(
        &self,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> HidResult<NusbDevice> {
        let candidates = self.enumerate(vendor_id, product_id)?;
        let info = candidates
            .into_iter()
            .find(|info| match serial {
                Some(s) => info.serial_number.as_deref() == Some(s),
                None => true,
            })
            .ok_or(HidError::DeviceNotFound)?;
        self.open_path(&info.path)
    }

    /// Open a device by `usb:<bus>:<device-address>:<interface>` path, as
    /// reported by [`enumerate`](Self::enumerate).
    pub fn open_path(&self, path: &str) -> HidResult<NusbDevice> {
        let (bus_id, device_address, interface_number) =
            parse_path(path).ok_or_else(|| HidError::InvalidData {
                message: format!("invalid USB device path: {path}"),
            })?;
        let mut devices = nusb::list_devices()
            .wait()
            .map_err(|e| HidError::backend(format!("listing USB devices: {e}")))?;
        let dev = devices
            .find(|d| d.bus_id() == bus_id && d.device_address() == device_address)
            .ok_or(HidError::DeviceNotFound)?;
        NusbDevice::open(&dev, interface_number)
    }
}

// --- device handle ---------------------------------------------------------------

/// Input state guarded by the [`Shared`] mutex.
#[derive(Default)]
struct InputQueue {
    /// Completed input reports, oldest first, capped at
    /// [`MAX_QUEUED_REPORTS`].
    reports: VecDeque<Vec<u8>>,
    /// Tasks parked in [`Shared::poll_read`] on an empty queue, deduplicated
    /// via [`Waker::will_wake`]. Drained (and woken, after the lock is
    /// released) whenever a report is queued or the reader exits.
    wakers: Vec<Waker>,
}

/// State shared between the device handle and its reader thread.
#[derive(Default)]
struct Shared {
    queue: Mutex<InputQueue>,
    /// Signaled whenever a report is queued or the reader exits.
    data_available: Condvar,
    /// Reader thread has exited (or must exit).
    shutdown: AtomicBool,
    /// The device is gone; reads fail once the queue drains.
    disconnected: AtomicBool,
}

impl Shared {
    /// Queue a completed report, dropping the oldest beyond the cap, and
    /// wake every blocked or parked reader.
    fn push_report(&self, report: Vec<u8>) {
        let mut input = self.queue.lock().unwrap();
        if input.reports.len() >= MAX_QUEUED_REPORTS {
            input.reports.pop_front(); // drop the oldest, like hidapi
        }
        input.reports.push_back(report);
        let wakers = std::mem::take(&mut input.wakers);
        drop(input);
        self.data_available.notify_all();
        for waker in wakers {
            waker.wake();
        }
    }

    /// Wake every blocked or parked reader without queueing data; called
    /// after the disconnect/shutdown flags change so waiters re-check them.
    fn wake_readers(&self) {
        let wakers = std::mem::take(&mut self.queue.lock().unwrap().wakers);
        self.data_available.notify_all();
        for waker in wakers {
            waker.wake();
        }
    }

    /// Pop-or-park core of [`NusbDevice::read_async`]: copy one queued
    /// report into `buf`, fail once the device is gone and the queue has
    /// drained, or park the task's waker on the queue.
    ///
    /// The flag checks and the waker registration happen under the queue
    /// lock, and the reader thread sets the flags before taking that lock,
    /// so a parked waker is never missed.
    fn poll_read(&self, buf: &mut [u8], cx: &mut Context<'_>) -> Poll<HidResult<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            }));
        }
        let mut input = self.queue.lock().unwrap();
        if let Some(report) = input.reports.pop_front() {
            let len = report.len().min(buf.len());
            buf[..len].copy_from_slice(&report[..len]);
            return Poll::Ready(Ok(len));
        }
        // Queued reports drain even after a disconnect, like the sync path.
        if self.disconnected.load(Ordering::SeqCst) {
            return Poll::Ready(Err(HidError::Disconnected));
        }
        if self.shutdown.load(Ordering::SeqCst) {
            return Poll::Ready(Err(HidError::backend("USB reader thread terminated")));
        }
        if !input.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            input.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

/// An open USB HID interface; the platform device behind
/// [`crate::HidDevice`] when the `nusb` feature is enabled.
///
/// Holding this claims the interface exclusively (detached from the kernel
/// driver on Linux); dropping it releases the interface, returning it to the
/// OS. All methods take `&self`; the handle is `Send + Sync`.
pub(crate) struct NusbDevice {
    /// Keeps the device open; also used for string descriptor requests.
    device: nusb::Device,
    interface: Interface,
    interface_number: u8,
    /// Enumeration-style metadata captured at open time.
    info: DeviceInfo,
    /// Report descriptor read at open time; empty when unreadable.
    report_descriptor: Vec<u8>,
    /// Interrupt OUT endpoint; writes fall back to `SET_REPORT` without one.
    out_endpoint: Option<Mutex<Endpoint<Interrupt, Out>>>,
    // Part of the backend contract; the wrapper now reads input via
    // `read_async`, so the blocking-mode state is unused on this path.
    #[allow(dead_code)]
    blocking: AtomicBool,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

impl NusbDevice {
    fn open(dev_info: &nusb::DeviceInfo, interface_number: u8) -> HidResult<Self> {
        let device = dev_info.open().wait().map_err(|e| HidError::OpenFailed {
            message: format!("opening USB device: {e}"),
        })?;
        // Detaches the kernel driver on Linux; plain claim elsewhere.
        let interface = device
            .detach_and_claim_interface(interface_number)
            .wait()
            .map_err(|e| HidError::OpenFailed {
                message: format!("claiming interface {interface_number}: {e}"),
            })?;

        // Probe the report descriptor once, like hidapi: it determines report
        // ID usage / input sizes and backs `get_report_descriptor`.
        let report_descriptor = read_report_descriptor(&interface).unwrap_or_default();
        let parsed = ReportDescriptor::parse(&report_descriptor).ok();
        let max_input_wire = parsed
            .as_ref()
            .map(|d| d.max_wire_size(ReportKind::Input))
            .unwrap_or(0);

        // Interrupt endpoints from alternate setting 0; IN is mandatory for
        // HID, OUT is optional.
        let mut in_address = None;
        let mut out_address = None;
        let alt0 = interface
            .descriptors()
            .find(|d| d.alternate_setting() == 0)
            .or_else(|| interface.descriptor());
        if let Some(desc) = alt0 {
            for ep in desc.endpoints() {
                if ep.transfer_type() != TransferType::Interrupt {
                    continue;
                }
                match ep.direction() {
                    Direction::In if in_address.is_none() => in_address = Some(ep.address()),
                    Direction::Out if out_address.is_none() => out_address = Some(ep.address()),
                    _ => {}
                }
            }
        }
        let in_address = in_address
            .ok_or_else(|| HidError::backend("HID interface has no interrupt IN endpoint"))?;
        let in_endpoint: Endpoint<Interrupt, In> = interface
            .endpoint(in_address)
            .map_err(|e| HidError::backend(format!("opening interrupt IN endpoint: {e}")))?;
        let out_endpoint = match out_address {
            Some(address) => Some(Mutex::new(
                interface.endpoint::<Interrupt, Out>(address).map_err(|e| {
                    HidError::backend(format!("opening interrupt OUT endpoint: {e}"))
                })?,
            )),
            None => None,
        };

        let max_packet_size = in_endpoint.max_packet_size();
        if max_packet_size == 0 {
            return Err(HidError::backend(
                "interrupt IN endpoint declares a zero wMaxPacketSize",
            ));
        }
        let transfer_len = transfer_length(max_input_wire, max_packet_size);

        let mut info = device_info(dev_info, interface_number);
        if let Some((page, usage)) = parsed
            .as_ref()
            .and_then(|d| d.top_level_usages().first().copied())
        {
            info.usage_page = page;
            info.usage = usage;
        }

        let shared = Arc::new(Shared::default());
        let reader = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("hidra-usb-read".into())
                .spawn(move || reader_loop(in_endpoint, shared, transfer_len))
                .map_err(|e| HidError::io("spawning USB reader thread", e))?
        };

        Ok(NusbDevice {
            device,
            interface,
            interface_number,
            info,
            report_descriptor,
            out_endpoint,
            blocking: AtomicBool::new(true),
            shared,
            reader: Some(reader),
        })
    }

    /// Send an output report. `data[0]` is the report ID; like hidapi's
    /// libusb backend, a 0 ID byte is stripped before transmission on both
    /// the interrupt and the `SET_REPORT` control path, while a nonzero ID
    /// is sent on the wire. Returns the original length on success.
    pub fn write(&self, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "write data must contain a report ID byte".into(),
            });
        }
        let report_number = data[0];
        let (payload, skipped) = if report_number == 0 {
            (&data[1..], 1)
        } else {
            (data, 0)
        };
        match &self.out_endpoint {
            Some(endpoint) => {
                let mut endpoint = endpoint.lock().unwrap();
                let completion =
                    endpoint.transfer_blocking(payload.to_vec().into(), CONTROL_TIMEOUT);
                match completion.status {
                    Ok(()) => Ok(completion.actual_len + skipped),
                    Err(e) => Err(transfer_error("interrupt OUT write", e)),
                }
            }
            None => {
                // No interrupt OUT endpoint: use SET_REPORT(Output), like
                // hidapi.
                self.interface
                    .control_out(
                        ControlOut {
                            control_type: ControlType::Class,
                            recipient: Recipient::Interface,
                            request: HID_SET_REPORT,
                            value: (REPORT_TYPE_OUTPUT << 8) | u16::from(report_number),
                            index: u16::from(self.interface_number),
                            data: payload,
                        },
                        CONTROL_TIMEOUT,
                    )
                    .wait()
                    .map_err(|e| transfer_error("SET_REPORT (output)", e))?;
                Ok(data.len())
            }
        }
    }

    /// Read an input report, honoring the blocking mode.
    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn read(&self, buf: &mut [u8]) -> HidResult<usize> {
        let timeout = if self.blocking.load(Ordering::Relaxed) {
            -1
        } else {
            0
        };
        self.read_timeout(buf, timeout)
    }

    /// Read an input report queued by the reader thread. Negative timeout
    /// blocks forever, `0` polls; returns `Ok(0)` on timeout. Reports pass
    /// through in USB wire format, which already matches hidapi's
    /// convention: the report ID prefix is present only for devices with
    /// numbered reports.
    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            });
        }
        let deadline =
            (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
        let mut queue = self.shared.queue.lock().unwrap();
        loop {
            if let Some(report) = queue.reports.pop_front() {
                let len = report.len().min(buf.len());
                buf[..len].copy_from_slice(&report[..len]);
                return Ok(len);
            }
            // Queued reports drain even after a disconnect, like hidapi.
            if self.shared.disconnected.load(Ordering::SeqCst) {
                return Err(HidError::Disconnected);
            }
            if self.shared.shutdown.load(Ordering::SeqCst) {
                return Err(HidError::backend("USB reader thread terminated"));
            }
            if timeout_ms == 0 {
                return Ok(0);
            }
            queue = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(0);
                    }
                    self.shared
                        .data_available
                        .wait_timeout(queue, deadline - now)
                        .unwrap()
                        .0
                }
                None => self.shared.data_available.wait(queue).unwrap(),
            };
        }
    }

    /// Read an input report asynchronously (hidra extension; hidapi has no
    /// async API).
    ///
    /// Resolves once a report queued by the reader thread has been copied
    /// into `buf`, returning its length, never `Ok(0)`; use your runtime's
    /// timeout combinator (e.g. `tokio::time::timeout`) instead of
    /// [`read_timeout`]. Fails with [`HidError::Disconnected`] when the
    /// device is removed and the queue has drained, like [`read`].
    ///
    /// The future is runtime-agnostic (plain `Waker` wake-ups, like nusb,
    /// works under tokio, async-std, smol or a hand-rolled executor) and
    /// cancel-safe: reports are only dequeued inside `poll`, so dropping it
    /// never loses input; pending reports stay queued for the next read.
    /// The blocking mode set by [`set_blocking_mode`] is ignored.
    ///
    /// [`read`]: Self::read
    /// [`read_timeout`]: Self::read_timeout
    /// [`set_blocking_mode`]: Self::set_blocking_mode
    pub fn read_async<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = HidResult<usize>> + Send + 'a {
        ReadAsync {
            shared: &self.shared,
            buf,
        }
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn set_blocking_mode(&self, blocking: bool) -> HidResult<()> {
        self.blocking.store(blocking, Ordering::Relaxed);
        Ok(())
    }

    /// Send a feature report via `SET_REPORT(Feature)`. `data[0]` is the
    /// report ID; a 0 ID byte is stripped, like hidapi.
    pub fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "feature report must contain a report ID byte".into(),
            });
        }
        let report_number = data[0];
        let payload = if report_number == 0 { &data[1..] } else { data };
        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: HID_SET_REPORT,
                    value: (REPORT_TYPE_FEATURE << 8) | u16::from(report_number),
                    index: u16::from(self.interface_number),
                    data: payload,
                },
                CONTROL_TIMEOUT,
            )
            .wait()
            .map_err(|e| transfer_error("SET_REPORT (feature)", e))?;
        Ok(())
    }

    /// `GET_REPORT` shared by feature and input reports. `buf[0]` carries the
    /// report ID on entry; for ID 0 the returned data is written at
    /// `buf[1..]` so the ID stays in byte 0, exactly like hidapi.
    fn get_report(
        &self,
        report_type: u16,
        buf: &mut [u8],
        operation: &'static str,
    ) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "buffer must contain a report ID byte".into(),
            });
        }
        let report_number = buf[0];
        let offset = usize::from(report_number == 0);
        let length = (buf.len() - offset).min(usize::from(u16::MAX)) as u16;
        let data = self
            .interface
            .control_in(
                ControlIn {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: HID_GET_REPORT,
                    value: (report_type << 8) | u16::from(report_number),
                    index: u16::from(self.interface_number),
                    length,
                },
                CONTROL_TIMEOUT,
            )
            .wait()
            .map_err(|e| transfer_error(operation, e))?;
        let len = data.len().min(buf.len() - offset);
        buf[offset..offset + len].copy_from_slice(&data[..len]);
        Ok(len + offset)
    }

    /// Get a feature report via `GET_REPORT(Feature)`. Set `buf[0]` to the
    /// report ID before calling.
    pub fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(REPORT_TYPE_FEATURE, buf, "GET_REPORT (feature)")
    }

    /// Get an input report synchronously via `GET_REPORT(Input)`. Same
    /// buffer convention as [`get_feature_report`](Self::get_feature_report).
    pub fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(REPORT_TYPE_INPUT, buf, "GET_REPORT (input)")
    }

    pub fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        Ok(self.info.manufacturer_string.clone())
    }

    pub fn get_product_string(&self) -> HidResult<Option<String>> {
        Ok(self.info.product_string.clone())
    }

    pub fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        Ok(self.info.serial_number.clone())
    }

    /// Read a string descriptor by index (US English), which only this
    /// backend supports, the native hidraw backend cannot.
    pub fn get_indexed_string(&self, index: u32) -> HidResult<Option<String>> {
        let index = u8::try_from(index)
            .ok()
            .and_then(NonZeroU8::new)
            .ok_or_else(|| HidError::InvalidData {
                message: "string descriptor index must be in 1..=255".into(),
            })?;
        let s = self
            .device
            .get_string_descriptor(index, US_ENGLISH, CONTROL_TIMEOUT)
            .wait()
            .map_err(|e| match e {
                nusb::GetDescriptorError::Transfer(TransferError::Disconnected) => {
                    HidError::Disconnected
                }
                e => HidError::backend(format!("reading string descriptor: {e}")),
            })?;
        Ok(Some(s))
    }

    /// Raw report descriptor, served from the copy read at open time.
    pub fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
        if self.report_descriptor.is_empty() {
            return Err(HidError::backend(
                "the HID report descriptor could not be read when the device was opened",
            ));
        }
        let len = self.report_descriptor.len().min(buf.len());
        buf[..len].copy_from_slice(&self.report_descriptor[..len]);
        Ok(len)
    }

    /// Enumeration-style metadata for this interface, captured at open time.
    pub fn get_device_info(&self) -> HidResult<DeviceInfo> {
        Ok(self.info.clone())
    }
}

impl Drop for NusbDevice {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.wake_readers();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Future returned by [`NusbDevice::read_async`].
///
/// Cancel-safe: reports are popped from the shared queue only inside
/// [`Future::poll`], so dropping the future before completion leaves any
/// pending report queued for the next read.
struct ReadAsync<'a> {
    shared: &'a Shared,
    buf: &'a mut [u8],
}

impl Future for ReadAsync<'_> {
    type Output = HidResult<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.shared.poll_read(this.buf, cx)
    }
}

/// Reader thread: keeps one interrupt IN transfer pending and queues
/// completed reports, mirroring hidapi's `read_callback` loop.
fn reader_loop(mut endpoint: Endpoint<Interrupt, In>, shared: Arc<Shared>, transfer_len: usize) {
    let buf = endpoint.allocate(transfer_len);
    endpoint.submit(buf);
    while !shared.shutdown.load(Ordering::SeqCst) {
        let Some(completion) = endpoint.wait_next_complete(READER_POLL_INTERVAL) else {
            continue; // idle; re-check the shutdown flag
        };
        match completion.status {
            Ok(()) => {
                let len = completion.actual_len.min(completion.buffer.len());
                if len > 0 {
                    shared.push_report(completion.buffer[..len].to_vec());
                }
                endpoint.submit(completion.buffer);
            }
            Err(TransferError::Disconnected) => {
                shared.disconnected.store(true, Ordering::SeqCst);
                shared.wake_readers();
                break;
            }
            Err(TransferError::Cancelled) => break,
            // Transient conditions (stall, fault): resubmit, like hidapi.
            Err(_) => endpoint.submit(completion.buffer),
        }
    }
    // Reclaim any pending transfer so the endpoint drops cleanly.
    endpoint.cancel_all();
    while endpoint.pending() > 0 {
        if endpoint
            .wait_next_complete(Duration::from_secs(1))
            .is_none()
        {
            break;
        }
    }
    shared.shutdown.store(true, Ordering::SeqCst);
    shared.wake_readers();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_round_trip() {
        let path = format_path("3", 7, 1);
        assert_eq!(path, "usb:3:7:1");
        assert_eq!(parse_path(&path), Some(("3", 7, 1)));
        assert_eq!(parse_path("usb:0:255:255"), Some(("0", 255, 255)));
    }

    #[test]
    fn path_parse_allows_colons_in_bus_id() {
        // Some platforms use non-numeric bus identifiers.
        assert_eq!(parse_path("usb:PCI0@14:5:2"), Some(("PCI0@14", 5, 2)));
        assert_eq!(parse_path("usb:a:b:5:2"), Some(("a:b", 5, 2)));
    }

    #[test]
    fn path_parse_rejects_malformed_paths() {
        assert_eq!(parse_path(""), None);
        assert_eq!(parse_path("usb:"), None);
        assert_eq!(parse_path("usb:1:2"), None);
        assert_eq!(parse_path("usb::2:3"), None);
        assert_eq!(parse_path("usb:1:x:3"), None);
        assert_eq!(parse_path("usb:1:2:300"), None);
        assert_eq!(parse_path("hid:1:2:3"), None);
        assert_eq!(parse_path("/dev/hidraw0"), None);
    }

    #[test]
    fn transfer_length_is_a_packet_multiple() {
        assert_eq!(transfer_length(0, 64), 64);
        assert_eq!(transfer_length(8, 64), 64);
        assert_eq!(transfer_length(64, 64), 64);
        assert_eq!(transfer_length(65, 64), 128);
        assert_eq!(transfer_length(300, 64), 320);
        assert_eq!(transfer_length(17, 8), 24);
    }

    #[test]
    fn api_and_device_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NusbApi>();
        assert_send_sync::<NusbDevice>();
    }

    /// Drive `Shared::poll_read` as a future, as `read_async` does.
    fn block_on_read(shared: &Shared, buf: &mut [u8]) -> HidResult<usize> {
        crate::test_util::block_on(std::future::poll_fn(|cx| shared.poll_read(buf, cx)))
    }

    #[test]
    fn poll_read_pops_queued_reports_with_truncation() {
        let shared = Shared::default();
        shared.push_report(vec![1, 2, 3, 4]);
        shared.push_report(vec![5]);
        let mut buf = [0u8; 3];
        // Same truncation semantics as the sync path: excess bytes are lost.
        assert_eq!(block_on_read(&shared, &mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(block_on_read(&shared, &mut buf).unwrap(), 1);
        assert_eq!(buf[0], 5);
    }

    #[test]
    fn poll_read_rejects_empty_buffer() {
        let shared = Shared::default();
        shared.push_report(vec![1]);
        let err = block_on_read(&shared, &mut []).unwrap_err();
        assert!(matches!(err, HidError::InvalidData { .. }));
        // The queued report must not have been consumed.
        assert_eq!(shared.queue.lock().unwrap().reports.len(), 1);
    }

    #[test]
    fn poll_read_drains_queue_before_reporting_disconnect() {
        let shared = Shared::default();
        shared.push_report(vec![9]);
        shared.disconnected.store(true, Ordering::SeqCst);
        let mut buf = [0u8; 4];
        assert_eq!(block_on_read(&shared, &mut buf).unwrap(), 1);
        let err = block_on_read(&shared, &mut buf).unwrap_err();
        assert!(matches!(err, HidError::Disconnected));
    }

    #[test]
    fn poll_read_parks_until_a_report_is_pushed() {
        let shared = Arc::new(Shared::default());
        let pusher = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                // Give the main thread a chance to park its waker first.
                while shared.queue.lock().unwrap().wakers.is_empty() {
                    std::thread::yield_now();
                }
                shared.push_report(vec![7, 8]);
            })
        };
        let mut buf = [0u8; 4];
        assert_eq!(block_on_read(&shared, &mut buf).unwrap(), 2);
        assert_eq!(buf[..2], [7, 8]);
        pusher.join().unwrap();
    }

    #[test]
    fn poll_read_parks_until_disconnect_is_flagged() {
        let shared = Arc::new(Shared::default());
        let disconnector = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                while shared.queue.lock().unwrap().wakers.is_empty() {
                    std::thread::yield_now();
                }
                // Same order as the reader thread: flag first, then wake.
                shared.disconnected.store(true, Ordering::SeqCst);
                shared.wake_readers();
            })
        };
        let mut buf = [0u8; 4];
        let err = block_on_read(&shared, &mut buf).unwrap_err();
        assert!(matches!(err, HidError::Disconnected));
        disconnector.join().unwrap();
    }

    #[test]
    fn poll_read_dedups_wakers_of_the_same_task() {
        struct CountingWaker(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let shared = Shared::default();
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        let mut buf = [0u8; 4];
        // Re-polling the same task must not pile up waker clones.
        assert!(shared.poll_read(&mut buf, &mut cx).is_pending());
        assert!(shared.poll_read(&mut buf, &mut cx).is_pending());
        assert_eq!(shared.queue.lock().unwrap().wakers.len(), 1);
        // A pushed report drains the parked waker and wakes it exactly once.
        shared.push_report(vec![1]);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(shared.queue.lock().unwrap().wakers.is_empty());
    }

    #[test]
    fn enumerate_does_not_panic() {
        // The machine may or may not have USB HID devices; either way this
        // must return cleanly. Enumeration never claims interfaces.
        let api = NusbApi::new().unwrap();
        let devices = api.enumerate(0, 0).unwrap();
        for d in &devices {
            assert!(d.path().starts_with("usb:"));
            assert!(parse_path(d.path()).is_some());
            assert_eq!(d.bus_type(), BusType::Usb);
        }
    }
}

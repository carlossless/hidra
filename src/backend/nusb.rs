//! USB-transport backend built on [nusb], compiled in by the `nusb` feature
//! and selected at run time with
//! [`Backend::Nusb`](crate::Backend::Nusb).
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
//! Opening a device **claims the
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
//! Input reports are read asynchronously ([`NusbDevice::read_async`]) from a
//! queue filled by a background reader thread. Writes and feature reports are
//! blocking; they are control or interrupt OUT transfers that complete
//! quickly.
//!
//! [nusb]: https://docs.rs/nusb

use std::future::Future;
use std::num::NonZeroU8;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;

use nusb::descriptors::TransferType;
use nusb::transfer::{
    ControlIn, ControlOut, ControlType, Direction, In, Interrupt, Out, Recipient, TransferError,
};
use nusb::{Endpoint, Interface, MaybeFuture};

use super::queue::ReportQueue;
use super::{payload_after_report_id, HidBackend, HidDeviceBackend};
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
/// A fixed 1000 ms timeout for every transfer issued, control and interrupt
/// OUT alike.
const TRANSFER_TIMEOUT: Duration = Duration::from_millis(1000);
/// How often the reader thread re-checks the shutdown flag while idle.
const READER_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// The string-descriptor language requested.
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

/// `bDescriptorType` of the HID class descriptor inside an interface.
const DESCRIPTOR_TYPE_HID: u8 = 0x21;

/// `wDescriptorLength` declared by the HID class descriptor of the given
/// interface's alternate setting 0.
///
/// We request exactly this many bytes: some
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
            TRANSFER_TIMEOUT,
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
            TRANSFER_TIMEOUT,
        )
        .wait()
        .ok()
        .filter(|d| !d.is_empty())?;
    data.truncate(length);
    Some(data)
}

/// `WinUSB` only allows control transfers through a claimed interface handle,
/// so enumeration stays non-invasive and reports usage 0/0, without opening
/// the device.
#[cfg(target_os = "windows")]
fn read_report_descriptor_unclaimed(
    _device: &nusb::Device,
    _interface_number: u8,
) -> Option<Vec<u8>> {
    None
}

#[cfg(not(target_os = "windows"))]
fn device_control_out(device: &nusb::Device, request: ControlOut<'_>) -> Result<(), TransferError> {
    device.control_out(request, TRANSFER_TIMEOUT).wait()?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn device_control_in(device: &nusb::Device, request: ControlIn) -> Result<Vec<u8>, TransferError> {
    device.control_in(request, TRANSFER_TIMEOUT).wait()
}

#[cfg(target_os = "windows")]
fn device_control_out(
    _device: &nusb::Device,
    _request: ControlOut<'_>,
) -> Result<(), TransferError> {
    Err(TransferError::InvalidArgument)
}

#[cfg(target_os = "windows")]
fn device_control_in(
    _device: &nusb::Device,
    _request: ControlIn,
) -> Result<Vec<u8>, TransferError> {
    Err(TransferError::InvalidArgument)
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

/// Entry point for the USB backend; behind a [`crate::Hidra`] built with
/// [`Backend::Nusb`](crate::Backend::Nusb). See the [module docs](self) for
/// when to prefer it.
pub(crate) struct NusbApi;

impl HidBackend for NusbApi {
    type Device = NusbDevice;

    /// Initialize the backend.
    fn new() -> HidResult<Self> {
        Ok(NusbApi)
    }

    /// Enumerate connected USB HID interfaces. `vendor_id`/`product_id` of 0
    /// act as wildcards.
    ///
    /// Usage page/usage require reading each device's report descriptor,
    /// which needs the device opened; this is attempted best-effort and the
    /// fields stay 0/0 when the device cannot be opened (e.g. missing udev
    /// permissions).
    fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
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
            // `device_info` reads cached descriptors only, so the filter runs
            // before anything is opened: a device nobody asked for is never
            // touched.
            let wanted: Vec<(u8, DeviceInfo)> = dev
                .interfaces()
                .filter(|i| i.class() == USB_CLASS_HID)
                .map(|i| {
                    let number = i.interface_number();
                    (number, device_info(&dev, number))
                })
                .filter(|(_, info)| info.matches(vendor_id, product_id))
                .collect();
            if wanted.is_empty() {
                continue;
            }
            // Opening does not claim any interface, so this is non-invasive.
            let opened = dev.open().wait().ok();
            for (interface_number, info) in wanted {
                let usages = opened
                    .as_ref()
                    .and_then(|d| read_report_descriptor_unclaimed(d, interface_number))
                    .and_then(|bytes| ReportDescriptor::parse(&bytes).ok())
                    .map(|d| d.top_level_usages())
                    .unwrap_or_default();
                result.extend(info.per_usage(&usages));
            }
        }
        Ok(result)
    }

    /// Open a device by `usb:<bus>:<device-address>:<interface>` path, as
    /// reported by [`HidBackend::enumerate`].
    fn open_path(&self, path: &str) -> HidResult<NusbDevice> {
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

/// An open USB HID interface; behind a [`crate::HidDevice`] opened through
/// [`Backend::Nusb`](crate::Backend::Nusb).
///
/// Holding this claims the interface exclusively (detached from the kernel
/// driver on Linux); dropping it releases the interface, returning it to the
/// OS; a refused claim degrades the handle to control-only, losing only
/// interrupt reads. All methods take `&self`; the handle is `Send + Sync`.
pub(crate) struct NusbDevice {
    /// Keeps the device open; also used for string descriptor requests.
    device: nusb::Device,
    /// `None` on a control-only handle, whose transfers go through `device`.
    interface: Option<Interface>,
    interface_number: u8,
    /// Enumeration-style metadata captured at open time.
    info: DeviceInfo,
    /// Report descriptor read at open time; empty when unreadable.
    report_descriptor: Vec<u8>,
    /// Interrupt OUT endpoint; writes fall back to `SET_REPORT` without one.
    out_endpoint: Option<Mutex<Endpoint<Interrupt, Out>>>,
    queue: Arc<ReportQueue>,
    /// Interrupt IN reader thread; absent on interfaces with no such endpoint,
    /// which support only the control-transfer report calls.
    reader: Option<JoinHandle<()>>,
}

impl NusbDevice {
    fn open(dev_info: &nusb::DeviceInfo, interface_number: u8) -> HidResult<Self> {
        let device = dev_info.open().wait().map_err(|e| HidError::OpenFailed {
            message: format!("opening USB device: {e}"),
        })?;
        // Detaches the kernel driver on Linux; plain claim elsewhere.
        // macOS never releases keyboard/pointer interfaces, and ep0 needs no claim.
        let interface = match device.detach_and_claim_interface(interface_number).wait() {
            Ok(interface) => Some(interface),
            #[cfg(not(target_os = "windows"))]
            Err(_) => None,
            // WinUSB routes control transfers through the interface handle.
            #[cfg(target_os = "windows")]
            Err(e) => {
                return Err(HidError::OpenFailed {
                    message: format!("claiming interface {interface_number}: {e}"),
                })
            }
        };

        // Probe the report descriptor once: it determines report
        // ID usage / input sizes and backs `get_report_descriptor`.
        let report_descriptor = match interface.as_ref() {
            Some(interface) => read_report_descriptor(interface),
            None => read_report_descriptor_unclaimed(&device, interface_number),
        }
        .unwrap_or_default();
        let parsed = ReportDescriptor::parse(&report_descriptor).ok();
        let max_input_wire = parsed
            .as_ref()
            .map(|d| d.max_wire_size(ReportKind::Input))
            .unwrap_or(0);

        // Interrupt endpoints from alternate setting 0; both are optional. HID
        // 1.11 §4.4 mandates an interrupt IN endpoint, but control-only devices
        // that declare bNumEndpoints 0 exist in the wild and are perfectly
        // usable through GET_REPORT/SET_REPORT on the control pipe.
        let (in_endpoint, out_endpoint) = match interface.as_ref() {
            Some(interface) => {
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
                            Direction::In if in_address.is_none() => {
                                in_address = Some(ep.address())
                            }
                            Direction::Out if out_address.is_none() => {
                                out_address = Some(ep.address())
                            }
                            _ => {}
                        }
                    }
                }
                let in_endpoint: Option<Endpoint<Interrupt, In>> = in_address
                    .map(|address| {
                        interface.endpoint(address).map_err(|e| {
                            HidError::backend(format!("opening interrupt IN endpoint: {e}"))
                        })
                    })
                    .transpose()?;
                let out_endpoint = match out_address {
                    Some(address) => Some(Mutex::new(
                        interface.endpoint::<Interrupt, Out>(address).map_err(|e| {
                            HidError::backend(format!("opening interrupt OUT endpoint: {e}"))
                        })?,
                    )),
                    None => None,
                };
                (in_endpoint, out_endpoint)
            }
            None => (None, None),
        };

        let transfer_len = match in_endpoint.as_ref() {
            Some(endpoint) => {
                let max_packet_size = endpoint.max_packet_size();
                if max_packet_size == 0 {
                    return Err(HidError::backend(
                        "interrupt IN endpoint declares a zero wMaxPacketSize",
                    ));
                }
                transfer_length(max_input_wire, max_packet_size)
            }
            None => 0,
        };

        let mut info = device_info(dev_info, interface_number);
        if let Some((page, usage)) = parsed
            .as_ref()
            .and_then(|d| d.top_level_usages().first().copied())
        {
            info.usage_page = page;
            info.usage = usage;
        }

        let queue = Arc::new(ReportQueue::new(
            match (interface.is_some(), in_endpoint.is_some()) {
                (_, true) => "USB reader thread terminated",
                (true, false) => {
                    "this interface declares no interrupt IN endpoint; use get_input_report"
                }
                (false, false) => {
                    "interrupt reads need a claimed interface, and another driver holds this one"
                }
            },
        ));
        let reader = match in_endpoint {
            Some(in_endpoint) => {
                let queue = Arc::clone(&queue);
                Some(
                    std::thread::Builder::new()
                        .name("hidra-usb-read".into())
                        .spawn(move || reader_loop(in_endpoint, queue, transfer_len))
                        .map_err(|e| HidError::io("spawning USB reader thread", e))?,
                )
            }
            None => {
                // Nothing will ever queue an input report, so mark the queue shut
                // down at open time: reads then fail like a departed reader
                // thread instead of parking forever.
                queue.set_shutdown();
                None
            }
        };

        Ok(NusbDevice {
            device,
            interface,
            interface_number,
            info,
            report_descriptor,
            out_endpoint,
            queue,
            reader,
        })
    }

    /// Control transfer to this interface, through the claim if there is one, else ep0.
    fn control_out_req(&self, request: ControlOut<'_>) -> Result<(), TransferError> {
        match self.interface.as_ref() {
            Some(interface) => interface.control_out(request, TRANSFER_TIMEOUT).wait()?,
            None => device_control_out(&self.device, request)?,
        };
        Ok(())
    }

    /// [`control_out_req`](Self::control_out_req)'s IN counterpart.
    fn control_in_req(&self, request: ControlIn) -> Result<Vec<u8>, TransferError> {
        match self.interface.as_ref() {
            Some(interface) => interface.control_in(request, TRANSFER_TIMEOUT).wait(),
            None => device_control_in(&self.device, request),
        }
    }

    /// `GET_REPORT` shared by feature and input reports. `buf[0]` carries the
    /// report ID on entry; for ID 0 the returned data is written at
    /// `buf[1..]` so the ID stays in byte 0.
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
            .control_in_req(ControlIn {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: HID_GET_REPORT,
                value: (report_type << 8) | u16::from(report_number),
                index: u16::from(self.interface_number),
                length,
            })
            .map_err(|e| transfer_error(operation, e))?;
        let len = data.len().min(buf.len() - offset);
        buf[offset..offset + len].copy_from_slice(&data[..len]);
        Ok(len + offset)
    }
}

impl HidDeviceBackend for NusbDevice {
    type Read<'a> = ReadAsync<'a>;

    /// Send an output report. `data[0]` is the report ID; like hidapi's
    /// libusb backend, a 0 ID byte is stripped before transmission on both
    /// the interrupt and the `SET_REPORT` control path, while a nonzero ID
    /// is sent on the wire. Returns the original length on success.
    fn write(&self, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "write data must contain a report ID byte".into(),
            });
        }
        let report_number = data[0];
        let payload = payload_after_report_id(data);
        match &self.out_endpoint {
            Some(endpoint) => {
                let mut endpoint = endpoint.lock().unwrap();
                let completion =
                    endpoint.transfer_blocking(payload.to_vec().into(), TRANSFER_TIMEOUT);
                match completion.status {
                    // Report the caller's original length (report-ID byte
                    // included), matching the documented contract, hidapi, and
                    // the SET_REPORT path below. `actual_len` counts only the
                    // payload bytes the endpoint transferred, so it would
                    // under-report on a short transfer.
                    Ok(()) => Ok(data.len()),
                    Err(e) => Err(transfer_error("interrupt OUT write", e)),
                }
            }
            None => {
                // No interrupt OUT endpoint: use SET_REPORT(Output), like
                // hidapi.
                self.control_out_req(ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: HID_SET_REPORT,
                    value: (REPORT_TYPE_OUTPUT << 8) | u16::from(report_number),
                    index: u16::from(self.interface_number),
                    data: payload,
                })
                .map_err(|e| transfer_error("SET_REPORT (output)", e))?;
                Ok(data.len())
            }
        }
    }

    /// Read an input report asynchronously (hidra extension; hidapi has no
    /// async API).
    ///
    /// Resolves once a report queued by the reader thread has been copied
    /// into `buf`, returning its length, never `Ok(0)`; use your runtime's
    /// timeout combinator (e.g. `tokio::time::timeout`) to bound the wait.
    /// Fails with [`HidError::Disconnected`] when the device is removed and
    /// the queue has drained.
    ///
    /// The future is runtime-agnostic (plain `Waker` wake-ups, like nusb,
    /// works under tokio, async-std, smol or a hand-rolled executor) and
    /// cancel-safe: reports are only dequeued inside `poll`, so dropping it
    /// never loses input; pending reports stay queued for the next read.
    fn read_async<'a>(&'a self, buf: &'a mut [u8]) -> ReadAsync<'a> {
        ReadAsync {
            queue: &self.queue,
            buf,
        }
    }

    /// Send a feature report via `SET_REPORT(Feature)`. `data[0]` is the
    /// report ID; a 0 ID byte is stripped, like hidapi.
    fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "feature report must contain a report ID byte".into(),
            });
        }
        let report_number = data[0];
        let payload = payload_after_report_id(data);
        self.control_out_req(ControlOut {
            control_type: ControlType::Class,
            recipient: Recipient::Interface,
            request: HID_SET_REPORT,
            value: (REPORT_TYPE_FEATURE << 8) | u16::from(report_number),
            index: u16::from(self.interface_number),
            data: payload,
        })
        .map_err(|e| transfer_error("SET_REPORT (feature)", e))?;
        Ok(())
    }

    /// Get a feature report via `GET_REPORT(Feature)`. Set `buf[0]` to the
    /// report ID before calling.
    fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(REPORT_TYPE_FEATURE, buf, "GET_REPORT (feature)")
    }

    /// Get an input report synchronously via `GET_REPORT(Input)`. Same
    /// buffer convention as [`get_feature_report`](Self::get_feature_report).
    fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(REPORT_TYPE_INPUT, buf, "GET_REPORT (input)")
    }

    fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        Ok(self.info.manufacturer_string.clone())
    }

    fn get_product_string(&self) -> HidResult<Option<String>> {
        Ok(self.info.product_string.clone())
    }

    fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        Ok(self.info.serial_number.clone())
    }

    /// Read a string descriptor by index (US English), which only this
    /// backend supports, the native hidraw backend cannot.
    fn get_indexed_string(&self, index: u32) -> HidResult<Option<String>> {
        let index = u8::try_from(index)
            .ok()
            .and_then(NonZeroU8::new)
            .ok_or_else(|| HidError::InvalidData {
                message: "string descriptor index must be in 1..=255".into(),
            })?;
        let s = self
            .device
            .get_string_descriptor(index, US_ENGLISH, TRANSFER_TIMEOUT)
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
    fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
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
    fn get_device_info(&self) -> HidResult<DeviceInfo> {
        Ok(self.info.clone())
    }
}

impl Drop for NusbDevice {
    fn drop(&mut self) {
        self.queue.set_shutdown();
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
pub(crate) struct ReadAsync<'a> {
    queue: &'a ReportQueue,
    buf: &'a mut [u8],
}

impl Future for ReadAsync<'_> {
    type Output = HidResult<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.queue.poll_read(this.buf, cx)
    }
}

/// Reader thread: keeps one interrupt IN transfer pending and queues
/// completed reports, mirroring hidapi's `read_callback` loop.
fn reader_loop(
    mut endpoint: Endpoint<Interrupt, In>,
    queue: Arc<ReportQueue>,
    transfer_len: usize,
) {
    let buf = endpoint.allocate(transfer_len);
    endpoint.submit(buf);
    while !queue.is_shutdown() {
        let Some(completion) = endpoint.wait_next_complete(READER_POLL_INTERVAL) else {
            continue;
        };
        match completion.status {
            Ok(()) => {
                let len = completion.actual_len.min(completion.buffer.len());
                if len > 0 {
                    queue.push(completion.buffer[..len].to_vec());
                }
                endpoint.submit(completion.buffer);
            }
            Err(TransferError::Disconnected) => {
                queue.set_disconnected();
                break;
            }
            Err(TransferError::Cancelled) => break,
            // Transient conditions (stall, fault): resubmit.
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
    queue.set_shutdown();
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

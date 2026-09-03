//! `WebUSB` backend, for devices the browser will not surface over `WebHID`.
//!
//! `WebHID` only exposes interfaces the host recognises as HID. A device whose
//! interface declares a vendor-specific class is invisible there, but remains
//! reachable over `WebUSB`, and many such devices still speak the HID protocol
//! on the wire. This backend drives them through `nusb`'s `WebUSB` platform,
//! mapping hidra's report calls onto the same control and interrupt transfers
//! the native `nusb` backend uses.
//!
//! Blink refuses `claimInterface` on the protected classes, HID (0x03) among
//! them, so this backend deliberately does not filter for HID interfaces the
//! way the native `nusb` backend does: the devices it is for are precisely the
//! ones declaring something else.

use core::cell::RefCell;
use std::time::Duration;

use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, ControlIn, ControlOut, ControlType, Direction, Recipient};
use nusb::transfer::{In, Interrupt, Out};
use nusb::{Endpoint, Interface};

use super::payload_after_report_id;
use crate::descriptor::{ReportDescriptor, ReportKind};
use crate::error::{HidError, HidResult};
use crate::{BusType, DeviceInfo, MAX_REPORT_DESCRIPTOR_SIZE};

const GET_DESCRIPTOR: u8 = 0x06;
const DESCRIPTOR_TYPE_HID_REPORT: u8 = 0x22;
const HID_GET_REPORT: u8 = 0x01;
const HID_SET_REPORT: u8 = 0x09;

const REPORT_TYPE_INPUT: u16 = 1;
const REPORT_TYPE_OUTPUT: u16 = 2;
const REPORT_TYPE_FEATURE: u16 = 3;

const TRANSFER_TIMEOUT: Duration = Duration::from_millis(1000);

/// Cap on a single interrupt IN transfer, matching the native backend.
fn transfer_length(max_input_wire: usize, max_packet_size: usize) -> usize {
    max_input_wire.max(max_packet_size).max(1)
}

fn transfer_error(operation: &'static str, err: nusb::transfer::TransferError) -> HidError {
    match err {
        nusb::transfer::TransferError::Disconnected => HidError::Disconnected,
        e => HidError::backend(format!("{operation}: {e}")),
    }
}

fn device_info(dev: &nusb::DeviceInfo, interface_number: u8) -> DeviceInfo {
    DeviceInfo {
        path: format!(
            "webusb:{:04x}:{:04x}:{interface_number}",
            dev.vendor_id(),
            dev.product_id()
        ),
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

/// Entry point: `navigator.usb`, reached through `nusb`.
pub(crate) struct WebUsbApi;

impl WebUsbApi {
    pub(crate) fn new() -> HidResult<Self> {
        Ok(WebUsbApi)
    }

    /// Devices the user has already granted access to.
    pub(crate) async fn device_list(&self) -> HidResult<Vec<nusb::DeviceInfo>> {
        let devices = nusb::list_devices()
            .await
            .map_err(|e| HidError::backend(format!("navigator.usb.getDevices: {e}")))?;
        Ok(devices.collect())
    }

    /// Show the browser's device chooser, resolving with the granted device.
    ///
    /// `selectors` narrows what the chooser offers; an empty slice offers
    /// everything. Resolves with `None` when the user dismisses the chooser.
    pub(crate) async fn request_device(
        &self,
        selectors: &[nusb::DeviceSelector],
    ) -> HidResult<Option<nusb::DeviceInfo>> {
        nusb::request_device(selectors)
            .await
            .map_err(|e| HidError::backend(format!("navigator.usb.requestDevice: {e}")))
    }
}

/// One open device, claimed on a single interface.
pub(crate) struct WebUsbDevice {
    interface: Interface,
    interface_number: u8,
    info: DeviceInfo,
    report_descriptor: Vec<u8>,
    in_endpoint: Option<RefCell<Option<Endpoint<Interrupt, In>>>>,
    out_endpoint: Option<RefCell<Option<Endpoint<Interrupt, Out>>>>,
    transfer_len: usize,
}

impl WebUsbDevice {
    /// Claim `interface_number` and probe its endpoints and report descriptor.
    ///
    /// When `interface_number` is `None` the first interface carrying an
    /// interrupt IN endpoint is used, falling back to the first interface.
    pub(crate) async fn open(
        dev_info: &nusb::DeviceInfo,
        interface_number: Option<u8>,
    ) -> HidResult<Self> {
        let interface_number = match interface_number {
            Some(n) => n,
            None => dev_info
                .interfaces()
                .next()
                .map(|i| i.interface_number())
                .ok_or_else(|| HidError::OpenFailed {
                    message: "device exposes no interfaces".into(),
                })?,
        };

        let device = dev_info.open().await.map_err(|e| HidError::OpenFailed {
            message: format!("opening USB device: {e}"),
        })?;
        let interface = device
            .claim_interface(interface_number)
            .await
            .map_err(|e| HidError::OpenFailed {
                message: format!(
                    "claiming interface {interface_number}: {e}; \
                     the browser refuses the protected classes, HID among them"
                ),
            })?;

        let report_descriptor = read_report_descriptor(&interface, interface_number)
            .await
            .unwrap_or_default();
        let parsed = ReportDescriptor::parse(&report_descriptor).ok();
        let max_input_wire = parsed
            .as_ref()
            .map(|d| d.max_wire_size(ReportKind::Input))
            .unwrap_or(0);

        let mut in_address = None;
        let mut out_address = None;
        if let Some(desc) = interface
            .descriptors()
            .find(|d| d.alternate_setting() == 0)
            .or_else(|| interface.descriptor())
        {
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

        let in_endpoint = in_address
            .map(|address| {
                interface
                    .endpoint::<Interrupt, In>(address)
                    .map_err(|e| HidError::backend(format!("opening interrupt IN endpoint: {e}")))
            })
            .transpose()?;
        let out_endpoint = out_address
            .map(|address| {
                interface
                    .endpoint::<Interrupt, Out>(address)
                    .map_err(|e| HidError::backend(format!("opening interrupt OUT endpoint: {e}")))
            })
            .transpose()?;

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

        Ok(WebUsbDevice {
            interface,
            interface_number,
            info,
            report_descriptor,
            in_endpoint: in_endpoint.map(|e| RefCell::new(Some(e))),
            out_endpoint: out_endpoint.map(|e| RefCell::new(Some(e))),
            transfer_len,
        })
    }

    /// Send an output report: interrupt OUT when the interface has one,
    /// otherwise `SET_REPORT(Output)` on the control pipe.
    pub(crate) async fn write(&self, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "output report must contain a report ID byte".into(),
            });
        }
        let report_number = data[0];
        let payload = payload_after_report_id(data);

        if let Some(cell) = self.out_endpoint.as_ref() {
            let mut endpoint = take_endpoint(cell)?;
            let mut buf = Buffer::new(payload.len());
            buf.extend_from_slice(payload);
            endpoint.submit(buf);
            let completion = endpoint.next_complete().await;
            *cell.borrow_mut() = Some(endpoint);
            completion
                .status
                .map_err(|e| transfer_error("interrupt OUT", e))?;
            return Ok(completion.actual_len + usize::from(report_number == 0));
        }

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
                TRANSFER_TIMEOUT,
            )
            .await
            .map_err(|e| transfer_error("SET_REPORT (output)", e))?;
        Ok(data.len())
    }

    /// Resolve with one input report from the interrupt IN endpoint.
    pub(crate) async fn read(&self, buf: &mut [u8]) -> HidResult<usize> {
        let cell = self
            .in_endpoint
            .as_ref()
            .ok_or_else(|| HidError::Unsupported {
                message: "interface has no interrupt IN endpoint; use get_input_report".into(),
            })?;
        let mut endpoint = take_endpoint(cell)?;
        endpoint.submit(Buffer::new(self.transfer_len));
        let completion = endpoint.next_complete().await;
        *cell.borrow_mut() = Some(endpoint);
        completion
            .status
            .map_err(|e| transfer_error("interrupt IN", e))?;
        let data = &completion.buffer[..completion.actual_len];
        let len = data.len().min(buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok(len)
    }

    /// `GET_REPORT`, shared by the feature and input paths. `buf[0]` carries
    /// the report ID on entry; for ID 0 the data lands at `buf[1..]`.
    async fn get_report(
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
                TRANSFER_TIMEOUT,
            )
            .await
            .map_err(|e| transfer_error(operation, e))?;
        let len = data.len().min(buf.len() - offset);
        buf[offset..offset + len].copy_from_slice(&data[..len]);
        Ok(len + offset)
    }

    pub(crate) async fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "feature report must contain a report ID byte".into(),
            });
        }
        let report_number = data[0];
        let payload = payload_after_report_id(data);
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
                TRANSFER_TIMEOUT,
            )
            .await
            .map_err(|e| transfer_error("SET_REPORT (feature)", e))?;
        Ok(())
    }

    pub(crate) async fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(REPORT_TYPE_FEATURE, buf, "GET_REPORT (feature)")
            .await
    }

    pub(crate) async fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.get_report(REPORT_TYPE_INPUT, buf, "GET_REPORT (input)")
            .await
    }

    pub(crate) fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
        if self.report_descriptor.is_empty() {
            return Err(HidError::Unsupported {
                message: "the interface exposes no HID report descriptor".into(),
            });
        }
        let len = self.report_descriptor.len().min(buf.len());
        buf[..len].copy_from_slice(&self.report_descriptor[..len]);
        Ok(len)
    }

    pub(crate) fn device_info(&self) -> DeviceInfo {
        self.info.clone()
    }

    pub(crate) fn raw(&self) -> &Interface {
        &self.interface
    }
}

/// `GET_DESCRIPTOR(Report)`; `None` on a vendor-class interface with no HID
/// class descriptor, which is the common case for this backend.
async fn read_report_descriptor(interface: &Interface, interface_number: u8) -> Option<Vec<u8>> {
    interface
        .control_in(
            ControlIn {
                control_type: ControlType::Standard,
                recipient: Recipient::Interface,
                request: GET_DESCRIPTOR,
                value: u16::from(DESCRIPTOR_TYPE_HID_REPORT) << 8,
                index: u16::from(interface_number),
                length: MAX_REPORT_DESCRIPTOR_SIZE as u16,
            },
            TRANSFER_TIMEOUT,
        )
        .await
        .ok()
        .filter(|d| !d.is_empty())
}

/// Take an endpoint out of its cell for the duration of a transfer.
///
/// wasm is single-threaded but futures still interleave, so a second transfer
/// started while one is in flight finds the cell empty and is refused rather
/// than aliasing the endpoint.
fn take_endpoint<T>(cell: &RefCell<Option<T>>) -> HidResult<T> {
    cell.borrow_mut()
        .take()
        .ok_or_else(|| HidError::backend("a transfer is already in flight on this endpoint"))
}

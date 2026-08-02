#!/usr/bin/env python3
"""Facedancer HID device for the hidra conformance suite — emulates a *real*
USB HID device (Cynthion + Facedancer gateware) with the shared conformance
identity (VID 0x1209 / PID 0x000c, "hidra-conformance").

A TCP control server lets the Rust harness drive the device side:

    inject <hex>     input report streamed on interrupt IN
    prime  <hex>     payload returned for GET_REPORT (feature/input)
    output?          -> hex of last output report received (or "none")
    setfeature?      -> hex of last SET_REPORT (feature) received (or "none")
    reset            clear last_output / last_set_feature

Run (Cynthion in Facedancer mode):
    python3 device.py            # control server on 0.0.0.0:9999
    HIDRA_CTRL_HOST=0.0.0.0 HIDRA_CTRL_PORT=9999 python3 device.py
"""

import os
import threading
import socketserver

from facedancer import main
from facedancer import (
    USBDevice,
    USBConfiguration,
    USBInterface,
    USBEndpoint,
    USBDescriptor,
    USBDirection,
    USBTransferType,
    USBDescriptorTypeNumber,
)
from facedancer import use_inner_classes_automatically
from facedancer.request import class_request_handler, to_this_interface

VID = 0x1209
PID = 0x000C

# HIDRA_CYNTHION_NUMBERED=1 -> numbered descriptor variant (IDs 0x11/0x22/0x33);
# the Rust side reads the same env var so both agree.
NUMBERED = bool(os.environ.get("HIDRA_CYNTHION_NUMBERED"))
RID_INPUT, RID_OUTPUT, RID_FEATURE = 0x11, 0x22, 0x33

# make_descriptor(numbered=false): vendor page 0xFF00, 8-byte input/output/feature.
REPORT_DESC_UNNUMBERED = bytes([
    0x06, 0x00, 0xFF,  # Usage Page (Vendor 0xFF00)
    0x09, 0x01,        # Usage 0x01
    0xA1, 0x01,        # Collection (Application)
    0x14,              #   Logical Minimum 0 (1-byte short form, as hidra emits)
    0x26, 0xFF, 0x00,  #   Logical Maximum 255
    0x75, 0x08,        #   Report Size 8
    0x95, 0x08,        #   Report Count 8
    0x09, 0x02, 0x81, 0x02,  #   Usage 0x02, Input (Var)
    0x09, 0x03, 0x91, 0x02,  #   Usage 0x03, Output (Var)
    0x09, 0x04, 0xB1, 0x02,  #   Usage 0x04, Feature (Var)
    0xC0,              # End Collection
])

# make_descriptor(numbered=true): same, with a Report ID (0x85 <id>) before each
# of the Input/Output/Feature items.
REPORT_DESC_NUMBERED = bytes([
    0x06, 0x00, 0xFF,  # Usage Page (Vendor 0xFF00)
    0x09, 0x01,        # Usage 0x01
    0xA1, 0x01,        # Collection (Application)
    0x14,              #   Logical Minimum 0
    0x26, 0xFF, 0x00,  #   Logical Maximum 255
    0x75, 0x08,        #   Report Size 8
    0x95, 0x08,        #   Report Count 8
    0x85, RID_INPUT, 0x09, 0x02, 0x81, 0x02,    #   Report ID, Usage 0x02, Input
    0x85, RID_OUTPUT, 0x09, 0x03, 0x91, 0x02,   #   Report ID, Usage 0x03, Output
    0x85, RID_FEATURE, 0x09, 0x04, 0xB1, 0x02,  #   Report ID, Usage 0x04, Feature
    0xC0,              # End Collection
])

REPORT_DESC = REPORT_DESC_NUMBERED if NUMBERED else REPORT_DESC_UNNUMBERED

state = {
    "input": bytes([0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7]),
    "feature": bytes([0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7]),
    "last_output": None,
    "last_setfeature": None,
}


@use_inner_classes_automatically
class ConformanceHID(USBDevice):
    name: str = "hidra conformance HID"
    vendor_id: int = VID
    product_id: int = PID
    manufacturer_string: str = "hidra"
    product_string: str = "hidra-conformance"
    serial_number_string: str = "HIDRA-CONF-01"
    device_revision: int = 0x0100
    usb_spec_version: int = 0x0200

    class Cfg(USBConfiguration):
        class Iface(USBInterface):
            name: str = "hidra HID interface"
            class_number: int = 3  # HID

            class InEp(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.IN
                transfer_type: USBTransferType = USBTransferType.INTERRUPT
                # A numbered report (8 data + report-ID) would overflow an 8-byte
                # endpoint and wedge the gadget, so size to 9 when numbered.
                max_packet_size: int = 9 if NUMBERED else 8
                interval: int = 8

            class HIDInfo(USBDescriptor):
                number: int = 0
                type_number: int = USBDescriptorTypeNumber.HID
                include_in_config: bool = True
                # HID desc: bcdHID 1.11, country 0, 1 descriptor, report(0x22) len
                raw: bytes = bytes([
                    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22,
                    len(REPORT_DESC) & 0xFF, (len(REPORT_DESC) >> 8) & 0xFF,
                ])

            class ReportDesc(USBDescriptor):
                number: int = 0
                type_number: int = 0x22  # HID REPORT descriptor
                raw: bytes = REPORT_DESC

            def handle_data_requested(self, endpoint: USBEndpoint):
                endpoint.send(state["input"])

            # GET_REPORT returns the primed report verbatim: [report-ID|body] when
            # numbered, body when unnumbered. The host stack takes the report-ID
            # from the data (numbered) or prepends 0x00 (unnumbered).
            @class_request_handler(number=1)
            @to_this_interface
            def handle_get_report(self, request):
                request.reply(state["feature"])

            # SET_IDLE / SET_PROTOCOL: ack so hosts (esp. Windows) don't stall during HID setup.
            @class_request_handler(number=0x0A)
            @to_this_interface
            def handle_set_idle(self, request):
                request.ack()

            @class_request_handler(number=0x0B)
            @to_this_interface
            def handle_set_protocol(self, request):
                request.ack()

            # SET_REPORT: record output (type 2) / feature (type 3).
            @class_request_handler(number=9)
            @to_this_interface
            def handle_set_report(self, request):
                report_type = (request.value >> 8) & 0xFF
                data = bytes(request.data)
                if report_type == 2:
                    state["last_output"] = data
                elif report_type == 3:
                    state["last_setfeature"] = data
                request.ack()


class ControlHandler(socketserver.StreamRequestHandler):
    def handle(self):
        for raw in self.rfile:
            line = raw.decode(errors="replace").strip()
            if not line:
                continue
            parts = line.split()
            cmd = parts[0]
            arg = parts[1] if len(parts) > 1 else ""
            if cmd == "inject":
                state["input"] = bytes.fromhex(arg)
                self.wfile.write(b"ok\n")
            elif cmd == "prime":
                state["feature"] = bytes.fromhex(arg)
                self.wfile.write(b"ok\n")
            elif cmd == "output?":
                o = state["last_output"]
                self.wfile.write(((o.hex() if o else "none") + "\n").encode())
            elif cmd == "setfeature?":
                s = state["last_setfeature"]
                self.wfile.write(((s.hex() if s else "none") + "\n").encode())
            elif cmd == "reset":
                state["last_output"] = None
                state["last_setfeature"] = None
                self.wfile.write(b"ok\n")
            elif cmd == "disconnect":
                # Drop the device so hidra's pending read observes removal (-> Disconnected).
                d = state.get("device")
                try:
                    if d is not None:
                        d.disconnect()
                except Exception as e:  # noqa: BLE001
                    print(f"[control] disconnect error: {e}", flush=True)
                self.wfile.write(b"ok\n")
            elif cmd == "reconnect":
                # Re-present the device so a later pass can re-open it.
                d = state.get("device")
                try:
                    if d is not None:
                        d.connect()
                except Exception as e:  # noqa: BLE001
                    print(f"[control] reconnect error: {e}", flush=True)
                self.wfile.write(b"ok\n")
            else:
                self.wfile.write(b"err\n")
            self.wfile.flush()


def start_control_server(host, port):
    socketserver.TCPServer.allow_reuse_address = True
    srv = socketserver.ThreadingTCPServer((host, port), ControlHandler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    print(f"[control] listening on {host}:{port}", flush=True)


if __name__ == "__main__":
    host = os.environ.get("HIDRA_CTRL_HOST", "0.0.0.0")
    port = int(os.environ.get("HIDRA_CTRL_PORT", "9999"))
    start_control_server(host, port)
    # Stored in state so the control server can drive disconnect/reconnect.
    device = ConformanceHID()
    state["device"] = device
    main(device)

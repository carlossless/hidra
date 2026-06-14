//! Windows backend: HID class devices via `hid.dll` and SetupAPI.
//!
//! Mirrors hidapi's `windows/hid.c`: enumeration walks the HID device
//! interface class with SetupAPI, devices are opened overlapped and all I/O
//! goes through one persistent background `ReadFile` plus event-driven
//! `WriteFile`/`DeviceIoControl` calls.
//!
//! Known deviations from hidapi, each documented at the relevant method:
//!
//! * the bus type is classified from the devnode's enumerator/hardware IDs
//!   instead of `DEVPKEY_Device_BusTypeGuid`;
//! * `get_report_descriptor` reconstructs the descriptor from the documented
//!   HidP API rather than the undocumented preparsed-data layout;
//! * a timed-out `write` cancels the pending I/O before returning (hidapi
//!   leaves it running), which is required for memory safety in Rust.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDeviceInterfaceW,
    CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, MAX_DEVICE_ID_LEN,
    SETUP_DI_REGISTRY_PROPERTY, SPDRP_COMPATIBLEIDS, SPDRP_ENUMERATOR_NAME, SPDRP_HARDWAREID,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetIndexedString,
    HidD_GetInputReport, HidD_GetManufacturerString, HidD_GetPreparsedData, HidD_GetProductString,
    HidD_GetSerialNumberString, HidD_SetFeature, HidD_SetNumInputBuffers, HidP_Feature,
    HidP_GetButtonCaps, HidP_GetCaps, HidP_GetLinkCollectionNodes, HidP_GetValueCaps, HidP_Input,
    HidP_Output, HIDD_ATTRIBUTES, HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_LINK_COLLECTION_NODE,
    HIDP_REPORT_TYPE, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_ContainerId, DEVPROPTYPE, DEVPROP_TYPE_GUID,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_DEVICE_NOT_CONNECTED,
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_FUNCTION, ERROR_IO_INCOMPLETE,
    ERROR_IO_PENDING, ERROR_NOT_SUPPORTED, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, RegisterWaitForSingleObject, ResetEvent, SetEvent, UnregisterWaitEx,
    WaitForSingleObject, INFINITE, WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};

use crate::descriptor::{CollectionKind, DescriptorBuilder, MainFlags};
use crate::error::{HidError, HidResult};
use crate::{BusType, DeviceInfo};

/// `hid_winapi_set_write_timeout` default (hidapi uses 1000 ms too).
const DEFAULT_WRITE_TIMEOUT_MS: u32 = 1000;

// DDK ioctls not exported by windows-sys. CTL_CODE(FILE_DEVICE_KEYBOARD=0x0B,
// function, METHOD_OUT_DIRECT=2, FILE_ANY_ACCESS=0):
//   (0x0B << 16) | (function << 2) | 2
/// `IOCTL_HID_GET_FEATURE` (function 100).
const IOCTL_HID_GET_FEATURE: u32 = 0x000B_0192;
/// `IOCTL_HID_GET_INPUT_REPORT` (function 104).
const IOCTL_HID_GET_INPUT_REPORT: u32 = 0x000B_01A2;

/// Number of UTF-16 code units used for `HidD_Get*String` buffers.
const MAX_STRING_WCHARS: usize = 256;

// --- small helpers ------------------------------------------------------------

/// NUL-terminated UTF-16 for Win32 `W` APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Convert a NUL-terminated UTF-16 buffer to a `String`; empty becomes `None`.
fn utf16_until_nul(buf: &[u16]) -> Option<String> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// Parse the USB interface number out of a device interface path
/// (`...&mi_03#...`).
///
/// Composite USB devices carry an `&mi_XX` token per interface. Non-composite
/// USB devices omit it: Windows only adds `MI_` for multi-interface devices.
/// hidapi reports `-1` in that case, but Linux and macOS report the real
/// interface number (`0`, from `bInterfaceNumber`); we match the latter so a
/// single-interface USB device reads the same `interface_number` on every
/// platform. `bus_type` distinguishes a non-composite USB device (→ 0) from a
/// genuinely interface-less transport like Bluetooth (→ -1).
fn interface_number_from_path(path: &str, bus_type: BusType) -> i32 {
    let lower = path.to_ascii_lowercase();
    let Some(pos) = lower.find("&mi_") else {
        return if bus_type == BusType::Usb { 0 } else { -1 };
    };
    let hex: String = lower[pos + 4..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.is_empty() {
        return if bus_type == BusType::Usb { 0 } else { -1 };
    }
    i32::from_str_radix(&hex, 16).unwrap_or(-1)
}

/// A GUID as 16 bytes in its in-memory (little-endian) Windows layout:
/// `Data1`/`Data2`/`Data3` little-endian followed by `Data4` verbatim.
fn guid_to_bytes(guid: &GUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&guid.data1.to_le_bytes());
    out[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    out[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    out[8..16].copy_from_slice(&guid.data4);
    out
}

/// Classify the bus from the devnode's enumerator name plus hardware /
/// compatible IDs.
///
/// hidapi inspects `DEVPKEY_Device_CompatibleIds` of the *parent* devnode;
/// this heuristic uses the same markers but reads them from the HID devnode
/// itself, which SetupAPI hands us for free during enumeration:
///
/// * enumerator `USB` (or a `USB` hardware id)        -> USB
/// * enumerator `BTHENUM` / `BTHLEDEVICE` (or id)     -> Bluetooth (incl. BLE)
/// * ACPI/I2C HIDs carry the `PNP0C50` compatible id  -> I2C
/// * SPI HIDs carry the `PNP0C51` compatible id       -> SPI
fn classify_bus(enumerator: &str, ids: &str) -> BusType {
    let enumerator = enumerator.to_ascii_uppercase();
    let ids = ids.to_ascii_uppercase();
    if ids.contains("PNP0C50") {
        return BusType::I2c;
    }
    if ids.contains("PNP0C51") {
        return BusType::Spi;
    }
    if enumerator == "USB" || ids.contains("USB") {
        return BusType::Usb;
    }
    if enumerator.starts_with("BTH") || ids.contains("BTHENUM") || ids.contains("BTHLEDEVICE") {
        return BusType::Bluetooth;
    }
    BusType::Unknown
}

// --- RAII wrappers --------------------------------------------------------------

/// Owned Win32 handle, closed on drop. The raw value is a pointer, but every
/// API taking it is thread-safe, hence the manual `Send`/`Sync`.
struct Handle(HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: handle is owned and only closed here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Non-owning copy of a Win32 handle for use from other threads.
///
/// SAFETY (of the impls): the raw value is a pointer-sized kernel handle;
/// every API it is passed to (`SetEvent`, the wait APIs) is thread-safe, and
/// the owner of the handle outlives all users (see [`ReadWake`]).
#[derive(Clone, Copy)]
struct RawHandle(HANDLE);

unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

/// Heap-pinned `OVERLAPPED` plus its auto-reset event.
///
/// The kernel keeps a pointer to the `OVERLAPPED` while an operation is in
/// flight; the `Box` keeps its address stable even when the owning
/// `WinDevice` moves.
struct OverlappedIo {
    ol: Box<OVERLAPPED>,
    event: Handle,
}

// SAFETY: the OVERLAPPED is only touched under the Mutex that owns this.
unsafe impl Send for OverlappedIo {}

impl OverlappedIo {
    fn new() -> HidResult<Self> {
        // Auto-reset, initially non-signaled, unnamed, like hidapi.
        // SAFETY: CreateEventW with null attributes/name is always valid.
        let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        if event.is_null() {
            return Err(HidError::last_os_error("CreateEventW"));
        }
        let mut ol: Box<OVERLAPPED> = Box::new(unsafe { core::mem::zeroed() });
        ol.hEvent = event;
        Ok(OverlappedIo {
            ol,
            event: Handle(event),
        })
    }

    fn ol_mut(&mut self) -> *mut OVERLAPPED {
        &mut *self.ol
    }
}

/// SetupAPI device information set, destroyed on drop.
struct DevInfoList(HDEVINFO);

impl Drop for DevInfoList {
    fn drop(&mut self) {
        // SAFETY: the list handle is owned and only destroyed here.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// `HidD_GetPreparsedData` result, freed on drop.
struct PreparsedData(PHIDP_PREPARSED_DATA);

impl PreparsedData {
    fn get(handle: HANDLE) -> HidResult<Self> {
        let mut pp: PHIDP_PREPARSED_DATA = 0;
        // SAFETY: handle is an open HID device handle.
        if !unsafe { HidD_GetPreparsedData(handle, &mut pp) } {
            return Err(HidError::last_os_error("HidD_GetPreparsedData"));
        }
        Ok(PreparsedData(pp))
    }

    fn caps(&self) -> HidResult<HIDP_CAPS> {
        let mut caps = HIDP_CAPS::default();
        // SAFETY: self.0 is valid preparsed data.
        if unsafe { HidP_GetCaps(self.0, &mut caps) } != HIDP_STATUS_SUCCESS {
            return Err(HidError::backend("HidP_GetCaps failed"));
        }
        Ok(caps)
    }
}

impl Drop for PreparsedData {
    fn drop(&mut self) {
        // SAFETY: the preparsed data block is owned and only freed here.
        unsafe { HidD_FreePreparsedData(self.0) };
    }
}

// --- SetupAPI plumbing ----------------------------------------------------------

/// Read a registry property of a devnode (two-call protocol), as raw UTF-16.
/// `REG_MULTI_SZ` properties keep their embedded NULs.
fn registry_property(
    list: HDEVINFO,
    devinfo: &SP_DEVINFO_DATA,
    property: SETUP_DI_REGISTRY_PROPERTY,
) -> Option<Vec<u16>> {
    let mut required = 0u32;
    // SAFETY: list/devinfo come from live SetupAPI calls; sizing call.
    unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            list,
            devinfo,
            property,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required == 0 {
        return None;
    }
    let mut buf = vec![0u16; (required as usize).div_ceil(2)];
    // SAFETY: buf holds at least `required` bytes.
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            list,
            devinfo,
            property,
            core::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            required,
            core::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(buf)
}

/// Device instance ID of a devnode, e.g. `HID\VID_05AC&PID_024F&...` or
/// `USB\VID_05AC&PID_024F\...`. The leading token is the enumerator.
fn device_instance_id(devinst: u32) -> Option<String> {
    let mut buf = vec![0u16; MAX_DEVICE_ID_LEN as usize + 1];
    // SAFETY: buf has room for MAX_DEVICE_ID_LEN chars plus a NUL.
    let cr = unsafe { CM_Get_Device_IDW(devinst, buf.as_mut_ptr(), buf.len() as u32, 0) };
    (cr == CR_SUCCESS).then(|| utf16_until_nul(&buf)).flatten()
}

/// Bus type of the devnode backing a device interface.
///
/// HID devices enumerate under the `HID` enumerator, so the HID devnode's own
/// IDs never carry the `USB`/`BTH` markers [`classify_bus`] looks for, they
/// live on an ancestor (the USB device is the grandparent of a HID
/// collection). We therefore classify the devnode and, while the result is
/// inconclusive, walk up parents via cfgmgr32 and classify their instance IDs.
/// This matches what hidapi reports for HID-over-USB/Bluetooth devices.
fn bus_type_for_devnode(list: HDEVINFO, devinfo: &SP_DEVINFO_DATA) -> BusType {
    let enumerator = registry_property(list, devinfo, SPDRP_ENUMERATOR_NAME)
        .and_then(|b| utf16_until_nul(&b))
        .unwrap_or_default();
    // Hardware + compatible IDs are REG_MULTI_SZ; a lossy conversion with the
    // embedded NULs intact is fine for substring matching.
    let mut ids = String::new();
    for prop in [SPDRP_HARDWAREID, SPDRP_COMPATIBLEIDS] {
        if let Some(b) = registry_property(list, devinfo, prop) {
            ids.push_str(&String::from_utf16_lossy(&b));
            ids.push(' ');
        }
    }
    let bus = classify_bus(&enumerator, &ids);
    if bus != BusType::Unknown {
        return bus;
    }

    // Walk ancestors: their instance ID's enumerator token (e.g. `USB`) is the
    // real transport. Bounded to avoid looping on a malformed devnode tree.
    let mut devinst = devinfo.DevInst;
    for _ in 0..8 {
        let mut parent = 0u32;
        // SAFETY: devinst is a live devnode handle from SetupAPI/cfgmgr32.
        if unsafe { CM_Get_Parent(&mut parent, devinst, 0) } != CR_SUCCESS {
            break;
        }
        if let Some(id) = device_instance_id(parent) {
            let enumerator = id.split('\\').next().unwrap_or("");
            let bus = classify_bus(enumerator, &id);
            if bus != BusType::Unknown {
                return bus;
            }
        }
        devinst = parent;
    }
    BusType::Unknown
}

/// Locate the devnode backing `path` in a fresh device information set.
/// Returns the set (kept alive for follow-up property queries) and the
/// devnode data.
fn devnode_for_interface(path: &str) -> HidResult<(DevInfoList, SP_DEVINFO_DATA)> {
    // SAFETY: creating an empty device info list has no preconditions.
    let raw = unsafe { SetupDiCreateDeviceInfoList(core::ptr::null(), core::ptr::null_mut()) };
    if raw == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(HidError::last_os_error("SetupDiCreateDeviceInfoList"));
    }
    let list = DevInfoList(raw);

    let wpath = wide(path);
    let mut iface = SP_DEVICE_INTERFACE_DATA {
        cbSize: core::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: wpath is NUL terminated, iface.cbSize is initialized.
    if unsafe { SetupDiOpenDeviceInterfaceW(list.0, wpath.as_ptr(), 0, &mut iface) } == 0 {
        return Err(HidError::last_os_error("SetupDiOpenDeviceInterfaceW"));
    }

    // The detail call fails with ERROR_INSUFFICIENT_BUFFER when no output
    // buffer is given, but still fills the devnode data, the documented way
    // to map an interface to its devnode.
    let mut devinfo = SP_DEVINFO_DATA {
        cbSize: core::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    let mut required = 0u32;
    // SAFETY: iface comes from the call above; sizing call with devinfo out.
    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list.0,
            &iface,
            core::ptr::null_mut(),
            0,
            &mut required,
            &mut devinfo,
        )
    };
    if ok == 0 && unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return Err(HidError::last_os_error("SetupDiGetDeviceInterfaceDetailW"));
    }
    Ok((list, devinfo))
}

/// Open a device interface path with the requested access rights, always
/// shared read/write and overlapped, like hidapi's `open_device`.
fn open_interface(path: &str, access: u32) -> Result<Handle, u32> {
    let wpath = wide(path);
    // SAFETY: wpath is NUL terminated; null security attributes/template.
    let handle = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(unsafe { GetLastError() });
    }
    Ok(Handle(handle))
}

/// Build a `DeviceInfo` for an open handle, mirroring what hidapi gathers at
/// enumeration time.
fn query_device_info(handle: HANDLE, path: &str, bus_type: BusType) -> DeviceInfo {
    let mut info = DeviceInfo {
        path: path.to_string(),
        interface_number: interface_number_from_path(path, bus_type),
        bus_type,
        ..Default::default()
    };

    let mut attrs = HIDD_ATTRIBUTES {
        Size: core::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    // SAFETY: handle is an open HID device handle, attrs.Size is initialized.
    if unsafe { HidD_GetAttributes(handle, &mut attrs) } {
        info.vendor_id = attrs.VendorID;
        info.product_id = attrs.ProductID;
        info.release_number = attrs.VersionNumber;
    }

    let mut buf = [0u16; MAX_STRING_WCHARS];
    let byte_len = (MAX_STRING_WCHARS * 2) as u32;
    // SAFETY: buf holds byte_len bytes; failures leave the field None.
    unsafe {
        if HidD_GetSerialNumberString(handle, buf.as_mut_ptr().cast(), byte_len) {
            info.serial_number = utf16_until_nul(&buf);
        }
        buf = [0u16; MAX_STRING_WCHARS];
        if HidD_GetManufacturerString(handle, buf.as_mut_ptr().cast(), byte_len) {
            info.manufacturer_string = utf16_until_nul(&buf);
        }
        buf = [0u16; MAX_STRING_WCHARS];
        if HidD_GetProductString(handle, buf.as_mut_ptr().cast(), byte_len) {
            info.product_string = utf16_until_nul(&buf);
        }
    }

    if let Ok(pp) = PreparsedData::get(handle) {
        if let Ok(caps) = pp.caps() {
            info.usage_page = caps.UsagePage;
            info.usage = caps.Usage;
        }
    }
    info
}

// --- backend API -------------------------------------------------------------

pub(crate) struct WinApi;

impl WinApi {
    pub fn new() -> HidResult<Self> {
        Ok(WinApi)
    }

    pub fn enumerate(&self, vendor_id: u16, product_id: u16) -> HidResult<Vec<DeviceInfo>> {
        let mut hid_guid: GUID = unsafe { core::mem::zeroed() };
        // SAFETY: out-pointer to a GUID.
        unsafe { HidD_GetHidGuid(&mut hid_guid) };

        // SAFETY: hid_guid is initialized; flags select present interfaces.
        let raw = unsafe {
            SetupDiGetClassDevsW(
                &hid_guid,
                core::ptr::null(),
                core::ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        };
        if raw == INVALID_HANDLE_VALUE as HDEVINFO {
            return Err(HidError::last_os_error("SetupDiGetClassDevsW"));
        }
        let list = DevInfoList(raw);

        let mut result = Vec::new();
        for index in 0.. {
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: core::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: list/iface are valid; index walks until no-more-items.
            let ok = unsafe {
                SetupDiEnumDeviceInterfaces(list.0, core::ptr::null(), &hid_guid, index, &mut iface)
            };
            if ok == 0 {
                if unsafe { GetLastError() } == ERROR_NO_MORE_ITEMS {
                    break;
                }
                return Err(HidError::last_os_error("SetupDiEnumDeviceInterfaces"));
            }

            let mut devinfo = SP_DEVINFO_DATA {
                cbSize: core::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            let Some(path) = interface_detail_path(list.0, &iface, &mut devinfo) else {
                continue;
            };

            // hidapi opens with no access rights (shared) so devices held
            // exclusively by other processes still enumerate.
            let Ok(handle) = open_interface(&path, 0) else {
                continue;
            };
            let bus_type = bus_type_for_devnode(list.0, &devinfo);
            let info = query_device_info(handle.raw(), &path, bus_type);

            let vid_ok = vendor_id == 0 || info.vendor_id == vendor_id;
            let pid_ok = product_id == 0 || info.product_id == product_id;
            if vid_ok && pid_ok {
                result.push(info);
            }
        }
        Ok(result)
    }

    pub fn open(
        &self,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> HidResult<WinDevice> {
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

    pub fn open_path(&self, path: &str) -> HidResult<WinDevice> {
        WinDevice::open(path)
    }
}

/// `SetupDiGetDeviceInterfaceDetailW` two-call dance; also fills `devinfo`
/// with the backing devnode.
fn interface_detail_path(
    list: HDEVINFO,
    iface: &SP_DEVICE_INTERFACE_DATA,
    devinfo: &mut SP_DEVINFO_DATA,
) -> Option<String> {
    let mut required = 0u32;
    // SAFETY: sizing call, no output buffer.
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list,
            iface,
            core::ptr::null_mut(),
            0,
            &mut required,
            core::ptr::null_mut(),
        );
    }
    let path_offset = core::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
    if (required as usize) <= path_offset {
        return None;
    }
    // u32-aligned backing storage for the variable-length detail struct.
    let mut storage = vec![0u32; (required as usize).div_ceil(4)];
    let detail = storage.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    // SAFETY: storage is large and aligned enough for the header + path.
    unsafe {
        (*detail).cbSize = core::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        let ok = SetupDiGetDeviceInterfaceDetailW(
            list,
            iface,
            detail,
            required,
            core::ptr::null_mut(),
            devinfo,
        );
        if ok == 0 {
            return None;
        }
        let chars = (required as usize - path_offset) / 2;
        let path = core::slice::from_raw_parts(
            core::ptr::addr_of!((*detail).DevicePath).cast::<u16>(),
            chars,
        );
        utf16_until_nul(path)
    }
}

// --- device handle ------------------------------------------------------------

/// State of the single background `ReadFile` hidapi keeps per device.
struct ReadState {
    io: OverlappedIo,
    /// Staging buffer of `InputReportByteLength` bytes; Windows always
    /// delivers full-length reports prefixed with the report ID byte.
    buf: Vec<u8>,
    /// Whether a `ReadFile` is currently in flight on `io`.
    pending: bool,
    /// Most recent `RegisterWaitForSingleObject` handle for `read_async`
    /// wake-ups; null when none was ever registered. Only touched under the
    /// read lock (and in `Drop`), never by the wait callback.
    wait: RawHandle,
}

/// State shared between [`WinDevice::read_async`] futures and the
/// thread-pool wait callback ([`read_wait_callback`]).
///
/// Owned by the device through an `Arc`; one extra strong count is leaked at
/// open time to back the raw context pointer handed to
/// `RegisterWaitForSingleObject`, and reclaimed in `WinDevice::drop` *after*
/// `UnregisterWaitEx(_, INVALID_HANDLE_VALUE)` has blocked until no callback
/// is (or can start) running. A dropped future therefore can never leave a
/// callback with a dangling context.
struct ReadWake {
    /// Waker stored by the latest pending poll; taken (and woken) by the
    /// callback.
    waker: Mutex<Option<Waker>>,
    /// Whether a one-shot wait is currently armed on the read event. Set by
    /// the registering poll, cleared by the callback *before* it takes the
    /// waker (see the ordering notes in [`WinDevice::park_read`]).
    wait_registered: AtomicBool,
    /// The read event (owned by `ReadState::io`, which outlives every
    /// callback, see above).
    event: RawHandle,
}

/// `RegisterWaitForSingleObject` callback: the read event signaled, i.e. the
/// background `ReadFile` completed (or the signal was restored by a previous
/// callback). Wakes whatever task is parked in `read_async`.
unsafe extern "system" fn read_wait_callback(ctx: *mut core::ffi::c_void, _timer_fired: bool) {
    // SAFETY: ctx is the `Arc<ReadWake>` pointer leaked at open; the device's
    // Drop blockingly unregisters the wait before releasing that count.
    let wake = unsafe { &*ctx.cast_const().cast::<ReadWake>() };
    // The wait satisfied on an *auto-reset* event and thereby consumed its
    // signal; restore it so the synchronous paths (`WaitForSingleObject` in
    // `read_timeout`, `GetOverlappedResult(.., wait=1)`) still observe the
    // completion. A stale signal is harmless: every new `ReadFile` is
    // preceded by `ResetEvent`.
    // SAFETY: the event handle outlives the callback (see ReadWake docs).
    unsafe { SetEvent(wake.event.0) };
    // Clear the flag before taking the waker so a concurrently parking poll
    // either sees it cleared (and arms a fresh wait) or stores its waker in
    // time for the `take` below to pick it up, never both misses.
    wake.wait_registered.store(false, Ordering::Release);
    let waker = wake.waker.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(waker) = waker {
        waker.wake();
    }
}

struct WriteState {
    io: OverlappedIo,
    /// Zero-pad staging buffer of `OutputReportByteLength` bytes.
    buf: Vec<u8>,
}

pub(crate) struct WinDevice {
    read: Mutex<ReadState>,
    /// Waker hand-off for `read_async`; see [`ReadWake`] for the lifecycle.
    wake: Arc<ReadWake>,
    write: Mutex<WriteState>,
    handle: Handle,
    /// Device interface path used to open the handle.
    path: String,
    /// Metadata captured at open time (hidapi caches it the same way).
    info: DeviceInfo,
    feature_report_len: u16,
    // Part of the backend contract; the wrapper now reads input via
    // `read_async`, so the blocking-mode state is unused on this path.
    #[allow(dead_code)]
    blocking: AtomicBool,
    write_timeout_ms: AtomicU32,
}

impl WinDevice {
    fn open(path: &str) -> HidResult<Self> {
        // hidapi opens read/write, and on ERROR_ACCESS_DENIED retries with 0
        // desired access. Windows refuses GENERIC_READ/WRITE on the system
        // keyboard/mouse top-level collections, but a 0-access handle still
        // serves preparsed data, caps, the (reconstructed) report descriptor
        // and string/device metadata, so enumeration scans that only read
        // descriptors succeed. Actual report I/O on such a handle fails, which
        // is correct: those collections are not usable for I/O anyway.
        let handle = match open_interface(path, GENERIC_READ | GENERIC_WRITE) {
            Ok(h) => h,
            Err(ERROR_ACCESS_DENIED) => open_interface(path, 0).map_err(|err| match err {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => HidError::DeviceNotFound,
                ERROR_ACCESS_DENIED => HidError::OpenFailed {
                    message: format!("{path}: access denied"),
                },
                _ => HidError::OpenFailed {
                    message: format!("{path}: {}", std::io::Error::from_raw_os_error(err as i32)),
                },
            })?,
            Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
                return Err(HidError::DeviceNotFound)
            }
            Err(err) => {
                return Err(HidError::OpenFailed {
                    message: format!("{path}: {}", std::io::Error::from_raw_os_error(err as i32)),
                })
            }
        };

        // SAFETY: handle is open; hidapi requests 64 queued input reports.
        if !unsafe { HidD_SetNumInputBuffers(handle.raw(), 64) } {
            return Err(HidError::last_os_error("HidD_SetNumInputBuffers"));
        }

        let pp = PreparsedData::get(handle.raw())?;
        let caps = pp.caps()?;
        drop(pp);

        let bus_type = devnode_for_interface(path)
            .map(|(list, devinfo)| bus_type_for_devnode(list.0, &devinfo))
            .unwrap_or_default();
        let info = query_device_info(handle.raw(), path, bus_type);

        let read_io = OverlappedIo::new()?;
        let wake = Arc::new(ReadWake {
            waker: Mutex::new(None),
            wait_registered: AtomicBool::new(false),
            event: RawHandle(read_io.event.raw()),
        });
        // Leak one strong count to back the context pointer passed to
        // RegisterWaitForSingleObject (`Arc::as_ptr` returns the same pointer
        // `Arc::into_raw` does); reclaimed in Drop after the blocking
        // unregister, so callbacks can never outlive the state.
        let _ctx = Arc::into_raw(Arc::clone(&wake));

        Ok(WinDevice {
            read: Mutex::new(ReadState {
                io: read_io,
                buf: vec![0u8; (caps.InputReportByteLength as usize).max(1)],
                pending: false,
                wait: RawHandle(core::ptr::null_mut()),
            }),
            wake,
            write: Mutex::new(WriteState {
                io: OverlappedIo::new()?,
                buf: vec![0u8; caps.OutputReportByteLength as usize],
            }),
            handle,
            path: path.to_string(),
            info,
            feature_report_len: caps.FeatureReportByteLength,
            blocking: AtomicBool::new(true),
            write_timeout_ms: AtomicU32::new(DEFAULT_WRITE_TIMEOUT_MS),
        })
    }

    /// Map a Win32 error from an I/O path, turning device removal into
    /// [`HidError::Disconnected`].
    fn io_error(operation: &'static str, err: u32) -> HidError {
        if err == ERROR_DEVICE_NOT_CONNECTED {
            return HidError::Disconnected;
        }
        HidError::io(operation, std::io::Error::from_raw_os_error(err as i32))
    }

    pub fn write(&self, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "write data must contain a report ID byte".into(),
            });
        }
        let mut st = self.write.lock().unwrap_or_else(|e| e.into_inner());

        // Windows expects exactly OutputReportByteLength bytes; shorter
        // writes are zero-padded like hidapi does. Longer payloads are passed
        // through and rejected by the driver.
        let (ptr, len) = if data.len() >= st.buf.len() {
            (data.as_ptr(), data.len())
        } else {
            let n = data.len();
            st.buf[..n].copy_from_slice(data);
            st.buf[n..].fill(0);
            (st.buf.as_ptr(), st.buf.len())
        };

        // SAFETY: event is owned; the OVERLAPPED is reused after each
        // operation fully completes or is cancelled below.
        unsafe { ResetEvent(st.io.event.raw()) };
        let ol = st.io.ol_mut();
        // SAFETY: ptr/len describe memory that outlives the operation (we do
        // not return until it completed or was cancelled and reaped).
        let res = unsafe {
            WriteFile(
                self.handle.raw(),
                ptr,
                len as u32,
                core::ptr::null_mut(),
                ol,
            )
        };
        if res == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(Self::io_error("hid write", err));
            }
        }

        let timeout = self.write_timeout_ms.load(Ordering::Relaxed);
        // SAFETY: event/ol belong to this in-flight operation.
        let wait = unsafe { WaitForSingleObject(st.io.event.raw(), timeout) };
        if wait != WAIT_OBJECT_0 {
            // Unlike hidapi we cancel and reap the request before returning,
            // so the staging buffer / caller's slice can be safely reused.
            unsafe {
                CancelIoEx(self.handle.raw(), ol);
                let mut n = 0u32;
                GetOverlappedResult(self.handle.raw(), ol, &mut n, 1);
            }
            return if wait == WAIT_TIMEOUT {
                Err(HidError::backend(format!(
                    "hid write timed out after {timeout} ms"
                )))
            } else {
                Err(HidError::last_os_error("WaitForSingleObject on write"))
            };
        }

        let mut written = 0u32;
        // SAFETY: the operation has signaled; no further wait needed.
        if unsafe { GetOverlappedResult(self.handle.raw(), ol, &mut written, 0) } == 0 {
            return Err(Self::io_error("hid write", unsafe { GetLastError() }));
        }
        // Report the caller's length, not the zero-padded one.
        Ok(data.len())
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn read(&self, buf: &mut [u8]) -> HidResult<usize> {
        let timeout = if self.blocking.load(Ordering::Relaxed) {
            -1
        } else {
            0
        };
        self.read_timeout(buf, timeout)
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            });
        }
        let mut st = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let bytes_read = match self.start_read(&mut st)? {
            Some(n) => n,
            None => {
                if timeout_ms >= 0 {
                    // SAFETY: event belongs to the in-flight read.
                    let wait = unsafe { WaitForSingleObject(st.io.event.raw(), timeout_ms as u32) };
                    if wait == WAIT_TIMEOUT {
                        // No data yet; leave the overlapped read running
                        // (hidapi semantics) and report a timeout.
                        return Ok(0);
                    }
                    if wait != WAIT_OBJECT_0 {
                        return Err(HidError::last_os_error("WaitForSingleObject on read"));
                    }
                }
                let ol = st.io.ol_mut();
                let mut n = 0u32;
                // SAFETY: wait=1 blocks until completion when timeout_ms < 0.
                let res = unsafe { GetOverlappedResult(self.handle.raw(), ol, &mut n, 1) };
                st.pending = false;
                if res == 0 {
                    return Err(Self::io_error("hid read", unsafe { GetLastError() }));
                }
                n
            }
        };
        Ok(Self::copy_report(&st, bytes_read, buf))
    }

    /// Ensure the single background `ReadFile` hidapi keeps per device is in
    /// flight. `Ok(Some(n))` when the call completed synchronously with `n`
    /// bytes (the read is no longer pending), `Ok(None)` when it is pending
    /// (newly started or left over from an earlier call).
    fn start_read(&self, st: &mut ReadState) -> HidResult<Option<u32>> {
        if st.pending {
            return Ok(None);
        }
        st.pending = true;
        st.buf.fill(0);
        // SAFETY: event is owned and not in use (no read pending).
        unsafe { ResetEvent(st.io.event.raw()) };
        let len = st.buf.len() as u32;
        let buf_ptr = st.buf.as_mut_ptr();
        let ol = st.io.ol_mut();
        let mut bytes_read = 0u32;
        // SAFETY: st.buf and the boxed OVERLAPPED stay alive (and at a
        // stable address) for as long as the operation is pending.
        let res = unsafe { ReadFile(self.handle.raw(), buf_ptr, len, &mut bytes_read, ol) };
        if res == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                // SAFETY: drop the failed request before reporting.
                unsafe { CancelIoEx(self.handle.raw(), ol) };
                st.pending = false;
                return Err(Self::io_error("hid read", err));
            }
            return Ok(None);
        }
        st.pending = false;
        Ok(Some(bytes_read))
    }

    /// Copy a completed report out of the staging buffer into `buf`.
    fn copy_report(st: &ReadState, bytes_read: u32, buf: &mut [u8]) -> usize {
        let mut report = &st.buf[..(bytes_read as usize).min(st.buf.len())];
        // Windows always prefixes input reports with a report ID byte; for
        // devices without numbered reports it is 0 and hidapi strips it.
        if report.first() == Some(&0) {
            report = &report[1..];
        }
        let copy_len = report.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&report[..copy_len]);
        copy_len
    }

    /// Read one input report without ever resolving with `Ok(0)`: the future
    /// completes once the persistent background `ReadFile` delivers a report,
    /// and fails with [`HidError::Disconnected`] when the device is removed.
    /// Wake-ups come from a one-shot thread-pool wait on the read event
    /// (`RegisterWaitForSingleObject`, raw [`Waker`]s, no executor assumed).
    pub fn read_async<'a>(&'a self, buf: &'a mut [u8]) -> ReadAsync<'a> {
        ReadAsync { dev: self, buf }
    }

    /// `Future::poll` body of [`ReadAsync`].
    fn poll_read(&self, buf: &mut [u8], cx: &mut Context<'_>) -> Poll<HidResult<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            }));
        }
        let mut st = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let bytes_read = match self.start_read(&mut st) {
            Err(e) => return Poll::Ready(Err(e)),
            Ok(Some(n)) => n,
            Ok(None) => {
                // Non-blocking completion check. The OVERLAPPED status is
                // inspected (wait=0) instead of the event because an already
                // fired thread-pool wait consumes the auto-reset event's
                // signal (the callback restores it for the sync paths).
                let ol = st.io.ol_mut();
                let mut n = 0u32;
                // SAFETY: ol identifies the in-flight read on our handle.
                let res = unsafe { GetOverlappedResult(self.handle.raw(), ol, &mut n, 0) };
                if res == 0 {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_IO_INCOMPLETE {
                        // Still in flight: park until the event signals. The
                        // read keeps running when the future is dropped, so
                        // no report is ever lost, it lands in the staging
                        // buffer for the next read/read_timeout/read_async.
                        return self.park_read(&mut st, cx);
                    }
                    st.pending = false;
                    return Poll::Ready(Err(Self::io_error("hid read", err)));
                }
                st.pending = false;
                n
            }
        };
        Poll::Ready(Ok(Self::copy_report(&st, bytes_read, buf)))
    }

    /// Store the task's waker and make sure a one-shot thread-pool wait is
    /// armed on the read event. Called with the read lock held while the
    /// background read is pending; always returns `Poll::Pending`.
    ///
    /// Re-registration strategy: each wait is `WT_EXECUTEONLYONCE` and
    /// `wait_registered` is cleared by the callback, so the first poll that
    /// still finds the read pending afterwards (or before any wait existed)
    /// arms a fresh wait, non-blockingly unregistering the spent one first.
    /// The waker is stored *before* the flag is checked: a callback racing
    /// with this either picks the new waker up (flag still set), or has
    /// already cleared the flag, making this poll register a new wait on
    /// the still/again-signaled event, which fires immediately. Either way
    /// the wake-up cannot be lost.
    fn park_read(&self, st: &mut ReadState, cx: &mut Context<'_>) -> Poll<HidResult<usize>> {
        *self.wake.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        if !self.wake.wait_registered.swap(true, Ordering::AcqRel) {
            if !st.wait.0.is_null() {
                // The previous one-shot wait has fired; release it without
                // waiting for its callback (the callback only touches
                // `self.wake`, which outlives the device, see ReadWake).
                // SAFETY: st.wait holds a wait handle we registered.
                unsafe { UnregisterWaitEx(st.wait.0, core::ptr::null_mut()) };
                st.wait.0 = core::ptr::null_mut();
            }
            let mut wait: HANDLE = core::ptr::null_mut();
            // SAFETY: the event outlives the wait (unregistered in Drop at
            // the latest) and the context pointer is backed by the strong
            // count leaked at open, reclaimed only after that unregister.
            let ok = unsafe {
                RegisterWaitForSingleObject(
                    &mut wait,
                    st.io.event.raw(),
                    Some(read_wait_callback),
                    Arc::as_ptr(&self.wake).cast(),
                    INFINITE,
                    WT_EXECUTEONLYONCE | WT_EXECUTEINWAITTHREAD,
                )
            };
            if ok == 0 {
                self.wake.wait_registered.store(false, Ordering::Release);
                return Poll::Ready(Err(HidError::last_os_error("RegisterWaitForSingleObject")));
            }
            st.wait.0 = wait;
        }
        Poll::Pending
    }

    #[allow(dead_code)] // part of the backend contract; wrapper reads via read_async
    pub fn set_blocking_mode(&self, blocking: bool) -> HidResult<()> {
        self.blocking.store(blocking, Ordering::Relaxed);
        Ok(())
    }

    pub fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        if data.is_empty() {
            return Err(HidError::InvalidData {
                message: "feature report must contain a report ID byte".into(),
            });
        }
        // HidD_SetFeature wants at least FeatureReportByteLength bytes;
        // zero-pad shorter reports like hidapi.
        let padded;
        let buf: &[u8] = if data.len() >= self.feature_report_len as usize {
            data
        } else {
            let mut v = vec![0u8; self.feature_report_len as usize];
            v[..data.len()].copy_from_slice(data);
            padded = v;
            &padded
        };
        // SAFETY: buf is a live slice of at least the required length.
        let ok =
            unsafe { HidD_SetFeature(self.handle.raw(), buf.as_ptr().cast(), buf.len() as u32) };
        if !ok {
            return Err(Self::io_error("HidD_SetFeature", unsafe { GetLastError() }));
        }
        Ok(())
    }

    /// Synchronous `DeviceIoControl` GET_FEATURE / GET_INPUT_REPORT, used by
    /// hidapi because it reports the actual returned length (unlike the
    /// `HidD_*` wrappers).
    fn ioctl_get_report(
        &self,
        ioctl: u32,
        buf: &mut [u8],
        operation: &'static str,
    ) -> HidResult<usize> {
        if buf.is_empty() {
            return Err(HidError::InvalidData {
                message: "buffer must contain a report ID byte".into(),
            });
        }
        // A private event/OVERLAPPED per call: the request never outlives
        // this function (GetOverlappedResult below waits for completion).
        let mut io = OverlappedIo::new()?;
        let ol = io.ol_mut();
        let mut returned = 0u32;
        // SAFETY: buf is used as both input (report ID) and output, exactly
        // like hidapi; ol stays alive until the operation is reaped.
        let res = unsafe {
            DeviceIoControl(
                self.handle.raw(),
                ioctl,
                buf.as_ptr().cast(),
                buf.len() as u32,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut returned,
                ol,
            )
        };
        if res == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(Self::io_error(operation, err));
            }
        }
        // SAFETY: wait=1 blocks until the request completes.
        if unsafe { GetOverlappedResult(self.handle.raw(), ol, &mut returned, 1) } == 0 {
            return Err(Self::io_error(operation, unsafe { GetLastError() }));
        }
        // bytes_returned excludes the leading report ID byte (hidapi adds 1).
        Ok((returned as usize + 1).min(buf.len()))
    }

    pub fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        self.ioctl_get_report(IOCTL_HID_GET_FEATURE, buf, "IOCTL_HID_GET_FEATURE")
    }

    pub fn get_input_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        match self.ioctl_get_report(
            IOCTL_HID_GET_INPUT_REPORT,
            buf,
            "IOCTL_HID_GET_INPUT_REPORT",
        ) {
            // Old HID drivers may not implement the ioctl; fall back to
            // HidD_GetInputReport, which cannot report the actual length.
            Err(HidError::Io { source, .. })
                if matches!(
                    source.raw_os_error(),
                    Some(e) if e == ERROR_INVALID_FUNCTION as i32
                        || e == ERROR_NOT_SUPPORTED as i32
                ) =>
            {
                // SAFETY: buf is non-empty and writable.
                let ok = unsafe {
                    HidD_GetInputReport(
                        self.handle.raw(),
                        buf.as_mut_ptr().cast(),
                        buf.len() as u32,
                    )
                };
                if !ok {
                    return Err(Self::io_error("HidD_GetInputReport", unsafe {
                        GetLastError()
                    }));
                }
                Ok(buf.len())
            }
            other => other,
        }
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

    pub fn get_indexed_string(&self, index: u32) -> HidResult<Option<String>> {
        let mut buf = [0u16; MAX_STRING_WCHARS];
        // SAFETY: buffer length is in bytes, per the HidD_* contract.
        let ok = unsafe {
            HidD_GetIndexedString(
                self.handle.raw(),
                index,
                buf.as_mut_ptr().cast(),
                (buf.len() * 2) as u32,
            )
        };
        if !ok {
            return Err(Self::io_error("HidD_GetIndexedString", unsafe {
                GetLastError()
            }));
        }
        Ok(utf16_until_nul(&buf))
    }

    /// Reconstruct the report descriptor from preparsed data.
    ///
    /// Windows never exposes the raw descriptor, so, like hidapi's
    /// `hid_winapi_descriptor_reconstruct.c`, one is rebuilt. Unlike hidapi
    /// this uses only the documented HidP API (`HidP_GetCaps`,
    /// `HidP_GetLinkCollectionNodes`, `HidP_GetButtonCaps`,
    /// `HidP_GetValueCaps`), with these limitations:
    ///
    /// * the output is not byte-identical to the original (hidapi's is not
    ///   either); it parses back via [`crate::descriptor::ReportDescriptor`]
    ///   with correct report IDs, usages and field sizes;
    /// * fields are ordered by report type, then report ID, then HidP
    ///   enumeration order, the documented API does not expose bit offsets,
    ///   so the in-report field order may differ from the device's;
    /// * constant padding bits are not enumerable; when a report type uses a
    ///   single report ID, trailing padding is synthesized so the total size
    ///   matches `*ReportByteLength`, otherwise padding is omitted and
    ///   reports may parse shorter than the device sends them;
    /// * array (non-variable) button fields are approximated: the documented
    ///   API does not expose their report size, so 8 bits (16 when the usage
    ///   range exceeds 255) per element is assumed, the keyboard layout.
    pub fn get_report_descriptor(&self, buf: &mut [u8]) -> HidResult<usize> {
        let bytes = self.reconstruct_descriptor()?;
        let len = bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
    }

    fn reconstruct_descriptor(&self) -> HidResult<Vec<u8>> {
        let pp = PreparsedData::get(self.handle.raw())?;
        let caps = pp.caps()?;

        // Link collection tree; node 0 is the top-level collection.
        let count = caps.NumberLinkCollectionNodes as usize;
        if count == 0 {
            return Err(HidError::backend("device reports no link collections"));
        }
        let mut nodes = vec![HIDP_LINK_COLLECTION_NODE::default(); count];
        let mut len = count as u32;
        // SAFETY: nodes has room for `len` entries.
        let status = unsafe { HidP_GetLinkCollectionNodes(nodes.as_mut_ptr(), &mut len, pp.0) };
        if status != HIDP_STATUS_SUCCESS {
            return Err(HidError::backend("HidP_GetLinkCollectionNodes failed"));
        }
        nodes.truncate(len as usize);

        let mut fields = collect_fields(&pp, &caps);
        synthesize_padding(&caps, &mut fields);

        // Group fields by owning collection, ordered by report type, report
        // ID, then HidP enumeration order.
        let mut by_node: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (i, f) in fields.iter().enumerate() {
            if let Some(v) = by_node.get_mut(f.link as usize) {
                v.push(i);
            }
        }
        for v in &mut by_node {
            v.sort_by_key(|&i| (fields[i].kind, fields[i].report_id, fields[i].order));
        }

        let mut b = DescriptorBuilder::new();
        emit_collection(&mut b, &nodes, &by_node, &fields, 0, 0)?;
        Ok(b.build())
    }

    pub fn get_device_info(&self) -> HidResult<DeviceInfo> {
        Ok(self.info.clone())
    }

    /// `hid_winapi_get_container_id`: the `DEVPKEY_Device_ContainerId` GUID
    /// of the devnode behind this interface, as 16 bytes in the GUID's
    /// in-memory (little-endian fields) layout.
    pub fn container_id(&self) -> HidResult<[u8; 16]> {
        let (list, devinfo) = devnode_for_interface(&self.path)?;
        let mut guid: GUID = unsafe { core::mem::zeroed() };
        let mut prop_type: DEVPROPTYPE = 0;
        // SAFETY: out-buffer is a GUID, matching DEVPROP_TYPE_GUID.
        let ok = unsafe {
            SetupDiGetDevicePropertyW(
                list.0,
                &devinfo,
                &DEVPKEY_Device_ContainerId,
                &mut prop_type,
                (&mut guid as *mut GUID).cast(),
                core::mem::size_of::<GUID>() as u32,
                core::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(HidError::last_os_error("SetupDiGetDevicePropertyW"));
        }
        if prop_type != DEVPROP_TYPE_GUID {
            return Err(HidError::backend("container id property is not a GUID"));
        }
        Ok(guid_to_bytes(&guid))
    }

    /// `hid_winapi_set_write_timeout`.
    pub fn set_write_timeout(&self, timeout_ms: u32) {
        self.write_timeout_ms.store(timeout_ms, Ordering::Relaxed);
    }
}

impl Drop for WinDevice {
    fn drop(&mut self) {
        let st = self.read.get_mut().unwrap_or_else(|e| e.into_inner());
        if !st.wait.0.is_null() {
            // Blocking unregister: returns only once the wait callback is
            // guaranteed not to be (and never again become) running, so it
            // cannot race the teardown below.
            // SAFETY: st.wait holds the latest registered wait handle.
            unsafe { UnregisterWaitEx(st.wait.0, INVALID_HANDLE_VALUE) };
            st.wait.0 = core::ptr::null_mut();
        }
        if st.pending {
            // Reap the background read so the kernel stops writing into the
            // buffers before they are freed.
            // SAFETY: ol identifies the in-flight read on our handle.
            unsafe {
                let ol = st.io.ol_mut();
                CancelIoEx(self.handle.raw(), ol);
                let mut n = 0u32;
                GetOverlappedResult(self.handle.raw(), ol, &mut n, 1);
            }
        }
        // Reclaim the strong count leaked at open for the wait-callback
        // context; safe now that no callback can run anymore.
        // SAFETY: the count was leaked via Arc::into_raw(self.wake.clone()),
        // and Arc::as_ptr returns that same pointer.
        unsafe { drop(Arc::from_raw(Arc::as_ptr(&self.wake))) };
    }
}

/// Future returned by [`WinDevice::read_async`].
///
/// Cancel-safe: the report is only harvested inside `poll`, and the
/// persistent background `ReadFile` keeps running when the future is dropped
///, a completed report stays in the staging buffer for the next
/// `read`/`read_timeout`/`read_async` (the same semantics as a timed-out
/// synchronous read). A drop may leave the one-shot thread-pool wait armed
/// with a stale waker; it fires at most once as a spurious (or no-op) wake
/// and is unregistered on the next registration, or blockingly in
/// `WinDevice`'s `Drop`.
pub(crate) struct ReadAsync<'a> {
    dev: &'a WinDevice,
    buf: &'a mut [u8],
}

impl Future for ReadAsync<'_> {
    type Output = HidResult<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.dev.poll_read(this.buf, cx)
    }
}

// --- descriptor reconstruction --------------------------------------------------

/// Report types in emission order; doubles as the sort key.
const KIND_INPUT: u8 = 0;
const KIND_OUTPUT: u8 = 1;
const KIND_FEATURE: u8 = 2;

enum FieldData {
    Button {
        usage_page: u16,
        /// `Some((min, max))` for usage ranges, otherwise `usage` applies.
        range: Option<(u16, u16)>,
        usage: u16,
        report_count: u16,
        /// Raw main-item data (`BitField`) as declared by the device.
        flags: u32,
    },
    Value {
        usage_page: u16,
        range: Option<(u16, u16)>,
        usage: u16,
        report_count: u16,
        bit_size: u16,
        logical_min: i32,
        logical_max: i32,
        physical_min: i32,
        physical_max: i32,
        unit: u32,
        unit_exp: i32,
        flags: u32,
    },
    /// Synthesized constant padding (see `get_report_descriptor` docs).
    Padding { bits: u32 },
}

struct Field {
    kind: u8,
    report_id: u8,
    /// Owning link collection index.
    link: u16,
    /// HidP enumeration order, used as the in-report tiebreaker.
    order: usize,
    data: FieldData,
}

impl FieldData {
    /// `(report_size, report_count)` as they will be emitted.
    fn layout(&self) -> (u32, u32) {
        match *self {
            FieldData::Button {
                range,
                report_count,
                flags,
                ..
            } => {
                if flags & MainFlags::VARIABLE != 0 {
                    let count = match range {
                        Some((lo, hi)) => u32::from(hi.saturating_sub(lo)) + 1,
                        None => report_count.max(1) as u32,
                    };
                    (1, count)
                } else {
                    // Array selector; size approximated (see method docs).
                    let max = range.map(|(_, hi)| hi).unwrap_or(1);
                    (if max > 255 { 16 } else { 8 }, report_count.max(1) as u32)
                }
            }
            FieldData::Value {
                range,
                report_count,
                bit_size,
                ..
            } => {
                let mut count = report_count.max(1) as u32;
                if let Some((lo, hi)) = range {
                    count = count.max(u32::from(hi.saturating_sub(lo)) + 1);
                }
                (bit_size as u32, count)
            }
            FieldData::Padding { bits } => (bits, 1),
        }
    }
}

/// Pull every button/value cap for all three report types.
fn collect_fields(pp: &PreparsedData, caps: &HIDP_CAPS) -> Vec<Field> {
    let kinds: [(u8, HIDP_REPORT_TYPE, u16, u16); 3] = [
        (
            KIND_INPUT,
            HidP_Input,
            caps.NumberInputButtonCaps,
            caps.NumberInputValueCaps,
        ),
        (
            KIND_OUTPUT,
            HidP_Output,
            caps.NumberOutputButtonCaps,
            caps.NumberOutputValueCaps,
        ),
        (
            KIND_FEATURE,
            HidP_Feature,
            caps.NumberFeatureButtonCaps,
            caps.NumberFeatureValueCaps,
        ),
    ];
    let mut fields = Vec::new();
    for (kind, report_type, n_buttons, n_values) in kinds {
        if n_buttons > 0 {
            let mut buttons = vec![HIDP_BUTTON_CAPS::default(); n_buttons as usize];
            let mut len = n_buttons;
            // SAFETY: buttons has room for `len` entries.
            let status =
                unsafe { HidP_GetButtonCaps(report_type, buttons.as_mut_ptr(), &mut len, pp.0) };
            if status == HIDP_STATUS_SUCCESS {
                buttons.truncate(len as usize);
                for c in &buttons {
                    if c.IsAlias {
                        // Aliased usages share a field with the primary one.
                        continue;
                    }
                    // SAFETY: IsRange selects the active union arm.
                    let (range, usage) = unsafe {
                        if c.IsRange {
                            (
                                Some((c.Anonymous.Range.UsageMin, c.Anonymous.Range.UsageMax)),
                                0,
                            )
                        } else {
                            (None, c.Anonymous.NotRange.Usage)
                        }
                    };
                    fields.push(Field {
                        kind,
                        report_id: c.ReportID,
                        link: c.LinkCollection,
                        order: fields.len(),
                        data: FieldData::Button {
                            usage_page: c.UsagePage,
                            range,
                            usage,
                            report_count: c.ReportCount,
                            flags: c.BitField as u32,
                        },
                    });
                }
            }
        }
        if n_values > 0 {
            let mut values = vec![HIDP_VALUE_CAPS::default(); n_values as usize];
            let mut len = n_values;
            // SAFETY: values has room for `len` entries.
            let status =
                unsafe { HidP_GetValueCaps(report_type, values.as_mut_ptr(), &mut len, pp.0) };
            if status == HIDP_STATUS_SUCCESS {
                values.truncate(len as usize);
                for c in &values {
                    if c.IsAlias {
                        continue;
                    }
                    // SAFETY: IsRange selects the active union arm.
                    let (range, usage) = unsafe {
                        if c.IsRange {
                            (
                                Some((c.Anonymous.Range.UsageMin, c.Anonymous.Range.UsageMax)),
                                0,
                            )
                        } else {
                            (None, c.Anonymous.NotRange.Usage)
                        }
                    };
                    fields.push(Field {
                        kind,
                        report_id: c.ReportID,
                        link: c.LinkCollection,
                        order: fields.len(),
                        data: FieldData::Value {
                            usage_page: c.UsagePage,
                            range,
                            usage,
                            report_count: c.ReportCount,
                            bit_size: c.BitSize,
                            logical_min: c.LogicalMin,
                            logical_max: c.LogicalMax,
                            physical_min: c.PhysicalMin,
                            physical_max: c.PhysicalMax,
                            unit: c.Units,
                            unit_exp: c.UnitsExp as i32,
                            flags: c.BitField as u32,
                        },
                    });
                }
            }
        }
    }
    fields
}

/// Add trailing constant padding to single-report-ID report types whose
/// enumerated fields do not fill `*ReportByteLength` (the report ID byte is
/// not part of the descriptor-declared bits).
fn synthesize_padding(caps: &HIDP_CAPS, fields: &mut Vec<Field>) {
    for (kind, report_len) in [
        (KIND_INPUT, caps.InputReportByteLength),
        (KIND_OUTPUT, caps.OutputReportByteLength),
        (KIND_FEATURE, caps.FeatureReportByteLength),
    ] {
        if report_len < 2 {
            continue;
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut bits = 0u64;
        for f in fields.iter().filter(|f| f.kind == kind) {
            ids.insert(f.report_id);
            let (size, count) = f.data.layout();
            bits += u64::from(size) * u64::from(count);
        }
        if ids.len() != 1 {
            // Per-report lengths are unknowable for multi-ID report types.
            continue;
        }
        let declared = (u64::from(report_len) - 1) * 8;
        if bits < declared {
            fields.push(Field {
                kind,
                report_id: *ids.first().expect("one id"),
                link: 0,
                order: usize::MAX, // sorts after all real fields
                data: FieldData::Padding {
                    bits: (declared - bits) as u32,
                },
            });
        }
    }
}

/// Recursively emit one link collection: usage items, the collection, its
/// fields and children. Children follow the `FirstChild`/`NextSibling`
/// chain, which Windows happens to keep in reverse declaration order; the
/// resulting descriptor is equivalent either way.
fn emit_collection(
    b: &mut DescriptorBuilder,
    nodes: &[HIDP_LINK_COLLECTION_NODE],
    by_node: &[Vec<usize>],
    fields: &[Field],
    index: usize,
    depth: usize,
) -> HidResult<()> {
    if depth > 64 {
        return Err(HidError::backend("link collection tree too deep"));
    }
    let node = nodes[index];
    // Copy out of the packed struct before use.
    let (page, usage) = (node.LinkUsagePage, node.LinkUsage);
    let kind = CollectionKind::from_value((node._bitfield & 0xFF) as u8);
    b.usage_page(page);
    b.usage(usage as u32);
    b.collection(kind);

    for &i in &by_node[index] {
        emit_field(b, &fields[i]);
    }

    let mut child = node.FirstChild as usize;
    let mut guard = 0;
    while child != 0 && child < nodes.len() && guard < nodes.len() {
        emit_collection(b, nodes, by_node, fields, child, depth + 1)?;
        child = nodes[child].NextSibling as usize;
        guard += 1;
    }

    b.end_collection();
    Ok(())
}

fn emit_field(b: &mut DescriptorBuilder, field: &Field) {
    if let FieldData::Padding { bits } = field.data {
        if field.report_id != 0 {
            b.report_id(field.report_id);
        }
        b.report_size(bits);
        b.report_count(1);
        emit_main(b, field.kind, MainFlags::CONSTANT);
        return;
    }

    let (usage_page, range, usage, flags) = match field.data {
        FieldData::Button {
            usage_page,
            range,
            usage,
            flags,
            ..
        }
        | FieldData::Value {
            usage_page,
            range,
            usage,
            flags,
            ..
        } => (usage_page, range, usage, flags),
        FieldData::Padding { .. } => unreachable!(),
    };

    b.usage_page(usage_page);
    if field.report_id != 0 {
        b.report_id(field.report_id);
    }
    match range {
        Some((lo, hi)) => {
            b.usage_minimum(lo as u32);
            b.usage_maximum(hi as u32);
        }
        None => {
            b.usage(usage as u32);
        }
    }

    // Globals are re-emitted for every field (zero values encode as
    // zero-length payloads), so nothing leaks between fields.
    match field.data {
        FieldData::Button { range, flags, .. } => {
            if flags & MainFlags::VARIABLE != 0 {
                b.logical_minimum(0);
                b.logical_maximum(1);
            } else {
                b.logical_minimum(0);
                b.logical_maximum(range.map(|(_, hi)| hi).unwrap_or(1) as i32);
            }
            b.physical_minimum(0);
            b.physical_maximum(0);
            b.unit_exponent(0);
            b.unit(0);
        }
        FieldData::Value {
            logical_min,
            logical_max,
            physical_min,
            physical_max,
            unit,
            unit_exp,
            ..
        } => {
            b.logical_minimum(logical_min);
            b.logical_maximum(logical_max);
            b.physical_minimum(physical_min);
            b.physical_maximum(physical_max);
            b.unit_exponent(unit_exp);
            b.unit(unit);
        }
        FieldData::Padding { .. } => unreachable!(),
    }

    let (size, count) = field.data.layout();
    b.report_size(size);
    b.report_count(count);
    emit_main(b, field.kind, flags);
}

fn emit_main(b: &mut DescriptorBuilder, kind: u8, flags: u32) {
    match kind {
        KIND_INPUT => b.input(flags),
        KIND_OUTPUT => b.output(flags),
        _ => b.feature(flags),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interface_number_from_path() {
        // Composite USB device: the &mi_ token wins regardless of bus type.
        let path = r"\\?\hid#vid_046d&pid_c216&mi_01#7&2d80a3c5&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(interface_number_from_path(path, BusType::Usb), 1);
        let upper =
            r"\\?\HID#VID_046D&PID_C216&MI_0A#8&dead&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(interface_number_from_path(upper, BusType::Usb), 0x0A);
        // Non-composite USB device (no &mi_): interface 0, like Linux/macOS.
        let no_mi = r"\\?\hid#vid_0603&pid_1020&col01#7&2f51268c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(interface_number_from_path(no_mi, BusType::Usb), 0);
        // Interface-less transport (e.g. Bluetooth) stays -1.
        assert_eq!(interface_number_from_path(no_mi, BusType::Bluetooth), -1);
    }

    #[test]
    fn converts_utf16_strings() {
        let buf: Vec<u16> = "Logitech".encode_utf16().chain([0, 0x41]).collect();
        assert_eq!(utf16_until_nul(&buf).as_deref(), Some("Logitech"));
        assert_eq!(utf16_until_nul(&[0u16; 4]), None);
        assert_eq!(utf16_until_nul(&[]), None);
        // No terminator: the whole buffer is the string.
        let raw: Vec<u16> = "ab".encode_utf16().collect();
        assert_eq!(utf16_until_nul(&raw).as_deref(), Some("ab"));
    }

    #[test]
    fn classifies_bus_types() {
        assert_eq!(classify_bus("USB", r"USB\VID_046D&PID_C216"), BusType::Usb);
        assert_eq!(classify_bus("BTHENUM", ""), BusType::Bluetooth);
        assert_eq!(classify_bus("BTHLEDEVICE", ""), BusType::Bluetooth);
        assert_eq!(classify_bus("ACPI", r"ACPI\PNP0C50 PNP0C50"), BusType::I2c);
        assert_eq!(classify_bus("ACPI", r"ACPI\PNP0C51"), BusType::Spi);
        assert_eq!(classify_bus("ROOT", ""), BusType::Unknown);
    }

    #[test]
    fn guid_bytes_use_little_endian_field_layout() {
        let guid = GUID::from_u128(0x00112233_4455_6677_8899_aabbccddeeff);
        assert_eq!(
            guid_to_bytes(&guid),
            [
                0x33, 0x22, 0x11, 0x00, // data1 LE
                0x55, 0x44, // data2 LE
                0x77, 0x66, // data3 LE
                0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // data4 verbatim
            ]
        );
    }

    #[test]
    fn hid_ioctls_match_ddk_values() {
        // CTL_CODE(FILE_DEVICE_KEYBOARD, fn, METHOD_OUT_DIRECT, FILE_ANY_ACCESS)
        assert_eq!(IOCTL_HID_GET_FEATURE, (0x0B << 16) | (100 << 2) | 2);
        assert_eq!(IOCTL_HID_GET_INPUT_REPORT, (0x0B << 16) | (104 << 2) | 2);
    }

    #[test]
    fn padding_fills_single_report_devices() {
        let caps = HIDP_CAPS {
            InputReportByteLength: 9, // 1 id byte + 8 data bytes
            ..Default::default()
        };
        let mut fields = vec![Field {
            kind: KIND_INPUT,
            report_id: 0,
            link: 0,
            order: 0,
            data: FieldData::Value {
                usage_page: 1,
                range: None,
                usage: 0x30,
                report_count: 2,
                bit_size: 8,
                logical_min: 0,
                logical_max: 255,
                physical_min: 0,
                physical_max: 0,
                unit: 0,
                unit_exp: 0,
                flags: MainFlags::VARIABLE,
            },
        }];
        synthesize_padding(&caps, &mut fields);
        assert_eq!(fields.len(), 2);
        match fields[1].data {
            FieldData::Padding { bits } => assert_eq!(bits, 64 - 16),
            _ => panic!("expected padding"),
        }
    }
}

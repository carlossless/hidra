//! Stand-in for a backend this build does not contain.
//!
//! [`super::dispatch`] has one enum variant per [`Backend`](super::Backend),
//! whatever the target and feature set. Filling an absent variant with an
//! uninhabited type keeps that code free of `cfg`s: the variant can never be
//! constructed, so every arm handling it is statically dead, and the compiler
//! still type-checks it. Selection is refused before construction, by
//! [`Backend::is_available`](super::Backend::is_available).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::{HidBackend, HidDeviceBackend};
use crate::{DeviceInfo, HidError, HidResult};

/// A backend that was not compiled in.
pub(crate) enum Unsupported {}

fn absent() -> HidError {
    HidError::Unsupported {
        message: "backend not compiled into this build".into(),
    }
}

impl HidBackend for Unsupported {
    type Device = Unsupported;

    fn new() -> HidResult<Self> {
        Err(absent())
    }

    fn enumerate(&self, _vendor_id: u16, _product_id: u16) -> HidResult<Vec<DeviceInfo>> {
        match *self {}
    }

    fn open_path(&self, _path: &str) -> HidResult<Self::Device> {
        match *self {}
    }
}

impl HidDeviceBackend for Unsupported {
    type Read<'a> = Unsupported;

    fn write(&self, _data: &[u8]) -> HidResult<usize> {
        match *self {}
    }

    fn read_async<'a>(&'a self, _buf: &'a mut [u8]) -> Self::Read<'a> {
        match *self {}
    }

    fn send_feature_report(&self, _data: &[u8]) -> HidResult<()> {
        match *self {}
    }

    fn get_feature_report(&self, _buf: &mut [u8]) -> HidResult<usize> {
        match *self {}
    }

    fn get_input_report(&self, _buf: &mut [u8]) -> HidResult<usize> {
        match *self {}
    }

    fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        match *self {}
    }

    fn get_product_string(&self) -> HidResult<Option<String>> {
        match *self {}
    }

    fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        match *self {}
    }

    fn get_indexed_string(&self, _index: u32) -> HidResult<Option<String>> {
        match *self {}
    }

    fn get_report_descriptor(&self, _buf: &mut [u8]) -> HidResult<usize> {
        match *self {}
    }

    fn get_device_info(&self) -> HidResult<DeviceInfo> {
        match *self {}
    }
}

impl Future for Unsupported {
    type Output = HidResult<usize>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match *self {}
    }
}

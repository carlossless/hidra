//! List every connected HID device, through whichever backend is selected.
//!
//! ```sh
//! cargo run --example enumerate
//! HIDRA_BACKEND=nusb cargo run --features nusb --example enumerate
//! ```

use hidra::{HidBackend, Hidra, Native};

fn main() -> hidra::HidResult<()> {
    // The backend is a type, so the choice is made once, here, and the rest of
    // the program is generic over it.
    match std::env::var("HIDRA_BACKEND").as_deref() {
        #[cfg(all(
            feature = "nusb",
            any(target_os = "linux", target_os = "macos", target_os = "windows")
        ))]
        Ok("nusb") => run::<hidra::Nusb>(),
        Ok(other) if other != "native" => Err(hidra::HidError::Unsupported {
            message: format!("no backend named {other:?} in this build"),
        }),
        _ => run::<Native>(),
    }
}

fn run<B: HidBackend>() -> hidra::HidResult<()> {
    let api = Hidra::<B>::builder().build()?;
    for dev in api.device_list() {
        println!(
            "{:04x}:{:04x} bus={} usage={:04x}:{:04x} iface={} path={}",
            dev.vendor_id(),
            dev.product_id(),
            dev.bus_type(),
            dev.usage_page(),
            dev.usage(),
            dev.interface_number(),
            dev.path(),
        );
        println!(
            "  manufacturer: {:?}\n  product:      {:?}\n  serial:       {:?}",
            dev.manufacturer_string(),
            dev.product_string(),
            dev.serial_number(),
        );
    }
    Ok(())
}

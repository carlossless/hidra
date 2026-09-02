//! List every connected HID device, through whichever backend is selected.
//!
//! ```sh
//! cargo run --example enumerate
//! HIDRA_BACKEND=nusb cargo run --features nusb --example enumerate
//! ```

use hidra::Backend;

fn main() -> hidra::HidResult<()> {
    let backend = match std::env::var("HIDRA_BACKEND") {
        Ok(name) => name.parse::<Backend>()?,
        Err(_) => Backend::default(),
    };
    eprintln!(
        "backend: {backend} (available: {})",
        Backend::available()
            .map(Backend::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let api = hidra::Hidra::builder().backend(backend).build()?;
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

//! List every connected HID device, like hidapi's `hidtest` enumeration.
//!
//! ```sh
//! cargo run --example enumerate
//! ```

fn main() -> hidra::HidResult<()> {
    let api = hidra::Hidra::new()?;
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

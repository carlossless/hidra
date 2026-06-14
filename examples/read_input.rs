//! Open a device by VID/PID and dump input reports plus its parsed report
//! descriptor, driving hidra's futures blocking with `.wait()`.
//!
//! ```sh
//! cargo run --example read_input -- 046d c216
//! ```

use std::env;

use hidra::MaybeFuture;

fn main() -> hidra::HidResult<()> {
    let mut args = env::args().skip(1);
    let vid = u16::from_str_radix(&args.next().expect("usage: read_input <vid> <pid>"), 16)
        .expect("vid must be hex");
    let pid = u16::from_str_radix(&args.next().expect("usage: read_input <vid> <pid>"), 16)
        .expect("pid must be hex");

    let api = hidra::HidApi::new()?;
    let device = api.open(vid, pid).wait()?;

    println!("product: {:?}", device.get_product_string().wait()?);
    let descriptor = device.parsed_report_descriptor().wait()?;
    println!(
        "report descriptor: {} top-level collection(s), max input report {} bytes, numbered ids: {}",
        descriptor.collections.len(),
        descriptor.max_report_size(hidra::descriptor::ReportKind::Input),
        descriptor.uses_report_ids(),
    );

    println!("reading input reports (ctrl-c to quit)...");
    let mut buf = [0u8; 256];
    loop {
        // Blocks until a report arrives; never returns 0.
        let len = device.read(&mut buf).wait()?;
        println!("{:02x?}", &buf[..len]);
    }
}

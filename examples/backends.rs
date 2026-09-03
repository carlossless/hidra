//! Drive every available backend from one process: what each one enumerates,
//! and opening a device by falling back from one to the next.
//!
//! ```sh
//! cargo run --example backends
//! cargo run --example backends -- 046d c216
//! ```

use std::env;

use hidra::{Backend, HidResult, Hidra, MaybeFuture};

fn main() -> HidResult<()> {
    let mut args = env::args().skip(1);
    let target = match (args.next(), args.next()) {
        (Some(vid), Some(pid)) => Some((
            u16::from_str_radix(&vid, 16).expect("vid must be hex"),
            u16::from_str_radix(&pid, 16).expect("pid must be hex"),
        )),
        _ => None,
    };

    for backend in Backend::available() {
        let api = match Hidra::builder().backend(backend).build() {
            Ok(api) => api,
            Err(e) => {
                println!("{backend}: {e}");
                continue;
            }
        };
        let count = api
            .device_list()
            .filter(|d| {
                target.is_none_or(|(vid, pid)| d.vendor_id() == vid && d.product_id() == pid)
            })
            .count();
        println!("{backend}: {count} device(s)");
    }

    let Some((vid, pid)) = target else {
        return Ok(());
    };

    for backend in Backend::available() {
        let Ok(api) = Hidra::builder().backend(backend).build() else {
            continue;
        };
        match api.open(vid, pid).wait() {
            Ok(device) => {
                println!("opened {vid:04x}:{pid:04x} on {backend}");
                println!("product: {:?}", device.get_product_string().wait()?);
                return Ok(());
            }
            Err(e) => println!("{backend}: {e}"),
        }
    }
    println!("{vid:04x}:{pid:04x} not reachable through any backend");
    Ok(())
}

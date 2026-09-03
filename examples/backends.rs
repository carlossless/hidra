//! Drive both backends from one process, and open a device by falling back
//! from one to the next.
//!
//! Backends are types, so a handle that can hold either is the caller's to
//! declare -- `Api` below is the whole of it, forwarding only the two methods
//! this program actually uses.
//!
//! ```sh
//! cargo run --features nusb --example backends
//! cargo run --features nusb --example backends -- 046d c216
//! ```

use std::env;

use hidra::{DeviceInfo, HidResult, Hidra, MaybeFuture, Native, Nusb};

enum Api {
    Native(Hidra<Native>),
    Nusb(Hidra<Nusb>),
}

impl Api {
    fn name(&self) -> &'static str {
        match self {
            Api::Native(_) => "native",
            Api::Nusb(_) => "nusb",
        }
    }

    fn device_list(&self) -> Vec<&DeviceInfo> {
        match self {
            Api::Native(api) => api.device_list().collect(),
            Api::Nusb(api) => api.device_list().collect(),
        }
    }

    fn product_string_of(&self, vid: u16, pid: u16) -> HidResult<Option<String>> {
        match self {
            Api::Native(api) => api.open(vid, pid).wait()?.get_product_string().wait(),
            Api::Nusb(api) => api.open(vid, pid).wait()?.get_product_string().wait(),
        }
    }
}

fn main() -> HidResult<()> {
    let mut args = env::args().skip(1);
    let target = match (args.next(), args.next()) {
        (Some(vid), Some(pid)) => Some((
            u16::from_str_radix(&vid, 16).expect("vid must be hex"),
            u16::from_str_radix(&pid, 16).expect("pid must be hex"),
        )),
        _ => None,
    };

    let mut apis = Vec::new();
    match Hidra::<Native>::builder().build() {
        Ok(api) => apis.push(Api::Native(api)),
        Err(e) => println!("native: {e}"),
    }
    match Hidra::<Nusb>::builder().build() {
        Ok(api) => apis.push(Api::Nusb(api)),
        Err(e) => println!("nusb: {e}"),
    }

    for api in &apis {
        let count = api
            .device_list()
            .into_iter()
            .filter(|d| {
                target.is_none_or(|(vid, pid)| d.vendor_id() == vid && d.product_id() == pid)
            })
            .count();
        println!("{}: {count} device(s)", api.name());
    }

    let Some((vid, pid)) = target else {
        return Ok(());
    };

    for api in &apis {
        match api.product_string_of(vid, pid) {
            Ok(product) => {
                println!("opened {vid:04x}:{pid:04x} on {}", api.name());
                println!("product: {product:?}");
                return Ok(());
            }
            Err(e) => println!("{}: {e}", api.name()),
        }
    }
    println!("{vid:04x}:{pid:04x} not reachable through either backend");
    Ok(())
}

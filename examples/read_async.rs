//! Read input reports asynchronously.
//!
//! hidra's `read` futures are runtime-agnostic, they work under tokio,
//! async-std, smol, or (as here) a minimal hand-rolled executor, because
//! wake-ups use plain `Waker`s backed by OS readiness, like nusb.
//!
//! ```sh
//! cargo run --example read_async -- 046d c216
//! ```

use std::env;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

/// Minimal single-future executor: park the thread until woken.
fn block_on<F: Future>(fut: F) -> F::Output {
    struct ThreadWaker(Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}

fn main() -> hidra::HidResult<()> {
    let mut args = env::args().skip(1);
    let vid = u16::from_str_radix(&args.next().expect("usage: read_async <vid> <pid>"), 16)
        .expect("vid must be hex");
    let pid = u16::from_str_radix(&args.next().expect("usage: read_async <vid> <pid>"), 16)
        .expect("pid must be hex");

    let api = hidra::HidApi::new()?;

    block_on(async {
        let device = api.open(vid, pid).await?;
        println!("product: {:?}", device.get_product_string().await?);
        let mut buf = [0u8; 256];
        loop {
            // Never returns 0: resolves only when a report arrives.
            let len = device.read(&mut buf).await?;
            println!("{:02x?}", &buf[..len]);
        }
    })
}

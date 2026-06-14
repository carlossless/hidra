//! [`MaybeFuture`]: an action that can be awaited or, on native targets, run
//! synchronously.
//!
//! Every [`HidApi`](crate::HidApi) / [`HidDevice`](crate::HidDevice) method
//! returns an `impl Future`. Bring [`MaybeFuture`] into scope to also run it
//! synchronously with `.wait()`:
//!
//! ```no_run
//! # #[cfg(not(target_arch = "wasm32"))] fn demo(dev: &hidra::HidDevice) -> hidra::HidResult<()> {
//! use hidra::MaybeFuture;
//! let mut buf = [0u8; 64];
//! let len = dev.read(&mut buf).wait()?;   // blocking
//! # let _ = len; Ok(()) }
//! ```
//!
//! This mirrors nusb's design: the same method serves blocking and async
//! callers, so the two libraries compose. `.wait()` is unavailable on
//! `wasm32` because a browser cannot block; there you must `.await`.

use core::future::{Future, IntoFuture};

/// Extension trait adding a blocking `.wait()` to every action.
///
/// Blanket-implemented for everything awaitable, so any
/// [`HidApi`](crate::HidApi) / [`HidDevice`](crate::HidDevice) method can be
/// driven with `.wait()` on native targets. Not available on `wasm32`, where
/// blocking the (single) thread is impossible; use `.await` instead.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeFuture: IntoFuture + Sized {
    /// Run the action to completion, blocking the current thread.
    fn wait(self) -> Self::Output {
        block_on(self.into_future())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<T: IntoFuture> MaybeFuture for T {}

/// Minimal current-thread executor backing [`MaybeFuture::wait`]. Parks the
/// thread between wake-ups, so it drives both the immediately-ready futures
/// the native backends return for synchronous operations and the genuinely
/// async input-read futures (woken by the platform reactor) without any async
/// runtime.
#[cfg(not(target_arch = "wasm32"))]
fn block_on<F: Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread::{self, Thread};

    struct ThreadWaker(Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}

/// A future that runs a synchronous closure the first time it is polled.
///
/// Lets the native backends expose their blocking operations (feature
/// reports, writes, opening) through the async interface: the work runs when
/// the action is awaited or `.wait()`ed, never before, honoring the
/// "nothing happens until you poll" contract. Quick synchronous calls are
/// fine to run inline on the polling thread.
pub(crate) struct Blocking<F>(Option<F>);

impl<F> Blocking<F> {
    pub(crate) fn new(f: F) -> Self {
        Blocking(Some(f))
    }
}

impl<F, T> Future for Blocking<F>
where
    F: FnOnce() -> T + Unpin,
{
    type Output = T;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        _cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<T> {
        let f = self
            .0
            .take()
            .expect("Blocking future polled after completion");
        core::task::Poll::Ready(f())
    }
}

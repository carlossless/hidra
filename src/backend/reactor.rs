//! Minimal `poll(2)` reactor backing async hidraw reads.
//!
//! A single lazily-spawned thread polls every fd that has a parked future
//! and wakes the registered [`Waker`]s when the fd becomes readable (or
//! errors). Registration is one-shot: woken fds are dropped from the
//! interest set and the future re-registers on its next poll. This is the
//! same runtime-agnostic readiness model nusb uses; no async runtime is
//! required or assumed.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::sync::{Mutex, OnceLock};
use std::task::Waker;

pub(crate) struct Reactor {
    interests: Mutex<HashMap<RawFd, Vec<Waker>>>,
    /// eventfd used to interrupt `poll` when the interest set changes.
    wake_fd: RawFd,
}

impl Reactor {
    /// The process-wide reactor, spawning its thread on first use.
    pub fn global() -> &'static Reactor {
        static REACTOR: OnceLock<&'static Reactor> = OnceLock::new();
        REACTOR.get_or_init(|| {
            let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
            assert!(wake_fd >= 0, "eventfd creation failed");
            let reactor: &'static Reactor = Box::leak(Box::new(Reactor {
                interests: Mutex::new(HashMap::new()),
                wake_fd,
            }));
            std::thread::Builder::new()
                .name("hidra-reactor".into())
                .spawn(move || reactor.run())
                .expect("failed to spawn hidra reactor thread");
            reactor
        })
    }

    /// Wake `waker` once `fd` is readable (or in an error state). The
    /// registration is consumed by the wake-up; spurious wake-ups after a
    /// stale registration are allowed by the `Future` contract.
    pub fn register(&self, fd: RawFd, waker: &Waker) {
        let mut interests = self.interests.lock().unwrap();
        let wakers = interests.entry(fd).or_default();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
        drop(interests);
        self.nudge();
    }

    /// Interrupt the poll loop so it picks up interest-set changes.
    fn nudge(&self) {
        let one: u64 = 1;
        // A full eventfd counter still wakes the loop; ignore the result.
        unsafe { libc::write(self.wake_fd, (&one as *const u64).cast(), 8) };
    }

    fn run(&self) {
        loop {
            let mut pollfds = vec![libc::pollfd {
                fd: self.wake_fd,
                events: libc::POLLIN,
                revents: 0,
            }];
            {
                let interests = self.interests.lock().unwrap();
                pollfds.extend(interests.keys().map(|&fd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                }));
            }

            let res = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, -1) };
            if res < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                // Nothing sensible to do; wake everyone so futures can
                // observe the failure through their own syscalls.
                let mut interests = self.interests.lock().unwrap();
                for (_, wakers) in interests.drain() {
                    wakers.into_iter().for_each(Waker::wake);
                }
                continue;
            }

            if pollfds[0].revents & libc::POLLIN != 0 {
                let mut counter = [0u8; 8];
                unsafe { libc::read(self.wake_fd, counter.as_mut_ptr().cast(), 8) };
            }

            let mut interests = self.interests.lock().unwrap();
            for pfd in &pollfds[1..] {
                // Errors and hang-ups wake too: the future's own read will
                // surface the failure.
                if pfd.revents != 0 {
                    if let Some(wakers) = interests.remove(&pfd.fd) {
                        wakers.into_iter().for_each(Waker::wake);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Test future resolving once a pipe read end is readable.
    struct Readable(RawFd);

    impl Future for Readable {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let mut pfd = libc::pollfd {
                fd: self.0,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
                return Poll::Ready(());
            }
            Reactor::global().register(self.0, cx.waker());
            Poll::Pending
        }
    }

    #[test]
    fn wakes_on_readable_fd() {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let [read_end, write_end] = fds;

        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            unsafe { libc::write(write_end, b"x".as_ptr().cast(), 1) };
        });

        crate::test_util::block_on(Readable(read_end));
        writer.join().unwrap();
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
    }

    #[test]
    fn immediately_ready_fd_does_not_hang() {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) };
        crate::test_util::block_on(Readable(fds[0]));
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}

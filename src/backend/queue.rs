//! Bounded input-report queue shared by the backends that receive reports on
//! a producer thread.
//!
//! macOS (`IOHIDManager` callbacks on a run-loop thread) and nusb (an
//! interrupt-IN reader thread) both need the same thing: a capped queue of
//! completed reports, a set of parked [`Waker`]s, and two sticky flags for
//! "device is gone" and "producer stopped". They had a copy each.
//!
//! Windows is deliberately not a user: it has no producer thread, its single
//! background `ReadFile` writes straight into a staging buffer, and there is
//! never more than one completed report to hold.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use crate::error::{HidError, HidResult};

/// Unread input reports kept before the oldest is dropped, matching hidapi's
/// queue cap on both backends.
const MAX_QUEUED_REPORTS: usize = 30;

#[derive(Default)]
struct Inner {
    /// Completed reports, oldest first, capped at [`MAX_QUEUED_REPORTS`].
    reports: VecDeque<Vec<u8>>,
    /// Tasks parked in [`ReportQueue::poll_read`] on an empty queue,
    /// deduplicated via [`Waker::will_wake`]. Drained (and woken, after the
    /// lock is released) whenever a report arrives or a flag is set.
    wakers: Vec<Waker>,
}

/// A bounded queue of input reports plus the parked readers waiting on it.
///
/// The flags are atomics rather than fields of `Inner` so a producer thread
/// can poll them without taking the lock; [`poll_read`](Self::poll_read)
/// still reads them *under* the lock, which is what closes the gap between a
/// reader deciding to park and a producer setting a flag.
#[derive(Default)]
pub(crate) struct ReportQueue {
    inner: Mutex<Inner>,
    /// The device is gone. Reads fail once the queue has drained.
    disconnected: AtomicBool,
    /// The producer has exited, or must exit.
    shutdown: AtomicBool,
    /// What to say when a read finds the producer gone; each backend has its
    /// own wording for the thread it failed to keep alive.
    shutdown_message: &'static str,
}

impl ReportQueue {
    /// A queue whose "producer stopped" error carries `shutdown_message`.
    pub(crate) fn new(shutdown_message: &'static str) -> Self {
        ReportQueue {
            shutdown_message,
            ..Default::default()
        }
    }

    /// Poisoning cannot leave the queue inconsistent: every critical section
    /// is a push, a pop, or a waker swap.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a report, dropping the oldest beyond the cap (like hidapi), and
    /// wake every parked reader.
    pub(crate) fn push(&self, report: Vec<u8>) {
        let wakers = {
            let mut inner = self.lock();
            if inner.reports.len() >= MAX_QUEUED_REPORTS {
                inner.reports.pop_front();
            }
            inner.reports.push_back(report);
            std::mem::take(&mut inner.wakers)
        };
        // Wake outside the lock so no executor code runs while it is held.
        wake_all(wakers);
    }

    /// Flag the device as gone and wake parked readers so they observe it.
    pub(crate) fn set_disconnected(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        self.wake_parked();
    }

    /// Flag the producer as stopped and wake parked readers.
    pub(crate) fn set_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake_parked();
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Wake parked readers without queueing anything, so they re-check the
    /// flags. Called after a flag changes, and by a producer winding down.
    pub(crate) fn wake_parked(&self) {
        let wakers = std::mem::take(&mut self.lock().wakers);
        wake_all(wakers);
    }

    /// Pop-or-park core of every backend's `read_async`: copy one queued
    /// report into `buf`, fail once the device is gone and the queue has
    /// drained, or park this task's waker.
    ///
    /// Never returns `Ok(0)`: an empty buffer is a caller error, and a queued
    /// report is never empty.
    pub(crate) fn poll_read(&self, buf: &mut [u8], cx: &mut Context<'_>) -> Poll<HidResult<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Err(HidError::InvalidData {
                message: "read buffer must not be empty".into(),
            }));
        }
        let mut inner = self.lock();
        if let Some(report) = inner.reports.pop_front() {
            let len = report.len().min(buf.len());
            buf[..len].copy_from_slice(&report[..len]);
            return Poll::Ready(Ok(len));
        }
        // Queued reports drain even after a disconnect, like hidapi.
        if self.is_disconnected() {
            return Poll::Ready(Err(HidError::Disconnected));
        }
        if self.is_shutdown() {
            return Poll::Ready(Err(HidError::backend(self.shutdown_message)));
        }
        if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            inner.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

fn wake_all(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn queue() -> ReportQueue {
        ReportQueue::new("producer stopped")
    }

    fn block_on_read(queue: &ReportQueue, buf: &mut [u8]) -> HidResult<usize> {
        crate::maybe_future::block_on(std::future::poll_fn(|cx| queue.poll_read(buf, cx)))
    }

    #[test]
    fn pops_queued_reports_with_truncation() {
        let queue = queue();
        queue.push(vec![1, 2, 3, 4]);
        queue.push(vec![5]);
        let mut buf = [0u8; 3];
        // Excess bytes are lost, matching hidapi.
        assert_eq!(block_on_read(&queue, &mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(block_on_read(&queue, &mut buf).unwrap(), 1);
        assert_eq!(buf[0], 5);
    }

    #[test]
    fn drops_the_oldest_report_beyond_the_cap() {
        let queue = queue();
        for i in 0..MAX_QUEUED_REPORTS + 5 {
            queue.push(vec![i as u8]);
        }
        assert_eq!(queue.lock().reports.len(), MAX_QUEUED_REPORTS);
        let mut buf = [0u8; 1];
        // The first five were dropped, so the oldest survivor is #5.
        assert_eq!(block_on_read(&queue, &mut buf).unwrap(), 1);
        assert_eq!(buf[0], 5);
    }

    #[test]
    fn rejects_an_empty_buffer_without_consuming_a_report() {
        let queue = queue();
        queue.push(vec![1]);
        let err = block_on_read(&queue, &mut []).unwrap_err();
        assert!(matches!(err, HidError::InvalidData { .. }));
        assert_eq!(queue.lock().reports.len(), 1);
    }

    #[test]
    fn drains_the_queue_before_reporting_disconnect() {
        let queue = queue();
        queue.push(vec![9]);
        queue.set_disconnected();
        let mut buf = [0u8; 4];
        assert_eq!(block_on_read(&queue, &mut buf).unwrap(), 1);
        assert!(matches!(
            block_on_read(&queue, &mut buf).unwrap_err(),
            HidError::Disconnected
        ));
    }

    #[test]
    fn fails_fast_when_shut_down_before_any_reader() {
        // What a backend does for a device that can never deliver input (an
        // interface with no interrupt IN endpoint): reads must error rather
        // than park forever.
        let queue = queue();
        queue.set_shutdown();
        let mut buf = [0u8; 4];
        assert!(matches!(
            block_on_read(&queue, &mut buf).unwrap_err(),
            HidError::Backend { .. }
        ));
    }

    #[test]
    fn parks_until_a_report_is_pushed() {
        let queue = Arc::new(queue());
        let pusher = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                // Let the main thread park its waker first.
                while queue.lock().wakers.is_empty() {
                    std::thread::yield_now();
                }
                queue.push(vec![7, 8]);
            })
        };
        let mut buf = [0u8; 4];
        assert_eq!(block_on_read(&queue, &mut buf).unwrap(), 2);
        assert_eq!(buf[..2], [7, 8]);
        pusher.join().unwrap();
    }

    #[test]
    fn parks_until_disconnect_is_flagged() {
        let queue = Arc::new(queue());
        let disconnector = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                while queue.lock().wakers.is_empty() {
                    std::thread::yield_now();
                }
                queue.set_disconnected();
            })
        };
        let mut buf = [0u8; 4];
        assert!(matches!(
            block_on_read(&queue, &mut buf).unwrap_err(),
            HidError::Disconnected
        ));
        disconnector.join().unwrap();
    }

    #[test]
    fn dedups_wakers_of_the_same_task() {
        struct CountingWaker(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let queue = queue();
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        let mut buf = [0u8; 4];
        // Re-polling the same task must not pile up waker clones.
        assert!(queue.poll_read(&mut buf, &mut cx).is_pending());
        assert!(queue.poll_read(&mut buf, &mut cx).is_pending());
        assert_eq!(queue.lock().wakers.len(), 1);
        // A pushed report drains the parked waker and wakes it exactly once.
        queue.push(vec![1]);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(queue.lock().wakers.is_empty());
    }
}

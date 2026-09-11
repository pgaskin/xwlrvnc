//! Clipboard work only the Wayland thread may do, posted by X threads.
//!
//! Every data-control object — the offers the compositor announces, the
//! sources we publish, the connection they live on — belongs to the Wayland
//! thread's event queue. X connection threads used to reach into them
//! directly (`receive`, `create_data_source`, `set_selection`, `destroy`),
//! which is legal in wayland-client (requests take the backend lock) but
//! races the dispatch thread on the *objects*: an X thread could clone an
//! offer and ask it for a value while the Wayland thread was already
//! destroying it, having seen the selection replaced.
//!
//! So they post a job here instead and go on serving their client. The
//! Wayland thread polls [`wake_fd`](Jobs::wake_fd) alongside its own socket,
//! the way `-dynres` parks a mode change (see [`crate::bridge::dynres`]), and
//! carries the job out on its own. Unlike a mode change nothing waits for the
//! answer: a conversion is finished by the job itself, through
//! [`PendingConversion`].

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bridge::clipboard::{DataOffer, Sel};

/// How long a job may sit in the queue before it is of no use to whoever asked.
/// The same budget a conversion gets once it is running (`RECEIVE_TIMEOUT`), so
/// waiting to start and waiting to finish cost the requestor the same.
///
/// It has to be enforced from outside the Wayland thread, because the case it
/// exists for is that thread being *wedged* rather than gone: `running` stays
/// true, nothing drains the queue, and a conversion would otherwise wait for a
/// `SelectionNotify` that never comes. So it is swept by the X side, from
/// [`Jobs::sweep`] and from [`Jobs::post`] — which is opportunistic rather than
/// absolute: the bound is this long after the next selection request by any
/// client, not this long full stop.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many jobs may be waiting at once. Only a wedged Wayland thread gets
/// anywhere near this; past it the work is refused rather than queued, since
/// a conversion answered much later is of no use to the client that asked.
const MAX_JOBS: usize = 64;

/// One piece of work for the Wayland thread.
pub enum Job {
    /// Publish `data` for `sel` as a fresh data-control source, or with `None`
    /// withdraw the one we published.
    Publish(Sel, Option<Arc<[u8]>>),
    /// Pipe the current value of `sel` out of the Wayland app that owns it, as
    /// `mime`, and answer the X client waiting on it.
    Receive {
        sel: Sel,
        mime: String,
        /// The ownership generation `mime` was resolved against. If the
        /// compositor has replaced the owner since, the job is refused rather
        /// than asked of a successor that may not offer this mime at all.
        generation: u64,
        /// When the X client asked, so that the time this job spent queued
        /// counts against the transfer's total budget rather than extending it.
        since: Instant,
        conversion: PendingConversion,
    },
    /// Offers the compositor has replaced, which the protocol asks us to
    /// destroy.
    Destroy(Vec<DataOffer>),
}

/// A `ConvertSelection` waiting on a value that has to come out of a Wayland
/// app, and what to do once it does: store the property and send the
/// requestor its `SelectionNotify`.
///
/// Built by the X thread that took the request (it is the only one that knows
/// where the property goes), run by the Wayland thread that finishes the
/// transfer. Dropping it unrun refuses the conversion, so a job dropped for
/// any reason — a full queue, no Wayland thread, the compositor gone — leaves
/// no client waiting for an answer that is not coming.
pub struct PendingConversion {
    deliver: Option<Deliver>,
}

/// What a finished conversion does with the value, or with `None` when there
/// is none to be had: see [`PendingConversion`].
type Deliver = Box<dyn FnOnce(Option<Vec<u8>>) + Send>;

impl PendingConversion {
    pub fn new(deliver: impl FnOnce(Option<Vec<u8>>) + Send + 'static) -> Self {
        Self {
            deliver: Some(Box::new(deliver)),
        }
    }

    /// Answers the requestor with `data`, or with `None` refuses it.
    pub fn finish(mut self, data: Option<Vec<u8>>) {
        if let Some(deliver) = self.deliver.take() {
            deliver(data);
        }
    }
}

impl Drop for PendingConversion {
    fn drop(&mut self) {
        if let Some(deliver) = self.deliver.take() {
            crate::cliplog!("a pending conversion was dropped; refusing it");
            deliver(None);
        }
    }
}

/// The queue itself: see the module documentation.
pub struct Jobs {
    queue: Mutex<VecDeque<Job>>,
    /// An eventfd the Wayland thread polls, so a posted job is picked up at
    /// once rather than at the end of the capture interval.
    wake: OwnedFd,
    /// Whether the Wayland thread is there to run them. With no compositor it
    /// never starts and nothing would ever drain the queue, so a job posted
    /// then is refused on the spot instead of waiting forever.
    running: AtomicBool,
}

/// A job the queue would not take, carried out of the lock before it is
/// dropped. See [`Jobs::post`].
enum Refused {
    Stopped(Job),
    Full(Job),
}

/// Takes out the jobs that have waited longer than [`QUEUE_TIMEOUT`]. Only a
/// conversion carries a deadline; a publication or a destroy is still worth
/// running whenever the Wayland thread gets to it.
fn drain_expired(queue: &mut VecDeque<Job>) -> Vec<Job> {
    let now = Instant::now();
    let mut kept = VecDeque::with_capacity(queue.len());
    let mut expired = Vec::new();
    for job in queue.drain(..) {
        match &job {
            Job::Receive { since, .. } if now.duration_since(*since) >= QUEUE_TIMEOUT => {
                expired.push(job);
            }
            _ => kept.push_back(job),
        }
    }
    *queue = kept;
    expired
}

impl Default for Jobs {
    fn default() -> Self {
        // SAFETY: eventfd takes no pointers; the fd is owned from here on
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(
            fd >= 0,
            "eventfd failed: {}",
            std::io::Error::last_os_error()
        );
        Self {
            queue: Mutex::new(VecDeque::new()),
            // SAFETY: a fresh, valid fd nobody else owns
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            running: AtomicBool::new(false),
        }
    }
}

impl Jobs {
    /// Posts `job` and wakes the Wayland thread, returning whether it was
    /// taken. A refused job is dropped here, on the posting thread, which for
    /// a conversion is what refuses the requestor.
    pub fn post(&self, job: Job) -> bool {
        // Refused and expired jobs travel out of the lock's scope and are
        // dropped below, after the guard: refusing a conversion takes the
        // requestor's property and outbox locks, and taking those under the
        // queue lock is the one lock-order inversion this design has to avoid.
        let mut refused = None;
        let mut stale = Vec::new();
        {
            let mut q = self.queue.lock().unwrap();
            // read under the queue lock, which `set_running` also holds while
            // it drains: against a stale `true` read outside it, a job can be
            // pushed after the final drain, into a queue nobody empties again
            if !self.running() {
                refused = Some(Refused::Stopped(job));
            } else {
                stale = drain_expired(&mut q);
                if q.len() >= MAX_JOBS {
                    refused = Some(Refused::Full(job));
                } else {
                    q.push_back(job);
                }
            }
        }
        drop(stale);
        match refused {
            Some(Refused::Stopped(job)) => {
                crate::cliplog!("no wayland thread to run a clipboard job; dropped");
                drop(job);
                return false;
            }
            Some(Refused::Full(job)) => {
                crate::warning!(
                    "the wayland thread is not keeping up with the clipboard; \
                                 dropping a job"
                );
                drop(job);
                return false;
            }
            None => {}
        }
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a u64 to an eventfd
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        true
    }

    /// Refuses whatever has waited past [`QUEUE_TIMEOUT`], answering the X
    /// clients behind it.
    ///
    /// Called from the X side on every `ConvertSelection`, because a client
    /// blocked on its own conversion issues no further requests: with only
    /// `post` sweeping, a lone RealVNC waiting on a wedged Wayland thread would
    /// never be answered by anyone.
    pub fn sweep(&self) {
        let stale = {
            let mut q = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drain_expired(&mut q)
        };
        drop(stale); // outside the lock: refusing takes the requestor's locks
    }

    /// Whether the Wayland thread is there to run jobs at all.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The fd to poll for posted jobs (Wayland thread).
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Clears the wake fd after it polled readable (Wayland thread).
    pub fn drain_wake(&self) {
        let mut buf = 0u64;
        // SAFETY: reading 8 bytes into a u64 from an eventfd; EAGAIN is fine
        unsafe {
            libc::read(
                self.wake.as_raw_fd(),
                (&mut buf as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// Takes everything posted so far, in the order it was posted (Wayland
    /// thread). The lock is released before any of it runs, so a job that
    /// posts another cannot deadlock.
    pub fn take(&self) -> Vec<Job> {
        self.queue.lock().unwrap().drain(..).collect()
    }

    /// Announces that the Wayland thread is (or is no longer) draining. On the
    /// way out, whatever is still queued is dropped, which refuses the
    /// conversions among it.
    pub fn set_running(&self, running: bool) {
        // stored and drained under the queue lock so that `post`, which reads
        // it under the same lock, cannot slip a job past the final drain
        let stale = {
            // not `unwrap`: this runs from the `Draining` guard while unwinding
            // a panic, and panicking here would abort the process
            let mut q = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.running.store(running, Ordering::Release);
            if running {
                Vec::new()
            } else {
                q.drain(..).collect::<Vec<_>>()
            }
        };
        drop(stale); // outside the lock, as in `post`
    }

    /// The publications posted so far, for the tests: the selection and the
    /// value's length, or `None` for a withdrawal.
    #[cfg(test)]
    pub fn publishes(&self) -> Vec<(Sel, Option<usize>)> {
        self.queue
            .lock()
            .unwrap()
            .iter()
            .filter_map(|j| match j {
                Job::Publish(sel, data) => Some((*sel, data.as_ref().map(|d| d.len()))),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    #[test]
    fn a_dropped_conversion_refuses_its_requestor() {
        // whatever becomes of a job, the client that asked must be answered:
        // an X requestor with no SelectionNotify waits for one forever
        let answered = Arc::new(StdMutex::new(None));
        let seen = answered.clone();
        let c = PendingConversion::new(move |data| *seen.lock().unwrap() = Some(data));
        drop(c);
        assert_eq!(*answered.lock().unwrap(), Some(None));
    }

    #[test]
    fn jobs_posted_before_the_wayland_thread_are_refused() {
        let jobs = Jobs::default();
        let answered = Arc::new(StdMutex::new(0usize));
        let seen = answered.clone();
        assert!(!jobs.post(Job::Receive {
            generation: 0,
            sel: Sel::Clipboard,
            mime: "text/plain".into(),
            since: Instant::now(),
            conversion: PendingConversion::new(move |_| *seen.lock().unwrap() += 1),
        }));
        assert_eq!(*answered.lock().unwrap(), 1, "the requestor was answered");
        jobs.set_running(true);
        assert!(jobs.post(Job::Publish(Sel::Clipboard, Some(b"hi".to_vec().into()))));
        assert_eq!(jobs.publishes(), [(Sel::Clipboard, Some(2))]);
    }

    #[test]
    fn a_job_that_waited_out_its_budget_is_refused() {
        // the Wayland thread is wedged rather than gone, so `running` stays
        // true and nothing drains: without a sweep the requestor waits for a
        // SelectionNotify that is never coming
        let jobs = Jobs::default();
        jobs.set_running(true);
        let answered = Arc::new(StdMutex::new(0usize));
        let seen = answered.clone();
        jobs.post(Job::Receive {
            sel: Sel::Clipboard,
            mime: "text/plain".into(),
            generation: 0,
            // `Instant` is time since boot here, so subtracting outright
            // panics on a host (or container) that has been up for less
            since: Instant::now()
                .checked_sub(QUEUE_TIMEOUT + Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            conversion: PendingConversion::new(move |_| *seen.lock().unwrap() += 1),
        });
        assert_eq!(
            *answered.lock().unwrap(),
            0,
            "not swept until someone looks"
        );
        // the X side sweeps on every ConvertSelection, which is what reaches a
        // client blocked on its own conversion; posting sweeps as well
        jobs.sweep();
        assert_eq!(
            *answered.lock().unwrap(),
            1,
            "the stale conversion was refused"
        );
        assert_eq!(jobs.take().len(), 0, "and is gone from the queue");
    }

    #[test]
    fn only_conversions_are_swept() {
        // a publish or a destroy carries no deadline — structurally, since
        // `Job` only records `since` for a conversion — so a sweep that finds
        // an expired conversion must leave them where they are
        let jobs = Jobs::default();
        jobs.set_running(true);
        jobs.post(Job::Publish(Sel::Clipboard, Some(b"hi".to_vec().into())));
        jobs.post(Job::Receive {
            sel: Sel::Clipboard,
            mime: "text/plain".into(),
            generation: 0,
            since: Instant::now()
                .checked_sub(QUEUE_TIMEOUT + Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            conversion: PendingConversion::new(|_| {}),
        });
        jobs.post(Job::Destroy(Vec::new()));
        jobs.sweep();
        assert_eq!(jobs.take().len(), 2, "the publish and the destroy survived");
    }

    #[test]
    fn a_full_queue_refuses_rather_than_grows() {
        let jobs = Jobs::default();
        jobs.set_running(true);
        for _ in 0..MAX_JOBS {
            assert!(jobs.post(Job::Destroy(Vec::new())));
        }
        assert!(!jobs.post(Job::Destroy(Vec::new())));
        assert_eq!(jobs.take().len(), MAX_JOBS);
        assert!(jobs.post(Job::Destroy(Vec::new())), "drained, so it fits");
    }

    #[test]
    fn stopping_refuses_what_is_left() {
        let jobs = Jobs::default();
        jobs.set_running(true);
        let answered = Arc::new(StdMutex::new(0usize));
        let seen = answered.clone();
        jobs.post(Job::Receive {
            generation: 0,
            sel: Sel::Primary,
            mime: "text/plain".into(),
            since: Instant::now(),
            conversion: PendingConversion::new(move |_| *seen.lock().unwrap() += 1),
        });
        assert_eq!(*answered.lock().unwrap(), 0, "still queued");
        jobs.set_running(false);
        assert_eq!(*answered.lock().unwrap(), 1);
        assert!(!jobs.post(Job::Destroy(Vec::new())));
    }
}

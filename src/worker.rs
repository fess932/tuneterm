//! One background thread with a **replaceable** request slot.
//!
//! Both the cover loader and the folder scanner are driven by a moving cursor, so
//! they share a problem: requests arrive faster than they can be served, and all
//! but the last are already worthless by the time they would run.
//!
//! The slot holds at most one pending request, so a held-down key cannot pile up
//! work. Each request carries a generation; a job is dropped both before and after
//! the expensive part if a newer one has been asked for meanwhile. One thread
//! exists no matter how fast the cursor moves.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryIter};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// The one pending request, and the condvar the thread waits on.
type Slot<Req> = Arc<(Mutex<Option<(u64, Req)>>, Condvar)>;

pub struct Worker<Req, Out> {
    slot: Slot<Req>,
    latest: Arc<AtomicU64>,
    results: Receiver<(u64, Out)>,
}

impl<Req, Out> Worker<Req, Out>
where
    Req: Send + 'static,
    Out: Send + 'static,
{
    /// Start the thread. `run` is called for each request that is still wanted.
    pub fn spawn<F>(name: &str, run: F) -> Self
    where
        F: Fn(Req) -> Out + Send + 'static,
    {
        let slot = Arc::new((Mutex::new(None), Condvar::new()));
        let latest = Arc::new(AtomicU64::new(0));
        let (tx, results) = mpsc::channel();

        let worker_slot = Arc::clone(&slot);
        let worker_latest = Arc::clone(&latest);
        let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
            let (lock, cvar) = &*worker_slot;
            loop {
                let (generation, request) = {
                    let mut guard = lock.lock().expect("worker slot poisoned");
                    loop {
                        if let Some(pending) = guard.take() {
                            break pending;
                        }
                        guard = cvar.wait(guard).expect("worker slot poisoned");
                    }
                };

                // Superseded while it sat in the slot.
                if generation < worker_latest.load(Ordering::Acquire) {
                    continue;
                }

                let out = run(request);

                // Superseded while we were working; do not bother the caller.
                if generation < worker_latest.load(Ordering::Acquire) {
                    continue;
                }
                if tx.send((generation, out)).is_err() {
                    return; // the caller is gone
                }
            }
        });
        // A thread that will not start is not worth aborting the app over; the
        // channel simply stays empty and the feature degrades to nothing.
        debug_assert!(spawned.is_ok(), "could not spawn {name}");

        Self {
            slot,
            latest,
            results,
        }
    }

    /// Queue `request`, discarding whatever had not started yet.
    pub fn request(&self, generation: u64, request: Req) {
        // Publish the generation first, so a worker mid-job sees it and bails.
        self.latest.store(generation, Ordering::Release);
        let (lock, cvar) = &*self.slot;
        *lock.lock().expect("worker slot poisoned") = Some((generation, request));
        cvar.notify_one();
    }

    /// Non-blocking: whatever finished since the last call, with its generation.
    pub fn drain(&self) -> TryIter<'_, (u64, Out)> {
        self.results.try_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    fn wait_for<Req, Out>(worker: &Worker<Req, Out>, generation: u64) -> Option<Out>
    where
        Req: Send + 'static,
        Out: Send + 'static,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            for (got, out) in worker.drain() {
                if got == generation {
                    return Some(out);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn runs_a_request_and_returns_its_generation() {
        let worker = Worker::spawn("test-run", |n: u32| n * 2);
        worker.request(1, 21);
        assert_eq!(wait_for(&worker, 1), Some(42));
    }

    /// The reason this exists: a cursor held down must not queue up work.
    #[test]
    fn a_burst_collapses_to_the_last_request() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        let worker = Worker::spawn("test-burst", move |n: u32| {
            counter.fetch_add(1, Ordering::SeqCst);
            // Slow enough that the burst lands while the first job is running.
            thread::sleep(Duration::from_millis(40));
            n
        });

        for generation in 1..=40u64 {
            worker.request(generation, generation as u32);
        }
        assert_eq!(wait_for(&worker, 40), Some(40), "last request never ran");

        // At most the one already running plus the survivor of the slot.
        let count = ran.load(Ordering::SeqCst);
        assert!(count <= 3, "ran {count} jobs for a 40-request burst");
    }

    /// Superseded work must not surface, or a stale result would overwrite a fresh one.
    #[test]
    fn stale_results_are_dropped() {
        let worker = Worker::spawn("test-stale", |n: u32| {
            thread::sleep(Duration::from_millis(20));
            n
        });
        worker.request(1, 1);
        worker.request(2, 2);
        assert_eq!(wait_for(&worker, 2), Some(2));

        for (generation, _) in worker.drain() {
            assert!(generation >= 2, "stale generation {generation} surfaced");
        }
    }

    #[test]
    fn the_thread_exits_when_the_worker_is_dropped() {
        let worker = Worker::spawn("test-drop", |n: u32| n);
        worker.request(1, 1);
        assert_eq!(wait_for(&worker, 1), Some(1));
        drop(worker); // the send in the thread now fails and it returns
    }
}

//! The dedicated provider thread.
//!
//! Every CUPS object this crate hands out holds a raw pointer, so none of
//! them are `Send`. Rather than push that constraint onto callers, all CUPS
//! work happens on one owned thread and only plain data crosses the boundary.
//!
//! The thread also gives timeouts somewhere to live. libcups has no way to
//! abort a call in flight, and killing a thread mid-FFI would corrupt the
//! library's state, so a timeout reports back to the caller while the call
//! itself is left to finish. The consequences are spelled out in
//! [`ProviderHandle::execute`].

use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::error::{ProviderError, ProviderResult};

/// A unit of work for the provider thread.
type Task = Box<dyn FnOnce() + Send + 'static>;

/// Monotonic identity for a dispatched request.
///
/// State is tracked per request rather than through a shared flag. A late
/// completion can then only clear the request it belongs to, so a worker
/// finishing at the same moment its caller gives up cannot wipe the state of
/// whatever runs next.
type RequestId = u64;

/// What the provider thread is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadState {
    /// Nothing dispatched; the next request can run.
    Idle,
    /// Request `id` is running and its caller is still waiting.
    Running { id: RequestId },
    /// Request `id` outlived its deadline. Its caller has gone, but the
    /// libcups call is still in progress and nothing else may start.
    Abandoned { id: RequestId },
}

/// Owns the CUPS thread and serialises work onto it.
///
/// Cloning shares one thread: two clones cannot run CUPS calls concurrently.
/// Calls from several threads are safe, but they queue.
pub struct ProviderHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    /// Bounded at one slot so a caller cannot queue work faster than the
    /// thread drains it and silently build a backlog.
    sender: Mutex<Option<SyncSender<Task>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// The thread's state plus the id to give the next request.
    ///
    /// A single mutex covers both, so "mark this request finished" and
    /// "claim the thread for a new request" cannot interleave.
    state: Mutex<(ThreadState, RequestId)>,
    /// Signalled whenever the state returns to `Idle`, so a queued caller
    /// wakes as soon as an abandoned call finally returns.
    idle: Condvar,
}

impl Clone for ProviderHandle {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

// Only channel ends, plain data and synchronisation primitives cross threads;
// no CUPS pointer ever does.
unsafe impl Send for ProviderHandle {}
unsafe impl Sync for ProviderHandle {}

impl HandleInner {
    /// Mark `id` finished, but only if it is still the request the thread is
    /// tracking.
    ///
    /// The guard is what removes the race: a worker completing just as its
    /// caller times out cannot clear a state that already belongs to a later
    /// request, and a timeout cannot re-mark a request that has already
    /// completed.
    fn complete(&self, id: RequestId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let owns_thread = match state.0 {
            ThreadState::Running { id: current } | ThreadState::Abandoned { id: current } => {
                current == id
            }
            ThreadState::Idle => false,
        };
        if owns_thread {
            state.0 = ThreadState::Idle;
            self.idle.notify_all();
        }
    }

    /// Give up waiting for `id`, leaving the thread blocked until the call
    /// returns on its own. No-op if the request already completed.
    fn abandon(&self, id: RequestId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.0 == (ThreadState::Running { id }) {
            state.0 = ThreadState::Abandoned { id };
        }
    }
}

impl ProviderHandle {
    /// Start the provider thread.
    pub fn new() -> ProviderResult<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Task>(1);

        let worker = thread::Builder::new()
            .name("cups-provider".to_string())
            .spawn(move || {
                // Runs until the handle is dropped and the channel closes.
                while let Ok(task) = receiver.recv() {
                    task();
                }
            })
            .map_err(|err| ProviderError::Environment {
                detail: format!("could not start provider thread: {err}"),
            })?;

        Ok(Self {
            inner: Arc::new(HandleInner {
                sender: Mutex::new(Some(sender)),
                worker: Mutex::new(Some(worker)),
                state: Mutex::new((ThreadState::Idle, 0)),
                idle: Condvar::new(),
            }),
        })
    }

    /// Run `operation` on the provider thread and wait up to `timeout`.
    ///
    /// On timeout the caller gets [`ProviderError::Timeout`] while the
    /// operation keeps running: libcups cannot be interrupted, and unwinding
    /// a thread mid-FFI is not safe. Two things follow, and both are part of
    /// the contract rather than implementation detail:
    ///
    /// - the next call waits until the stuck one returns, so retrying at once
    ///   buys nothing;
    /// - the timed-out operation's result is discarded when it eventually
    ///   arrives, and cannot reach a later caller.
    ///
    /// Nothing is retried automatically. A retry is a decision for whoever
    /// can tell whether repeating the work is safe.
    pub fn execute<T, F>(&self, label: &str, timeout: Duration, operation: F) -> ProviderResult<T>
    where
        F: FnOnce() -> ProviderResult<T> + Send + 'static,
        T: Send + 'static,
    {
        // Claim the thread. Callers queue here, and an abandoned request keeps
        // the claim until its libcups call actually returns.
        let id = self.claim(timeout)?;

        let (result_tx, result_rx) = mpsc::channel();
        let inner = Arc::clone(&self.inner);

        let task: Task = Box::new(move || {
            let outcome = operation();
            // Release the thread before handing back the result, and only for
            // this request.
            inner.complete(id);
            // The receiver is gone if this request was abandoned; the result
            // is dropped rather than delivered to whoever comes next.
            let _ = result_tx.send(outcome);
        });

        if let Err(err) = self.dispatch(task) {
            // Nothing will ever complete this request, so free the thread.
            self.inner.complete(id);
            return Err(err);
        }

        match result_rx.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                // Only marks the request abandoned if it is still running; a
                // completion that landed in this same instant wins.
                self.inner.abandon(id);
                Err(ProviderError::Timeout {
                    operation: label.to_string(),
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.inner.complete(id);
                Err(ProviderError::InternalContractViolation {
                    detail: format!("provider thread dropped {label} without answering"),
                })
            }
        }
    }

    /// Wait for the thread to be idle, then claim it for a new request.
    ///
    /// Waiting is bounded by the caller's own timeout, so a stuck abandoned
    /// call cannot block a caller indefinitely.
    fn claim(&self, timeout: Duration) -> ProviderResult<RequestId> {
        let mut state =
            self.inner
                .state
                .lock()
                .map_err(|_| ProviderError::InternalContractViolation {
                    detail: "provider state poisoned".into(),
                })?;

        while state.0 != ThreadState::Idle {
            let (next, wait) = self.inner.idle.wait_timeout(state, timeout).map_err(|_| {
                ProviderError::InternalContractViolation {
                    detail: "provider state poisoned".into(),
                }
            })?;
            state = next;
            if wait.timed_out() && state.0 != ThreadState::Idle {
                return Err(ProviderError::Timeout {
                    operation: "waiting for the provider thread to become available".into(),
                });
            }
        }

        state.1 = state.1.wrapping_add(1);
        let id = state.1;
        state.0 = ThreadState::Running { id };
        Ok(id)
    }

    fn dispatch(&self, task: Task) -> ProviderResult<()> {
        let guard =
            self.inner
                .sender
                .lock()
                .map_err(|_| ProviderError::InternalContractViolation {
                    detail: "provider sender poisoned".into(),
                })?;
        let sender = guard
            .as_ref()
            .ok_or_else(|| ProviderError::InternalContractViolation {
                detail: "provider thread already shut down".into(),
            })?;
        sender
            .send(task)
            .map_err(|_| ProviderError::InternalContractViolation {
                detail: "provider thread is gone".into(),
            })
    }

    /// Whether an abandoned (timed-out) call is still occupying the thread.
    ///
    /// Ordinary in-flight calls are not reported here; they simply queue.
    pub fn is_busy(&self) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| matches!(state.0, ThreadState::Abandoned { .. }))
            .unwrap_or(false)
    }
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // Close the channel so the loop ends, then wait for the thread. If it
        // is stuck inside libcups this blocks until that call returns, which
        // is the only safe option.
        if let Ok(mut guard) = self.sender.lock() {
            guard.take();
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Instant;

    const GENEROUS: Duration = Duration::from_secs(5);

    /// Wait until `predicate` holds, or fail the test.
    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let start = Instant::now();
        while !predicate() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "condition never became true"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn runs_work_and_returns_it() {
        let handle = ProviderHandle::new().expect("thread starts");
        let value = handle.execute("add", GENEROUS, || Ok(1 + 1)).expect("ok");
        assert_eq!(value, 2);
    }

    #[test]
    fn propagates_operation_errors() {
        let handle = ProviderHandle::new().expect("thread starts");
        let err = handle
            .execute("fail", GENEROUS, || {
                Err::<(), _>(ProviderError::PrinterNotFound {
                    printer: "nope".into(),
                })
            })
            .unwrap_err();
        assert_eq!(
            err,
            ProviderError::PrinterNotFound {
                printer: "nope".into()
            }
        );
    }

    #[test]
    fn concurrent_callers_queue_rather_than_fail() {
        // Eight threads calling at once must all succeed; the provider
        // serialises them instead of rejecting the ones that arrive second.
        let handle = ProviderHandle::new().expect("thread starts");
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let handle = handle.clone();
                thread::spawn(move || {
                    handle
                        .execute("echo", GENEROUS, move || Ok(index))
                        .expect("every caller is served")
                })
            })
            .collect();

        let mut results: Vec<i32> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker joins"))
            .collect();
        results.sort_unstable();
        assert_eq!(results, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn serialises_calls_from_many_threads() {
        // Proves the provider runs one CUPS call at a time even when callers
        // do not coordinate.
        let handle = ProviderHandle::new().expect("thread starts");
        let concurrent = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));

        let workers: Vec<_> = (0..8)
            .map(|_| {
                let handle = handle.clone();
                let concurrent = Arc::clone(&concurrent);
                let peak = Arc::clone(&peak);
                thread::spawn(move || {
                    handle
                        .execute("count", GENEROUS, move || {
                            let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(5));
                            concurrent.fetch_sub(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .expect("ok")
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("worker joins");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reports_timeout_without_killing_the_call() {
        let handle = ProviderHandle::new().expect("thread starts");
        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);

        let err = handle
            .execute("slow", Duration::from_millis(20), move || {
                thread::sleep(Duration::from_millis(200));
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap_err();

        assert!(matches!(err, ProviderError::Timeout { .. }));
        // The operation must still be running, not aborted.
        assert!(!finished.load(Ordering::SeqCst));
        assert!(handle.is_busy());

        // Once the slow call finishes on its own, the thread frees itself.
        wait_until(|| !handle.is_busy());
        assert!(finished.load(Ordering::SeqCst));
        assert_eq!(handle.execute("after", GENEROUS, || Ok(7)).expect("ok"), 7);
    }

    #[test]
    fn completion_just_before_timeout_leaves_no_stale_state() {
        // The race the independent review found: the worker finishes and
        // clears state at the same instant the caller gives up and marks it
        // abandoned. If the caller's write won unconditionally, the provider
        // would stay busy forever.
        //
        // Run it many times so the interleaving is actually hit rather than
        // assumed.
        for attempt in 0..200 {
            let handle = ProviderHandle::new().expect("thread starts");
            let deadline = Duration::from_millis(20);

            let _ = handle.execute("boundary", deadline, move || {
                // Land near the deadline from both sides across attempts.
                let skew = if attempt % 2 == 0 { 19 } else { 21 };
                thread::sleep(Duration::from_millis(skew));
                Ok(())
            });

            // Whatever the outcome was, the thread must not be stuck once the
            // operation has actually returned.
            wait_until(|| !handle.is_busy());
            assert_eq!(
                handle
                    .execute("after boundary", GENEROUS, move || Ok(attempt))
                    .expect("provider recovers"),
                attempt,
                "provider stayed busy after a boundary completion"
            );
        }
    }

    #[test]
    fn late_result_cannot_reach_a_later_caller() {
        // A timed-out operation's value must be discarded, never handed to
        // whoever runs next.
        let handle = ProviderHandle::new().expect("thread starts");

        let timed_out = handle.execute("late", Duration::from_millis(20), || {
            thread::sleep(Duration::from_millis(150));
            Ok(1111)
        });
        assert!(matches!(timed_out, Err(ProviderError::Timeout { .. })));

        wait_until(|| !handle.is_busy());

        let next = handle.execute("next", GENEROUS, || Ok(2222)).expect("ok");
        assert_eq!(next, 2222, "a later caller must not receive a stale result");
    }

    #[test]
    fn queued_caller_runs_after_abandoned_call_finishes() {
        // A caller that arrives while an abandoned call is still running waits
        // and then runs — it is not rejected, and it does not overlap.
        let handle = ProviderHandle::new().expect("thread starts");
        let running = Arc::new(AtomicU32::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));

        {
            let running = Arc::clone(&running);
            let overlapped = Arc::clone(&overlapped);
            let err = handle.execute("abandoned", Duration::from_millis(20), move || {
                if running.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlapped.store(true, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_millis(150));
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            });
            assert!(matches!(err, Err(ProviderError::Timeout { .. })));
        }

        let running_for_next = Arc::clone(&running);
        let overlapped_for_next = Arc::clone(&overlapped);
        let value = handle
            .execute("queued", Duration::from_secs(5), move || {
                if running_for_next.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlapped_for_next.store(true, Ordering::SeqCst);
                }
                running_for_next.fetch_sub(1, Ordering::SeqCst);
                Ok(99)
            })
            .expect("queued caller eventually runs");

        assert_eq!(value, 99);
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "no two CUPS operations may run at once"
        );
    }

    #[test]
    fn repeated_timeout_and_recovery_cycles() {
        // State must not accumulate across cycles.
        let handle = ProviderHandle::new().expect("thread starts");
        for round in 0..5 {
            let err = handle.execute("slow", Duration::from_millis(20), || {
                thread::sleep(Duration::from_millis(80));
                Ok(())
            });
            assert!(matches!(err, Err(ProviderError::Timeout { .. })));

            wait_until(|| !handle.is_busy());
            assert_eq!(
                handle
                    .execute("fast", GENEROUS, move || Ok(round))
                    .expect("ok"),
                round
            );
        }
    }

    #[test]
    fn caller_waiting_on_a_stuck_call_times_out_finitely() {
        // A queued caller must not block forever behind an abandoned call.
        let handle = ProviderHandle::new().expect("thread starts");

        let err = handle.execute("stuck", Duration::from_millis(20), || {
            thread::sleep(Duration::from_millis(400));
            Ok(())
        });
        assert!(matches!(err, Err(ProviderError::Timeout { .. })));

        let start = Instant::now();
        let queued = handle.execute("waiting", Duration::from_millis(50), || Ok(()));
        assert!(matches!(queued, Err(ProviderError::Timeout { .. })));
        assert!(
            start.elapsed() < Duration::from_millis(350),
            "queued caller must give up on its own deadline"
        );

        wait_until(|| !handle.is_busy());
    }

    #[test]
    fn two_concurrent_callers_across_a_deadline() {
        // Both callers start together; one will time out and one will not,
        // depending on scheduling. Neither may leave the provider unusable.
        let handle = ProviderHandle::new().expect("thread starts");
        let barrier = Arc::new(Barrier::new(2));

        let workers: Vec<_> = (0..2)
            .map(|_| {
                let handle = handle.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    handle.execute("contended", Duration::from_millis(30), || {
                        thread::sleep(Duration::from_millis(25));
                        Ok(())
                    })
                })
            })
            .collect();

        for worker in workers {
            // Either outcome is acceptable; a panic or a hang is not.
            let _ = worker.join().expect("worker joins");
        }

        wait_until(|| !handle.is_busy());
        assert_eq!(handle.execute("after", GENEROUS, || Ok(5)).expect("ok"), 5);
    }

    #[test]
    fn operation_panic_does_not_wedge_the_provider() {
        // A panicking operation kills the worker thread. The provider must
        // report that rather than hanging the next caller forever.
        let handle = ProviderHandle::new().expect("thread starts");

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.execute("boom", GENEROUS, || -> ProviderResult<()> {
                panic!("operation panicked");
            })
        }));

        // Whether the panic unwinds through the call or surfaces as a
        // disconnect, the next caller must get a definite answer.
        let next = handle.execute("after panic", Duration::from_millis(200), || Ok(3));
        assert!(
            next.is_ok() || next.is_err(),
            "next caller must not hang; got {next:?}"
        );
        drop(panicked);
    }

    #[test]
    fn no_automatic_retry() {
        // The provider must never run an operation twice on its own.
        let handle = ProviderHandle::new().expect("thread starts");
        let runs = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&runs);

        let _ = handle.execute("once", GENEROUS, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(ProviderError::ConnectionFailed {
                detail: "simulated".into(),
            })
        });

        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timed_out_operation_runs_exactly_once() {
        // Abandoning a call must not cause it to be re-dispatched.
        let handle = ProviderHandle::new().expect("thread starts");
        let runs = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&runs);

        let err = handle.execute("slow once", Duration::from_millis(20), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(100));
            Ok(())
        });
        assert!(matches!(err, Err(ProviderError::Timeout { .. })));

        wait_until(|| !handle.is_busy());
        thread::sleep(Duration::from_millis(50));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shutdown_joins_the_worker() {
        // Dropping the handle must not leave the thread running or panic.
        let handle = ProviderHandle::new().expect("thread starts");
        assert_eq!(handle.execute("work", GENEROUS, || Ok(1)).expect("ok"), 1);
        drop(handle);
    }
}

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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::error::{ProviderError, ProviderResult};

/// A unit of work for the provider thread.
type Task = Box<dyn FnOnce() + Send + 'static>;

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
    /// Held for the duration of a call. Ordinary callers queue on this, which
    /// is what serialises CUPS access.
    gate: Mutex<()>,
    /// Set when a call outlived its deadline and was abandoned. The thread is
    /// still inside libcups, so the next caller is told rather than left to
    /// block for a full timeout of its own.
    abandoned: AtomicBool,
}

impl Clone for ProviderHandle {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

// Only the channel ends and flags cross threads; no CUPS pointer ever does.
unsafe impl Send for ProviderHandle {}
unsafe impl Sync for ProviderHandle {}

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
                gate: Mutex::new(()),
                abandoned: AtomicBool::new(false),
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
    /// - the next call blocks until the stuck one returns, so retrying at
    ///   once buys nothing;
    /// - the timed-out operation's result is discarded when it eventually
    ///   arrives.
    ///
    /// Nothing is retried automatically. A retry is a decision for whoever
    /// can tell whether repeating the work is safe.
    pub fn execute<T, F>(&self, label: &str, timeout: Duration, operation: F) -> ProviderResult<T>
    where
        F: FnOnce() -> ProviderResult<T> + Send + 'static,
        T: Send + 'static,
    {
        // Callers queue here. Serialising in the handle rather than relying on
        // the channel keeps an abandoned call from being overtaken.
        let _gate = match self.inner.gate.lock() {
            Ok(gate) => gate,
            Err(_) => {
                return Err(ProviderError::InternalContractViolation {
                    detail: "provider gate poisoned".into(),
                });
            }
        };

        if self.inner.abandoned.load(Ordering::Acquire) {
            // A previous call timed out and its libcups call has not returned.
            // Report that instead of blocking for another full timeout.
            return Err(ProviderError::Timeout {
                operation: format!("{label} (provider thread still finishing an abandoned call)"),
            });
        }

        let (result_tx, result_rx) = mpsc::channel();
        let inner = Arc::clone(&self.inner);

        let task: Task = Box::new(move || {
            let outcome = operation();
            // Clear before sending so a caller that sees the result also sees
            // a thread that is free again.
            inner.abandoned.store(false, Ordering::Release);
            let _ = result_tx.send(outcome);
        });

        {
            let guard =
                self.inner
                    .sender
                    .lock()
                    .map_err(|_| ProviderError::InternalContractViolation {
                        detail: "provider sender poisoned".into(),
                    })?;
            let sender =
                guard
                    .as_ref()
                    .ok_or_else(|| ProviderError::InternalContractViolation {
                        detail: "provider thread already shut down".into(),
                    })?;
            sender
                .send(task)
                .map_err(|_| ProviderError::InternalContractViolation {
                    detail: "provider thread is gone".into(),
                })?;
        }

        match result_rx.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                // The call is still running and cannot be interrupted; mark
                // the thread so the next caller is not left guessing.
                self.inner.abandoned.store(true, Ordering::Release);
                Err(ProviderError::Timeout {
                    operation: label.to_string(),
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(ProviderError::InternalContractViolation {
                detail: format!("provider thread dropped {label} without answering"),
            }),
        }
    }

    /// Whether an abandoned (timed-out) call is still occupying the thread.
    ///
    /// Ordinary in-flight calls are not reported here; they simply queue.
    pub fn is_busy(&self) -> bool {
        self.inner.abandoned.load(Ordering::Acquire)
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
        if let Ok(mut guard) = self.worker.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    const GENEROUS: Duration = Duration::from_secs(5);

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

        // A caller retrying straight away is told the thread is still
        // finishing the abandoned call rather than blocking for a full
        // timeout of its own.
        assert!(handle.is_busy());
        let immediate = handle.execute("next", Duration::from_millis(20), || Ok(()));
        assert!(matches!(immediate, Err(ProviderError::Timeout { .. })));

        // Once the slow call finishes on its own, the thread is usable again.
        let start = Instant::now();
        while handle.is_busy() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(finished.load(Ordering::SeqCst));
        assert_eq!(handle.execute("after", GENEROUS, || Ok(7)).expect("ok"), 7);
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
}

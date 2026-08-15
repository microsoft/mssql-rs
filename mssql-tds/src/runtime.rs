// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime interoperability for synchronous consumers.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use tokio::runtime::{Handle, Runtime};

/// Failure to start synchronous execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SyncFirstError {
    /// The calling thread already has an entered Tokio runtime context.
    #[error("sync-first execution requires a thread without an entered Tokio runtime context")]
    RuntimeContext,
}

/// Runs a future synchronously, entering Tokio only when the first poll suspends.
///
/// The future is pinned once and polled under `runtime` with the standard
/// library's no-op waker. A ready future returns directly. A pending future is
/// passed, still pinned and without reconstruction, to [`Runtime::block_on`],
/// whose first poll replaces the temporary waker with the runtime's task waker.
///
/// This is useful at synchronous FFI boundaries where protocol work commonly
/// completes from bytes already buffered by the transport. It does not change
/// the future's cancellation, timeout, or I/O behavior after suspension.
///
/// # Errors
///
/// Returns [`SyncFirstError::RuntimeContext`] before polling when
/// [`Handle::try_current`] finds an entered Tokio runtime context. This is
/// deliberately stricter than [`Runtime::block_on`]: Tokio blocking-pool threads
/// also carry an entered handle, even though they may call `block_on`. Callers
/// already using Tokio should stay async instead; synchronous FFI callers should
/// invoke this function from their native, unentered thread.
///
/// Rejecting every entered context up front guarantees a stateful protocol
/// future is never partially advanced before the fallback execution policy is
/// known to be valid.
#[inline]
pub fn block_on_sync_first<F>(runtime: &Runtime, future: F) -> Result<F::Output, SyncFirstError>
where
    F: Future,
{
    if Handle::try_current().is_ok() {
        return Err(SyncFirstError::RuntimeContext);
    }

    let mut future = std::pin::pin!(future);
    let first_poll = {
        let _guard = runtime.enter();
        let mut context = Context::from_waker(Waker::noop());
        future.as_mut().poll(&mut context)
    };

    match first_poll {
        Poll::Ready(output) => Ok(output),
        Poll::Pending => Ok(runtime.block_on(future)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CancelHandle, TdsResult};
    use crate::error::Error;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Waker;
    use std::time::Duration;

    struct PendingThenReady {
        address: Option<usize>,
        first_waker: Option<Waker>,
        polls: Arc<AtomicUsize>,
    }

    impl Future for PendingThenReady {
        type Output = usize;

        fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
            let address = (&*self as *const Self) as usize;
            let poll = self.polls.fetch_add(1, Ordering::Relaxed) + 1;
            match self.address {
                None => {
                    self.address = Some(address);
                    self.first_waker = Some(context.waker().clone());
                    Poll::Pending
                }
                Some(first_address) => {
                    assert_eq!(
                        address, first_address,
                        "the pending future must not be reconstructed or moved"
                    );
                    assert!(
                        !self
                            .first_waker
                            .as_ref()
                            .expect("first poll stores its waker")
                            .will_wake(context.waker()),
                        "Runtime::block_on must replace the temporary no-op waker"
                    );
                    Poll::Ready(poll)
                }
            }
        }
    }

    struct CountPolls<'a>(&'a AtomicUsize);

    impl Future for CountPolls<'_> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(())
        }
    }

    fn runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn ready_future_completes_on_first_poll() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime without I/O or time drivers");

        let output =
            block_on_sync_first(&runtime, std::future::ready(42)).expect("outside runtime");

        assert_eq!(output, 42);
    }

    #[test]
    fn pending_future_resumes_with_runtime_waker_without_reconstruction() {
        let runtime = runtime();
        let polls = Arc::new(AtomicUsize::new(0));
        let future = PendingThenReady {
            address: None,
            first_waker: None,
            polls: Arc::clone(&polls),
        };

        let output = block_on_sync_first(&runtime, future).expect("outside runtime");

        assert_eq!(output, 2);
        assert_eq!(polls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pending_future_resumes_on_current_thread_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let polls = Arc::new(AtomicUsize::new(0));
        let future = PendingThenReady {
            address: None,
            first_waker: None,
            polls: Arc::clone(&polls),
        };

        let output = block_on_sync_first(&runtime, future).expect("outside runtime");

        assert_eq!(output, 2);
        assert_eq!(polls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn cancellation_survives_the_waker_handoff() {
        let runtime = runtime();
        let cancel = CancelHandle::new();
        let child = cancel.child_handle();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            cancel.cancel();
        });
        let pending = std::future::pending::<TdsResult<()>>();

        let result = block_on_sync_first(
            &runtime,
            CancelHandle::run_until_cancelled(Some(&child), pending),
        )
        .expect("outside runtime");
        canceller.join().expect("canceller thread");

        assert!(matches!(result, Err(Error::OperationCancelledError(_))));
    }

    #[test]
    fn exhausted_timeout_survives_the_waker_handoff() {
        let runtime = runtime();
        let future =
            async { tokio::time::timeout(Duration::ZERO, std::future::pending::<()>()).await };

        let result = block_on_sync_first(&runtime, future).expect("outside runtime");

        assert!(
            result.is_err(),
            "a pending future must not evade a zero timeout"
        );
    }

    #[test]
    fn nested_runtime_is_rejected_before_polling() {
        let runtime = runtime();
        let polls = AtomicUsize::new(0);

        runtime.block_on(async {
            let error = block_on_sync_first(&runtime, CountPolls(&polls))
                .expect_err("nested execution must be rejected");
            assert_eq!(error, SyncFirstError::RuntimeContext);
        });

        assert_eq!(
            polls.load(Ordering::Relaxed),
            0,
            "nested rejection must happen before a stateful future advances"
        );
    }

    #[test]
    fn blocking_pool_runtime_context_is_rejected_before_polling() {
        let runtime = Arc::new(runtime());
        let worker_runtime = Arc::clone(&runtime);
        let polls = Arc::new(AtomicUsize::new(0));
        let worker_polls = Arc::clone(&polls);

        let error = runtime.block_on(async move {
            tokio::task::spawn_blocking(move || {
                block_on_sync_first(&worker_runtime, CountPolls(&worker_polls))
                    .expect_err("blocking-pool threads carry an entered runtime handle")
            })
            .await
            .expect("blocking task")
        });

        assert_eq!(error, SyncFirstError::RuntimeContext);
        assert_eq!(
            polls.load(Ordering::Relaxed),
            0,
            "context rejection must happen before a stateful future advances"
        );
    }
}

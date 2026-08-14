// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Lazy arming of the remaining-request-timeout budget.
//!
//! See [`await_within_request_timeout`].

/// Awaits `$fut` under the remaining request budget `$budget`
/// (an `Option<Duration>`), arming a timer only if `$fut` suspends.
///
/// `tokio::time::Timeout::poll` polls the inner future *before* the delay, so a
/// future that is ready on its first poll returns `Ok` without the timer ever
/// being observed. Polling once up front and only then constructing the
/// `timeout` is therefore observationally identical, and skips the cost of
/// arming a timer for every row served from an already-buffered packet (#271).
///
/// That cost is not a timer-wheel registration: on the ready path the eager
/// form never polls the `Sleep`, so no wheel entry is ever created. It is the
/// `Instant::now()` clock read plus building the `Sleep`, and — because
/// `timeout` takes its future by value — moving the multi-kilobyte row future
/// into `Timeout<F>`. Pinning first and passing `Pin<&mut F>` avoids the move
/// as well as the timer.
///
/// Because that dominant term scales with the size of the inner future, an
/// isolated micro-benchmark on a trivial future understates it. In a paired
/// A/B benchmark after the packet-reader de-boxing merged in #264, this saves
/// ~254 ns per 48-column row, about 15% of the isolated decode cost.
///
/// The condition is *suspension*, deliberately not "the budget is zero".
/// `update_remaining_timeout` saturates to `Duration::ZERO` rather than `None`,
/// so an exhausted budget still arrives as `Some(ZERO)`; skipping the timer for
/// it would turn a fetch that must fail with `Elapsed` into an unbounded wait.
///
/// This is a macro rather than a function so that it expands at the call site.
/// The same logic behind an `async fn` measures ~4% slower, because an `async
/// fn` that awaits inside one match arm stores that arm's future in its state
/// machine, growing the per-row future and paying a memcpy on every move.
///
/// On the suspending path `$fut` is polled once more than it would be today
/// (once by this macro, then again by `Timeout`'s first poll) and the deadline
/// starts from after that poll rather than before it. Both are permitted:
/// futures must tolerate spurious polls, and the shift is one poll of CPU time
/// against a budget measured in seconds. It does not change the verdict on an
/// exhausted budget either, because `sleep(ZERO)` is itself `Pending` on its
/// first poll, so eager `timeout` grants that same extra poll.
macro_rules! await_within_request_timeout {
    ($budget:expr, $fut:expr) => {{
        let mut fut = ::std::pin::pin!($fut);
        match $budget {
            Some(budget) => {
                let first = ::std::future::poll_fn(|cx| {
                    ::std::task::Poll::Ready(::std::future::Future::poll(fut.as_mut(), cx))
                })
                .await;
                match first {
                    ::std::task::Poll::Ready(result) => result,
                    ::std::task::Poll::Pending => match ::tokio::time::timeout(budget, fut).await {
                        Ok(result) => result,
                        Err(elapsed) => Err($crate::error::Error::TimeoutError(
                            $crate::error::TimeoutErrorType::Elapsed(elapsed),
                        )),
                    },
                }
            }
            None => fut.await,
        }
    }};
}

pub(crate) use await_within_request_timeout;

#[cfg(test)]
mod tests {
    use crate::core::TdsResult;
    use crate::error::Error;
    use std::future::{Future, ready};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// Suspends forever, counting polls so tests can prove the macro polls the
    /// inner future rather than short-circuiting on the budget.
    struct NeverReady<'a>(&'a AtomicUsize);

    impl Future for NeverReady<'_> {
        type Output = TdsResult<u8>;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Poll::Pending
        }
    }

    fn is_elapsed<T>(result: &TdsResult<T>) -> bool {
        matches!(
            result,
            Err(Error::TimeoutError(
                crate::error::TimeoutErrorType::Elapsed(_)
            ))
        )
    }

    /// The trap #271 documents: an exhausted budget arrives as `Some(ZERO)`,
    /// not `None`. Keying the arming decision on the budget being zero rather
    /// than on suspension inverts the second case here into an unbounded wait.
    #[tokio::test]
    async fn exhausted_budget_succeeds_when_ready_and_elapses_when_suspending() {
        let zero = Some(Duration::ZERO);

        let ready_result: TdsResult<u8> =
            await_within_request_timeout!(zero, ready(Ok::<u8, Error>(7)));
        assert_eq!(
            ready_result.expect("ready-on-first-poll must succeed on a zero budget"),
            7,
            "matches tokio: Timeout::poll polls the inner future before the delay"
        );

        let polls = AtomicUsize::new(0);
        let suspend_result = await_within_request_timeout!(zero, NeverReady(&polls));
        assert!(
            is_elapsed(&suspend_result),
            "lazy arming must not turn an exhausted budget into an unbounded wait"
        );
        assert!(
            polls.load(Ordering::Relaxed) >= 1,
            "the inner future must be polled, not skipped on a zero budget"
        );
    }

    /// Same two cases through `tokio::time::timeout` directly, pinning the
    /// macro's behaviour to the combinator it replaces rather than to an
    /// independently asserted expectation.
    #[tokio::test]
    async fn matches_eager_timeout_on_exhausted_budget() {
        let zero = Duration::ZERO;
        let polls = AtomicUsize::new(0);

        assert!(tokio::time::timeout(zero, ready(7u8)).await.is_ok());
        assert!(
            tokio::time::timeout(zero, NeverReady(&polls))
                .await
                .is_err(),
            "baseline: eager timeout elapses on a suspending future"
        );
    }

    /// Suspends on its first poll, then completes. Wakes itself so the suspend
    /// is not a deadlock, mirroring a decode that stalls and is immediately
    /// resumable.
    struct PendingThenReady(bool);

    impl Future for PendingThenReady {
        type Output = TdsResult<u8>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0 {
                return Poll::Ready(Ok(5));
            }
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    /// Review raised this as a divergence: probing before arming grants the
    /// inner future a poll that `Timeout` supposedly withholds, so a future
    /// suspending once on a spent budget should elapse eagerly but succeed
    /// lazily. Measured, it does not. `sleep(ZERO)` returns `Pending` on its
    /// first poll and only elapses after a timer wake, so `Timeout`'s first
    /// poll suspends too and the self-wake lets the inner future win in both
    /// forms. Asserted as equivalence rather than a fixed outcome, so the two
    /// stay pinned together if tokio ever changes that.
    #[tokio::test]
    async fn suspend_then_complete_on_a_spent_budget_matches_eager_timeout() {
        let eager = tokio::time::timeout(Duration::ZERO, PendingThenReady(false)).await;
        let lazy = await_within_request_timeout!(Some(Duration::ZERO), PendingThenReady(false));

        assert_eq!(
            eager.is_err(),
            is_elapsed(&lazy),
            "lazy arming must reach the same verdict as eager timeout"
        );
        assert!(
            eager.is_ok() && !is_elapsed(&lazy),
            "both forms let the future complete: sleep(ZERO) is Pending on its first poll"
        );
        assert_eq!(
            lazy.expect("completed on the re-poll"),
            5,
            "and both yield the value the future produced"
        );
    }

    #[tokio::test]
    async fn live_budget_still_elapses_on_a_suspending_future() {
        let polls = AtomicUsize::new(0);
        let result =
            await_within_request_timeout!(Some(Duration::from_millis(20)), NeverReady(&polls));

        assert!(is_elapsed(&result));
    }

    #[tokio::test]
    async fn absent_budget_awaits_without_a_timer() {
        let result: TdsResult<u8> =
            await_within_request_timeout!(None::<Duration>, ready(Ok::<u8, Error>(9)));

        assert_eq!(result.expect("no budget means no timeout"), 9);
    }

    /// A future that suspends once and then completes must still succeed: the
    /// macro hands it to `timeout`, which re-polls it after the wake.
    #[tokio::test]
    async fn suspending_future_that_completes_is_not_cut_short() {
        let result: TdsResult<u8> = await_within_request_timeout!(
            Some(Duration::from_secs(30)),
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok::<u8, Error>(3)
            })
        );

        assert_eq!(result.expect("completed inside the budget"), 3);
    }
}

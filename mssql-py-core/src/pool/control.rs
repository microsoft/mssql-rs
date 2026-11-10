// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::config::validate_duration;
use super::error::{AcquireStage, PoolError, PoolResult};

// Every acquisition stage shares this deadline; stage labels never restart it.
pub(crate) struct AcquireControl {
    deadline: Instant,
    cancel: CancellationToken,
}

impl AcquireControl {
    pub(crate) fn new(deadline: Instant, cancel: CancellationToken) -> Self {
        Self { deadline, cancel }
    }

    pub(crate) fn from_timeout(
        now: Instant,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> PoolResult<Self> {
        validate_duration("acquire_timeout", timeout, false)?;
        let deadline = now.checked_add(timeout).ok_or(PoolError::InvalidConfig {
            field: "acquire_timeout",
            reason: "exceeds the monotonic clock range",
        })?;
        Ok(Self::new(deadline, cancel))
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    pub(crate) fn check(&self, stage: AcquireStage) -> PoolResult<()> {
        if self.cancel.is_cancelled() {
            return Err(PoolError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(PoolError::AcquireTimeout { stage });
        }
        Ok(())
    }

    /// Drops caller-owned work on interruption; detached tasks still need their
    /// own cancellation/join guards, as do TDS CancelHandles and Python tasks.
    pub(crate) async fn run<T>(
        &self,
        stage: AcquireStage,
        work: impl Future<Output = PoolResult<T>>,
    ) -> PoolResult<T> {
        self.check(stage)?;
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(PoolError::Cancelled),
            _ = tokio::time::sleep_until(self.deadline) => Err(PoolError::AcquireTimeout { stage }),
            result = work => {
                self.check(stage)?;
                result
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::{Context, Poll, Waker};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn sequential_stages_share_one_absolute_deadline() {
        let control = AcquireControl::from_timeout(
            Instant::now(),
            Duration::from_secs(10),
            CancellationToken::new(),
        )
        .unwrap();
        let original = control.deadline();
        control
            .run(AcquireStage::Wait, async {
                tokio::time::sleep(Duration::from_secs(6)).await;
                Ok(())
            })
            .await
            .unwrap();
        let result = control
            .run(AcquireStage::Open, async {
                tokio::time::sleep(Duration::from_secs(6)).await;
                Ok(())
            })
            .await;
        assert!(matches!(
            result,
            Err(PoolError::AcquireTimeout {
                stage: AcquireStage::Open
            })
        ));
        assert_eq!(Instant::now(), original);
        assert_eq!(control.deadline(), original);
    }

    #[derive(Debug)]
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    async fn observed_work(polled: &AtomicBool) -> PoolResult<()> {
        polled.store(true, Ordering::SeqCst);
        Ok(())
    }

    #[tokio::test]
    async fn explicit_cancellation_drops_work_and_preserves_cancellation() {
        let token = CancellationToken::new();
        let control =
            AcquireControl::from_timeout(Instant::now(), Duration::from_secs(10), token.clone())
                .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropProbe(dropped.clone());
        let mut work = Box::pin(control.run(AcquireStage::Open, async move {
            let _guard = guard;
            std::future::pending::<PoolResult<()>>().await
        }));
        assert!(
            work.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        token.cancel();
        assert!(control.cancellation().is_cancelled());
        assert!(matches!(
            work.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(PoolError::Cancelled))
        ));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_and_cancelled_controls_do_not_poll_work() {
        let polled = AtomicBool::new(false);
        let active = AcquireControl::from_timeout(
            Instant::now(),
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .unwrap();
        active
            .run(AcquireStage::Admission, observed_work(&polled))
            .await
            .unwrap();
        assert!(polled.swap(false, Ordering::SeqCst));
        let token = CancellationToken::new();
        let control = AcquireControl::new(Instant::now(), token.clone());
        assert!(matches!(
            control
                .run(AcquireStage::Admission, observed_work(&polled))
                .await,
            Err(PoolError::AcquireTimeout {
                stage: AcquireStage::Admission
            })
        ));
        assert!(!polled.load(Ordering::SeqCst));
        token.cancel();
        assert!(matches!(
            control
                .run(AcquireStage::Admission, observed_work(&polled))
                .await,
            Err(PoolError::Cancelled)
        ));
        assert!(!polled.load(Ordering::SeqCst));
        for invalid in [Duration::ZERO, Duration::MAX] {
            assert!(matches!(
                AcquireControl::from_timeout(Instant::now(), invalid, CancellationToken::new()),
                Err(PoolError::InvalidConfig {
                    field: "acquire_timeout",
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn cancellation_during_the_final_work_poll_prevents_publication() {
        let token = CancellationToken::new();
        let control =
            AcquireControl::from_timeout(Instant::now(), Duration::from_secs(10), token.clone())
                .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let result_guard = DropProbe(dropped.clone());
        assert!(matches!(
            control
                .run(AcquireStage::Delivery, async {
                    token.cancel();
                    Ok(result_guard)
                })
                .await,
            Err(PoolError::Cancelled)
        ));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_drops_incomplete_work_before_returning() {
        let control = AcquireControl::from_timeout(
            Instant::now(),
            Duration::from_nanos(1),
            CancellationToken::new(),
        )
        .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropProbe(dropped.clone());
        let result = control
            .run(AcquireStage::Reset, async move {
                let _guard = guard;
                std::future::pending::<PoolResult<()>>().await
            })
            .await;
        assert!(matches!(
            result,
            Err(PoolError::AcquireTimeout {
                stage: AcquireStage::Reset
            })
        ));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn shifted_start_instant_cannot_overflow_the_absolute_deadline() {
        let now = Instant::now();
        let (shifted, timeout) = (0..64)
            .find_map(|power| {
                let timeout = Duration::from_secs(1_u64 << power);
                let shifted = now.checked_add(timeout)?;
                shifted
                    .checked_add(timeout)
                    .is_none()
                    .then_some((shifted, timeout))
            })
            .expect("a finite monotonic clock has an upper boundary");
        assert!(matches!(
            AcquireControl::from_timeout(shifted, timeout, CancellationToken::new()),
            Err(PoolError::InvalidConfig {
                field: "acquire_timeout",
                reason: "exceeds the monotonic clock range"
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn completed_work_keeps_its_value_or_error_at_every_stage() {
        let control = AcquireControl::from_timeout(
            Instant::now(),
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .unwrap();
        let stages = [
            AcquireStage::Admission,
            AcquireStage::Wait,
            AcquireStage::Open,
            AcquireStage::Reset,
            AcquireStage::Isolation,
            AcquireStage::Delivery,
        ];
        for stage in stages {
            assert_eq!(
                control.run(stage, async { Ok(stage) }).await.unwrap(),
                stage
            );
            assert!(matches!(
                control
                    .run(stage, async { Err::<(), _>(PoolError::TokenExpired) })
                    .await,
                Err(PoolError::TokenExpired)
            ));
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        for stage in stages {
            assert!(matches!(
                control.check(stage),
                Err(PoolError::AcquireTimeout { stage: actual }) if actual == stage
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wins_over_a_simultaneously_ready_timer_and_work() {
        for expire in [false, true] {
            let token = CancellationToken::new();
            let control =
                AcquireControl::from_timeout(Instant::now(), Duration::from_secs(1), token.clone())
                    .unwrap();
            let ready = AtomicBool::new(false);
            let polls = std::sync::atomic::AtomicUsize::new(0);
            let mut work = Box::pin(control.run(
                AcquireStage::Open,
                std::future::poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    if ready.load(Ordering::SeqCst) {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Pending
                    }
                }),
            ));
            assert!(
                work.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            if expire {
                tokio::time::advance(Duration::from_secs(1)).await;
            }
            ready.store(true, Ordering::SeqCst);
            token.cancel();
            assert!(matches!(
                work.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Err(PoolError::Cancelled))
            ));
            assert_eq!(polls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_wins_over_work_that_becomes_ready_at_the_boundary() {
        let control = AcquireControl::from_timeout(
            Instant::now(),
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .unwrap();
        let ready = AtomicBool::new(false);
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let mut work = Box::pin(control.run(
            AcquireStage::Delivery,
            std::future::poll_fn(|_| {
                polls.fetch_add(1, Ordering::SeqCst);
                if ready.load(Ordering::SeqCst) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }),
        ));
        assert!(
            work.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        ready.store(true, Ordering::SeqCst);
        assert!(matches!(
            work.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(PoolError::AcquireTimeout {
                stage: AcquireStage::Delivery
            }))
        ));
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_crossed_inside_the_final_work_poll_drops_the_result() {
        let control = AcquireControl::from_timeout(
            Instant::now(),
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropProbe(dropped.clone());
        let result = control
            .run(AcquireStage::Delivery, async {
                // advance commits the clock change before yielding on its first poll.
                let mut advance = Box::pin(tokio::time::advance(Duration::from_secs(1)));
                assert!(
                    advance
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                assert_eq!(Instant::now(), control.deadline());
                Ok(guard)
            })
            .await;
        assert!(matches!(
            result,
            Err(PoolError::AcquireTimeout {
                stage: AcquireStage::Delivery
            })
        ));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("provider failure sentinel")]
    struct TrackedProviderError {
        _guard: DropProbe,
    }

    #[tokio::test(start_paused = true)]
    async fn final_poll_interruptions_override_work_errors_and_drop_their_causes() {
        for expire in [false, true] {
            let token = CancellationToken::new();
            let control =
                AcquireControl::from_timeout(Instant::now(), Duration::from_secs(1), token.clone())
                    .unwrap();
            let deadline = control.deadline();
            let dropped = Arc::new(AtomicBool::new(false));
            let cause = TrackedProviderError {
                _guard: DropProbe(dropped.clone()),
            };
            let result = control
                .run(AcquireStage::Open, async move {
                    if expire {
                        let mut advance = Box::pin(tokio::time::advance(Duration::from_secs(1)));
                        assert!(
                            advance
                                .as_mut()
                                .poll(&mut Context::from_waker(Waker::noop()))
                                .is_pending()
                        );
                        assert_eq!(Instant::now(), deadline);
                    } else {
                        token.cancel();
                    }
                    Err::<(), _>(super::super::error::OpenError::Provider(Box::new(cause)).into())
                })
                .await;
            if expire {
                assert!(matches!(
                    result,
                    Err(PoolError::AcquireTimeout {
                        stage: AcquireStage::Open
                    })
                ));
            } else {
                assert!(matches!(result, Err(PoolError::Cancelled)));
            }
            assert!(dropped.load(Ordering::SeqCst));
        }
    }
}

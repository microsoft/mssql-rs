// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::super::{connection::PoolKey, manager::AsyncPoolManager};
use super::*;

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn pool() -> Arc<PerKeyPool<()>> {
    let budget = Duration::from_secs(1);
    PerKeyPool::new(
        PoolConfig::new(budget, budget, budget).unwrap(),
        1,
        Arc::default(),
        tokio::runtime::Handle::current().id(),
    )
    .unwrap()
}

fn control() -> AcquireControl {
    AcquireControl::from_timeout(
        Instant::now(),
        Duration::from_secs(1),
        CancellationToken::new(),
    )
    .unwrap()
}

#[tokio::test]
async fn request_id_and_retirement_overflow_fail_before_mutation() {
    let pool = pool();
    lock(&pool.state).unwrap().next_id = u64::MAX;
    let mut request = pool.request(control()).unwrap();
    assert!(matches!(
        Pin::new(&mut request).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(PoolError::Internal {
            reason: "request ID overflow"
        }))
    ));
    assert_eq!(pool.counts(), (0, 0, 0));
    lock(&pool.state).unwrap().retiring = usize::MAX;
    assert!(matches!(
        pool.maintain(Instant::now(), false),
        Err(PoolError::Internal {
            reason: "retirement count overflow"
        })
    ));
    lock(&pool.state).unwrap().retiring = 0;
}

#[tokio::test]
async fn poisoned_pool_state_rejects_admission_enable_and_settlement() {
    let pool = pool();
    let poisoned = pool.clone();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _state = lock(&poisoned.state).unwrap();
            panic!("poison probe");
        }))
        .is_err()
    );
    assert!(matches!(
        pool.request(control()),
        Err(PoolError::Internal { .. })
    ));
    assert!(matches!(pool.enable(2), Err(PoolError::Internal { .. })));
    assert!(matches!(pool.busy(), Err(PoolError::Internal { .. })));
    assert!(matches!(pool.settled(), Err(PoolError::Internal { .. })));
    assert!(matches!(pool.inactive(), Err(PoolError::Internal { .. })));
}

#[tokio::test]
async fn demand_overflow_and_backend_closure_are_explicit_internal_failures() {
    let pool = pool();
    lock(&pool.state).unwrap().demand = usize::MAX;
    assert!(matches!(
        pool.request(control()).unwrap().await,
        Err(PoolError::Internal {
            reason: "demand count overflow"
        })
    ));
    lock(&pool.state).unwrap().demand = 0;
    // Fault injection only: production never closes this scheduler with owners.
    pool.slots.close();
    assert!(matches!(
        pool.request(control()).unwrap().await,
        Err(PoolError::Internal {
            reason: "slot scheduler failed"
        })
    ));
    assert_eq!(pool.counts(), (0, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn assigned_reclamation_preserves_idle_age_or_destroys_ineligible_payloads() {
    for (expire, close) in [(false, false), (true, false), (false, true)] {
        let budget = Duration::from_secs(30);
        let cfg = PoolConfig::new(budget, budget, budget)
            .unwrap()
            .with_max_size(2)
            .unwrap()
            .with_idle_timeout(Duration::from_secs(10))
            .unwrap();
        let pool = PerKeyPool::<()>::new(
            cfg,
            1,
            Arc::default(),
            tokio::runtime::Handle::current().id(),
        )
        .unwrap();
        let make_control = || {
            AcquireControl::from_timeout(Instant::now(), budget, CancellationToken::new()).unwrap()
        };
        let mut first = pool.request(make_control()).unwrap().await.unwrap();
        let mut held = pool.request(make_control()).unwrap().await.unwrap();
        first.install(()).unwrap();
        first.return_quiescent().unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        let barrier = pool.maintain(Instant::now(), false).unwrap();
        assert_eq!(pool.counts().1, 1);
        let mut empty = pool.request(make_control()).unwrap();
        assert!(
            Pin::new(&mut empty)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        held.install(()).unwrap();
        held.return_quiescent().unwrap();
        let returned = Instant::now();
        let cancel = CancellationToken::new();
        let mut assigned = pool
            .request(AcquireControl::new(Instant::now() + budget, cancel.clone()))
            .unwrap();
        assert!(
            Pin::new(&mut assigned)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        if expire {
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        cancel.cancel();
        let retirement = pool.refresh(Instant::now(), close, false).unwrap();
        assert_eq!(retirement.slots.len(), if close { 2 } else { 1 });
        assert_eq!(retirement.clients.len(), usize::from(expire || close));
        retirement.finish().unwrap();
        let mut found = false;
        pool.slots.retain(|slot, _| {
            if let Some(cached) = lock(&slot.cached).unwrap().as_ref() {
                assert_eq!(cached.returned, returned);
                found = true;
            }
            true
        });
        assert_eq!(found, !expire && !close);
        drop(barrier);
        assert!(matches!(
            assigned.await,
            Err(PoolError::Closed | PoolError::Cancelled)
        ));
        match empty.await {
            Ok(guard) => drop(guard),
            Err(PoolError::Closed) if close => {}
            Err(error) => panic!("{error}"),
        }
        assert_eq!(pool.counts(), (0, 0, 0));
    }
}

#[tokio::test(start_paused = true)]
async fn assigned_retirement_discards_cached_clients_before_shutdown_settles() {
    assigned_retirement_after_closure(false).await;
}

#[tokio::test(start_paused = true)]
async fn assigned_retirement_discards_old_generation_clients_after_reenable() {
    assigned_retirement_after_closure(true).await;
}

async fn assigned_retirement_after_closure(reenable: bool) {
    struct Client {
        dropped: Arc<AtomicUsize>,
        on_drop: Option<Box<dyn FnOnce() + Send>>,
    }

    impl Drop for Client {
        fn drop(&mut self) {
            if let Some(on_drop) = self.on_drop.take()
                && !std::thread::panicking()
            {
                on_drop();
            }
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    let budget = Duration::from_secs(30);
    let config = PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(2)
        .unwrap()
        .with_idle_timeout(Duration::from_secs(10))
        .unwrap();
    let manager = AsyncPoolManager::<Client>::new(config.clone()).unwrap();
    manager.enable(config.clone()).unwrap();
    let key = PoolKey::new(vec![1].into_boxed_slice()).unwrap();
    let mut first = manager
        .reserve(key.clone(), control())
        .await
        .unwrap()
        .unwrap();
    let pool = first.pool.clone();
    let mut held = pool.request(control()).unwrap().await.unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    first
        .install(Client {
            dropped: dropped.clone(),
            on_drop: None,
        })
        .unwrap();
    first.return_quiescent().unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let barrier = pool.maintain(Instant::now(), false).unwrap();
    let mut empty = pool.request(control()).unwrap();
    assert!(
        Pin::new(&mut empty)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let weak = Arc::downgrade(&pool);
    held.install(Client {
        dropped: dropped.clone(),
        on_drop: Some(Box::new(move || {
            let pool = weak.upgrade().unwrap();
            let state = pool.state.try_lock().expect("off-lock client destruction");
            assert_eq!((state.demand, state.retiring), (1, 1));
            drop(state);
            assert!(!pool.settled().unwrap());
            let mut available = Box::pin(pool.slots.get());
            let Poll::Ready(Ok(slot)) = available
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            else {
                panic!("the other empty slot must be available");
            };
            let mut extra = Box::pin(pool.slots.get());
            assert!(
                extra
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(extra);
            drop(slot);
        })),
    })
    .unwrap();
    held.return_quiescent().unwrap();
    let cancel = CancellationToken::new();
    let mut assigned = pool
        .request(AcquireControl::new(Instant::now() + budget, cancel.clone()))
        .unwrap();
    assert!(
        Pin::new(&mut assigned)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    cancel.cancel();
    let retirement = pool.refresh(Instant::now(), false, false).unwrap();
    assert_eq!(retirement.slots.len(), 1);
    assert!(retirement.clients.is_empty());
    barrier.finish().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    manager.disable();
    let mut shutdown = if reenable {
        manager.enable(config).unwrap();
        None
    } else {
        let mut shutdown = Box::pin(manager.shutdown());
        assert!(
            shutdown
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        Some(shutdown)
    };
    retirement.finish().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert!(pool.settled().unwrap());
    assert!(matches!(assigned.await, Err(PoolError::Closed)));
    assert!(matches!(empty.await, Err(PoolError::Closed)));
    pool.slots.retain(|slot, _| {
        assert!(lock(&slot.cached).unwrap().is_none());
        true
    });
    assert_eq!(pool.counts(), (0, 0, 0));
    if let Some(shutdown) = shutdown.take() {
        shutdown.await.unwrap();
    } else {
        let mut next = manager.reserve(key, control()).await.unwrap().unwrap();
        assert!(next.value_mut().is_none());
        drop(next);
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn poisoned_idle_slots_stop_reuse_and_report_maintenance_failure() {
    let pool = pool();
    let slot = pool.slots.get().await.unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cached = slot.cached.lock().unwrap();
            panic!("idle poison probe");
        }))
        .is_err()
    );
    drop(slot);
    assert!(matches!(pool.inactive(), Err(PoolError::Internal { .. })));
    assert!(matches!(
        pool.maintain(Instant::now(), false).unwrap().finish(),
        Err(PoolError::Internal { .. })
    ));
    assert!(matches!(
        pool.request(control()).unwrap().await,
        Err(PoolError::Closed)
    ));
}

#[tokio::test]
async fn poisoned_state_during_guard_and_registration_drop_never_recovers_counters() {
    let pool = pool();
    let guard = pool.request(control()).unwrap().await.unwrap();
    let poisoned = pool.clone();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _state = lock(&poisoned.state).unwrap();
            panic!("guard poison probe");
        }))
        .is_err()
    );
    drop(guard);
    assert!(matches!(pool.settled(), Err(PoolError::Internal { .. })));

    let pool = self::pool();
    let budget = Duration::from_secs(1);
    // Fill all configured slots so the registration really reaches Pending.
    let mut held = Vec::new();
    for _ in 0..100 {
        held.push(
            pool.request(
                AcquireControl::from_timeout(Instant::now(), budget, CancellationToken::new())
                    .unwrap(),
            )
            .unwrap()
            .await
            .unwrap(),
        );
    }
    let mut waiting = pool.request(control()).unwrap();
    assert!(
        Pin::new(&mut waiting)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let poisoned = pool.clone();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _state = lock(&poisoned.state).unwrap();
            panic!("registration poison probe");
        }))
        .is_err()
    );
    assert!(matches!(
        Pin::new(&mut waiting).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(PoolError::Internal { .. }))
    ));
    drop(waiting);
    drop(held);
}

#[tokio::test(start_paused = true)]
async fn dropping_an_assigned_request_reclaims_stale_clients_without_cancelling_its_parent() {
    let budget = Duration::from_secs(30);
    let cfg = PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(2)
        .unwrap()
        .with_idle_timeout(Duration::from_secs(10))
        .unwrap();
    let pool = PerKeyPool::<()>::new(
        cfg,
        1,
        Arc::default(),
        tokio::runtime::Handle::current().id(),
    )
    .unwrap();
    let parent = CancellationToken::new();
    let make_control =
        || AcquireControl::from_timeout(Instant::now(), budget, parent.clone()).unwrap();
    let mut idle = pool.request(make_control()).unwrap().await.unwrap();
    let mut held = pool.request(make_control()).unwrap().await.unwrap();
    idle.install(()).unwrap();
    idle.return_quiescent().unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let barrier = pool.maintain(Instant::now(), false).unwrap();
    let mut empty = pool.request(make_control()).unwrap();
    assert!(
        Pin::new(&mut empty)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    held.install(()).unwrap();
    held.return_quiescent().unwrap();
    let mut assigned = pool.request(make_control()).unwrap();
    assert!(
        Pin::new(&mut assigned)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    drop(assigned);
    assert!(!parent.is_cancelled());
    assert_eq!(pool.counts(), (1, 1, 1));
    pool.slots.retain(|slot, _| {
        let empty = lock(&slot.cached).unwrap().is_none();
        assert!(empty);
        true
    });
    drop(barrier);
    drop(empty.await.unwrap());
    assert_eq!(pool.counts(), (0, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn claim_checks_idle_expiry_even_without_an_admission_sweep() {
    for (elapsed, expired) in [(999, false), (1000, true)] {
        let budget = Duration::from_secs(30);
        let cfg = PoolConfig::new(budget, budget, budget)
            .unwrap()
            .with_max_size(1)
            .unwrap()
            .with_idle_timeout(Duration::from_secs(1))
            .unwrap();
        let pool = PerKeyPool::<DropCounter>::new(
            cfg,
            1,
            Arc::default(),
            tokio::runtime::Handle::current().id(),
        )
        .unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut held = pool.request(control()).unwrap().await.unwrap();
        held.install(DropCounter(dropped.clone())).unwrap();
        held.return_quiescent().unwrap();
        tokio::time::advance(Duration::from_millis(elapsed)).await;
        let mut claimed = pool.request(control()).unwrap().await.unwrap();
        assert_eq!(claimed.value_mut().is_none(), expired);
        assert_eq!(dropped.load(Ordering::SeqCst), usize::from(expired));
        assert_eq!(pool.counts(), (1, 0, 0));
        drop(claimed);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(pool.counts(), (0, 0, 0));
    }
}

#[tokio::test(start_paused = true)]
async fn claim_time_discard_consumes_the_original_acquisition_budget() {
    struct SlowDrop(Arc<AtomicUsize>);
    impl Drop for SlowDrop {
        fn drop(&mut self) {
            let mut advance = Box::pin(tokio::time::advance(Duration::from_secs(2)));
            let _ = advance
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let budget = Duration::from_secs(30);
    let cfg = PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(1)
        .unwrap()
        .with_idle_timeout(Duration::ZERO)
        .unwrap();
    let pool = PerKeyPool::<SlowDrop>::new(
        cfg,
        1,
        Arc::default(),
        tokio::runtime::Handle::current().id(),
    )
    .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut held = pool.request(control()).unwrap().await.unwrap();
    held.install(SlowDrop(dropped.clone())).unwrap();
    held.return_quiescent().unwrap();
    let acquisition = control();
    let deadline = acquisition.deadline();
    assert!(matches!(
        pool.request(acquisition).unwrap().await,
        Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Wait
        })
    ));
    assert!(Instant::now() >= deadline);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(pool.counts(), (0, 0, 0));
    drop(pool.request(control()).unwrap().await.unwrap());
    assert_eq!(pool.counts(), (0, 0, 0));
}

#[tokio::test]
async fn shutdown_reports_poisoned_pools_but_still_discards_healthy_idle_clients() {
    for poison_state in [false, true] {
        let budget = Duration::from_secs(30);
        let cfg = PoolConfig::new(budget, budget, budget).unwrap();
        let manager: AsyncPoolManager<DropCounter> = AsyncPoolManager::new(cfg.clone()).unwrap();
        manager.enable(cfg).unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut bad = manager
            .reserve(PoolKey::new(vec![1].into_boxed_slice()).unwrap(), control())
            .await
            .unwrap()
            .unwrap();
        bad.install(DropCounter(dropped.clone())).unwrap();
        let mut healthy = manager
            .reserve(PoolKey::new(vec![2].into_boxed_slice()).unwrap(), control())
            .await
            .unwrap()
            .unwrap();
        healthy.install(DropCounter(dropped.clone())).unwrap();
        healthy.return_quiescent().unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if poison_state {
                    let _state = bad.pool.state.lock().unwrap();
                    panic!("shutdown state poison probe");
                } else {
                    let _cached = bad.slot.as_ref().unwrap().cached.lock().unwrap();
                    panic!("shutdown idle poison probe");
                }
            }))
            .is_err()
        );
        if poison_state {
            assert!(matches!(
                manager.shutdown().await,
                Err(PoolError::Internal {
                    reason: "poisoned state mutex"
                })
            ));
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            drop(bad);
        } else {
            drop(bad);
            assert!(matches!(
                manager.shutdown().await,
                Err(PoolError::Internal {
                    reason: "poisoned idle slot"
                })
            ));
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert!(!manager.is_enabled());
        assert!(matches!(
            manager
                .reserve(PoolKey::new(vec![3].into_boxed_slice()).unwrap(), control())
                .await,
            Err(PoolError::Closed)
        ));
    }
}

#[tokio::test]
async fn poisoned_slot_load_fails_explicitly_and_returns_empty_capacity() {
    let pool = pool();
    let mut held = pool.request(control()).unwrap().await.unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cached = held.slot.as_ref().unwrap().cached.lock().unwrap();
            panic!("claim poison probe");
        }))
        .is_err()
    );
    assert!(matches!(
        held.load(),
        Err(PoolError::Internal {
            reason: "poisoned state mutex"
        })
    ));
    drop(held);
    assert_eq!(pool.counts(), (0, 0, 0));
    assert!(matches!(
        pool.maintain(Instant::now(), false).unwrap().finish(),
        Err(PoolError::Internal {
            reason: "poisoned idle slot"
        })
    ));
}

#[tokio::test]
async fn closing_retirement_rejects_claimed_work_before_delivering_cancellation() {
    let budget = Duration::from_secs(30);
    let cfg = PoolConfig::new(budget, budget, budget).unwrap();
    let pool = PerKeyPool::<DropCounter>::new(
        cfg,
        1,
        Arc::default(),
        tokio::runtime::Handle::current().id(),
    )
    .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut held = pool.request(control()).unwrap().await.unwrap();
    held.install(DropCounter(dropped.clone())).unwrap();
    let closing = pool.maintain(Instant::now(), true).unwrap();
    assert!(!held.generation_cancel.is_cancelled());
    let polled = AtomicUsize::new(0);
    for stage in [
        AcquireStage::Open,
        AcquireStage::Reset,
        AcquireStage::Isolation,
    ] {
        assert!(matches!(
            held.run(stage, async {
                polled.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await,
            Err(PoolError::Closed)
        ));
        assert_eq!(polled.load(Ordering::SeqCst), 0);
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    assert_eq!(pool.counts(), (1, 0, 0));
    drop(closing);
    assert!(held.generation_cancel.is_cancelled());
    drop(held);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(pool.counts(), (0, 0, 0));
}

#[tokio::test]
async fn slot_requests_reject_foreign_or_missing_runtime_before_creating_timers() {
    let pool = pool();
    let other = pool.clone();
    std::thread::spawn(move || {
        assert!(matches!(
            other.request(control()),
            Err(PoolError::InvalidRequest {
                field: "runtime",
                ..
            })
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(matches!(
                other.request(control()),
                Err(PoolError::InvalidRequest {
                    field: "runtime",
                    ..
                })
            ));
        });
    })
    .join()
    .unwrap();
    assert_eq!(pool.counts(), (0, 0, 0));
    drop(pool.request(control()).unwrap().await.unwrap());
    assert_eq!(pool.counts(), (0, 0, 0));
}

#[tokio::test]
async fn claimed_work_stops_while_closing_retirement_still_holds_cancellation() {
    let pool = pool();
    let mut held = pool.request(control()).unwrap().await.unwrap();
    let polled = AtomicUsize::new(0);
    let mut work = Box::pin(held.run(
        AcquireStage::Reset,
        std::future::poll_fn(|_| {
            polled.fetch_add(1, Ordering::SeqCst);
            Poll::<PoolResult<()>>::Pending
        }),
    ));
    assert!(
        work.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let closing = pool.maintain(Instant::now(), true).unwrap();
    assert!(!lock(&pool.state).unwrap().cancel.is_cancelled());
    assert!(matches!(
        work.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(PoolError::Closed))
    ));
    assert_eq!(polled.load(Ordering::SeqCst), 1);
    drop(work);
    assert_eq!(pool.counts(), (1, 0, 0));
    drop(closing);
    drop(held);
    assert_eq!(pool.counts(), (0, 0, 0));
}

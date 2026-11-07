// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;

use super::*;

fn pool() -> Arc<PerKeyPool<()>> {
    let budget = Duration::from_secs(1);
    PerKeyPool::new(
        PoolConfig::new(budget, budget, budget).unwrap(),
        1,
        Arc::default(),
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
        let pool = PerKeyPool::<()>::new(cfg, 1, Arc::default()).unwrap();
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
    let pool = PerKeyPool::<()>::new(cfg, 1, Arc::default()).unwrap();
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

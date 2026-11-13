// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Wake, Waker};

use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Default)]
struct Observations {
    live: usize,
    peak: usize,
    destroyed: Vec<usize>,
}

struct Client {
    id: Cell<usize>,
    observations: Arc<Mutex<Observations>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Client {
    fn new(id: usize, observations: &Arc<Mutex<Observations>>) -> Self {
        let mut state = observations.lock().unwrap();
        state.live += 1;
        state.peak = state.peak.max(state.live);
        Self {
            id: Cell::new(id),
            observations: observations.clone(),
            on_drop: None,
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
        let mut state = self.observations.lock().unwrap();
        state.live -= 1;
        state.destroyed.push(self.id.get());
    }
}

type TestManager = AsyncPoolManager<Client>;
type Request = Pin<Box<dyn Future<Output = PoolResult<Option<SlotGuard<Client>>>> + Send>>;

fn config(size: usize, waiting: Option<usize>, idle: u64) -> PoolConfig {
    let budget = Duration::from_secs(30);
    PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(size)
        .unwrap()
        .with_max_waiting(waiting)
        .unwrap()
        .with_idle_timeout(Duration::from_secs(idle))
        .unwrap()
}

fn manager(config: PoolConfig) -> Arc<TestManager> {
    let manager = Arc::new(TestManager::new(config.clone()).unwrap());
    manager.enable(config).unwrap();
    manager
}

fn key(id: u8) -> PoolKey {
    PoolKey::new(vec![id].into_boxed_slice()).unwrap()
}

fn control() -> AcquireControl {
    AcquireControl::from_timeout(
        Instant::now(),
        Duration::from_secs(30),
        CancellationToken::new(),
    )
    .unwrap()
}

fn request_with(manager: &Arc<TestManager>, id: u8, control: AcquireControl) -> Request {
    let manager = manager.clone();
    Box::pin(async move { manager.reserve(key(id), control).await })
}

fn request(manager: &Arc<TestManager>, id: u8) -> Request {
    request_with(manager, id, control())
}

fn poll<T, F: Future<Output = PoolResult<T>> + ?Sized>(future: Pin<&mut F>) -> Poll<PoolResult<T>> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn ready(request: &mut Request) -> SlotGuard<Client> {
    match poll(request.as_mut()) {
        Poll::Ready(Ok(Some(slot))) => slot,
        Poll::Ready(Err(error)) => panic!("{error}"),
        _ => panic!("expected immediate pooled capacity"),
    }
}

fn pool(manager: &TestManager, id: u8) -> Arc<PerKeyPool<Client>> {
    manager.state.lock().unwrap().pools[&key(id)].clone()
}

#[tokio::test]
async fn initially_disabled_is_the_only_direct_bypass_and_enable_is_idempotent() {
    let config = config(1, Some(1), 10);
    let manager = TestManager::new(config.clone()).unwrap();
    assert!(!manager.is_enabled());
    assert!(manager.reserve(key(1), control()).await.unwrap().is_none());
    manager.enable(config.clone()).unwrap();
    let generation = manager.state.lock().unwrap().generation;
    manager.enable(config.clone()).unwrap();
    assert_eq!(manager.state.lock().unwrap().generation, generation);
    assert!(matches!(
        manager.enable(config.with_max_size(2).unwrap()),
        Err(PoolError::InvalidConfig { .. })
    ));
    manager.shutdown().await.unwrap();
    assert!(!manager.is_enabled());
    assert!(matches!(
        manager.reserve(key(1), control()).await,
        Err(PoolError::Closed)
    ));
}

#[tokio::test]
async fn queue_limit_counts_queued_not_assigned_grants_and_preserves_fifo() {
    let manager = manager(config(1, Some(2), 10));
    let held = ready(&mut request(&manager, 1));
    let mut older = request(&manager, 1);
    let mut newer = request(&manager, 1);
    assert!(poll(older.as_mut()).is_pending());
    assert!(poll(newer.as_mut()).is_pending());
    let mut rejected = request(&manager, 1);
    assert!(matches!(
        poll(rejected.as_mut()),
        Poll::Ready(Err(PoolError::WaitQueueFull))
    ));
    drop(held);
    let mut third = request(&manager, 1);
    assert!(poll(third.as_mut()).is_pending());
    assert!(poll(newer.as_mut()).is_pending());
    let held = ready(&mut older);
    assert!(poll(third.as_mut()).is_pending());
    drop(held);
    let held = ready(&mut newer);
    assert!(poll(third.as_mut()).is_pending());
    drop(held);
    drop(ready(&mut third));
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn retries_reenter_at_the_tail_and_are_subject_to_the_same_limit() {
    let manager = manager(config(1, Some(1), 10));
    let held = ready(&mut request(&manager, 1));
    let mut older = request(&manager, 1);
    assert!(poll(older.as_mut()).is_pending());
    drop(held);
    let mut retry = request(&manager, 1);
    assert!(poll(retry.as_mut()).is_pending());
    let mut saturated_retry = request(&manager, 1);
    assert!(matches!(
        poll(saturated_retry.as_mut()),
        Poll::Ready(Err(PoolError::WaitQueueFull))
    ));
    assert!(poll(retry.as_mut()).is_pending());
    drop(ready(&mut older));
    drop(ready(&mut retry));
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
}

#[tokio::test]
async fn unlimited_queue_is_per_key_and_dropped_waiters_release_demand() {
    let manager = manager(config(1, None, 10));
    let held = ready(&mut request(&manager, 1));
    let other_key = ready(&mut request(&manager, 2));
    let mut waiters: Vec<_> = (0..12).map(|_| request(&manager, 1)).collect();
    for waiter in &mut waiters {
        assert!(poll(waiter.as_mut()).is_pending());
    }
    assert_eq!(pool(&manager, 1).counts(), (13, 0, 12));
    drop(waiters);
    drop(held);
    drop(other_key);
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
    assert_eq!(pool(&manager, 2).counts(), (0, 0, 0));
}

#[tokio::test]
async fn cancellation_reclaims_queued_and_unpolled_assigned_gets_before_new_admission() {
    for after_assignment in [false, true] {
        let manager = manager(config(1, Some(1), 10));
        let held = ready(&mut request(&manager, 1));
        let cancel = CancellationToken::new();
        let mut cancelled = request_with(
            &manager,
            1,
            AcquireControl::new(control().deadline(), cancel.clone()),
        );
        assert!(poll(cancelled.as_mut()).is_pending());
        if after_assignment {
            drop(held);
        } else {
            let mut later = request(&manager, 1);
            assert!(matches!(
                poll(later.as_mut()),
                Poll::Ready(Err(PoolError::WaitQueueFull))
            ));
            drop(held);
        }
        cancel.cancel();
        drop(ready(&mut request(&manager, 1)));
        assert!(matches!(
            poll(cancelled.as_mut()),
            Poll::Ready(Err(PoolError::Cancelled))
        ));
        assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
        assert_eq!(manager.activity.in_flight.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn deadlines_are_reclaimed_lazily_and_never_restart() {
    let manager = manager(config(1, Some(1), 10));
    let held = ready(&mut request(&manager, 1));
    let mut expired = request_with(
        &manager,
        1,
        AcquireControl::new(
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        ),
    );
    assert!(poll(expired.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    let mut replacement = request(&manager, 1);
    assert!(poll(replacement.as_mut()).is_pending());
    assert!(matches!(
        poll(expired.as_mut()),
        Poll::Ready(Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Wait
        }))
    ));
    drop(held);
    drop(ready(&mut replacement));
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
}

#[tokio::test]
async fn cancellation_and_expiry_at_admission_do_not_create_a_pool() {
    let manager = manager(config(1, None, 10));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        manager
            .reserve(key(1), AcquireControl::new(control().deadline(), cancel))
            .await,
        Err(PoolError::Cancelled)
    ));
    assert!(matches!(
        manager
            .reserve(
                key(1),
                AcquireControl::new(Instant::now(), CancellationToken::new())
            )
            .await,
        Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Admission
        })
    ));
    assert!(manager.state.lock().unwrap().pools.is_empty());
}

#[tokio::test]
async fn disable_reenable_retains_one_capacity_allowance_and_old_leases_discard() {
    let config = config(1, Some(1), 10);
    let manager = manager(config.clone());
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut old = ready(&mut request(&manager, 1));
    old.install(Client::new(1, &observations)).unwrap();
    old.publish().unwrap();
    let original_pool = pool(&manager, 1);
    let mut queued = request(&manager, 1);
    assert!(poll(queued.as_mut()).is_pending());
    manager.disable();
    manager.disable();
    assert_eq!(old.value_mut().unwrap().id.get(), 1);
    manager.enable(config).unwrap();
    assert!(Arc::ptr_eq(&original_pool, &pool(&manager, 1)));
    let mut fresh = request(&manager, 1);
    assert!(poll(fresh.as_mut()).is_pending());
    assert!(matches!(
        poll(queued.as_mut()),
        Poll::Ready(Err(PoolError::Closed))
    ));
    old.return_quiescent().unwrap();
    let mut fresh = ready(&mut fresh);
    assert!(fresh.value_mut().is_none());
    fresh.install(Client::new(2, &observations)).unwrap();
    fresh.publish().unwrap();
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(fresh);
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn changed_config_is_rejected_while_old_owners_or_admissions_exist() {
    let config = config(1, Some(1), 10);
    let manager = manager(config.clone());
    let held = ready(&mut request(&manager, 1));
    let mut queued = request(&manager, 1);
    assert!(poll(queued.as_mut()).is_pending());
    manager.disable();
    let changed = config.clone().with_max_size(2).unwrap();
    assert!(matches!(
        manager.enable(changed.clone()),
        Err(PoolError::InvalidConfig { .. })
    ));
    drop(held);
    assert!(matches!(
        manager.enable(changed.clone()),
        Err(PoolError::InvalidConfig { .. })
    ));
    assert!(matches!(
        poll(queued.as_mut()),
        Poll::Ready(Err(PoolError::Closed))
    ));
    let external_pool = pool(&manager, 1);
    assert!(matches!(
        manager.enable(changed.clone()),
        Err(PoolError::InvalidConfig { .. })
    ));
    drop(external_pool);
    manager.enable(changed).unwrap();
    assert!(manager.state.lock().unwrap().pools.is_empty());
    let first = ready(&mut request(&manager, 1));
    let second = ready(&mut request(&manager, 1));
    drop((first, second));
}

#[tokio::test]
async fn disable_wins_before_publication_and_published_lease_is_not_revoked() {
    let manager = manager(config(2, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut pending = ready(&mut request(&manager, 1));
    pending.install(Client::new(1, &observations)).unwrap();
    let mut published = ready(&mut request(&manager, 1));
    published.install(Client::new(2, &observations)).unwrap();
    published.publish().unwrap();
    manager.disable();
    assert!(matches!(pending.publish(), Err(PoolError::Closed)));
    assert_eq!(published.value_mut().unwrap().id.get(), 2);
    assert_eq!(observations.lock().unwrap().live, 2);
    drop((pending, published));
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn destruction_precedes_slot_return_even_when_a_waiter_polls_inside_drop() {
    let manager = manager(config(1, Some(1), 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    let mut waiting = request(&manager, 1);
    assert!(poll(waiting.as_mut()).is_pending());
    let pending = Arc::new(Mutex::new(None));
    let pending_probe = pending.clone();
    let seen = observations.clone();
    let mut client = Client::new(1, &observations);
    client.on_drop = Some(Box::new(move || {
        let outcome = poll(waiting.as_mut());
        if let Poll::Ready(Ok(Some(mut granted))) = outcome {
            granted.install(Client::new(2, &seen)).unwrap();
            *pending_probe.lock().unwrap() = Some((waiting, false, Some(granted)));
        } else {
            *pending_probe.lock().unwrap() = Some((waiting, outcome.is_pending(), None));
        }
    }));
    held.install(client).unwrap();
    drop(held);
    let peak = observations.lock().unwrap().peak;
    let (mut waiting, was_pending, unexpected) = pending.lock().unwrap().take().unwrap();
    assert_eq!(peak, 1);
    assert!(was_pending);
    assert!(unexpected.is_none());
    drop(ready(&mut waiting));
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test(start_paused = true)]
async fn idle_age_starts_at_successful_return_and_expires_at_the_exact_boundary() {
    let manager = manager(config(1, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    held.publish().unwrap();
    tokio::time::advance(Duration::from_secs(100)).await;
    held.return_quiescent().unwrap();
    tokio::time::advance(Duration::from_secs(9)).await;
    let mut hit = ready(&mut request(&manager, 1));
    assert_eq!(hit.value_mut().unwrap().id.get(), 1);
    hit.return_quiescent().unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut miss = ready(&mut request(&manager, 1));
    assert!(miss.value_mut().is_none());
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    drop(miss);
}

#[tokio::test(start_paused = true)]
async fn zero_idle_timeout_is_lazy_and_wider_sweeps_are_throttled_to_one_second() {
    let manager = manager(config(1, None, 0));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    held.return_quiescent().unwrap();
    assert_eq!(observations.lock().unwrap().live, 1);
    drop(ready(&mut request(&manager, 2)));
    assert_eq!(observations.lock().unwrap().live, 1);
    tokio::time::advance(Duration::from_millis(999)).await;
    drop(ready(&mut request(&manager, 2)));
    assert_eq!(observations.lock().unwrap().live, 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    drop(ready(&mut request(&manager, 3)));
    assert_eq!(observations.lock().unwrap().live, 0);
    assert_eq!(manager.state.lock().unwrap().last_sweep, Instant::now());
}

#[tokio::test(start_paused = true)]
async fn wider_sweep_prunes_other_keys_and_empty_cached_slots_do_not_prevent_eviction() {
    let manager = manager(config(1, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    for id in [1, 2] {
        let mut held = ready(&mut request(&manager, id));
        held.install(Client::new(id.into(), &observations)).unwrap();
        held.return_quiescent().unwrap();
    }
    tokio::time::advance(Duration::from_secs(10)).await;
    drop(ready(&mut request(&manager, 3)));
    assert_eq!(observations.lock().unwrap().live, 0);
    tokio::time::advance(Duration::from_secs(10)).await;
    drop(ready(&mut request(&manager, 4)));
    let state = manager.state.lock().unwrap();
    assert_eq!(state.pools.len(), 1);
    assert!(state.pools.contains_key(&key(4)));
}

#[tokio::test(start_paused = true)]
async fn external_pool_owners_prevent_eviction_until_they_drop() {
    let manager = manager(config(1, None, 0));
    drop(ready(&mut request(&manager, 1)));
    let external = pool(&manager, 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    drop(ready(&mut request(&manager, 2)));
    assert!(manager.state.lock().unwrap().pools.contains_key(&key(1)));
    drop(external);
    tokio::time::advance(Duration::from_secs(1)).await;
    drop(ready(&mut request(&manager, 3)));
    assert!(!manager.state.lock().unwrap().pools.contains_key(&key(1)));
}

#[tokio::test]
async fn maintenance_allows_exact_admission_and_assignment_but_blocks_publication() {
    let manager = manager(config(2, Some(1), 0));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    for id in [1, 2] {
        let mut held = ready(&mut request(&manager, 1));
        held.install(Client::new(id, &observations)).unwrap();
        held.return_quiescent().unwrap();
    }
    // Zero timeout prunes on each checkout; retain one idle client for the barrier.
    assert_eq!(observations.lock().unwrap().live, 1);
    let target = pool(&manager, 1);
    let retirement = target.maintain(Instant::now(), false).unwrap();
    let mut first = request(&manager, 1);
    let mut second = request(&manager, 1);
    let mut third = request(&manager, 1);
    let mut fourth = request(&manager, 1);
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(second.as_mut()).is_pending());
    assert!(poll(third.as_mut()).is_pending());
    assert!(matches!(
        poll(fourth.as_mut()),
        Poll::Ready(Err(PoolError::WaitQueueFull))
    ));
    assert_eq!(target.counts(), (3, 1, 3));
    drop(retirement);
    let first = ready(&mut first);
    let second = ready(&mut second);
    assert!(poll(third.as_mut()).is_pending());
    drop(first);
    drop(ready(&mut third));
    drop(second);
    assert_eq!(target.counts(), (0, 0, 0));
}

#[tokio::test]
async fn idle_destruction_runs_without_manager_or_pool_locks_and_preserves_capacity() {
    let cfg = config(1, Some(1), 10);
    let manager = manager(cfg.clone());
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let checked = Arc::new(Mutex::new(false));
    let mut held = ready(&mut request(&manager, 1));
    let mut client = Client::new(1, &observations);
    let target = pool(&manager, 1);
    let callback_manager = manager.clone();
    let callback_target = target.clone();
    let probe = checked.clone();
    client.on_drop = Some(Box::new(move || {
        assert!(callback_manager.state.try_lock().is_ok());
        assert!(callback_target.counts().1 > 0);
        callback_manager.enable(cfg.clone()).unwrap();
        let mut newcomer = request(&callback_manager, 1);
        assert!(poll(newcomer.as_mut()).is_pending());
        assert!(matches!(
            callback_manager.enable(cfg.with_max_size(2).unwrap()),
            Err(PoolError::InvalidConfig { .. })
        ));
        *probe.lock().unwrap() = true;
    }));
    held.install(client).unwrap();
    held.return_quiescent().unwrap();
    manager.disable();
    assert!(*checked.lock().unwrap());
    assert_eq!(observations.lock().unwrap().peak, 1);
    assert_eq!(target.counts(), (0, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn shutdown_timeout_and_cancelled_wait_remain_terminal_and_allow_later_settlement() {
    let manager = manager(config(1, Some(1), 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    held.publish().unwrap();
    let mut waiting = request(&manager, 1);
    assert!(poll(waiting.as_mut()).is_pending());
    let mut shutdown = Box::pin(manager.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    drop(shutdown);
    assert!(matches!(
        poll(waiting.as_mut()),
        Poll::Ready(Err(PoolError::Closed))
    ));
    assert!(matches!(
        manager.enable(config(1, None, 10)),
        Err(PoolError::Closed)
    ));
    assert!(matches!(
        manager.reserve(key(2), control()).await,
        Err(PoolError::Closed)
    ));
    assert!(matches!(
        manager.shutdown().await,
        Err(PoolError::ShutdownTimeout)
    ));
    assert_eq!(held.value_mut().unwrap().id.get(), 1);
    assert_eq!(observations.lock().unwrap().live, 1);
    drop(held);
    manager.shutdown().await.unwrap();
    manager.shutdown().await.unwrap();
    manager.disable();
    assert_eq!(manager.state.lock().unwrap().lifecycle, Lifecycle::Closed);
}

#[tokio::test]
async fn manager_drop_discards_idle_clients_without_revoking_checked_out_owners() {
    let manager = manager(config(2, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut idle = ready(&mut request(&manager, 1));
    idle.install(Client::new(1, &observations)).unwrap();
    idle.return_quiescent().unwrap();
    let mut held = ready(&mut request(&manager, 1));
    assert_eq!(held.value_mut().unwrap().id.get(), 1);
    let mut idle = ready(&mut request(&manager, 1));
    idle.install(Client::new(2, &observations)).unwrap();
    idle.return_quiescent().unwrap();
    drop(manager);
    assert_eq!(observations.lock().unwrap().destroyed, [2]);
    assert_eq!(held.value_mut().unwrap().id.get(), 1);
    drop(held);
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn publication_checks_final_cancellation_and_empty_or_duplicate_client_errors() {
    let manager = manager(config(1, None, 10));
    let cancel = CancellationToken::new();
    let mut held = ready(&mut request_with(
        &manager,
        1,
        AcquireControl::new(control().deadline(), cancel.clone()),
    ));
    assert!(matches!(held.publish(), Err(PoolError::Internal { .. })));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    held.install(Client::new(1, &observations)).unwrap();
    // Use a separate observation set: an invalid caller payload is not a pooled open.
    let other: Arc<Mutex<Observations>> = Arc::default();
    assert!(matches!(
        held.install(Client::new(2, &other)),
        Err(PoolError::Internal { .. })
    ));
    assert_eq!(other.lock().unwrap().live, 0);
    cancel.cancel();
    assert!(matches!(held.publish(), Err(PoolError::Cancelled)));
    drop(held);
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test(start_paused = true)]
async fn publication_checks_the_original_deadline_not_a_new_phase_budget() {
    let manager = manager(config(1, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request_with(
        &manager,
        1,
        AcquireControl::new(
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        ),
    ));
    held.install(Client::new(1, &observations)).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        held.publish(),
        Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Delivery
        })
    ));
    drop(held);
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn exhausted_cooperative_budget_cannot_delay_fresh_or_recycled_slot_get() {
    let manager = manager(config(1, Some(1), 10));
    while tokio::task::coop::has_budget_remaining() {
        tokio::task::consume_budget().await;
    }
    drop(ready(&mut request(&manager, 1)));
    while tokio::task::coop::has_budget_remaining() {
        tokio::task::consume_budget().await;
    }
    drop(ready(&mut request(&manager, 1)));
}

#[tokio::test]
async fn executor_wake_callbacks_run_outside_both_state_locks() {
    struct Reenter {
        manager: Arc<TestManager>,
        target: Arc<PerKeyPool<Client>>,
        wakes: AtomicUsize,
    }
    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.manager.state.try_lock().is_ok());
            self.target.counts();
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }
    let manager = manager(config(1, Some(1), 10));
    let held = ready(&mut request(&manager, 1));
    let reenter = Arc::new(Reenter {
        manager: manager.clone(),
        target: pool(&manager, 1),
        wakes: AtomicUsize::new(0),
    });
    let waker = Waker::from(reenter.clone());
    let mut waiting = request(&manager, 1);
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(held);
    assert!(reenter.wakes.load(Ordering::SeqCst) > 0);
    drop(ready(&mut waiting));
}

#[tokio::test]
async fn runtime_identity_is_enforced_and_no_runtime_is_created_by_the_manager() {
    let manager = manager(config(1, None, 10));
    let other = manager.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(matches!(
                other.reserve(key(1), control()).await,
                Err(PoolError::InvalidRequest {
                    field: "runtime",
                    ..
                })
            ));
            assert!(matches!(
                other.shutdown().await,
                Err(PoolError::InvalidRequest {
                    field: "runtime",
                    ..
                })
            ));
        });
    })
    .join()
    .unwrap();
    assert!(manager.is_enabled());
    assert!(manager.state.lock().unwrap().pools.is_empty());
}

#[test]
fn construction_outside_a_runtime_fails_explicitly() {
    assert!(matches!(
        TestManager::new(config(1, None, 10)),
        Err(PoolError::InvalidRequest {
            field: "runtime",
            ..
        })
    ));
}

#[tokio::test]
async fn poisoned_manager_state_fails_closed_without_unchecked_recovery() {
    let manager = manager(config(1, None, 10));
    let poisoned = manager.clone();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _state = poisoned.state.lock().unwrap();
            panic!("poison probe");
        }))
        .is_err()
    );
    assert!(!manager.is_enabled());
    manager.disable();
    assert!(matches!(
        manager.enable(config(1, None, 10)),
        Err(PoolError::Internal { .. })
    ));
    assert!(matches!(
        manager.reserve(key(1), control()).await,
        Err(PoolError::Internal { .. })
    ));
    assert!(matches!(
        manager.shutdown().await,
        Err(PoolError::Internal { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_acquisition_never_exceeds_physical_capacity() {
    let manager = manager(config(2, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut tasks = tokio::task::JoinSet::new();
    for id in 0..32 {
        let manager = manager.clone();
        let seen = observations.clone();
        tasks.spawn(async move {
            let mut held = manager.reserve(key(1), control()).await.unwrap().unwrap();
            held.install(Client::new(id, &seen)).unwrap();
            held.publish().unwrap();
            tokio::task::yield_now().await;
            drop(held);
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    {
        let seen = observations.lock().unwrap();
        assert!(seen.peak <= 2);
        assert_eq!(seen.live, 0);
        assert_eq!(seen.destroyed.len(), 32);
    }
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
    assert_eq!(manager.activity.in_flight.load(Ordering::SeqCst), 0);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn default_payload_preserves_real_tds_client_ownership() {
    let cfg = config(1, None, 10);
    let manager: AsyncPoolManager = AsyncPoolManager::new(cfg.clone()).unwrap();
    manager.enable(cfg).unwrap();
    let mut guard = manager.reserve(key(1), control()).await.unwrap().unwrap();
    let client = mssql_tds::test_client_support::tds_client_from_tokens(vec![]);
    let connection_id = client.client_connection_id();
    let auth = super::super::connection::AuthMetadata {
        binding: super::super::connection::AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
        expiry: super::super::connection::TokenExpiry::NonToken,
    };
    guard
        .install(OpenedConnection::new(client, auth.clone()))
        .unwrap();
    guard.publish().unwrap();
    guard.return_quiescent().unwrap();
    let mut guard = manager.reserve(key(1), control()).await.unwrap().unwrap();
    assert_eq!(
        guard
            .value_mut()
            .unwrap()
            .client_mut()
            .client_connection_id(),
        connection_id
    );
    guard
        .value_mut()
        .unwrap()
        .client_mut()
        .mark_connection_dead();
    assert!(guard.value_mut().unwrap().client_mut().is_connection_dead());
    drop(guard);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn closing_a_generation_interrupts_claimed_work_without_releasing_its_slot_early() {
    let cfg = config(1, Some(1), 10);
    let manager = manager(cfg.clone());
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    let partial = Client::new(1, &observations);
    let mut work = Box::pin(held.run(AcquireStage::Open, async move {
        let _partial = partial;
        std::future::pending::<PoolResult<()>>().await
    }));
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&work);
    assert!(poll(work.as_mut()).is_pending());
    manager.disable();
    assert!(matches!(
        poll(work.as_mut()),
        Poll::Ready(Err(PoolError::Closed))
    ));
    drop(work);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    manager.enable(cfg).unwrap();
    let mut next = request(&manager, 1);
    assert!(poll(next.as_mut()).is_pending());
    drop(held);
    drop(ready(&mut next));
    assert_eq!(observations.lock().unwrap().peak, 1);
}

#[tokio::test(start_paused = true)]
async fn claimed_work_shares_the_deadline_and_preserves_typed_errors() {
    let manager = manager(config(1, None, 10));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut held = ready(&mut request_with(
        &manager,
        1,
        AcquireControl::new(deadline, CancellationToken::new()),
    ));
    held.run(AcquireStage::Open, async {
        tokio::time::sleep(Duration::from_secs(6)).await;
        Ok(())
    })
    .await
    .unwrap();
    assert!(matches!(
        held.run::<()>(AcquireStage::Reset, async {
            Err(PoolError::IdentityMismatch)
        })
        .await,
        Err(PoolError::IdentityMismatch)
    ));
    assert!(matches!(
        held.run(AcquireStage::Isolation, async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            Ok(())
        })
        .await,
        Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Isolation
        })
    ));
    assert_eq!(Instant::now(), deadline);
    drop(held);
}

#[tokio::test]
async fn published_slots_cannot_reenter_acquisition_work_or_publish_twice() {
    let manager = manager(config(1, None, 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    held.publish().unwrap();
    assert!(matches!(held.publish(), Err(PoolError::Internal { .. })));
    assert!(matches!(
        held.run(AcquireStage::Open, async { Ok(()) }).await,
        Err(PoolError::Internal { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn shutdown_includes_off_lock_retirement_in_its_original_budget() {
    let cfg = PoolConfig::new(
        Duration::from_secs(10),
        Duration::from_secs(10),
        Duration::from_secs(1),
    )
    .unwrap()
    .with_max_size(1)
    .unwrap();
    let manager = manager(cfg);
    let seen: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    let mut client = Client::new(1, &seen);
    client.on_drop = Some(Box::new(|| {
        // advance moves the paused clock before its first yield; do not await in Drop.
        let mut advance = Box::pin(tokio::time::advance(Duration::from_secs(2)));
        let _ = advance
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
    }));
    held.install(client).unwrap();
    held.return_quiescent().unwrap();
    assert!(matches!(
        manager.shutdown().await,
        Err(PoolError::ShutdownTimeout)
    ));
    assert!(!manager.is_enabled());
    assert_eq!(seen.lock().unwrap().live, 0);
    manager.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_failed_admissions_even_after_the_last_client_is_destroyed() {
    let manager = manager(config(1, Some(1), 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    held.publish().unwrap();
    let target = pool(&manager, 1);
    let mut waiting = request(&manager, 1);
    assert!(poll(waiting.as_mut()).is_pending());
    let started = Instant::now();
    let mut shutdown = Box::pin(manager.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    drop(held);
    assert_eq!(observations.lock().unwrap().live, 0);
    assert_eq!(target.counts(), (0, 0, 1));
    assert_eq!(manager.activity.in_flight.load(Ordering::SeqCst), 1);
    assert!(poll(shutdown.as_mut()).is_pending());
    drop(waiting);
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(Instant::now(), started);
    assert_eq!(target.counts(), (0, 0, 0));
    assert_eq!(manager.activity.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(manager.state.lock().unwrap().lifecycle, Lifecycle::Closed);
}

#[tokio::test]
async fn claimed_work_cancellation_discards_partial_clients_but_retains_capacity_until_guard_drop()
{
    for before_poll in [false, true] {
        let manager = manager(config(1, Some(1), 10));
        let observations: Arc<Mutex<Observations>> = Arc::default();
        let cancelled = CancellationToken::new();
        let mut held = ready(&mut request_with(
            &manager,
            1,
            AcquireControl::new(control().deadline(), cancelled.clone()),
        ));
        let partial = Client::new(1, &observations);
        let polled = Arc::new(AtomicUsize::new(0));
        let observed = polled.clone();
        let mut work = Box::pin(held.run(AcquireStage::Open, async move {
            let _partial = partial;
            observed.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<PoolResult<()>>().await
        }));
        if !before_poll {
            assert!(poll(work.as_mut()).is_pending());
        }
        cancelled.cancel();
        assert!(matches!(
            poll(work.as_mut()),
            Poll::Ready(Err(PoolError::Cancelled))
        ));
        drop(work);
        assert_eq!(polled.load(Ordering::SeqCst), usize::from(!before_poll));
        assert_eq!(observations.lock().unwrap().destroyed, [1]);
        assert_eq!(pool(&manager, 1).counts(), (1, 0, 0));
        let mut next = request(&manager, 1);
        assert!(poll(next.as_mut()).is_pending());
        drop(held);
        drop(ready(&mut next));
        assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
        assert_eq!(observations.lock().unwrap().peak, 1);
    }
}

#[tokio::test]
async fn successful_work_cannot_cross_a_disable_reenable_generation_boundary() {
    let cfg = config(1, Some(1), 10);
    let manager = manager(cfg.clone());
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    let target = pool(&manager, 1);
    assert!(matches!(
        held.run(AcquireStage::Reset, async {
            manager.disable();
            manager.enable(cfg)?;
            Ok(())
        })
        .await,
        Err(PoolError::Closed)
    ));
    assert!(manager.is_enabled());
    assert!(Arc::ptr_eq(&target, &pool(&manager, 1)));
    assert!(matches!(held.publish(), Err(PoolError::Closed)));
    assert_eq!(held.value_mut().unwrap().id.get(), 1);
    let mut next = request(&manager, 1);
    assert!(poll(next.as_mut()).is_pending());
    drop(held);
    let mut next = ready(&mut next);
    assert!(next.value_mut().is_none());
    drop(next);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(observations.lock().unwrap().peak, 1);
}

#[tokio::test]
async fn generation_overflow_does_not_enable_or_mutate_configuration() {
    let cfg = config(1, Some(1), 10);
    let manager = TestManager::new(cfg.clone()).unwrap();
    manager.state.lock().unwrap().generation = u64::MAX;
    assert!(matches!(
        manager.enable(cfg.clone()),
        Err(PoolError::Internal {
            reason: "generation overflow"
        })
    ));
    let state = manager.state.lock().unwrap();
    assert_eq!(state.lifecycle, Lifecycle::Disabled);
    assert_eq!(state.generation, u64::MAX);
    assert_eq!(state.config, cfg);
    assert!(state.pools.is_empty());
}

#[tokio::test]
async fn claimed_work_and_publication_reject_foreign_runtimes_without_polling_work() {
    let manager = manager(config(1, Some(1), 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    let polled = Arc::new(AtomicUsize::new(0));
    let observed = polled.clone();
    let (mut held, work, publication) = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let work = held
                .run(AcquireStage::Open, async {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            let publication = held.publish();
            (held, work, publication)
        })
    })
    .join()
    .unwrap();
    assert!(matches!(
        work,
        Err(PoolError::InvalidRequest {
            field: "runtime",
            ..
        })
    ));
    assert!(matches!(
        publication,
        Err(PoolError::InvalidRequest {
            field: "runtime",
            ..
        })
    ));
    assert_eq!(polled.load(Ordering::SeqCst), 0);
    assert_eq!(pool(&manager, 1).counts(), (1, 0, 0));
    assert_eq!(held.value_mut().unwrap().id.get(), 1);
    held.run(AcquireStage::Open, async { Ok(()) })
        .await
        .unwrap();
    held.publish().unwrap();
    drop(held);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
}

#[tokio::test]
async fn queued_reservation_rejects_resuming_on_a_foreign_runtime_and_releases_admission() {
    let manager = manager(config(1, Some(1), 10));
    let held = ready(&mut request(&manager, 1));
    let mut waiting = request(&manager, 1);
    assert!(poll(waiting.as_mut()).is_pending());
    let (waiting, outcome) = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let outcome = poll(waiting.as_mut());
            (waiting, outcome)
        })
    })
    .join()
    .unwrap();
    assert!(matches!(
        outcome,
        Poll::Ready(Err(PoolError::InvalidRequest {
            field: "runtime",
            ..
        }))
    ));
    drop(waiting);
    assert_eq!(pool(&manager, 1).counts(), (1, 0, 0));
    assert_eq!(manager.activity.in_flight.load(Ordering::SeqCst), 0);
    drop(held);
    drop(ready(&mut request(&manager, 1)));
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
}

#[tokio::test]
async fn claimed_initialization_cannot_resume_on_a_foreign_runtime() {
    let manager = manager(config(1, Some(1), 10));
    let observations: Arc<Mutex<Observations>> = Arc::default();
    let mut held = ready(&mut request(&manager, 1));
    held.install(Client::new(1, &observations)).unwrap();
    let polled = AtomicUsize::new(0);
    let mut work = Box::pin(held.run(
        AcquireStage::Open,
        std::future::poll_fn(|_| {
            polled.fetch_add(1, Ordering::SeqCst);
            Poll::<PoolResult<()>>::Pending
        }),
    ));
    assert!(poll(work.as_mut()).is_pending());
    assert_eq!(polled.load(Ordering::SeqCst), 1);
    let (work, outcome) = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let outcome = poll(work.as_mut());
                    (work, outcome)
                })
            })
            .join()
            .unwrap()
    });
    assert!(matches!(
        outcome,
        Poll::Ready(Err(PoolError::InvalidRequest {
            field: "runtime",
            ..
        }))
    ));
    assert_eq!(polled.load(Ordering::SeqCst), 1);
    drop(work);
    assert_eq!(held.value_mut().unwrap().id.get(), 1);
    assert_eq!(pool(&manager, 1).counts(), (1, 0, 0));
    drop(held);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
}

#[tokio::test]
async fn shutdown_wait_cannot_resume_on_a_foreign_runtime_before_or_after_settlement() {
    for settle_before_resume in [true, false] {
        let manager = manager(config(1, Some(1), 10));
        let observations: Arc<Mutex<Observations>> = Arc::default();
        let mut held = ready(&mut request(&manager, 1));
        held.install(Client::new(1, &observations)).unwrap();
        held.publish().unwrap();
        let mut held = Some(held);
        let mut shutdown = Box::pin(manager.shutdown());
        assert!(poll(shutdown.as_mut()).is_pending());
        if settle_before_resume {
            drop(held.take());
        }
        let (shutdown, outcome) = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(async move {
                        let outcome = poll(shutdown.as_mut());
                        (shutdown, outcome)
                    })
                })
                .join()
                .unwrap()
        });
        assert!(matches!(
            outcome,
            Poll::Ready(Err(PoolError::InvalidRequest {
                field: "runtime",
                ..
            }))
        ));
        drop(shutdown);
        assert_eq!(
            manager.state.lock().unwrap().lifecycle,
            Lifecycle::ShuttingDown
        );
        assert!(matches!(
            manager.enable(config(1, None, 10)),
            Err(PoolError::Closed)
        ));
        if let Some(held) = held.as_mut() {
            assert_eq!(held.value_mut().unwrap().id.get(), 1);
            assert_eq!(observations.lock().unwrap().live, 1);
        }
        drop(held);
        manager.shutdown().await.unwrap();
        assert_eq!(manager.state.lock().unwrap().lifecycle, Lifecycle::Closed);
        assert_eq!(observations.lock().unwrap().destroyed, [1]);
        assert_eq!(pool(&manager, 1).counts(), (0, 0, 0));
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A1 executable fit experiment, not a production pool or AQE connection API.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Wake, Waker};

use deadpool::managed::{Manager, Metrics, Object, Pool, PoolError, RecycleResult};
use tokio::sync::{Notify, futures::OwnedNotified};
use tokio::time::{Instant, Sleep};

use super::config::PoolConfig;

#[derive(Default)]
struct Slot {
    client: Mutex<Option<Client>>,
    last_returned: Option<Instant>,
}

struct SlotManager;

impl Manager for SlotManager {
    type Type = Slot;
    type Error = Infallible;

    async fn create(&self) -> Result<Slot, Infallible> {
        Ok(Slot::default())
    }

    async fn recycle(&self, _: &mut Slot, _: &Metrics) -> RecycleResult<Infallible> {
        Ok(())
    }
}

#[derive(Default)]
struct Observations {
    live: usize,
    peak: usize,
    destroyed: Vec<usize>,
}

struct Client {
    id: usize,
    observations: Arc<Mutex<Observations>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Client {
    fn new(id: usize, observations: &Arc<Mutex<Observations>>) -> Self {
        let mut state = observations.lock().unwrap();
        state.live += 1;
        state.peak = state.peak.max(state.live);
        Self {
            id,
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
        state.destroyed.push(self.id);
    }
}

#[derive(Default)]
struct State {
    demand: usize,
    generation: u64,
    accepting: bool,
    terminal: bool,
    retiring: usize,
    maintenance_waiters: Vec<Waker>,
    closed: Arc<Notify>,
    pending: HashMap<u64, PendingGet>,
    next_request: u64,
}

struct PendingGet {
    get: Option<Get>,
    assigned: Option<Object<SlotManager>>,
    deadline: Instant,
    waker: Waker,
}

impl State {
    fn unregister(&mut self, id: u64) {
        if let Some(record) = self.pending.remove(&id) {
            drop(record);
            self.demand -= 1;
        }
    }

    fn expire(&mut self, now: Instant, wakes: &WakeBatch) {
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, entry)| entry.deadline <= now)
            .map(|(id, entry)| (*id, entry.waker.clone()))
            .collect();
        for (id, waker) in expired {
            self.unregister(id);
            wakes.0.lock().unwrap().push(waker);
        }
    }
}

// Semaphore release may wake synchronously. Never call executor wakers under
// the admission lock, including from get-future destruction.
#[derive(Default)]
struct WakeBatch(Mutex<Vec<Waker>>);

impl WakeBatch {
    fn flush(&self) {
        let wakers = std::mem::take(&mut *self.0.lock().unwrap());
        for waker in wakers {
            waker.wake();
        }
    }
}

struct DeferredWake {
    batch: Arc<WakeBatch>,
    target: Waker,
}

impl Wake for DeferredWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.batch.0.lock().unwrap().push(self.target.clone());
    }
}

struct Prototype {
    slots: Pool<SlotManager>,
    state: Mutex<State>,
    wakes: Arc<WakeBatch>,
    max_size: usize,
    max_waiting: Option<usize>,
    settled: Notify,
}

#[derive(Debug)]
enum Error {
    WaitQueueFull,
    Closed,
    AcquireTimeout,
    ShutdownTimeout,
    Backend(PoolError<Infallible>),
    Open(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(error) => write!(formatter, "slot acquisition failed: {error}"),
            Self::Open(error) => write!(formatter, "physical open failed: {error}"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::Open(error) => Some(error),
            _ => None,
        }
    }
}

impl Prototype {
    fn new(max_size: usize, max_waiting: Option<usize>) -> Arc<Self> {
        let budget = std::time::Duration::from_secs(30);
        let config = PoolConfig::new(budget, budget, budget)
            .unwrap()
            .with_max_size(max_size)
            .unwrap()
            .with_max_waiting(max_waiting)
            .unwrap();
        let max_size = config.max_size().get();
        let max_waiting = config.max_waiting().map(std::num::NonZeroUsize::get);
        Arc::new(Self {
            slots: Pool::builder(SlotManager)
                .max_size(max_size)
                .build()
                .unwrap(),
            state: Mutex::new(State {
                accepting: true,
                ..State::default()
            }),
            wakes: Arc::default(),
            max_size,
            max_waiting,
            settled: Notify::new(),
        })
    }

    fn acquire(self: &Arc<Self>, deadline: Instant) -> Acquire {
        let state = self.state.lock().unwrap();
        Acquire {
            pool: self.clone(),
            generation: state.generation,
            id: None,
            deadline: Box::pin(tokio::time::sleep_until(deadline)),
            closed: Box::pin(state.closed.clone().notified_owned()),
        }
    }

    fn finish_retirement(&self, clients: Vec<Client>, slots: Vec<Object<SlotManager>>) {
        drop(clients);
        let mut state = self.state.lock().unwrap();
        state.demand -= slots.len();
        drop(slots);
        state.retiring -= 1;
        let wakers = if state.retiring == 0 {
            std::mem::take(&mut state.maintenance_waiters)
        } else {
            Vec::new()
        };
        drop(state);
        for waker in wakers {
            waker.wake();
        }
        self.settled.notify_waiters();
        self.wakes.flush();
    }

    fn prune(&self, now: Instant, idle_timeout: std::time::Duration) {
        let mut state = self.state.lock().unwrap();
        state.retiring += 1;
        let mut clients = Vec::new();
        self.slots.retain(|slot, _| {
            if slot
                .last_returned
                .is_some_and(|returned| now.duration_since(returned) >= idle_timeout)
                && let Some(client) = slot.client.lock().unwrap().take()
            {
                clients.push(client);
            }
            true
        });
        drop(state);
        self.finish_retirement(clients, Vec::new());
    }

    fn disable(&self) {
        let mut state = self.state.lock().unwrap();
        state.accepting = false;
        state.retiring += 1;
        let pending = std::mem::take(&mut state.pending);
        let mut clients = Vec::new();
        let mut slots = Vec::new();
        for (_, mut record) in pending {
            if let Some(slot) = record.assigned.take() {
                if let Some(client) = slot.client.lock().unwrap().take() {
                    clients.push(client);
                }
                slots.push(slot);
            } else {
                state.demand -= 1;
            }
        }
        self.slots.retain(|slot, _| {
            if let Some(client) = slot.client.lock().unwrap().take() {
                clients.push(client);
            }
            true
        });
        let wakers = std::mem::take(&mut state.maintenance_waiters);
        let closed = state.closed.clone();
        drop(state);
        closed.notify_waiters();
        for waker in wakers {
            waker.wake();
        }
        self.finish_retirement(clients, slots);
    }

    fn enable(&self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        if state.terminal {
            return Err(Error::Closed);
        }
        if !state.accepting {
            state.generation += 1;
            state.accepting = true;
            state.closed = Arc::default();
        }
        Ok(())
    }

    async fn shutdown(&self, deadline: Instant) -> Result<(), Error> {
        {
            let mut state = self.state.lock().unwrap();
            state.terminal = true;
            state.accepting = false;
        }
        self.disable();
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                if state.demand == 0 && state.retiring == 0 {
                    return Ok(());
                }
            }
            tokio::time::timeout_at(deadline, settled)
                .await
                .map_err(|_| Error::ShutdownTimeout)?;
        }
    }
}

type Get = Pin<Box<dyn Future<Output = Result<Object<SlotManager>, PoolError<Infallible>>> + Send>>;

struct Acquire {
    pool: Arc<Prototype>,
    generation: u64,
    id: Option<u64>,
    deadline: Pin<Box<Sleep>>,
    closed: Pin<Box<OwnedNotified>>,
}

impl Future for Acquire {
    type Output = Result<Lease, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _ = this.closed.as_mut().poll(cx);
        let pool = this.pool.clone();
        let mut state = pool.state.lock().unwrap();
        state.expire(Instant::now(), &pool.wakes);
        let error = if !state.accepting || this.generation != state.generation {
            Some(Error::Closed)
        } else if this.deadline.as_mut().poll(cx).is_ready()
            || Instant::now() >= this.deadline.deadline()
        {
            Some(Error::AcquireTimeout)
        } else {
            None
        };
        if let Some(error) = error {
            if let Some(id) = this.id.take() {
                state.unregister(id);
            }
            drop(state);
            pool.settled.notify_waiters();
            pool.wakes.flush();
            return Poll::Ready(Err(error));
        }
        if this.id.is_none() {
            // With infallible, immediately-ready hooks, every registered request
            // either owns a permit or is in Deadpool's FIFO after its first poll.
            let queued = state.demand.saturating_sub(pool.max_size);
            if state.demand >= pool.max_size
                && pool.max_waiting.is_some_and(|limit| queued >= limit)
            {
                drop(state);
                pool.wakes.flush();
                return Poll::Ready(Err(Error::WaitQueueFull));
            }
            state.demand += 1;
            let slots = pool.slots.clone();
            let id = state.next_request;
            state.next_request = id.checked_add(1).expect("request ID overflow");
            state.pending.insert(
                id,
                PendingGet {
                    get: Some(Box::pin(tokio::task::unconstrained(async move {
                        slots.get().await
                    }))),
                    assigned: None,
                    deadline: this.deadline.deadline(),
                    waker: cx.waker().clone(),
                },
            );
            this.id = Some(id);
        }
        let waker = Waker::from(Arc::new(DeferredWake {
            batch: pool.wakes.clone(),
            target: cx.waker().clone(),
        }));
        let record = state.pending.get_mut(&this.id.unwrap()).unwrap();
        record.waker = cx.waker().clone();
        let outcome = if let Some(get) = record.get.as_mut() {
            get.as_mut().poll(&mut Context::from_waker(&waker))
        } else {
            Poll::Ready(Ok(record.assigned.take().unwrap()))
        };
        let outcome = match outcome {
            Poll::Ready(Ok(slot)) => {
                if Instant::now() >= this.deadline.deadline() {
                    let record = state.pending.get_mut(&this.id.unwrap()).unwrap();
                    record.get = None;
                    record.assigned = Some(slot);
                    state.unregister(this.id.take().unwrap());
                    Poll::Ready(Err(Error::AcquireTimeout))
                } else if state.retiring > 0 {
                    let record = state.pending.get_mut(&this.id.unwrap()).unwrap();
                    record.get = None;
                    record.assigned = Some(slot);
                    if !state
                        .maintenance_waiters
                        .iter()
                        .any(|waker| waker.will_wake(cx.waker()))
                    {
                        state.maintenance_waiters.push(cx.waker().clone());
                    }
                    Poll::Pending
                } else {
                    state.pending.remove(&this.id.take().unwrap());
                    Poll::Ready(Ok(Lease {
                        slot: Some(slot),
                        pool: pool.clone(),
                        generation: this.generation,
                    }))
                }
            }
            Poll::Ready(Err(error)) => {
                state.unregister(this.id.take().unwrap());
                Poll::Ready(Err(Error::Backend(error)))
            }
            Poll::Pending => Poll::Pending,
        };
        drop(state);
        pool.settled.notify_waiters();
        pool.wakes.flush();
        outcome
    }
}

impl Drop for Acquire {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let mut state = self.pool.state.lock().unwrap();
            state.unregister(id);
            drop(state);
            self.pool.settled.notify_waiters();
            self.pool.wakes.flush();
        }
    }
}

struct Lease {
    slot: Option<Object<SlotManager>>,
    pool: Arc<Prototype>,
    generation: u64,
}

impl Lease {
    fn install(&self, client: Client) {
        let mut value = self.slot.as_ref().unwrap().client.lock().unwrap();
        assert!(value.is_none());
        *value = Some(client);
    }

    fn client_id(&self) -> Option<usize> {
        self.slot
            .as_ref()
            .unwrap()
            .client
            .lock()
            .unwrap()
            .as_ref()
            .map(|client| client.id)
    }

    // The test caller asserts successful quiescence. No TDS cleanup is modeled.
    fn release_clean(mut self) {
        let mut state = self.pool.state.lock().unwrap();
        state.expire(Instant::now(), &self.pool.wakes);
        if state.accepting && self.generation == state.generation {
            self.slot.as_mut().unwrap().last_returned = Some(Instant::now());
            drop(self.slot.take());
            state.demand -= 1;
        }
        drop(state);
        self.pool.settled.notify_waiters();
        self.pool.wakes.flush();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            let client = slot.client.lock().unwrap().take();
            drop(client);
            let mut state = self.pool.state.lock().unwrap();
            state.expire(Instant::now(), &self.pool.wakes);
            drop(slot);
            state.demand -= 1;
            drop(state);
            self.pool.settled.notify_waiters();
            self.pool.wakes.flush();
        }
    }
}

type Open = Pin<Box<dyn Future<Output = Result<Client, std::io::Error>> + Send>>;

struct Opening {
    open: Option<Open>,
    lease: Option<Lease>,
}

impl Future for Opening {
    type Output = Result<Lease, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.open.as_mut().unwrap().as_mut().poll(cx) {
            Poll::Ready(result) => {
                drop(this.open.take());
                match result {
                    Ok(client) => {
                        let lease = this.lease.take().unwrap();
                        lease.install(client);
                        Poll::Ready(Ok(lease))
                    }
                    Err(error) => {
                        drop(this.lease.take());
                        Poll::Ready(Err(Error::Open(error)))
                    }
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        drop(self.open.take());
        drop(self.lease.take());
    }
}

fn poll(request: &mut Acquire) -> Poll<Result<Lease, Error>> {
    Pin::new(request).poll(&mut Context::from_waker(Waker::noop()))
}

fn ready(request: &mut Acquire) -> Lease {
    match poll(request) {
        Poll::Ready(Ok(lease)) => lease,
        Poll::Ready(Err(error)) => panic!("acquisition failed: {error:?}"),
        Poll::Pending => panic!("expected an immediate grant"),
    }
}

fn request(pool: &Arc<Prototype>) -> Acquire {
    pool.acquire(Instant::now() + std::time::Duration::from_secs(30))
}

#[tokio::test]
async fn exhausted_cooperative_budget_acquires_free_and_recycled_slots_promptly() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    while tokio::task::coop::has_budget_remaining() {
        tokio::task::consume_budget().await;
    }

    let lease = ready(&mut request(&pool));
    lease.install(Client::new(1, &observations));
    lease.release_clean();
    assert!(!tokio::task::coop::has_budget_remaining());
    let lease = ready(&mut request(&pool));
    assert_eq!(lease.client_id(), Some(1));
    drop(lease);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn exhausted_cooperative_budget_admits_waiters_before_newcomers() {
    let pool = Prototype::new(1, Some(1));
    let lease = ready(&mut request(&pool));
    while tokio::task::coop::has_budget_remaining() {
        tokio::task::consume_budget().await;
    }

    let mut older = request(&pool);
    assert!(poll(&mut older).is_pending());
    drop(lease);
    let mut newer = request(&pool);
    assert!(
        tokio::task::unconstrained(async { poll(&mut newer) })
            .await
            .is_pending()
    );
    assert!(matches!(
        poll(&mut request(&pool)),
        Poll::Ready(Err(Error::WaitQueueFull))
    ));
    let older = ready(&mut older);
    assert!(poll(&mut newer).is_pending());
    drop(older);
    drop(ready(&mut newer));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn assigned_unpolled_grant_does_not_consume_queue_limit() {
    let pool = Prototype::new(1, Some(1));
    let a = ready(&mut request(&pool));
    let mut b = request(&pool);
    assert!(poll(&mut b).is_pending());
    let mut rejected = request(&pool);
    assert!(matches!(
        poll(&mut rejected),
        Poll::Ready(Err(Error::WaitQueueFull))
    ));
    drop(a);
    let mut c = request(&pool);
    assert!(poll(&mut c).is_pending());
    assert!(matches!(
        poll(&mut request(&pool)),
        Poll::Ready(Err(Error::WaitQueueFull))
    ));
    let b = ready(&mut b);
    assert!(poll(&mut c).is_pending());
    drop(b);
    drop(ready(&mut c));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn cancellation_before_and_after_assignment_preserves_fifo() {
    for cancel_after_assignment in [false, true] {
        let pool = Prototype::new(1, None);
        let a = ready(&mut request(&pool));
        let mut b = request(&pool);
        let mut c = request(&pool);
        assert!(poll(&mut b).is_pending());
        assert!(poll(&mut c).is_pending());
        if cancel_after_assignment {
            drop(a);
            drop(b);
        } else {
            drop(b);
            drop(a);
        }
        drop(ready(&mut c));
        assert_eq!(pool.state.lock().unwrap().demand, 0);
    }
}

#[tokio::test]
async fn retry_joins_tail_and_is_subject_to_queue_limit() {
    let pool = Prototype::new(1, Some(1));
    let candidate = ready(&mut request(&pool));
    let mut older = request(&pool);
    assert!(poll(&mut older).is_pending());
    drop(candidate);
    let mut newcomer = request(&pool);
    assert!(poll(&mut newcomer).is_pending());
    let mut retry = request(&pool);
    assert!(matches!(
        poll(&mut retry),
        Poll::Ready(Err(Error::WaitQueueFull))
    ));
    let older = ready(&mut older);
    drop(older);
    let mut retry = request(&pool);
    assert!(poll(&mut retry).is_pending());
    let newcomer = ready(&mut newcomer);
    assert!(poll(&mut retry).is_pending());
    drop(newcomer);
    drop(ready(&mut retry));
}

#[tokio::test(start_paused = true)]
async fn deadline_is_not_restarted_after_assignment() {
    let pool = Prototype::new(1, Some(1));
    let a = ready(&mut request(&pool));
    let mut b = pool.acquire(Instant::now() + std::time::Duration::from_secs(1));
    assert!(poll(&mut b).is_pending());
    drop(a);
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(matches!(
        poll(&mut b),
        Poll::Ready(Err(Error::AcquireTimeout))
    ));
    drop(ready(&mut request(&pool)));
}

#[tokio::test]
async fn interrupted_owner_destroys_client_before_slot_reuse() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let mut operation = Box::pin(async {
        let lease = ready(&mut request(&pool));
        lease.install(Client::new(1, &observations));
        std::future::pending::<()>().await;
        drop(lease);
    });
    assert!(
        operation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut next = request(&pool);
    assert!(poll(&mut next).is_pending());
    drop(operation);
    let next = ready(&mut next);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    next.install(Client::new(2, &observations));
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(next);
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn generations_keep_old_leases_within_the_same_cap() {
    let pool = Prototype::new(1, Some(1));
    let observations = Arc::default();
    let old = ready(&mut request(&pool));
    old.install(Client::new(1, &observations));
    let mut cancelled = request(&pool);
    assert!(poll(&mut cancelled).is_pending());
    pool.disable();
    pool.enable().unwrap();
    assert!(matches!(
        poll(&mut cancelled),
        Poll::Ready(Err(Error::Closed))
    ));
    assert_eq!(old.client_id(), Some(1));
    let mut new = request(&pool);
    assert!(poll(&mut new).is_pending());
    old.release_clean();
    let new = ready(&mut new);
    assert_eq!(new.client_id(), None);
    new.install(Client::new(2, &observations));
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(new);
}

#[tokio::test(start_paused = true)]
async fn idle_age_starts_on_return_and_zero_timeout_prunes() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let lease = ready(&mut request(&pool));
    lease.install(Client::new(1, &observations));
    tokio::time::advance(std::time::Duration::from_secs(100)).await;
    lease.release_clean();
    pool.prune(Instant::now(), std::time::Duration::from_secs(10));
    let lease = ready(&mut request(&pool));
    assert_eq!(lease.client_id(), Some(1));
    lease.release_clean();
    pool.prune(Instant::now(), std::time::Duration::ZERO);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    let lease = ready(&mut request(&pool));
    assert_eq!(lease.client_id(), None);
    drop(lease);
}

#[tokio::test]
async fn queue_limit_is_exact_for_multiple_slots_and_independent_pools() {
    for max_size in [1, 2, 4] {
        for max_waiting in [1, 3] {
            let pool = Prototype::new(max_size, Some(max_waiting));
            let other = Prototype::new(1, Some(1));
            let leases: Vec<_> = (0..max_size).map(|_| ready(&mut request(&pool))).collect();
            let mut requests: Vec<_> = (0..max_waiting).map(|_| request(&pool)).collect();
            for entry in &mut requests {
                assert!(poll(entry).is_pending());
            }
            assert!(matches!(
                poll(&mut request(&pool)),
                Poll::Ready(Err(Error::WaitQueueFull))
            ));
            drop(ready(&mut request(&other)));
            drop(leases);
            let grants = max_size.min(max_waiting);
            for entry in requests.iter_mut().take(grants) {
                drop(ready(entry));
            }
            for entry in requests.iter_mut().skip(grants) {
                drop(ready(entry));
            }
            assert_eq!(pool.state.lock().unwrap().demand, 0);
        }
    }
}

struct LockCheckingWake {
    pool: Weak<Prototype>,
    calls: std::sync::atomic::AtomicUsize,
}

impl Wake for LockCheckingWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let pool = self.pool.upgrade().unwrap();
        assert!(
            pool.state.try_lock().is_ok(),
            "notification under state lock"
        );
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn disable_wakes_queued_acquisitions_without_capacity_release() {
    let pool = Prototype::new(1, None);
    let lease = ready(&mut request(&pool));
    let mut queued = request(&pool);
    let observed = Arc::new(LockCheckingWake {
        pool: Arc::downgrade(&pool),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let waker = Waker::from(observed.clone());
    assert!(
        Pin::new(&mut queued)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    pool.disable();
    assert!(observed.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    assert!(matches!(poll(&mut queued), Poll::Ready(Err(Error::Closed))));
    drop(lease);
}

#[tokio::test]
async fn slot_handoff_wakes_executor_only_outside_state_lock() {
    let pool = Prototype::new(1, None);
    let lease = ready(&mut request(&pool));
    let mut queued = request(&pool);
    let observed = Arc::new(LockCheckingWake {
        pool: Arc::downgrade(&pool),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let waker = Waker::from(observed.clone());
    assert!(
        Pin::new(&mut queued)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(lease);
    assert!(observed.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    drop(ready(&mut queued));
}

#[tokio::test]
async fn pruning_blocks_claims_until_off_lock_destruction_finishes() {
    let pool = Prototype::new(1, Some(1));
    let observations = Arc::default();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let mut client = Client::new(1, &observations);
    let weak = Arc::downgrade(&pool);
    client.on_drop = Some(Box::new(move || {
        assert!(weak.upgrade().unwrap().state.try_lock().is_ok());
        started_tx.send(()).unwrap();
        finish_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }));
    let lease = ready(&mut request(&pool));
    lease.install(client);
    lease.release_clean();
    let worker_pool = pool.clone();
    let now = Instant::now();
    let worker = std::thread::spawn(move || worker_pool.prune(now, std::time::Duration::ZERO));
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(pool.state.try_lock().is_ok());
    let mut next = request(&pool);
    assert!(poll(&mut next).is_pending());
    let mut queued = request(&pool);
    assert!(poll(&mut queued).is_pending());
    assert!(matches!(
        poll(&mut request(&pool)),
        Poll::Ready(Err(Error::WaitQueueFull))
    ));
    assert_eq!(observations.lock().unwrap().live, 1);
    finish_tx.send(()).unwrap();
    worker.join().unwrap();
    assert!(poll(&mut queued).is_pending());
    let next = ready(&mut next);
    next.install(Client::new(2, &observations));
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(next);
    drop(ready(&mut queued));
}

#[tokio::test(start_paused = true)]
async fn shutdown_times_out_without_revoking_a_lease_and_remains_terminal() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let lease = ready(&mut request(&pool));
    lease.install(Client::new(1, &observations));
    let mut shutdown = Box::pin(pool.shutdown(Instant::now() + std::time::Duration::from_secs(1)));
    assert!(
        shutdown
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(matches!(
        shutdown
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(Error::ShutdownTimeout))
    ));
    drop(shutdown);
    assert!(matches!(pool.enable(), Err(Error::Closed)));
    assert_eq!(lease.client_id(), Some(1));
    drop(lease);
    pool.shutdown(Instant::now()).await.unwrap();
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn disable_reclaims_unpolled_old_requests_before_reenable() {
    let pool = Prototype::new(1, Some(1));
    let old = ready(&mut request(&pool));
    let mut stale = request(&pool);
    assert!(poll(&mut stale).is_pending());
    pool.disable();
    pool.enable().unwrap();
    let mut current = request(&pool);
    assert!(poll(&mut current).is_pending());
    assert!(matches!(poll(&mut stale), Poll::Ready(Err(Error::Closed))));
    drop(old);
    drop(ready(&mut current));

    let old = ready(&mut request(&pool));
    let mut assigned = request(&pool);
    assert!(poll(&mut assigned).is_pending());
    drop(old);
    pool.disable();
    pool.enable().unwrap();
    drop(ready(&mut request(&pool)));
    assert!(matches!(
        poll(&mut assigned),
        Poll::Ready(Err(Error::Closed))
    ));
}

#[tokio::test(start_paused = true)]
async fn expired_unpolled_waiter_is_removed_before_grant_and_admission() {
    let pool = Prototype::new(1, Some(1));
    let lease = ready(&mut request(&pool));
    let mut expired = pool.acquire(Instant::now() + std::time::Duration::from_secs(1));
    assert!(poll(&mut expired).is_pending());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let mut current = request(&pool);
    assert!(poll(&mut current).is_pending());
    drop(lease);
    drop(ready(&mut current));
    assert!(matches!(
        poll(&mut expired),
        Poll::Ready(Err(Error::AcquireTimeout))
    ));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn backend_failure_preserves_cause_and_settles_registration() {
    let pool = Prototype::new(1, None);
    // Fault injection only; reversible lifecycle never closes the slot pool.
    pool.slots.close();
    let result = poll(&mut request(&pool));
    assert!(matches!(
        result,
        Poll::Ready(Err(Error::Backend(PoolError::Closed)))
    ));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn cancelled_open_drops_partial_client_before_releasing_its_slot() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let lease = ready(&mut request(&pool));
    let partial = Client::new(1, &observations);
    let mut opening = Opening {
        lease: Some(lease),
        open: Some(Box::pin(async move {
            std::future::pending::<()>().await;
            Ok(partial)
        })),
    };
    assert!(
        Pin::new(&mut opening)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut queued = request(&pool);
    assert!(poll(&mut queued).is_pending());
    drop(opening);
    let next = ready(&mut queued);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    next.install(Client::new(2, &observations));
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(next);
}

#[tokio::test]
async fn caller_owned_open_preserves_fatal_error_without_retry() {
    let pool = Prototype::new(1, None);
    let error = std::io::Error::other("provider failure sentinel");
    let result = Opening {
        lease: Some(ready(&mut request(&pool))),
        open: Some(Box::pin(async { Err(error) })),
    }
    .await;
    assert!(matches!(
        result,
        Err(Error::Open(error)) if error.to_string() == "provider failure sentinel"
    ));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
    let observations = Arc::default();
    let client = Client::new(1, &observations);
    let lease = Opening {
        lease: Some(ready(&mut request(&pool))),
        open: Some(Box::pin(async { Ok(client) })),
    }
    .await
    .unwrap();
    assert_eq!(lease.client_id(), Some(1));
    drop(lease);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contended_caller_owned_clients_never_exceed_capacity() {
    let pool = Prototype::new(3, None);
    let observations = Arc::new(Mutex::new(Observations::default()));
    let mut tasks = Vec::new();
    for id in 0..64 {
        let pool = pool.clone();
        let observations = observations.clone();
        tasks.push(tokio::spawn(async move {
            let lease = request(&pool).await.unwrap();
            lease.install(Client::new(id, &observations));
            tokio::task::yield_now().await;
            drop(lease);
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let observations = observations.lock().unwrap();
    assert_eq!(observations.destroyed.len(), 64);
    assert_eq!(observations.live, 0);
    assert!(observations.peak <= 3);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn disable_reclaims_grants_assigned_behind_maintenance_barrier() {
    let pool = Prototype::new(1, Some(1));
    pool.state.lock().unwrap().retiring += 1;
    let mut assigned = request(&pool);
    let mut queued = request(&pool);
    assert!(poll(&mut assigned).is_pending());
    assert!(poll(&mut queued).is_pending());
    pool.disable();
    pool.enable().unwrap();
    let mut current = request(&pool);
    assert!(poll(&mut current).is_pending());
    pool.finish_retirement(Vec::new(), Vec::new());
    drop(ready(&mut current));
    assert!(matches!(
        poll(&mut assigned),
        Poll::Ready(Err(Error::Closed))
    ));
    assert!(matches!(poll(&mut queued), Poll::Ready(Err(Error::Closed))));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn cancellation_of_shutdown_does_not_reopen_admission() {
    let pool = Prototype::new(1, None);
    let lease = ready(&mut request(&pool));
    let mut shutdown = Box::pin(pool.shutdown(Instant::now() + std::time::Duration::from_secs(30)));
    assert!(
        shutdown
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(shutdown);
    assert!(matches!(pool.enable(), Err(Error::Closed)));
    drop(lease);
    pool.shutdown(Instant::now()).await.unwrap();
}

#[tokio::test]
async fn cancelling_a_maintenance_grant_hands_capacity_to_the_oldest_waiter() {
    let pool = Prototype::new(1, Some(1));
    pool.state.lock().unwrap().retiring += 1;
    let mut assigned = request(&pool);
    let mut older = request(&pool);
    assert!(poll(&mut assigned).is_pending());
    assert!(poll(&mut older).is_pending());
    drop(assigned);
    let mut newcomer = request(&pool);
    assert!(poll(&mut newcomer).is_pending());
    pool.finish_retirement(Vec::new(), Vec::new());
    assert!(poll(&mut newcomer).is_pending());
    drop(ready(&mut older));
    drop(ready(&mut newcomer));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test(start_paused = true)]
async fn expired_admission_does_not_consume_capacity_or_a_full_queue_entry() {
    let pool = Prototype::new(1, Some(1));
    let lease = ready(&mut request(&pool));
    let mut queued = request(&pool);
    assert!(poll(&mut queued).is_pending());
    assert!(matches!(
        poll(&mut pool.acquire(Instant::now())),
        Poll::Ready(Err(Error::AcquireTimeout))
    ));
    assert_eq!(pool.state.lock().unwrap().demand, 2);
    pool.disable();
    assert!(matches!(
        poll(&mut pool.acquire(Instant::now())),
        Poll::Ready(Err(Error::Closed))
    ));
    assert!(matches!(poll(&mut queued), Poll::Ready(Err(Error::Closed))));
    drop(lease);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn shutdown_waits_for_clean_return_and_destroys_the_client_before_success() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let lease = ready(&mut request(&pool));
    lease.install(Client::new(1, &observations));
    let mut queued = request(&pool);
    assert!(poll(&mut queued).is_pending());
    let mut shutdown = Box::pin(pool.shutdown(Instant::now() + std::time::Duration::from_secs(30)));
    assert!(
        shutdown
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(matches!(poll(&mut queued), Poll::Ready(Err(Error::Closed))));
    assert_eq!(lease.client_id(), Some(1));
    lease.release_clean();
    assert!(matches!(
        shutdown
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
    assert!(matches!(pool.enable(), Err(Error::Closed)));
    pool.shutdown(Instant::now()).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_expiry_is_inclusive_and_never_prunes_a_checked_out_client() {
    let pool = Prototype::new(2, None);
    let observations = Arc::default();
    let idle = ready(&mut request(&pool));
    idle.install(Client::new(1, &observations));
    let borrowed = ready(&mut request(&pool));
    borrowed.install(Client::new(2, &observations));
    idle.release_clean();
    let timeout = std::time::Duration::from_secs(10);
    tokio::time::advance(timeout - std::time::Duration::from_nanos(1)).await;
    pool.prune(Instant::now(), timeout);
    assert!(observations.lock().unwrap().destroyed.is_empty());
    tokio::time::advance(std::time::Duration::from_nanos(1)).await;
    pool.prune(Instant::now(), timeout);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(borrowed.client_id(), Some(2));
    let replacement = ready(&mut request(&pool));
    assert_eq!(replacement.client_id(), None);
    replacement.install(Client::new(3, &observations));
    assert_eq!(observations.lock().unwrap().peak, 2);
    drop(borrowed);
    drop(replacement);
    assert_eq!(observations.lock().unwrap().live, 0);
}

#[tokio::test]
async fn repeated_lifecycle_calls_do_not_recreate_idle_clients_or_capacity() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let lease = ready(&mut request(&pool));
    lease.install(Client::new(1, &observations));
    lease.release_clean();
    let mut old_unpolled = request(&pool);
    let original_generation = pool.state.lock().unwrap().generation;
    pool.enable().unwrap();
    pool.enable().unwrap();
    assert_eq!(pool.state.lock().unwrap().generation, original_generation);
    let lease = ready(&mut request(&pool));
    assert_eq!(lease.client_id(), Some(1));
    lease.release_clean();
    pool.disable();
    pool.disable();
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    pool.enable().unwrap();
    pool.enable().unwrap();
    assert_eq!(
        pool.state.lock().unwrap().generation,
        original_generation + 1
    );
    assert!(matches!(
        poll(&mut old_unpolled),
        Poll::Ready(Err(Error::Closed))
    ));
    let lease = ready(&mut request(&pool));
    assert_eq!(lease.client_id(), None);
    lease.install(Client::new(2, &observations));
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(lease);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test]
async fn disable_destroys_a_client_bearing_maintenance_grant_off_lock_before_slot_return() {
    let pool = Prototype::new(1, None);
    let observations = Arc::default();
    let weak = Arc::downgrade(&pool);
    let destruction_order = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_order = destruction_order.clone();
    let mut client = Client::new(1, &observations);
    client.on_drop = Some(Box::new(move || {
        observed_order.store(
            weak.upgrade().is_some_and(|pool| {
                pool.state
                    .try_lock()
                    .is_ok_and(|state| state.demand == 1 && pool.slots.status().available == 0)
            }),
            std::sync::atomic::Ordering::SeqCst,
        );
    }));
    let lease = ready(&mut request(&pool));
    lease.install(client);
    lease.release_clean();
    pool.state.lock().unwrap().retiring += 1;
    let mut assigned = request(&pool);
    let observed = Arc::new(LockCheckingWake {
        pool: Arc::downgrade(&pool),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let waker = Waker::from(observed.clone());
    for _ in 0..2 {
        assert!(
            Pin::new(&mut assigned)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    let waiter_count = pool.state.lock().unwrap().maintenance_waiters.len();
    assert_eq!(waiter_count, 1);
    pool.disable();
    assert!(destruction_order.load(std::sync::atomic::Ordering::SeqCst));
    assert!(observed.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
    pool.enable().unwrap();
    let mut current = request(&pool);
    assert!(poll(&mut current).is_pending());
    pool.finish_retirement(Vec::new(), Vec::new());
    let current = ready(&mut current);
    assert_eq!(current.client_id(), None);
    assert!(matches!(
        poll(&mut assigned),
        Poll::Ready(Err(Error::Closed))
    ));
    drop(current);
    assert_eq!(pool.state.lock().unwrap().demand, 0);
}

#[tokio::test(start_paused = true)]
async fn an_expired_untouched_maintenance_grant_can_retain_its_quiescent_client() {
    let pool = Prototype::new(1, Some(1));
    let observations = Arc::default();
    let lease = ready(&mut request(&pool));
    lease.install(Client::new(1, &observations));
    lease.release_clean();
    pool.state.lock().unwrap().retiring += 1;
    let mut assigned = pool.acquire(Instant::now() + std::time::Duration::from_secs(1));
    assert!(poll(&mut assigned).is_pending());
    assert!(poll(&mut assigned).is_pending());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(matches!(
        poll(&mut assigned),
        Poll::Ready(Err(Error::AcquireTimeout))
    ));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
    assert!(observations.lock().unwrap().destroyed.is_empty());
    pool.finish_retirement(Vec::new(), Vec::new());
    let next = ready(&mut request(&pool));
    assert_eq!(next.client_id(), Some(1));
    assert_eq!(observations.lock().unwrap().peak, 1);
    drop(next);
    assert_eq!(observations.lock().unwrap().destroyed, [1]);
}

#[tokio::test(start_paused = true)]
async fn a_deadline_crossed_during_get_cannot_publish_or_leak_the_grant() {
    let pool = Prototype::new(1, None);
    let lease = ready(&mut request(&pool));
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    let mut queued = pool.acquire(deadline);
    assert!(poll(&mut queued).is_pending());
    {
        let mut state = pool.state.lock().unwrap();
        let record = state.pending.get_mut(&queued.id.unwrap()).unwrap();
        let get = record.get.take().unwrap();
        record.get = Some(Box::pin(async move {
            let result = get.await;
            assert!(result.is_ok());
            // Advance inside get's final poll, after the admission deadline check.
            let mut advance = Box::pin(tokio::time::advance(std::time::Duration::from_secs(1)));
            assert!(
                advance
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(Instant::now(), deadline);
            result
        }));
    }
    drop(lease);
    assert!(matches!(
        poll(&mut queued),
        Poll::Ready(Err(Error::AcquireTimeout))
    ));
    assert_eq!(pool.state.lock().unwrap().demand, 0);
    drop(ready(&mut request(&pool)));
    assert!(pool.state.lock().unwrap().pending.is_empty());
}

#[test]
fn prototype_errors_preserve_sources_and_distinct_diagnostics() {
    use std::error::Error as _;

    let backend = Error::Backend(PoolError::Closed);
    assert_eq!(
        backend.to_string(),
        "slot acquisition failed: Pool has been closed"
    );
    assert!(matches!(
        backend
            .source()
            .unwrap()
            .downcast_ref::<PoolError<Infallible>>(),
        Some(PoolError::Closed)
    ));
    let open = Error::Open(std::io::Error::other("open failure sentinel"));
    assert_eq!(
        open.to_string(),
        "physical open failed: open failure sentinel"
    );
    assert_eq!(
        open.source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .to_string(),
        "open failure sentinel"
    );
    for (error, message) in [
        (Error::WaitQueueFull, "WaitQueueFull"),
        (Error::Closed, "Closed"),
        (Error::AcquireTimeout, "AcquireTimeout"),
        (Error::ShutdownTimeout, "ShutdownTimeout"),
    ] {
        assert_eq!(error.to_string(), message);
        assert!(error.source().is_none());
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};

use deadpool::managed::{Manager, Metrics, Object, Pool, RecycleResult};
use tokio::time::{Instant, Sleep};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use super::config::PoolConfig;
use super::control::AcquireControl;
use super::error::{AcquireStage, PoolError, PoolResult};
use super::manager::Activity;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> PoolResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| PoolError::Internal {
        reason: "poisoned state mutex",
    })
}

struct Cached<C> {
    value: C,
    returned: Instant,
}

struct Slot<C> {
    cached: Mutex<Option<Cached<C>>>,
}

struct SlotManager<C>(PhantomData<fn() -> C>);

impl<C: Send + 'static> Manager for SlotManager<C> {
    type Type = Slot<C>;
    type Error = Infallible;

    async fn create(&self) -> Result<Self::Type, Infallible> {
        Ok(Slot {
            cached: Mutex::new(None),
        })
    }

    async fn recycle(&self, _: &mut Self::Type, _: &Metrics) -> RecycleResult<Infallible> {
        Ok(())
    }
}

type OwnedSlot<C> = Object<SlotManager<C>>;
type Get<C> = Pin<
    Box<dyn Future<Output = Result<OwnedSlot<C>, deadpool::managed::PoolError<Infallible>>> + Send>,
>;

#[derive(Clone, Copy)]
enum Failure {
    Closed,
    Cancelled,
    Timeout,
    Internal,
}

impl Failure {
    fn error(self) -> PoolError {
        match self {
            Self::Closed => PoolError::Closed,
            Self::Cancelled => PoolError::Cancelled,
            Self::Timeout => PoolError::AcquireTimeout {
                stage: AcquireStage::Wait,
            },
            Self::Internal => PoolError::Internal {
                reason: "slot scheduler failed",
            },
        }
    }
}

struct Pending<C: Send + 'static> {
    get: Option<Get<C>>,
    assigned: Option<OwnedSlot<C>>,
    deadline: Instant,
    cancel: CancellationToken,
    waker: Waker,
    failure: Option<Failure>,
}

struct State<C: Send + 'static> {
    generation: u64,
    accepting: bool,
    cancel: CancellationToken,
    demand: usize,
    retiring: usize,
    pending: HashMap<u64, Pending<C>>,
    next_id: u64,
}

// Only semaphore/get bookkeeping uses this waker. Executor callbacks are flushed
// after releasing state locks, including when a queued get future is dropped.
#[derive(Default)]
struct WakeBatch(Mutex<Vec<Waker>>);

impl WakeBatch {
    fn push(&self, waker: Waker) {
        self.0
            .lock()
            .expect("wake batch contains no callbacks while locked")
            .push(waker);
    }

    fn flush(&self) {
        let wakers = std::mem::take(
            &mut *self
                .0
                .lock()
                .expect("wake batch contains no callbacks while locked"),
        );
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
        self.batch.push(self.target.clone());
    }
}

pub(super) struct PerKeyPool<C: Send + 'static> {
    slots: Pool<SlotManager<C>>,
    state: Mutex<State<C>>,
    config: PoolConfig,
    wakes: Arc<WakeBatch>,
    activity: Arc<Activity>,
}

impl<C: Send + 'static> PerKeyPool<C> {
    pub(super) fn new(
        config: PoolConfig,
        generation: u64,
        activity: Arc<Activity>,
    ) -> PoolResult<Arc<Self>> {
        let slots = Pool::builder(SlotManager(PhantomData))
            .max_size(config.max_size().get())
            .build()
            .map_err(|_| PoolError::Internal {
                reason: "slot scheduler construction failed",
            })?;
        Ok(Arc::new(Self {
            slots,
            state: Mutex::new(State {
                generation,
                accepting: true,
                cancel: CancellationToken::new(),
                demand: 0,
                retiring: 0,
                pending: HashMap::new(),
                next_id: 0,
            }),
            config,
            wakes: Arc::default(),
            activity,
        }))
    }

    pub(super) fn enable(&self, generation: u64) -> PoolResult<()> {
        let mut state = lock(&self.state)?;
        state.generation = generation;
        state.accepting = true;
        state.cancel = CancellationToken::new();
        Ok(())
    }

    pub(super) fn busy(&self) -> PoolResult<bool> {
        let state = lock(&self.state)?;
        Ok(state.demand != 0 || state.retiring != 0 || !state.pending.is_empty())
    }

    pub(super) fn settled(&self) -> PoolResult<bool> {
        let state = lock(&self.state)?;
        Ok(state.demand == 0 && state.retiring == 0)
    }

    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize, usize) {
        let state = lock(&self.state).unwrap();
        (state.demand, state.retiring, state.pending.len())
    }

    pub(super) fn inactive(&self) -> PoolResult<bool> {
        if self.busy()? {
            return Ok(false);
        }
        let mut empty = true;
        let mut failed = false;
        self.slots.retain(|slot, _| {
            match lock(&slot.cached) {
                Ok(cached) => empty &= cached.is_none(),
                Err(_) => failed = true,
            }
            true
        });
        if failed {
            return Err(PoolError::Internal {
                reason: "poisoned idle slot",
            });
        }
        Ok(empty)
    }

    pub(super) fn request(self: &Arc<Self>, control: AcquireControl) -> PoolResult<AcquireSlot<C>> {
        let state = lock(&self.state)?;
        Ok(AcquireSlot {
            pool: self.clone(),
            generation: state.generation,
            closed: Box::pin(state.cancel.clone().cancelled_owned()),
            cancelled: Box::pin(control.cancellation().clone().cancelled_owned()),
            deadline: Box::pin(tokio::time::sleep_until(control.deadline())),
            control,
            id: None,
        })
    }

    pub(super) fn maintain(
        self: &Arc<Self>,
        now: Instant,
        close: bool,
    ) -> PoolResult<Retirement<C>> {
        self.refresh(now, close, true)
    }

    fn refresh(
        self: &Arc<Self>,
        now: Instant,
        close: bool,
        prune_idle: bool,
    ) -> PoolResult<Retirement<C>> {
        let mut state = lock(&self.state)?;
        let mut retirement = Retirement::new(self.clone(), &mut state)?;
        if close {
            state.accepting = false;
            retirement.cancel = Some(state.cancel.clone());
        }
        self.reap(&mut state, now, close, &mut retirement);
        if prune_idle {
            self.slots.retain(|slot, _| {
                match lock(&slot.cached) {
                    Ok(mut cached) => {
                        if cached.as_ref().is_some_and(|entry| {
                            close
                                || now.saturating_duration_since(entry.returned)
                                    >= self.config.idle_timeout()
                        }) {
                            retirement.clients.extend(cached.take());
                        }
                    }
                    Err(_) => {
                        state.accepting = false;
                        retirement.failed = true;
                        retirement.cancel = Some(state.cancel.clone());
                    }
                }
                true
            });
        }
        if !retirement.clients.is_empty() || !retirement.slots.is_empty() {
            state.retiring += 1;
            retirement.armed = true;
        }
        Ok(retirement)
    }

    fn reap(
        &self,
        state: &mut State<C>,
        now: Instant,
        close: bool,
        retirement: &mut Retirement<C>,
    ) {
        for record in state.pending.values_mut() {
            if record.failure.is_some() {
                continue;
            }
            let failure = if close {
                Some(Failure::Closed)
            } else if record.cancel.is_cancelled() {
                Some(Failure::Cancelled)
            } else if record.deadline <= now {
                Some(Failure::Timeout)
            } else {
                None
            };
            if let Some(failure) = failure {
                record.failure = Some(failure);
                self.wakes.push(record.waker.clone());
                if let Some(slot) = record.assigned.take() {
                    match lock(&slot.cached) {
                        Ok(mut cached) => {
                            if cached.as_ref().is_some_and(|entry| {
                                close
                                    || now.saturating_duration_since(entry.returned)
                                        >= self.config.idle_timeout()
                            }) {
                                retirement.clients.extend(cached.take());
                            }
                        }
                        Err(_) => {
                            state.accepting = false;
                            retirement.failed = true;
                            retirement.cancel = Some(state.cancel.clone());
                        }
                    }
                    retirement.slots.push(slot);
                } else {
                    // A pending get can own only semaphore bookkeeping: hooks
                    // cannot suspend, fail or carry physical work.
                    drop(record.get.take());
                    state.demand -= 1;
                }
            }
        }
    }

    fn finish_return(&self, slot: OwnedSlot<C>) -> PoolResult<()> {
        let mut state = lock(&self.state)?;
        drop(slot);
        state.demand -= 1;
        drop(state);
        self.activity.settled.notify_waiters();
        self.wakes.flush();
        Ok(())
    }
}

pub(super) struct Retirement<C: Send + 'static> {
    pool: Arc<PerKeyPool<C>>,
    clients: Vec<Cached<C>>,
    slots: Vec<OwnedSlot<C>>,
    cancel: Option<CancellationToken>,
    failed: bool,
    armed: bool,
}

impl<C: Send + 'static> Retirement<C> {
    fn new(pool: Arc<PerKeyPool<C>>, state: &mut State<C>) -> PoolResult<Self> {
        if state.retiring == usize::MAX {
            return Err(PoolError::Internal {
                reason: "retirement count overflow",
            });
        }
        Ok(Self {
            pool,
            clients: Vec::new(),
            slots: Vec::new(),
            cancel: None,
            failed: false,
            armed: false,
        })
    }

    pub(super) fn finish(self) -> PoolResult<()> {
        let failed = self.failed;
        drop(self);
        if failed {
            Err(PoolError::Internal {
                reason: "poisoned idle slot",
            })
        } else {
            Ok(())
        }
    }
}

impl<C: Send + 'static> Drop for Retirement<C> {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        drop(std::mem::take(&mut self.clients));
        match lock(&self.pool.state) {
            Ok(mut state) => {
                state.demand -= self.slots.len();
                drop(std::mem::take(&mut self.slots));
                if self.armed {
                    state.retiring -= 1;
                }
                if self.armed && state.retiring == 0 {
                    for record in state.pending.values() {
                        self.pool.wakes.push(record.waker.clone());
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "pool retirement could not settle bookkeeping");
                drop(std::mem::take(&mut self.slots));
            }
        }
        self.pool.activity.settled.notify_waiters();
        self.pool.wakes.flush();
    }
}

pub(super) struct AcquireSlot<C: Send + 'static> {
    pool: Arc<PerKeyPool<C>>,
    generation: u64,
    control: AcquireControl,
    closed: Pin<Box<WaitForCancellationFutureOwned>>,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    deadline: Pin<Box<Sleep>>,
    id: Option<u64>,
}

impl<C: Send + 'static> Future for AcquireSlot<C> {
    type Output = PoolResult<SlotGuard<C>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _ = this.closed.as_mut().poll(cx);
        let _ = this.cancelled.as_mut().poll(cx);
        let _ = this.deadline.as_mut().poll(cx);
        let pool = this.pool.clone();
        let retirement = match pool.refresh(Instant::now(), false, false) {
            Ok(retirement) => retirement,
            Err(error) => return Poll::Ready(Err(error)),
        };
        if let Err(error) = retirement.finish() {
            return Poll::Ready(Err(error));
        }
        let mut state = match lock(&pool.state) {
            Ok(state) => state,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let failure = if !state.accepting || state.generation != this.generation {
            Some(Failure::Closed)
        } else if this.control.cancellation().is_cancelled() {
            Some(Failure::Cancelled)
        } else if Instant::now() >= this.control.deadline() {
            Some(Failure::Timeout)
        } else {
            None
        };
        if let Some(failure) = failure {
            drop(state);
            return Poll::Ready(Err(failure.error()));
        }
        if this.id.is_none() {
            let max_size = pool.config.max_size().get();
            let queued = state.demand.saturating_sub(max_size);
            if state.demand >= max_size
                && pool
                    .config
                    .max_waiting()
                    .is_some_and(|limit| queued >= limit.get())
            {
                return Poll::Ready(Err(PoolError::WaitQueueFull));
            }
            let Some(next_id) = state.next_id.checked_add(1) else {
                return Poll::Ready(Err(PoolError::Internal {
                    reason: "request ID overflow",
                }));
            };
            let Some(demand) = state.demand.checked_add(1) else {
                return Poll::Ready(Err(PoolError::Internal {
                    reason: "demand count overflow",
                }));
            };
            let id = state.next_id;
            state.next_id = next_id;
            state.demand = demand;
            let slots = pool.slots.clone();
            state.pending.insert(
                id,
                Pending {
                    get: Some(Box::pin(tokio::task::unconstrained(async move {
                        slots.get().await
                    }))),
                    assigned: None,
                    deadline: this.control.deadline(),
                    cancel: this.control.cancellation().child_token(),
                    waker: cx.waker().clone(),
                    failure: None,
                },
            );
            this.id = Some(id);
        }
        let id = this.id.expect("registered request");
        let record = state.pending.get_mut(&id).expect("owned registration");
        record.waker = cx.waker().clone();
        if let Some(failure) = record.failure {
            state.pending.remove(&id);
            this.id = None;
            return Poll::Ready(Err(failure.error()));
        }
        let deferred = Waker::from(Arc::new(DeferredWake {
            batch: pool.wakes.clone(),
            target: cx.waker().clone(),
        }));
        let outcome = match record.get.as_mut() {
            Some(get) => get.as_mut().poll(&mut Context::from_waker(&deferred)),
            None => Poll::Ready(Ok(record.assigned.take().expect("held grant"))),
        };
        let result = match outcome {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => {
                record.failure = Some(Failure::Internal);
                drop(record.get.take());
                state.demand -= 1;
                state.pending.remove(&id);
                this.id = None;
                Poll::Ready(Err(Failure::Internal.error()))
            }
            Poll::Ready(Ok(slot)) => {
                record.get = None;
                record.assigned = Some(slot);
                if state.retiring != 0 {
                    Poll::Pending
                } else {
                    let mut record = state.pending.remove(&id).expect("owned registration");
                    let slot = record.assigned.take().expect("held grant");
                    this.id = None;
                    Poll::Ready(Ok(SlotGuard {
                        slot: Some(slot),
                        cached: None,
                        pool: pool.clone(),
                        generation: this.generation,
                        generation_cancel: state.cancel.clone(),
                        control: AcquireControl::new(
                            this.control.deadline(),
                            this.control.cancellation().clone(),
                        ),
                        published: false,
                    }))
                }
            }
        };
        drop(state);
        pool.wakes.flush();
        match result {
            Poll::Ready(Ok(mut guard)) => {
                let loaded = guard.load();
                Poll::Ready(loaded.map(|()| guard))
            }
            other => other,
        }
    }
}

impl<C: Send + 'static> Drop for AcquireSlot<C> {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return };
        let registration_cancel = match lock(&self.pool.state) {
            Ok(state) => state.pending.get(&id).map(|record| record.cancel.clone()),
            Err(error) => {
                tracing::error!(%error, "pool acquisition could not unregister");
                return;
            }
        };
        if let Some(cancel) = registration_cancel {
            cancel.cancel();
        }
        match self.pool.refresh(Instant::now(), false, false) {
            Ok(retirement) => drop(retirement),
            Err(error) => {
                tracing::error!(%error, "pool acquisition retirement failed");
                return;
            }
        }
        let removed = match lock(&self.pool.state) {
            Ok(mut state) => state.pending.remove(&id),
            Err(error) => {
                tracing::error!(%error, "pool acquisition could not unregister");
                return;
            }
        };
        drop(removed);
        self.pool.activity.settled.notify_waiters();
        self.pool.wakes.flush();
    }
}

/// Caller-owned capacity, not a ready SQL lease. Publication and clean return
/// must be wired after A3 initialization and A4 awaited quiescence.
pub(crate) struct SlotGuard<C: Send + 'static> {
    cached: Option<Cached<C>>,
    slot: Option<OwnedSlot<C>>,
    pool: Arc<PerKeyPool<C>>,
    generation: u64,
    generation_cancel: CancellationToken,
    control: AcquireControl,
    published: bool,
}

impl<C: Send + 'static> SlotGuard<C> {
    fn load(&mut self) -> PoolResult<()> {
        self.cached = lock(&self.slot.as_ref().expect("owned slot").cached)?.take();
        if self.cached.as_ref().is_some_and(|entry| {
            Instant::now().saturating_duration_since(entry.returned)
                >= self.pool.config.idle_timeout()
        }) {
            drop(self.cached.take());
        }
        self.control.check(AcquireStage::Wait)?;
        Ok(())
    }

    pub(super) fn value_mut(&mut self) -> Option<&mut C> {
        self.cached.as_mut().map(|entry| &mut entry.value)
    }

    pub(super) fn install(&mut self, value: C) -> PoolResult<()> {
        if self.cached.is_some() {
            return Err(PoolError::Internal {
                reason: "capacity slot already contains a client",
            });
        }
        self.cached = Some(Cached {
            value,
            returned: Instant::now(),
        });
        Ok(())
    }

    pub(super) async fn run<T>(
        &mut self,
        stage: AcquireStage,
        work: impl Future<Output = PoolResult<T>>,
    ) -> PoolResult<T> {
        if self.published {
            return Err(PoolError::Internal {
                reason: "published slot cannot perform acquisition work",
            });
        }
        let result = self
            .control
            .run(stage, async {
                tokio::select! {
                    biased;
                    _ = self.generation_cancel.cancelled() => Err(PoolError::Closed),
                    result = work => result,
                }
            })
            .await;
        let state = lock(&self.pool.state)?;
        if !state.accepting || state.generation != self.generation {
            return Err(PoolError::Closed);
        }
        self.control.check(stage)?;
        result
    }

    pub(super) fn publish(&mut self) -> PoolResult<()> {
        let state = lock(&self.pool.state)?;
        if !state.accepting || state.generation != self.generation {
            return Err(PoolError::Closed);
        }
        self.control.check(AcquireStage::Delivery)?;
        if self.cached.is_none() {
            return Err(PoolError::Internal {
                reason: "cannot publish an empty slot",
            });
        }
        if self.published {
            return Err(PoolError::Internal {
                reason: "slot has already been published",
            });
        }
        self.published = true;
        Ok(())
    }

    // A4 will call this only after its awaited quiescence proof; there is no
    // production clean-return entry point in A2.
    #[cfg(test)]
    pub(super) fn return_quiescent(mut self) -> PoolResult<()> {
        let mut state = lock(&self.pool.state)?;
        if state.accepting && state.generation == self.generation {
            let mut shelf = lock(&self.slot.as_ref().expect("owned slot").cached)?;
            if let Some(mut cached) = self.cached.take() {
                cached.returned = Instant::now();
                *shelf = Some(cached);
            }
            drop(shelf);
            drop(self.slot.take());
            state.demand -= 1;
        }
        drop(state);
        self.pool.activity.settled.notify_waiters();
        self.pool.wakes.flush();
        Ok(())
    }
}

impl<C: Send + 'static> Drop for SlotGuard<C> {
    fn drop(&mut self) {
        drop(self.cached.take());
        if let Some(slot) = self.slot.take()
            && let Err(error) = self.pool.finish_return(slot)
        {
            tracing::error!(%error, "discarded pool slot could not settle bookkeeping");
        }
    }
}

#[cfg(test)]
mod tests;

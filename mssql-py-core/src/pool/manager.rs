// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;
use std::time::Duration;

use tokio::runtime::{Handle, Id};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::config::PoolConfig;
use super::connection::{OpenedConnection, PoolKey};
use super::control::AcquireControl;
use super::error::{AcquireStage, PoolError, PoolResult};
use super::lifecycle::{AcquireSlot, PerKeyPool, SlotGuard, lock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Disabled,
    Enabled,
    ShuttingDown,
    Closed,
}

pub(super) fn check_runtime(expected: Id) -> PoolResult<()> {
    if Handle::try_current().is_ok_and(|current| current.id() == expected) {
        Ok(())
    } else {
        Err(PoolError::InvalidRequest {
            field: "runtime",
            reason: "operation requires the owning Tokio runtime",
        })
    }
}

#[derive(Default)]
pub(super) struct Activity {
    in_flight: AtomicUsize,
    pub(super) settled: Notify,
}

struct Admission(Arc<Activity>);

impl Drop for Admission {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.settled.notify_waiters();
    }
}

struct State<C: Send + 'static> {
    lifecycle: Lifecycle,
    generation: u64,
    config: PoolConfig,
    pools: HashMap<PoolKey, Arc<PerKeyPool<C>>>,
    last_sweep: Instant,
}

/// Pure Rust lifecycle manager. The default payload owns a TDS client by value;
/// generic payloads let tests instrument destruction without protocol I/O.
pub(crate) struct AsyncPoolManager<C: Send + 'static = OpenedConnection> {
    state: Mutex<State<C>>,
    activity: Arc<Activity>,
    runtime: Handle,
}

impl<C: Send + 'static> AsyncPoolManager<C> {
    pub(crate) fn new(config: PoolConfig) -> PoolResult<Self> {
        let runtime = Handle::try_current().map_err(|_| PoolError::InvalidRequest {
            field: "runtime",
            reason: "construction requires the owning Tokio runtime",
        })?;
        Ok(Self {
            state: Mutex::new(State {
                lifecycle: Lifecycle::Disabled,
                generation: 0,
                config,
                pools: HashMap::new(),
                last_sweep: Instant::now(),
            }),
            activity: Arc::default(),
            runtime,
        })
    }

    fn check_runtime(&self) -> PoolResult<()> {
        check_runtime(self.runtime.id())
    }

    pub(crate) fn enable(&self, config: PoolConfig) -> PoolResult<()> {
        let mut removed = Vec::new();
        let mut state = lock(&self.state)?;
        if matches!(state.lifecycle, Lifecycle::ShuttingDown | Lifecycle::Closed) {
            return Err(PoolError::Closed);
        }
        if state.lifecycle == Lifecycle::Enabled {
            return if state.config == config {
                Ok(())
            } else {
                Err(PoolError::InvalidConfig {
                    field: "configuration",
                    reason: "cannot change configuration while enabled",
                })
            };
        }
        if state.config != config {
            if self.activity.in_flight.load(Ordering::SeqCst) != 0 {
                return Err(PoolError::InvalidConfig {
                    field: "configuration",
                    reason: "acquisitions are still in flight",
                });
            }
            for pool in state.pools.values() {
                if pool.busy()? || Arc::strong_count(pool) != 1 || !pool.inactive()? {
                    return Err(PoolError::InvalidConfig {
                        field: "configuration",
                        reason: "old pool resources have not settled",
                    });
                }
            }
            removed.extend(state.pools.drain().map(|(_, pool)| pool));
        }
        let generation = state.generation.checked_add(1).ok_or(PoolError::Internal {
            reason: "generation overflow",
        })?;
        for pool in state.pools.values() {
            pool.enable(generation)?;
        }
        state.config = config;
        state.generation = generation;
        state.lifecycle = Lifecycle::Enabled;
        drop(state);
        drop(removed);
        Ok(())
    }

    fn stop(&self, terminal: bool) -> PoolResult<()> {
        let mut retirements = Vec::new();
        let mut state = lock(&self.state)?;
        if terminal {
            if state.lifecycle != Lifecycle::Closed {
                state.lifecycle = Lifecycle::ShuttingDown;
            }
        } else if state.lifecycle == Lifecycle::Enabled {
            state.lifecycle = Lifecycle::Disabled;
        }
        let mut error = None;
        for pool in state.pools.values() {
            match pool.maintain(Instant::now(), true) {
                Ok(retirement) => retirements.push(retirement),
                Err(failure) => error = Some(failure),
            }
        }
        drop(state);
        for retirement in retirements {
            if let Err(failure) = retirement.finish() {
                error = Some(failure);
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn disable(&self) {
        if let Err(error) = self.stop(false) {
            tracing::error!(%error, "pool disable failed; poisoned pools cannot admit work");
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        match lock(&self.state) {
            Ok(state) => state.lifecycle == Lifecycle::Enabled,
            Err(error) => {
                tracing::error!(%error, "pool status failed");
                false
            }
        }
    }

    /// Reserves capacity only. A3 supplies the public acquire initialization
    /// sequence; A2 does not publish a ready SQL connection.
    pub(super) async fn reserve(
        &self,
        key: PoolKey,
        control: AcquireControl,
    ) -> PoolResult<Option<SlotGuard<C>>> {
        self.check_runtime()?;
        control.check(AcquireStage::Admission)?;
        let Some((request, admission)) = self.admit(key, control)? else {
            return Ok(None);
        };
        let result = request.await.map(Some);
        drop(admission);
        result
    }

    fn admit(
        &self,
        key: PoolKey,
        control: AcquireControl,
    ) -> PoolResult<Option<(AcquireSlot<C>, Admission)>> {
        let mut retirements = Vec::new();
        let mut evicted = Vec::new();
        let admission;
        let mut state = lock(&self.state)?;
        match state.lifecycle {
            Lifecycle::Disabled => return Ok(None),
            Lifecycle::ShuttingDown | Lifecycle::Closed => return Err(PoolError::Closed),
            Lifecycle::Enabled => {}
        }
        self.activity.in_flight.fetch_add(1, Ordering::SeqCst);
        admission = Admission(self.activity.clone());
        let now = Instant::now();
        let interval = state.config.idle_timeout().max(Duration::from_secs(1));
        if now.saturating_duration_since(state.last_sweep) >= interval {
            let mut inactive = Vec::new();
            for (other_key, pool) in &state.pools {
                if *other_key != key {
                    if Arc::strong_count(pool) == 1 && pool.inactive()? {
                        inactive.push(other_key.clone());
                    } else {
                        retirements.push(pool.maintain(now, false)?);
                    }
                }
            }
            for victim in inactive {
                evicted.extend(state.pools.remove(&victim));
            }
            state.last_sweep = now;
        }
        let pool = match state.pools.get(&key) {
            Some(pool) => pool.clone(),
            None => {
                let pool = PerKeyPool::new(
                    state.config.clone(),
                    state.generation,
                    self.activity.clone(),
                    self.runtime.id(),
                )?;
                state.pools.insert(key, pool.clone());
                pool
            }
        };
        let request = pool.request(control)?;
        retirements.push(pool.maintain(now, false)?);
        drop(state);
        drop(evicted);
        for retirement in retirements {
            retirement.finish()?;
        }
        Ok(Some((request, admission)))
    }

    pub(crate) async fn shutdown(&self) -> PoolResult<()> {
        self.check_runtime()?;
        let timeout = lock(&self.state)?.config.shutdown_timeout();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(PoolError::InvalidConfig {
                field: "shutdown_timeout",
                reason: "exceeds the monotonic clock range",
            })?;
        self.stop(true)?;
        loop {
            let settled = self.activity.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            let complete = {
                let mut state = lock(&self.state)?;
                let mut complete = self.activity.in_flight.load(Ordering::SeqCst) == 0;
                for pool in state.pools.values() {
                    complete &= pool.settled()?;
                }
                if Instant::now() >= deadline {
                    return Err(PoolError::ShutdownTimeout);
                }
                if complete {
                    state.lifecycle = Lifecycle::Closed;
                }
                complete
            };
            if complete {
                return Ok(());
            }
            let wait = tokio::time::timeout_at(deadline, settled);
            tokio::pin!(wait);
            poll_fn(|cx| {
                if let Err(error) = self.check_runtime() {
                    return Poll::Ready(Err(error));
                }
                wait.as_mut()
                    .poll(cx)
                    .map(|result| result.map_err(|_| PoolError::ShutdownTimeout))
            })
            .await?;
        }
    }
}

impl<C: Send + 'static> Drop for AsyncPoolManager<C> {
    fn drop(&mut self) {
        self.disable();
    }
}

#[cfg(test)]
mod tests;

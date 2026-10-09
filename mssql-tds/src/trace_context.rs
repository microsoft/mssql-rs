// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Correlation shared by synchronous driver boundaries and their TDS work.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use uuid::Uuid;

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_OBJECT: AtomicU64 = AtomicU64::new(1);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn next_object_id() -> u64 {
    NEXT_OBJECT.fetch_add(1, Ordering::Relaxed)
}

/// All payload accesses are atomic. The sequence makes the two words one
/// coherent snapshot; SeqCst prevents either payload read crossing the checks.
/// Writers hold the odd sequence only across atomic stores, never logging,
/// allocating, calling user code, or awaiting.
#[derive(Debug, Default)]
struct AtomicGuid {
    sequence: AtomicU64,
    high: AtomicU64,
    low: AtomicU64,
}

impl AtomicGuid {
    fn load(&self) -> Option<Uuid> {
        loop {
            let before = self.sequence.load(Ordering::SeqCst);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let high = self.high.load(Ordering::SeqCst);
            let low = self.low.load(Ordering::SeqCst);
            if before == self.sequence.load(Ordering::SeqCst) {
                let id = Uuid::from_u64_pair(high, low);
                return (!id.is_nil()).then_some(id);
            }
        }
    }

    fn store(&self, id: Option<Uuid>) {
        let (high, low) = id.unwrap_or(Uuid::nil()).as_u64_pair();
        let sequence = loop {
            let sequence = self.sequence.load(Ordering::SeqCst);
            if sequence & 1 == 0
                && self
                    .sequence
                    .compare_exchange(
                        sequence,
                        sequence.wrapping_add(1),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
            {
                break sequence;
            }
            std::hint::spin_loop();
        };
        self.high.store(high, Ordering::SeqCst);
        self.low.store(low, Ordering::SeqCst);
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::SeqCst);
    }
}

#[derive(Debug)]
pub struct ConnectionTrace {
    id: u64,
    current: AtomicGuid,
    established: AtomicGuid,
}

impl Default for ConnectionTrace {
    fn default() -> Self {
        Self {
            id: next_object_id(),
            current: AtomicGuid::default(),
            established: AtomicGuid::default(),
        }
    }
}

impl ConnectionTrace {
    pub fn begin_attempt(&self, id: Uuid) {
        let previous = self.current.load();
        self.current.store(Some(id));
        if enabled() {
            CURRENT.with(|current| {
                let mut context = current.borrow_mut();
                if context
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.id == self.id)
                {
                    context.fallback_id = Some(id);
                    context.follow_connection = true;
                }
            });
        }
        tracing::debug!(?previous, client_connection_id = %id, "Starting connection attempt");
    }

    pub fn establish(&self, id: Uuid) {
        self.current.store(Some(id));
        self.established.store(Some(id));
    }

    pub fn client_connection_id(&self) -> Option<Uuid> {
        self.established.load()
    }

    pub fn clear(&self) {
        self.current.store(None);
        self.established.store(None);
    }
}

#[derive(Debug)]
pub struct StatementTrace {
    id: u64,
    next_execution: AtomicU64,
    active_execution: AtomicU64,
}

impl Default for StatementTrace {
    fn default() -> Self {
        Self {
            id: next_object_id(),
            next_execution: AtomicU64::new(1),
            active_execution: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Context {
    connection: Option<Arc<ConnectionTrace>>,
    statement: Option<Arc<StatementTrace>>,
    execution: u64,
    new_execution: bool,
    fallback_id: Option<Uuid>,
    follow_connection: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub dbc: Option<u64>,
    pub cid: Option<Uuid>,
    pub stmt: Option<u64>,
    pub exec: Option<u64>,
}

thread_local! {
    static CURRENT: RefCell<Context> = RefCell::new(Context::default());
}

impl Context {
    pub fn connection(connection: Arc<ConnectionTrace>) -> Self {
        let fallback_id = connection.current.load();
        Self {
            connection: Some(connection),
            fallback_id,
            follow_connection: true,
            ..Self::default()
        }
    }

    pub fn statement(mut self, statement: Arc<StatementTrace>, new_execution: bool) -> Self {
        self.execution = if new_execution {
            statement.next_execution.fetch_add(1, Ordering::Relaxed)
        } else {
            statement.active_execution.load(Ordering::Relaxed)
        };
        self.statement = Some(statement);
        self.new_execution = new_execution;
        self
    }

    pub fn pending_connection(mut self) -> Self {
        self.fallback_id = None;
        self.follow_connection = false;
        self
    }

    pub fn without_execution(mut self) -> Self {
        self.execution = 0;
        self.new_execution = false;
        self
    }

    pub fn capture() -> Self {
        if enabled() {
            CURRENT.with(|current| {
                let current = current.borrow();
                let mut captured = current.clone();
                captured.fallback_id = current.snapshot().cid;
                captured.follow_connection = false;
                captured
            })
        } else {
            Self::default()
        }
    }

    pub fn enter(self) -> Guard {
        let previous = CURRENT.with(|current| current.replace(self));
        Guard {
            previous,
            not_send: PhantomData,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            dbc: self.connection.as_ref().map(|connection| connection.id),
            cid: if self.follow_connection {
                self.connection
                    .as_ref()
                    .and_then(|connection| connection.current.load())
                    .or(self.fallback_id)
            } else {
                self.fallback_id
            },
            stmt: self.statement.as_ref().map(|statement| statement.id),
            exec: (self.execution != 0).then_some(self.execution),
        }
    }
}

/// A synchronous scope: never move it to another thread or hold it across an
/// independently scheduled async task. Worker closures capture and enter their
/// own context instead.
pub struct Guard {
    previous: Context,
    not_send: PhantomData<Rc<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        CURRENT.with(|current| {
            current.replace(previous);
        });
    }
}

pub fn snapshot() -> Snapshot {
    CURRENT.with(|current| current.borrow().snapshot())
}

/// Called only after the statement accepts a new execution. A rejected
/// concurrent execution must not relabel the active request's later fetches.
pub fn activate_execution() {
    if enabled() {
        CURRENT.with(|current| {
            let context = current.borrow();
            if context.new_execution
                && let Some(statement) = &context.statement
            {
                statement
                    .active_execution
                    .store(context.execution, Ordering::Relaxed);
            }
        });
    }
}

pub fn propagate<F, R>(work: F) -> impl FnOnce() -> R + Send + 'static
where
    F: FnOnce() -> R + Send + 'static,
{
    let context = enabled().then(Context::capture);
    move || {
        let _guard = context.map(Context::enter);
        work()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_propagation_does_not_capture_context() {
        assert!(!enabled());
        let _scope = Context::connection(Arc::default()).enter();
        assert!(Context::capture().snapshot().dbc.is_none());
        let work = propagate(snapshot);
        assert_eq!(
            std::thread::spawn(work).join().unwrap(),
            Snapshot::default()
        );
    }

    #[test]
    fn guid_snapshots_never_mix_writers() {
        let guid = Arc::new(AtomicGuid::default());
        let first = Uuid::from_u64_pair(1, 11);
        let second = Uuid::from_u64_pair(2, 22);
        std::thread::scope(|scope| {
            for id in [first, second] {
                let guid = Arc::clone(&guid);
                scope.spawn(move || {
                    for _ in 0..10_000 {
                        guid.store(Some(id));
                    }
                });
            }
            for _ in 0..20_000 {
                assert!(guid.load().is_none_or(|id| id == first || id == second));
            }
        });
    }

    #[test]
    fn scopes_restore_and_follow_reconnects() {
        let connection = Arc::new(ConnectionTrace::default());
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        connection.establish(first);
        let _scope = Context::connection(Arc::clone(&connection)).enter();
        assert_eq!(snapshot().cid, Some(first));
        {
            let _nested = Context::default().enter();
            assert_eq!(snapshot(), Snapshot::default());
        }

        connection.begin_attempt(second);
        assert_eq!(snapshot().cid, Some(second));
        assert_eq!(connection.client_connection_id(), Some(first));
        connection.establish(second);
        assert_eq!(connection.client_connection_id(), Some(second));
    }

    #[test]
    fn connect_does_not_inherit_a_previous_attempt_id() {
        enable();
        let connection = Arc::new(ConnectionTrace::default());
        connection.establish(Uuid::new_v4());
        let _scope = Context::connection(Arc::clone(&connection))
            .pending_connection()
            .enter();
        connection.clear();
        assert_eq!(snapshot().cid, None);
        let id = Uuid::new_v4();
        connection.begin_attempt(id);
        assert_eq!(snapshot().cid, Some(id));
        assert_eq!(connection.client_connection_id(), None);
    }

    #[test]
    fn worker_context_does_not_leak() {
        enable();
        let connection = Arc::new(ConnectionTrace::default());
        let id = Uuid::new_v4();
        connection.establish(id);
        let captured = {
            let _scope = Context::connection(Arc::clone(&connection)).enter();
            propagate(snapshot)
        };
        connection.begin_attempt(Uuid::new_v4());
        assert_eq!(snapshot(), Snapshot::default());
        let result = std::thread::spawn(move || {
            assert_eq!(captured().cid, Some(id));
            assert_eq!(snapshot(), Snapshot::default());
        });
        result.join().unwrap();
    }

    #[test]
    fn rejected_execution_does_not_replace_active_execution() {
        enable();
        let statement = Arc::new(StatementTrace::default());
        {
            let _scope = Context::default()
                .statement(Arc::clone(&statement), true)
                .enter();
            activate_execution();
            assert_eq!(snapshot().exec, Some(1));
            {
                let _rejected = Context::default()
                    .statement(Arc::clone(&statement), true)
                    .enter();
                assert_eq!(snapshot().exec, Some(2));
            }
            assert_eq!(snapshot().exec, Some(1));
        }
        let _fetch = Context::default().statement(statement, false).enter();
        assert_eq!(snapshot().exec, Some(1));
    }
}

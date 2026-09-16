// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, LockResult, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::HandleType;
use crate::api::odbc_types::SqlHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HandleId(NonZeroUsize);

impl HandleId {
    pub(crate) fn from_raw(raw: SqlHandle) -> Result<Self, RegistryError> {
        NonZeroUsize::new(raw.addr())
            .map(Self)
            .ok_or(RegistryError::InvalidId)
    }

    /// The pointer encodes identity only; it must never be dereferenced.
    pub(crate) fn to_raw(self) -> SqlHandle {
        std::ptr::without_provenance_mut(self.0.get())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryError {
    InvalidId,
    NotFound,
    WrongType,
    Busy,
    IdSpaceFull,
    Capacity,
    ActivityOverflow,
    InvalidActivity,
    OwnershipMismatch,
    DuplicateId,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidId => "null handle identity",
            Self::NotFound => "handle is missing or retired",
            Self::WrongType => "handle type does not match",
            Self::Busy => "handle or ancestor is in use or closing",
            Self::IdSpaceFull => "all handle identities are currently in use",
            Self::Capacity => "handle registry capacity could not be reserved",
            Self::ActivityOverflow => "handle activity count would overflow",
            Self::InvalidActivity => {
                "activity is already registered or belongs to another registry"
            }
            Self::OwnershipMismatch => "handle reference belongs to another registry entry",
            Self::DuplicateId => "retirement batch contains a duplicate identity",
        })
    }
}

impl std::error::Error for RegistryError {}


/// Handle IDs are dense integers, so the default SipHash is pure overhead.
/// Fibonacci multiply-shift, the same mix `rustc-hash` uses.
#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(26);
    }
}

type IdBuildHasher = std::hash::BuildHasherDefault<IdHasher>;

const OPEN: u8 = 0;
const CLOSING: u8 = 1;
const RETIRED: u8 = 2;

/// In-flight calls, independent of the Arcs that own handle storage.
#[derive(Debug)]
pub(crate) struct HandleActivity {
    parent: Option<Arc<Self>>,
    owner: OnceLock<Arc<()>>,
    registered: AtomicBool,
    active: AtomicUsize,
    admission: AtomicU8,
}

impl HandleActivity {
    pub(crate) fn new(parent: Option<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self {
            parent,
            owner: OnceLock::new(),
            registered: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            admission: AtomicU8::new(OPEN),
        })
    }

    fn ancestors(&self) -> impl Iterator<Item = &Self> {
        std::iter::successors(Some(self), |activity| activity.parent.as_deref())
    }

    /// Ancestors whose in-flight counter this call reserves. The root ENV is
    /// excluded unless it is the target: `SQLFreeHandle(ENV)` is DM-ordered
    /// after every child and already debug_asserted, so a process-shared
    /// counter on every call buys no safety the Arc parents do not give.
    fn reserved(&self) -> impl Iterator<Item = &Self> {
        self.ancestors()
            .filter(|a| a.parent.is_some() || std::ptr::eq(*a, self))
    }

    fn root(&self) -> &Self {
        let mut root = self;
        while let Some(parent) = root.parent.as_deref() {
            root = parent;
        }
        root
    }

    fn check_open(&self) -> Result<(), RegistryError> {
        for activity in self.ancestors() {
            match activity.admission.load(Ordering::Acquire) {
                OPEN => {}
                CLOSING => return Err(RegistryError::Busy),
                _ => return Err(RegistryError::NotFound),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn exhaust_for_test(self: &Arc<Self>) -> ActivityOverflowGuard {
        let previous = self.active.swap(usize::MAX, Ordering::AcqRel);
        ActivityOverflowGuard {
            activity: Arc::clone(self),
            previous,
        }
    }
}

#[cfg(test)]
pub(super) struct ActivityOverflowGuard {
    activity: Arc<HandleActivity>,
    previous: usize,
}

#[cfg(test)]
impl Drop for ActivityOverflowGuard {
    fn drop(&mut self) {
        self.activity.active.store(self.previous, Ordering::Release);
    }
}

/// Not Clone: use `clone_arc` for structural ownership or acquire another call.
#[derive(Debug)]
pub(crate) struct HandleRef<T> {
    id: HandleId,
    kind: HandleType,
    value: Arc<T>,
    // Fields drop in declaration order: payload cleanup must finish first.
    lease: ActivityLease,
}

#[derive(Debug)]
struct ActivityLease {
    activity: Arc<HandleActivity>,
}

impl<T> HandleRef<T> {
    pub(crate) fn id(&self) -> HandleId {
        self.id
    }

    /// Structural ownership does not count as an in-flight call.
    pub(crate) fn clone_arc(&self) -> Arc<T> {
        Arc::clone(&self.value)
    }

    pub(crate) fn into_arc(self) -> Arc<T> {
        self.clone_arc()
    }
}

impl<T> Deref for HandleRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        for activity in self.activity.reserved() {
            activity.active.fetch_sub(1, Ordering::Release);
        }
    }
}

#[must_use = "dropping the guard reopens admission unless the handle was retired"]
pub(crate) struct CloseGuard {
    activity: Arc<HandleActivity>,
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        // Retirement and reopening may race; a retired activity never reopens.
        let _ = self.activity.admission.compare_exchange(
            CLOSING,
            OPEN,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

struct RegistryEntry {
    kind: HandleType,
    value: Arc<dyn Any + Send + Sync>,
    activity: Arc<HandleActivity>,
}

struct RegistryState {
    entries: HashMap<HandleId, RegistryEntry, IdBuildHasher>,
    next_id: NonZeroUsize,
    #[cfg(test)]
    id_limit: NonZeroUsize,
    #[cfg(test)]
    reserve_additional: usize,
    #[cfg(test)]
    fail_retirement_after: Option<usize>,
}

impl RegistryState {
    fn successor(&self, id: NonZeroUsize) -> NonZeroUsize {
        let next = id.checked_add(1).unwrap_or(NonZeroUsize::MIN);
        #[cfg(test)]
        if next > self.id_limit {
            return NonZeroUsize::MIN;
        }
        next
    }

    fn available_id(&self) -> Result<HandleId, RegistryError> {
        let mut candidate = self.next_id;
        loop {
            let id = HandleId(candidate);
            if !self.entries.contains_key(&id) {
                return Ok(id);
            }
            candidate = self.successor(candidate);
            if candidate == self.next_id {
                return Err(RegistryError::IdSpaceFull);
            }
        }
    }

    #[cfg(test)]
    fn check_retirement(&mut self) -> Result<(), RegistryError> {
        match self.fail_retirement_after {
            Some(0) => {
                self.fail_retirement_after = None;
                Err(RegistryError::Capacity)
            }
            Some(count) => {
                self.fail_retirement_after = Some(count - 1);
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn reserve_entry(&mut self) -> Result<(), RegistryError> {
        #[cfg(test)]
        let additional = self.reserve_additional;
        #[cfg(not(test))]
        let additional = 1;
        self.entries
            .try_reserve(additional)
            .map_err(|_| RegistryError::Capacity)
    }
}

pub(crate) struct HandleRegistry {
    state: RwLock<RegistryState>,
    identity: Arc<()>,
}

impl Default for HandleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleRegistry {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(RegistryState {
                entries: HashMap::default(),
                next_id: NonZeroUsize::MIN,
                #[cfg(test)]
                id_limit: NonZeroUsize::MAX,
                #[cfg(test)]
                reserve_additional: 1,
                #[cfg(test)]
                fail_retirement_after: None,
            }),
            identity: Arc::new(()),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, RegistryState> {
        self.recover(self.state.read())
    }

    fn write(&self) -> RwLockWriteGuard<'_, RegistryState> {
        self.recover(self.state.write())
    }

    fn recover<T>(&self, result: LockResult<T>) -> T {
        // Critical sections contain only map operations on primitive keys,
        // reserved-capacity insertions, Arc clones and atomics. No user code or
        // payload destructor can unwind here and leave a partial update.
        result.unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned handle registry");
            self.state.clear_poison();
            poisoned.into_inner()
        })
    }

    #[cfg(test)]
    pub(super) fn poison_for_test(&self) {
        let _ = std::panic::catch_unwind(|| {
            let _state = self.state.write().unwrap();
            panic!("poison the registry without changing its invariants");
        });
    }

    #[cfg(test)]
    pub(super) fn fail_retirement_after(&self, successes: usize) {
        self.write().fail_retirement_after = Some(successes);
    }

    #[cfg(test)]
    pub(super) fn force_wrap_for_test(&self) {
        let mut state = self.write();
        state.next_id = state.id_limit;
    }

    /// An activity may be registered once. Its ancestors need not have entries:
    /// claiming their common root binds the entire activity tree to this registry.
    pub(crate) fn register<T: Any + Send + Sync>(
        &self,
        kind: HandleType,
        value: Arc<T>,
        activity: Arc<HandleActivity>,
    ) -> Result<HandleId, RegistryError> {
        let mut state = self.write();
        if kind == HandleType::Invalid {
            return Err(RegistryError::WrongType);
        }
        let root = activity.root();
        if activity.registered.load(Ordering::Acquire)
            || root
                .owner
                .get()
                .is_some_and(|owner| !Arc::ptr_eq(owner, &self.identity))
        {
            return Err(RegistryError::InvalidActivity);
        }
        activity.check_open()?;
        let id = state.available_id()?;
        state.reserve_entry()?;
        let owner = root.owner.get_or_init(|| Arc::clone(&self.identity));
        if !Arc::ptr_eq(owner, &self.identity) {
            return Err(RegistryError::InvalidActivity);
        }

        activity.registered.store(true, Ordering::Release);
        // The candidate remains vacant through insertion under this lock.
        let previous = state.entries.insert(
            id,
            RegistryEntry {
                kind,
                value,
                activity,
            },
        );
        state.next_id = state.successor(id.0);
        drop(state);
        drop(previous);
        Ok(id)
    }

    pub(crate) fn acquire<T: Any + Send + Sync>(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<HandleRef<T>, RegistryError> {
        let state = self.read();
        let entry = state.entries.get(&id).ok_or(RegistryError::NotFound)?;
        if entry.kind != expected {
            return Err(RegistryError::WrongType);
        }
        entry.activity.check_open()?;
        let value = match Arc::clone(&entry.value).downcast::<T>() {
            Ok(value) => value,
            Err(value) => {
                drop(state);
                drop(value);
                return Err(RegistryError::WrongType);
            }
        };
        let activity = Arc::clone(&entry.activity);
        // Readers may reserve the same ancestors concurrently. Close and
        // retirement take the write lock and cannot observe partial reservations.
        for (reserved, ancestor) in activity.reserved().enumerate() {
            if ancestor
                .active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_add(1)
                })
                .is_err()
            {
                for previous in activity.reserved().take(reserved) {
                    previous.active.fetch_sub(1, Ordering::Release);
                }
                drop(state);
                return Err(RegistryError::ActivityOverflow);
            }
        }
        drop(state);
        Ok(HandleRef {
            id,
            kind: expected,
            value,
            lease: ActivityLease { activity },
        })
    }

    /// Attribution probe: registry lookup with no ancestor reservation.
    #[cfg(test)]
    pub(crate) fn probe_lookup<T: Any + Send + Sync>(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<Arc<T>, RegistryError> {
        let state = self.read();
        let entry = state.entries.get(&id).ok_or(RegistryError::NotFound)?;
        if entry.kind != expected {
            return Err(RegistryError::WrongType);
        }
        entry.activity.check_open()?;
        let value = Arc::clone(&entry.value)
            .downcast::<T>()
            .map_err(|_| RegistryError::WrongType)?;
        drop(state);
        Ok(value)
    }

    /// Attribution probe: ancestor reservation with no registry lookup.
    #[cfg(test)]
    pub(crate) fn probe_reserve(activity: &Arc<HandleActivity>) {
        for ancestor in activity.ancestors() {
            ancestor.active.fetch_add(1, Ordering::AcqRel);
        }
        for ancestor in activity.ancestors() {
            ancestor.active.fetch_sub(1, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub(crate) fn kind(&self, id: HandleId) -> Result<Option<HandleType>, RegistryError> {
        Ok(self.read().entries.get(&id).map(|entry| entry.kind))
    }

    /// Diagnostic access ignores closing admission and activity limits, but
    /// never revives a retired handle or an entry with a retired ancestor.
    pub(super) fn diagnostics<T: Any + Send + Sync>(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<Arc<T>, RegistryError> {
        let value = {
            let state = self.read();
            let entry = state.entries.get(&id).ok_or(RegistryError::NotFound)?;
            if entry.kind != expected {
                return Err(RegistryError::WrongType);
            }
            if entry
                .activity
                .ancestors()
                .any(|ancestor| ancestor.admission.load(Ordering::Acquire) == RETIRED)
            {
                return Err(RegistryError::NotFound);
            }
            Arc::clone(&entry.value)
        };
        value.downcast().map_err(|_| RegistryError::WrongType)
    }

    /// Only for fixture cleanup after deliberately violating parent-free order.
    #[cfg(test)]
    pub(crate) fn inspect_for_test<T: Any + Send + Sync>(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<Arc<T>, RegistryError> {
        let value = {
            let state = self.read();
            let entry = state.entries.get(&id).ok_or(RegistryError::NotFound)?;
            if entry.kind != expected {
                return Err(RegistryError::WrongType);
            }
            Arc::clone(&entry.value)
        };
        value.downcast().map_err(|_| RegistryError::WrongType)
    }

    pub(crate) fn begin_close<T: Any + Send + Sync>(
        &self,
        handle: &HandleRef<T>,
    ) -> Result<CloseGuard, RegistryError> {
        let state = self.write();
        let entry = state
            .entries
            .get(&handle.id())
            .ok_or(RegistryError::NotFound)?;
        if entry.kind != handle.kind {
            return Err(RegistryError::WrongType);
        }
        if !Arc::ptr_eq(&entry.activity, &handle.lease.activity)
            || !entry
                .value
                .downcast_ref::<T>()
                .is_some_and(|value| std::ptr::eq(value, handle.value.as_ref()))
        {
            return Err(RegistryError::OwnershipMismatch);
        }
        entry.activity.check_open()?;
        if entry.activity.active.load(Ordering::Acquire) != 1 {
            return Err(RegistryError::Busy);
        }
        entry.activity.admission.store(CLOSING, Ordering::Release);
        let activity = Arc::clone(&entry.activity);
        drop(state);
        Ok(CloseGuard { activity })
    }

    /// Logical retirement only. The caller coordinates close/cascade policy;
    /// existing HandleRefs and structural Arcs continue to own their payloads.
    pub(crate) fn retire(&self, id: HandleId, expected: HandleType) -> Result<(), RegistryError> {
        let removed = {
            let mut state = self.write();
            #[cfg(test)]
            state.check_retirement()?;
            match state.entries.entry(id) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    if entry.get().kind != expected {
                        return Err(RegistryError::WrongType);
                    }
                    entry
                        .get()
                        .activity
                        .admission
                        .store(RETIRED, Ordering::Release);
                    entry.remove()
                }
                std::collections::hash_map::Entry::Vacant(_) => {
                    return Err(RegistryError::NotFound);
                }
            }
        };
        drop(removed);
        Ok(())
    }

    /// Preflights the entire batch before removing anything. No payload
    /// destructor runs while the registry write lock is held.
    pub(crate) fn retire_batch(
        &self,
        handles: &[(HandleId, HandleType)],
    ) -> Result<(), RegistryError> {
        let mut removed = Vec::new();
        removed
            .try_reserve(handles.len())
            .map_err(|_| RegistryError::Capacity)?;
        let mut state = self.write();
        #[cfg(test)]
        state.check_retirement()?;
        for (index, (id, expected)) in handles.iter().enumerate() {
            if handles
                .iter()
                .take(index)
                .any(|(previous, _)| previous == id)
            {
                return Err(RegistryError::DuplicateId);
            }
            let entry = state.entries.get(id).ok_or(RegistryError::NotFound)?;
            if entry.kind != *expected {
                return Err(RegistryError::WrongType);
            }
        }
        for (id, _) in handles {
            // Preflight and removal share the same lock.
            if let Some(entry) = state.entries.remove(id) {
                entry.activity.admission.store(RETIRED, Ordering::Release);
                removed.push(entry);
            }
        }
        drop(state);
        drop(removed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Barrier, Mutex, Weak};
    use std::thread;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    impl HandleRegistry {
        fn with_next_id(next: usize) -> Self {
            let registry = Self::new();
            registry.state.write().unwrap().next_id = NonZeroUsize::new(next).unwrap();
            registry
        }

        fn with_id_limit(limit: usize) -> Self {
            let registry = Self::new();
            registry.state.write().unwrap().id_limit = NonZeroUsize::new(limit).unwrap();
            registry
        }
    }

    fn register(
        registry: &HandleRegistry,
        kind: HandleType,
        parent: Option<Arc<HandleActivity>>,
    ) -> (HandleId, Arc<HandleActivity>) {
        let activity = HandleActivity::new(parent);
        let id = registry
            .register(kind, Arc::new(42_u32), Arc::clone(&activity))
            .unwrap();
        (id, activity)
    }

    fn count(activity: &HandleActivity) -> usize {
        activity.active.load(Ordering::Acquire)
    }

    #[test]
    fn activities_and_debug_payload_references_implement_debug() {
        let registry = HandleRegistry::new();
        let (id, activity) = register(&registry, HandleType::Env, None);
        let handle = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        assert!(format!("{activity:?}").contains("HandleActivity"));
        assert!(format!("{handle:?}").contains("value: 42"));
        assert_eq!(count(&activity), 1);
    }

    fn receive<T>(receiver: &Receiver<T>) -> T {
        receiver.recv_timeout(TIMEOUT).unwrap()
    }

    struct Payload {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn ids_advance_before_wrap_even_when_reregistering_the_same_allocation() {
        let registry = HandleRegistry::default();
        let value = Arc::new(42_u32);
        let mut previous = 0;
        for _ in 0..32 {
            let id = registry
                .register(
                    HandleType::Env,
                    Arc::clone(&value),
                    HandleActivity::new(None),
                )
                .unwrap();
            assert!(id.to_raw().addr() > previous);
            assert_eq!(HandleId::from_raw(id.to_raw()), Ok(id));
            assert_eq!(registry.kind(id), Ok(Some(HandleType::Env)));
            let handle = registry.acquire::<u32>(id, HandleType::Env).unwrap();
            assert_eq!(handle.id(), id);
            assert!(Arc::ptr_eq(&value, &handle.clone_arc()));
            assert_eq!(*handle, 42);
            if previous != 0 {
                let old = HandleId::from_raw(std::ptr::without_provenance_mut(previous)).unwrap();
                assert_eq!(registry.kind(old), Ok(None));
                assert_eq!(
                    registry.acquire::<u32>(old, HandleType::Env).err(),
                    Some(RegistryError::NotFound)
                );
            }
            drop(handle);
            registry.retire(id, HandleType::Env).unwrap();
            assert_eq!(registry.kind(id), Ok(None));
            assert_eq!(
                registry.acquire::<u32>(id, HandleType::Env).err(),
                Some(RegistryError::NotFound)
            );
            assert!(registry.state.read().unwrap().entries.is_empty());
            previous = id.to_raw().addr();
        }
    }

    #[test]
    fn null_missing_and_both_kinds_of_type_mismatch_are_distinct() {
        assert_eq!(
            HandleId::from_raw(std::ptr::null_mut()),
            Err(RegistryError::InvalidId)
        );
        assert_eq!(
            std::mem::size_of::<HandleId>(),
            std::mem::size_of::<SqlHandle>()
        );
        let registry = HandleRegistry::new();
        let missing = HandleId::from_raw(std::ptr::without_provenance_mut(123)).unwrap();
        assert_eq!(registry.kind(missing), Ok(None));
        assert_eq!(
            registry.acquire::<u32>(missing, HandleType::Env).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry.retire(missing, HandleType::Env),
            Err(RegistryError::NotFound)
        );
        let (id, activity) = register(&registry, HandleType::Env, None);
        assert_eq!(
            registry.acquire::<u32>(id, HandleType::Dbc).err(),
            Some(RegistryError::WrongType)
        );
        assert_eq!(
            registry.acquire::<u64>(id, HandleType::Env).err(),
            Some(RegistryError::WrongType)
        );
        assert_eq!(
            registry.retire(id, HandleType::Dbc),
            Err(RegistryError::WrongType)
        );
        assert_eq!(count(&activity), 0);
        assert_eq!(registry.kind(id), Ok(Some(HandleType::Env)));
        assert_eq!(
            registry.register(
                HandleType::Invalid,
                Arc::new(0_u32),
                HandleActivity::new(None)
            ),
            Err(RegistryError::WrongType)
        );
    }

    #[test]
    fn last_identity_is_valid_and_wrap_does_not_reset_when_empty() {
        let registry = HandleRegistry::with_next_id(usize::MAX - 1);
        let (first, _) = register(&registry, HandleType::Env, None);
        let (last, _) = register(&registry, HandleType::Env, None);
        assert_eq!(first.to_raw().addr(), usize::MAX - 1);
        assert_eq!(last.to_raw().addr(), usize::MAX);
        assert_eq!(HandleId::from_raw(last.to_raw()), Ok(last));
        let (wrapped, _) = register(&registry, HandleType::Env, None);
        assert_eq!(wrapped.to_raw().addr(), 1);
        for id in [first, last, wrapped] {
            assert_eq!(*registry.acquire::<u32>(id, HandleType::Env).unwrap(), 42);
        }
        registry
            .retire_batch(&[
                (first, HandleType::Env),
                (last, HandleType::Env),
                (wrapped, HandleType::Env),
            ])
            .unwrap();
        assert!(registry.state.read().unwrap().entries.is_empty());
        let (next, _) = register(&registry, HandleType::Env, None);
        assert_eq!(next.to_raw().addr(), 2);
    }

    #[test]
    fn wrap_skips_live_ids_and_reuses_only_retired_entries() {
        let registry = HandleRegistry::new();
        let (first, _) = register(&registry, HandleType::Env, None);
        let (second, _) = register(&registry, HandleType::Env, None);
        let (third, _) = register(&registry, HandleType::Env, None);
        registry.retire(second, HandleType::Env).unwrap();
        assert_eq!(registry.kind(second), Ok(None));
        registry.force_wrap_for_test();
        let (last, _) = register(&registry, HandleType::Env, None);
        assert_eq!(last.to_raw().addr(), usize::MAX);
        let (recycled, _) = register(&registry, HandleType::Env, None);
        assert_eq!(recycled, second);
        let (fourth, _) = register(&registry, HandleType::Env, None);
        assert_eq!(fourth.to_raw().addr(), 4);
        registry.force_wrap_for_test();
        let (fifth, _) = register(&registry, HandleType::Env, None);
        assert_eq!(fifth.to_raw().addr(), 5);
        assert_live_entries(
            &registry,
            &[
                (first, 42),
                (recycled, 42),
                (third, 42),
                (fourth, 42),
                (fifth, 42),
                (last, 42),
            ],
        );
    }

    fn assert_live_entries(registry: &HandleRegistry, live: &[(HandleId, u32)]) {
        let ids: std::collections::HashSet<_> = live.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), live.len());
        assert_eq!(registry.state.read().unwrap().entries.len(), live.len());
        for (id, value) in live {
            assert_eq!(HandleId::from_raw(id.to_raw()), Ok(*id));
            assert_eq!(registry.kind(*id), Ok(Some(HandleType::Env)));
            assert_eq!(
                *registry.acquire::<u32>(*id, HandleType::Env).unwrap(),
                *value
            );
        }
    }

    #[test]
    fn full_namespace_allows_retry_after_retirement_without_claiming_activity() {
        for limit in [1, 3, 7] {
            let registry = HandleRegistry::with_id_limit(limit);
            let mut live = Vec::new();
            for value in 1..=limit {
                let value = u32::try_from(value).unwrap();
                let id = registry
                    .register(HandleType::Env, Arc::new(value), HandleActivity::new(None))
                    .unwrap();
                live.push((id, value));
                assert_live_entries(&registry, &live);
            }
            let activity = HandleActivity::new(None);
            let candidate = registry.state.read().unwrap().next_id;
            for _ in 0..2 {
                assert_eq!(
                    registry.register(HandleType::Env, Arc::new(42_u32), Arc::clone(&activity)),
                    Err(RegistryError::IdSpaceFull)
                );
                assert_eq!(registry.state.read().unwrap().next_id, candidate);
                assert!(!activity.registered.load(Ordering::Acquire));
                assert!(activity.owner.get().is_none());
                assert_eq!(count(&activity), 0);
                assert_eq!(activity.admission.load(Ordering::Acquire), OPEN);
                assert_live_entries(&registry, &live);
            }
            let (retired, _) = live.remove(limit / 2);
            registry.retire(retired, HandleType::Env).unwrap();
            assert_live_entries(&registry, &live);
            let recycled = registry
                .register(HandleType::Env, Arc::new(42_u32), Arc::clone(&activity))
                .unwrap();
            assert_eq!(recycled, retired);
            assert!(activity.registered.load(Ordering::Acquire));
            live.push((recycled, 42));
            assert_live_entries(&registry, &live);
        }
    }

    #[test]
    fn bounded_live_entries_remain_unique_through_thousands_of_recycling_cycles() {
        for limit in [3, 7] {
            let registry = HandleRegistry::with_id_limit(limit);
            let mut live = Vec::new();
            for value in 0..2048_u32 {
                if live.len() == limit {
                    let (retired, _) = live.remove(usize::try_from(value).unwrap() % limit);
                    registry.retire(retired, HandleType::Env).unwrap();
                    assert_eq!(registry.kind(retired), Ok(None));
                    assert_live_entries(&registry, &live);
                }
                let id = registry
                    .register(HandleType::Env, Arc::new(value), HandleActivity::new(None))
                    .unwrap();
                assert!(id.to_raw().addr() <= limit);
                live.push((id, value));
                assert_live_entries(&registry, &live);
            }
            let batch: Vec<_> = live.iter().map(|(id, _)| (*id, HandleType::Env)).collect();
            registry.retire_batch(&batch).unwrap();
            assert_live_entries(&registry, &[]);
            register(&registry, HandleType::Env, None);
        }
    }

    #[test]
    fn retired_reference_cannot_close_recycled_id_even_with_the_same_payload() {
        for reuse_payload in [false, true] {
            let original = Arc::new(42_u32);
            let replacement = if reuse_payload {
                Arc::clone(&original)
            } else {
                Arc::new(7_u32)
            };
            let registry = HandleRegistry::with_id_limit(1);
            let old_activity = HandleActivity::new(None);
            let id = registry
                .register(HandleType::Env, original, Arc::clone(&old_activity))
                .unwrap();
            let old = registry.acquire::<u32>(id, HandleType::Env).unwrap();
            let old_closing = registry.begin_close(&old).unwrap();
            registry.retire(id, HandleType::Env).unwrap();
            assert_eq!(registry.kind(id), Ok(None));
            let new_activity = HandleActivity::new(None);
            let recycled = registry
                .register(
                    HandleType::Env,
                    Arc::clone(&replacement),
                    Arc::clone(&new_activity),
                )
                .unwrap();
            assert_eq!(recycled, id);
            assert_eq!(*old, 42);
            assert_eq!(
                registry.begin_close(&old).err(),
                Some(RegistryError::OwnershipMismatch)
            );
            assert_eq!(count(&new_activity), 0);
            assert_eq!(new_activity.admission.load(Ordering::Acquire), OPEN);
            let new = registry.acquire::<u32>(recycled, HandleType::Env).unwrap();
            assert_eq!(*new, *replacement);
            assert_eq!(
                *registry.diagnostics::<u32>(id, HandleType::Env).unwrap(),
                *replacement
            );
            let new_closing = registry.begin_close(&new).unwrap();
            drop(old_closing);
            drop(old);
            assert_eq!(count(&old_activity), 0);
            assert_eq!(old_activity.admission.load(Ordering::Acquire), RETIRED);
            assert_eq!(count(&new_activity), 1);
            assert_eq!(new_activity.admission.load(Ordering::Acquire), CLOSING);
            registry.retire(recycled, HandleType::Env).unwrap();
            drop(new_closing);
            assert_eq!(*new, *replacement);
        }
    }

    #[test]
    fn recycled_id_uses_the_replacement_kind_and_payload_type() {
        let registry = HandleRegistry::with_id_limit(1);
        let (old, _) = register(&registry, HandleType::Env, None);
        registry.retire(old, HandleType::Env).unwrap();
        let recycled = registry
            .register(HandleType::Desc, Arc::new(7_u64), HandleActivity::new(None))
            .unwrap();
        assert_eq!(recycled, old);
        assert_eq!(registry.kind(old), Ok(Some(HandleType::Desc)));
        for kind in [HandleType::Env, HandleType::Desc] {
            assert_eq!(
                registry.acquire::<u32>(old, kind).err(),
                Some(RegistryError::WrongType)
            );
            assert_eq!(
                registry.diagnostics::<u32>(old, kind).err(),
                Some(RegistryError::WrongType)
            );
        }
        assert_eq!(
            registry.retire(old, HandleType::Env),
            Err(RegistryError::WrongType)
        );
        assert_eq!(*registry.acquire::<u64>(old, HandleType::Desc).unwrap(), 7);
        registry.retire(recycled, HandleType::Desc).unwrap();
    }

    #[test]
    fn retirement_preserves_retained_storage_and_drops_payload_exactly_once() {
        let registry = HandleRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let activity = HandleActivity::new(None);
        let id = registry
            .register(
                HandleType::Env,
                Arc::new(Payload {
                    drops: Arc::clone(&drops),
                }),
                Arc::clone(&activity),
            )
            .unwrap();
        let handle = registry.acquire::<Payload>(id, HandleType::Env).unwrap();
        let structural = handle.clone_arc();
        assert_eq!(count(&activity), 1);
        registry.retire(id, HandleType::Env).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(handle.drops.load(Ordering::Relaxed), 0);
        assert_eq!(
            registry.begin_close(&handle).err(),
            Some(RegistryError::NotFound)
        );
        drop(handle);
        assert_eq!(count(&activity), 0);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(structural);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn into_arc_ends_the_call_without_releasing_structural_ownership() {
        let registry = HandleRegistry::new();
        let (parent, parent_activity) = register(&registry, HandleType::Env, None);
        let (child, child_activity) = register(
            &registry,
            HandleType::Dbc,
            Some(Arc::clone(&parent_activity)),
        );
        let handle = registry.acquire::<u32>(child, HandleType::Dbc).unwrap();
        assert_eq!(count(&parent_activity), 1);
        assert_eq!(count(&child_activity), 1);
        let structural = handle.into_arc();
        assert_eq!(count(&parent_activity), 0);
        assert_eq!(count(&child_activity), 0);
        let parent_ref = registry.acquire::<u32>(parent, HandleType::Env).unwrap();
        let closing = registry.begin_close(&parent_ref).unwrap();
        registry
            .retire_batch(&[(parent, HandleType::Env), (child, HandleType::Dbc)])
            .unwrap();
        assert_eq!(*structural, 42);
        drop(closing);
    }

    #[test]
    fn acquired_before_retirement_remains_alive_on_another_thread() {
        let registry = Arc::new(HandleRegistry::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let id = registry
            .register(
                HandleType::Env,
                Arc::new(Payload {
                    drops: Arc::clone(&drops),
                }),
                HandleActivity::new(None),
            )
            .unwrap();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (retired_tx, retired_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_registry = Arc::clone(&registry);
        let worker = thread::spawn(move || {
            let handle = worker_registry
                .acquire::<Payload>(id, HandleType::Env)
                .unwrap();
            acquired_tx.send(()).unwrap();
            receive(&retired_rx);
            assert_eq!(handle.drops.load(Ordering::Relaxed), 0);
            drop(handle);
            done_tx.send(()).unwrap();
        });
        receive(&acquired_rx);
        registry.retire(id, HandleType::Env).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        retired_tx.send(()).unwrap();
        receive(&done_rx);
        worker.join().unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn retired_before_acquisition_is_missing_on_another_thread() {
        let registry = Arc::new(HandleRegistry::new());
        let (id, activity) = register(&registry, HandleType::Env, None);
        let (retired_tx, retired_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker_registry = Arc::clone(&registry);
        let worker = thread::spawn(move || {
            receive(&retired_rx);
            result_tx
                .send(worker_registry.acquire::<u32>(id, HandleType::Env).err())
                .unwrap();
        });
        registry.retire(id, HandleType::Env).unwrap();
        retired_tx.send(()).unwrap();
        assert_eq!(receive(&result_rx), Some(RegistryError::NotFound));
        worker.join().unwrap();
        assert_eq!(count(&activity), 0);
    }

    struct Tree {
        registry: HandleRegistry,
        env: HandleId,
        env_activity: Arc<HandleActivity>,
        dbc: HandleId,
        dbc_activity: Arc<HandleActivity>,
        stmt: HandleId,
        stmt_activity: Arc<HandleActivity>,
        implicit_desc: HandleId,
        explicit_desc: HandleId,
    }

    impl Tree {
        fn new() -> Self {
            let registry = HandleRegistry::new();
            let (env, env_activity) = register(&registry, HandleType::Env, None);
            let (dbc, dbc_activity) =
                register(&registry, HandleType::Dbc, Some(Arc::clone(&env_activity)));
            let (stmt, stmt_activity) =
                register(&registry, HandleType::Stmt, Some(Arc::clone(&dbc_activity)));
            let (implicit_desc, _) = register(
                &registry,
                HandleType::Desc,
                Some(Arc::clone(&stmt_activity)),
            );
            let (explicit_desc, _) =
                register(&registry, HandleType::Desc, Some(Arc::clone(&dbc_activity)));
            Self {
                registry,
                env,
                env_activity,
                dbc,
                dbc_activity,
                stmt,
                stmt_activity,
                implicit_desc,
                explicit_desc,
            }
        }

        fn acquire(&self, id: HandleId, kind: HandleType) -> HandleRef<u32> {
            self.registry.acquire(id, kind).unwrap()
        }
    }

    #[test]
    fn descendant_call_blocks_parent_close_but_failed_close_keeps_admission_live() {
        let tree = Tree::new();
        let parent = tree.acquire(tree.dbc, HandleType::Dbc);
        let child = tree.acquire(tree.stmt, HandleType::Stmt);
        assert_eq!(count(&tree.dbc_activity), 2);
        assert_eq!(
            tree.registry.begin_close(&parent).err(),
            Some(RegistryError::Busy)
        );
        let another_child = tree.acquire(tree.stmt, HandleType::Stmt);
        drop(another_child);
        drop(child);
        assert_eq!(count(&tree.dbc_activity), 1);
        let closing = tree.registry.begin_close(&parent).unwrap();
        assert_eq!(
            tree.registry
                .acquire::<u32>(tree.stmt, HandleType::Stmt)
                .err(),
            Some(RegistryError::Busy)
        );
        drop(closing);
        drop(tree.acquire(tree.stmt, HandleType::Stmt));
    }

    #[test]
    fn implicit_descriptor_call_blocks_statement_connection_and_environment_close() {
        let tree = Tree::new();
        let descriptor = tree.acquire(tree.implicit_desc, HandleType::Desc);
        assert_eq!(count(&tree.stmt_activity), 1);
        for (id, kind) in [
            (tree.stmt, HandleType::Stmt),
            (tree.dbc, HandleType::Dbc),
            (tree.env, HandleType::Env),
        ] {
            let parent = tree.acquire(id, kind);
            assert_eq!(
                tree.registry.begin_close(&parent).err(),
                Some(RegistryError::Busy)
            );
        }
        drop(descriptor);
        let stmt = tree.acquire(tree.stmt, HandleType::Stmt);
        drop(tree.registry.begin_close(&stmt).unwrap());
    }

    #[test]
    fn explicit_descriptor_call_blocks_connection_and_environment_but_not_statement() {
        let tree = Tree::new();
        let descriptor = tree.acquire(tree.explicit_desc, HandleType::Desc);
        assert_eq!(count(&tree.stmt_activity), 0);
        assert_eq!(count(&tree.dbc_activity), 1);
        assert_eq!(count(&tree.env_activity), 1);
        for (id, kind) in [(tree.dbc, HandleType::Dbc), (tree.env, HandleType::Env)] {
            let parent = tree.acquire(id, kind);
            assert_eq!(
                tree.registry.begin_close(&parent).err(),
                Some(RegistryError::Busy)
            );
        }
        let stmt = tree.acquire(tree.stmt, HandleType::Stmt);
        drop(tree.registry.begin_close(&stmt).unwrap());
        drop(descriptor);
        assert_eq!(count(&tree.dbc_activity), 1);
        drop(stmt);
        assert_eq!(count(&tree.dbc_activity), 0);
    }

    #[test]
    fn temporary_close_rejects_self_and_all_descendants_then_reopens() {
        let tree = Tree::new();
        let parent = tree.acquire(tree.dbc, HandleType::Dbc);
        let closing = tree.registry.begin_close(&parent).unwrap();
        for (id, kind) in [
            (tree.dbc, HandleType::Dbc),
            (tree.stmt, HandleType::Stmt),
            (tree.implicit_desc, HandleType::Desc),
            (tree.explicit_desc, HandleType::Desc),
        ] {
            assert_eq!(
                tree.registry.acquire::<u32>(id, kind).err(),
                Some(RegistryError::Busy)
            );
            assert_eq!(tree.registry.kind(id), Ok(Some(kind)));
        }
        let child_activity = HandleActivity::new(Some(Arc::clone(&tree.dbc_activity)));
        assert_eq!(
            tree.registry.register(
                HandleType::Stmt,
                Arc::new(7_u32),
                Arc::clone(&child_activity)
            ),
            Err(RegistryError::Busy)
        );
        assert!(child_activity.owner.get().is_none());
        assert_eq!(count(&tree.dbc_activity), 1);
        assert_eq!(
            tree.registry.begin_close(&parent).err(),
            Some(RegistryError::Busy)
        );
        drop(tree.acquire(tree.env, HandleType::Env));
        drop(closing);
        drop(tree.acquire(tree.implicit_desc, HandleType::Desc));
        drop(tree.acquire(tree.explicit_desc, HandleType::Desc));
        tree.registry
            .register(HandleType::Stmt, Arc::new(7_u32), child_activity)
            .unwrap();
    }

    #[test]
    fn another_call_on_the_same_handle_blocks_close_without_changing_counts() {
        let registry = HandleRegistry::new();
        let (id, activity) = register(&registry, HandleType::Env, None);
        let first = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        let second = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        assert_eq!(count(&activity), 2);
        assert_eq!(
            registry.begin_close(&first).err(),
            Some(RegistryError::Busy)
        );
        assert_eq!(count(&activity), 2);
        drop(second);
        drop(registry.begin_close(&first).unwrap());
        drop(first);
        assert_eq!(count(&activity), 0);
    }

    #[test]
    fn retirement_cannot_be_reopened_by_a_close_guard() {
        let tree = Tree::new();
        let parent = tree.acquire(tree.dbc, HandleType::Dbc);
        let closing = tree.registry.begin_close(&parent).unwrap();
        tree.registry.retire(tree.dbc, HandleType::Dbc).unwrap();
        drop(closing);
        assert_eq!(tree.dbc_activity.admission.load(Ordering::Acquire), RETIRED);
        assert_eq!(
            tree.registry
                .acquire::<u32>(tree.implicit_desc, HandleType::Desc)
                .err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            tree.registry.register(
                HandleType::Stmt,
                Arc::new(7_u32),
                HandleActivity::new(Some(Arc::clone(&tree.dbc_activity)))
            ),
            Err(RegistryError::NotFound)
        );
    }

    #[test]
    fn close_guard_can_outlive_the_closing_call_and_reopens_without_locking() {
        let registry = HandleRegistry::new();
        let (id, activity) = register(&registry, HandleType::Env, None);
        let handle = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        let closing = registry.begin_close(&handle).unwrap();
        drop(handle);
        assert_eq!(count(&activity), 0);
        assert_eq!(
            registry.acquire::<u32>(id, HandleType::Env).err(),
            Some(RegistryError::Busy)
        );
        drop(closing);
        drop(registry.acquire::<u32>(id, HandleType::Env).unwrap());
    }

    #[test]
    fn activity_chain_keeps_parents_alive_after_retirement() {
        let registry = HandleRegistry::new();
        let (parent, parent_activity) = register(&registry, HandleType::Env, None);
        let weak_parent = Arc::downgrade(&parent_activity);
        let (child, child_activity) = register(&registry, HandleType::Dbc, Some(parent_activity));
        let child_ref = registry.acquire::<u32>(child, HandleType::Dbc).unwrap();
        registry
            .retire_batch(&[(parent, HandleType::Env), (child, HandleType::Dbc)])
            .unwrap();
        drop(child_activity);
        assert!(weak_parent.upgrade().is_some());
        drop(child_ref);
        assert!(weak_parent.upgrade().is_none());
    }

    #[test]
    fn overflow_rolls_back_preceding_counters_at_any_depth() {
        let tree = Tree::new();
        for saturated in [&tree.env_activity, &tree.dbc_activity, &tree.stmt_activity] {
            saturated.active.store(usize::MAX, Ordering::Release);
            assert_eq!(
                tree.registry
                    .acquire::<u32>(tree.implicit_desc, HandleType::Desc)
                    .err(),
                Some(RegistryError::ActivityOverflow)
            );
            for activity in [&tree.env_activity, &tree.dbc_activity, &tree.stmt_activity] {
                assert_eq!(
                    count(activity),
                    if Arc::ptr_eq(activity, saturated) {
                        usize::MAX
                    } else {
                        0
                    }
                );
            }
            let state = tree.registry.state.read().unwrap();
            assert_eq!(
                count(&state.entries.get(&tree.implicit_desc).unwrap().activity),
                0
            );
            saturated.active.store(0, Ordering::Release);
        }
        drop(tree.acquire(tree.implicit_desc, HandleType::Desc));
        assert_eq!(count(&tree.env_activity), 0);
    }

    #[test]
    fn maximum_nonoverflowing_count_can_be_acquired_and_released() {
        let registry = HandleRegistry::new();
        let (id, activity) = register(&registry, HandleType::Env, None);
        activity.active.store(usize::MAX - 1, Ordering::Release);
        let handle = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        assert_eq!(count(&activity), usize::MAX);
        assert_eq!(
            registry.acquire::<u32>(id, HandleType::Env).err(),
            Some(RegistryError::ActivityOverflow)
        );
        drop(handle);
        assert_eq!(count(&activity), usize::MAX - 1);
        activity.active.store(0, Ordering::Release);
    }

    #[test]
    fn concurrent_readers_cannot_overflow_a_shared_ancestor() {
        let registry = Arc::new(HandleRegistry::new());
        let (_, parent) = register(&registry, HandleType::Env, None);
        let children: Vec<_> = (0..32)
            .map(|_| register(&registry, HandleType::Dbc, Some(Arc::clone(&parent))))
            .collect();
        parent.active.store(usize::MAX - 1, Ordering::Release);

        let state = registry.read();
        let start = Arc::new(Barrier::new(children.len() + 1));
        let (sender, receiver) = mpsc::channel();
        let workers: Vec<_> = children
            .iter()
            .map(|&(id, _)| {
                let registry = Arc::clone(&registry);
                let start = Arc::clone(&start);
                let sender = sender.clone();
                thread::spawn(move || {
                    start.wait();
                    sender
                        .send(registry.acquire::<u32>(id, HandleType::Dbc))
                        .unwrap();
                })
            })
            .collect();
        start.wait();
        let results: Result<Vec<_>, _> = (0..children.len())
            .map(|_| receiver.recv_timeout(TIMEOUT))
            .collect();
        drop(state);
        for worker in workers {
            worker.join().unwrap();
        }

        let results = results.unwrap();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(count(&parent), usize::MAX);
        for error in results.iter().filter_map(|result| result.as_ref().err()) {
            assert_eq!(*error, RegistryError::ActivityOverflow);
        }
        for (id, activity) in &children {
            let acquired = results
                .iter()
                .any(|result| result.as_ref().is_ok_and(|handle| handle.id() == *id));
            assert_eq!(count(activity), usize::from(acquired));
        }
        drop(results);
        assert_eq!(count(&parent), usize::MAX - 1);
        assert!(children.iter().all(|(_, activity)| count(activity) == 0));
        parent.active.store(0, Ordering::Release);
    }

    #[test]
    fn failed_reservation_does_not_consume_identity_bind_activity_or_insert() {
        for wrap in [false, true] {
            let registry = HandleRegistry::new();
            let mut live = Vec::new();
            if wrap {
                let (first, _) = register(&registry, HandleType::Env, None);
                registry.force_wrap_for_test();
                let (last, _) = register(&registry, HandleType::Env, None);
                live.extend([(first, 42), (last, 42)]);
            }
            let activity = HandleActivity::new(None);
            registry.state.write().unwrap().reserve_additional = usize::MAX;
            assert_eq!(
                registry.register(HandleType::Env, Arc::new(42_u32), Arc::clone(&activity)),
                Err(RegistryError::Capacity)
            );
            assert_live_entries(&registry, &live);
            assert_eq!(registry.state.read().unwrap().next_id, NonZeroUsize::MIN);
            assert!(!activity.registered.load(Ordering::Acquire));
            assert!(activity.owner.get().is_none());
            assert_eq!(count(&activity), 0);
            registry.state.write().unwrap().reserve_additional = 1;
            let id = registry
                .register(HandleType::Env, Arc::new(42_u32), activity)
                .unwrap();
            assert_eq!(id.to_raw().addr(), if wrap { 2 } else { 1 });
            live.push((id, 42));
            assert_live_entries(&registry, &live);
        }
    }

    #[test]
    fn activities_cannot_be_shared_between_entries_or_registries() {
        let first = HandleRegistry::new();
        let second = HandleRegistry::new();
        let (id, activity) = register(&first, HandleType::Env, None);
        for registry in [&first, &second] {
            assert_eq!(
                registry.register(HandleType::Env, Arc::new(0_u32), Arc::clone(&activity)),
                Err(RegistryError::InvalidActivity)
            );
        }
        assert_eq!(
            second.register(
                HandleType::Dbc,
                Arc::new(0_u32),
                HandleActivity::new(Some(Arc::clone(&activity)))
            ),
            Err(RegistryError::InvalidActivity)
        );
        first.retire(id, HandleType::Env).unwrap();
        assert_eq!(
            first.register(HandleType::Env, Arc::new(0_u32), activity),
            Err(RegistryError::InvalidActivity)
        );
        assert!(first.state.read().unwrap().entries.is_empty());
        assert!(second.state.read().unwrap().entries.is_empty());
    }

    #[test]
    fn implicit_descriptors_can_register_before_their_statement_and_ancestors() {
        let registry = HandleRegistry::new();
        let env = HandleActivity::new(None);
        let dbc = HandleActivity::new(Some(Arc::clone(&env)));
        let stmt = HandleActivity::new(Some(Arc::clone(&dbc)));
        let mut children = Vec::new();
        for _ in 0..4 {
            children.push(register(
                &registry,
                HandleType::Desc,
                Some(Arc::clone(&stmt)),
            ));
        }
        assert!(env.owner.get().is_some());
        assert!(!env.registered.load(Ordering::Acquire));
        assert!(!dbc.registered.load(Ordering::Acquire));
        assert!(!stmt.registered.load(Ordering::Acquire));
        let stmt_id = registry
            .register(HandleType::Stmt, Arc::new(42_u32), Arc::clone(&stmt))
            .unwrap();
        registry
            .register(HandleType::Dbc, Arc::new(42_u32), Arc::clone(&dbc))
            .unwrap();
        registry
            .register(HandleType::Env, Arc::new(42_u32), Arc::clone(&env))
            .unwrap();
        let caller = registry.acquire::<u32>(stmt_id, HandleType::Stmt).unwrap();
        let mut descriptors = Vec::new();
        for (id, _) in &children {
            descriptors.push(registry.acquire::<u32>(*id, HandleType::Desc).unwrap());
        }
        assert_eq!(count(&stmt), 5);
        assert_eq!(
            registry.begin_close(&caller).err(),
            Some(RegistryError::Busy)
        );
        drop(descriptors);
        drop(registry.begin_close(&caller).unwrap());
    }

    #[test]
    fn failed_parent_registration_allows_unpublished_children_to_be_rolled_back() {
        let registry = HandleRegistry::with_id_limit(3);
        let parent = HandleActivity::new(None);
        let children: Vec<_> = (0..3)
            .map(|_| register(&registry, HandleType::Desc, Some(Arc::clone(&parent))))
            .collect();
        assert_eq!(
            registry.register(HandleType::Stmt, Arc::new(42_u32), Arc::clone(&parent)),
            Err(RegistryError::IdSpaceFull)
        );
        let batch: Vec<_> = children
            .iter()
            .map(|(id, _)| (*id, HandleType::Desc))
            .collect();
        registry.retire_batch(&batch).unwrap();
        assert_eq!(count(&parent), 0);
        for (_, activity) in children {
            assert_eq!(count(&activity), 0);
            assert_eq!(activity.admission.load(Ordering::Acquire), RETIRED);
            assert_eq!(
                registry.register(HandleType::Desc, Arc::new(42_u32), activity),
                Err(RegistryError::InvalidActivity)
            );
        }
        assert!(!parent.registered.load(Ordering::Acquire));
        assert!(registry.state.read().unwrap().entries.is_empty());
        let (child, _) = register(&registry, HandleType::Desc, Some(Arc::clone(&parent)));
        let parent_id = registry
            .register(HandleType::Stmt, Arc::new(42_u32), Arc::clone(&parent))
            .unwrap();
        assert_ne!(parent_id, child);
        assert_eq!(parent_id.to_raw().addr(), 2);
        assert_eq!(
            *registry.acquire::<u32>(child, HandleType::Desc).unwrap(),
            42
        );
        assert_eq!(
            *registry
                .acquire::<u32>(parent_id, HandleType::Stmt)
                .unwrap(),
            42
        );
        registry
            .retire_batch(&[(child, HandleType::Desc), (parent_id, HandleType::Stmt)])
            .unwrap();
    }

    #[test]
    fn concurrent_registries_claim_an_unregistered_tree_without_splitting_it() {
        let first = Arc::new(HandleRegistry::new());
        let second = Arc::new(HandleRegistry::new());
        let root = HandleActivity::new(None);
        let start = Arc::new(std::sync::Barrier::new(3));
        let (result_tx, result_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let probe = HandleId::from_raw(std::ptr::without_provenance_mut(usize::MAX)).unwrap();
        let mut workers = Vec::new();
        for (index, registry) in [Arc::clone(&first), Arc::clone(&second)]
            .into_iter()
            .enumerate()
        {
            let start = Arc::clone(&start);
            let result_tx = result_tx.clone();
            let child = HandleActivity::new(Some(Arc::clone(&root)));
            let value = reenter_payload(&registry, probe, &dropped_tx);
            workers.push(thread::spawn(move || {
                start.wait();
                result_tx
                    .send((index, registry.register(HandleType::Desc, value, child)))
                    .unwrap();
            }));
        }
        start.wait();
        let outcomes = [receive(&result_rx), receive(&result_rx)];
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            outcomes.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|(_, result)| *result == Err(RegistryError::InvalidActivity))
                .count(),
            1
        );
        for (index, result) in outcomes {
            let registry = if index == 0 { &first } else { &second };
            let root_result =
                registry.register(HandleType::Stmt, Arc::new(42_u32), Arc::clone(&root));
            if let Ok(id) = result {
                assert!(root_result.is_ok());
                registry.retire(id, HandleType::Desc).unwrap();
            } else {
                assert_eq!(root_result, Err(RegistryError::InvalidActivity));
                assert!(registry.state.read().unwrap().entries.is_empty());
                assert_eq!(registry.state.read().unwrap().next_id, NonZeroUsize::MIN);
            }
        }
        assert_eq!(receive(&dropped_rx), Ok(None));
        assert_eq!(receive(&dropped_rx), Ok(None));
    }

    #[test]
    fn concurrent_registration_of_one_activity_creates_exactly_one_entry() {
        let registry = Arc::new(HandleRegistry::new());
        let activity = HandleActivity::new(None);
        let start = Arc::new(std::sync::Barrier::new(3));
        let (result_tx, result_rx) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..2 {
            let registry = Arc::clone(&registry);
            let activity = Arc::clone(&activity);
            let start = Arc::clone(&start);
            let result_tx = result_tx.clone();
            workers.push(thread::spawn(move || {
                start.wait();
                result_tx
                    .send(registry.register(HandleType::Env, Arc::new(42_u32), activity))
                    .unwrap();
            }));
        }
        start.wait();
        let outcomes = [receive(&result_rx), receive(&result_rx)];
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| **result == Err(RegistryError::InvalidActivity))
                .count(),
            1
        );
        let state = registry.state.read().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.next_id.get(), 2);
        assert_eq!(count(&activity), 0);
    }

    #[test]
    fn foreign_reference_cannot_close_matching_id_kind_and_payload() {
        let first = HandleRegistry::new();
        let second = HandleRegistry::new();
        let value = Arc::new(42_u32);
        let first_id = first
            .register(
                HandleType::Env,
                Arc::clone(&value),
                HandleActivity::new(None),
            )
            .unwrap();
        let second_id = second
            .register(HandleType::Env, value, HandleActivity::new(None))
            .unwrap();
        assert_eq!(first_id, second_id);
        let handle = first.acquire::<u32>(first_id, HandleType::Env).unwrap();
        assert_eq!(
            second.begin_close(&handle).err(),
            Some(RegistryError::OwnershipMismatch)
        );
        drop(second.acquire::<u32>(second_id, HandleType::Env).unwrap());
        drop(first.begin_close(&handle).unwrap());
    }

    #[test]
    fn begin_close_checks_type_and_retired_ancestors_without_changing_admission() {
        let first = HandleRegistry::new();
        let second = HandleRegistry::new();
        let (first_id, _) = register(&first, HandleType::Env, None);
        let (second_id, second_activity) = register(&second, HandleType::Dbc, None);
        assert_eq!(first_id, second_id);
        let foreign = first.acquire::<u32>(first_id, HandleType::Env).unwrap();
        assert_eq!(
            second.begin_close(&foreign).err(),
            Some(RegistryError::WrongType)
        );
        assert_eq!(count(&second_activity), 0);
        assert_eq!(second_activity.admission.load(Ordering::Acquire), OPEN);

        let tree = Tree::new();
        let child = tree.acquire(tree.stmt, HandleType::Stmt);
        tree.registry.retire(tree.dbc, HandleType::Dbc).unwrap();
        assert_eq!(
            tree.registry.begin_close(&child).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(count(&tree.stmt_activity), 1);
        assert_eq!(tree.stmt_activity.admission.load(Ordering::Acquire), OPEN);
    }

    #[test]
    fn batch_preflight_prevents_partial_removal_or_activity_mutation() {
        let registry = HandleRegistry::new();
        let (first, first_activity) = register(&registry, HandleType::Env, None);
        let (second, second_activity) = register(&registry, HandleType::Dbc, None);
        let missing = HandleId::from_raw(std::ptr::without_provenance_mut(123)).unwrap();
        for (batch, error) in [
            (
                [(first, HandleType::Env), (second, HandleType::Stmt)],
                RegistryError::WrongType,
            ),
            (
                [(first, HandleType::Env), (missing, HandleType::Dbc)],
                RegistryError::NotFound,
            ),
            (
                [(first, HandleType::Env), (first, HandleType::Env)],
                RegistryError::DuplicateId,
            ),
        ] {
            assert_eq!(registry.retire_batch(&batch), Err(error));
            assert_eq!(registry.kind(first), Ok(Some(HandleType::Env)));
            assert_eq!(registry.kind(second), Ok(Some(HandleType::Dbc)));
            assert_eq!(first_activity.admission.load(Ordering::Acquire), OPEN);
            assert_eq!(second_activity.admission.load(Ordering::Acquire), OPEN);
            assert_eq!(count(&first_activity), 0);
            assert_eq!(count(&second_activity), 0);
        }
        registry.retire_batch(&[]).unwrap();
        registry
            .retire_batch(&[(first, HandleType::Env), (second, HandleType::Dbc)])
            .unwrap();
        assert!(registry.state.read().unwrap().entries.is_empty());
        assert_eq!(first_activity.admission.load(Ordering::Acquire), RETIRED);
        assert_eq!(second_activity.admission.load(Ordering::Acquire), RETIRED);
    }

    fn poison(registry: &Arc<HandleRegistry>) {
        let registry = Arc::clone(registry);
        assert!(
            thread::spawn(move || {
                let _guard = registry.state.write().unwrap();
                panic!("poison the test registry");
            })
            .join()
            .is_err()
        );
    }

    #[test]
    fn registry_recovers_poison_without_reopening_closed_handles() {
        let registry = Arc::new(HandleRegistry::new());
        let (id, activity) = register(&registry, HandleType::Env, None);
        let handle = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        let closing = registry.begin_close(&handle).unwrap();
        poison(&registry);
        assert_eq!(registry.kind(id), Ok(Some(HandleType::Env)));
        assert!(!registry.state.is_poisoned());
        assert_eq!(
            registry.acquire::<u32>(id, HandleType::Env).err(),
            Some(RegistryError::Busy)
        );
        assert_eq!(
            *registry.diagnostics::<u32>(id, HandleType::Env).unwrap(),
            *handle
        );
        assert_eq!(
            registry.begin_close(&handle).err(),
            Some(RegistryError::Busy)
        );
        let other = registry
            .register(HandleType::Env, Arc::new(0_u32), HandleActivity::new(None))
            .unwrap();
        registry.retire(other, HandleType::Env).unwrap();
        drop(closing);
        assert_eq!(activity.admission.load(Ordering::Acquire), OPEN);
        drop(registry.acquire::<u32>(id, HandleType::Env).unwrap());
        drop(handle);
        assert_eq!(count(&activity), 0);
        poison(&registry);
        registry.retire_batch(&[(id, HandleType::Env)]).unwrap();
        registry.retire_batch(&[]).unwrap();
        assert_eq!(registry.kind(id), Ok(None));
    }

    #[test]
    fn diagnostics_validate_identity_without_changing_activity() {
        let tree = Tree::new();
        let parent = tree.acquire(tree.dbc, HandleType::Dbc);
        let _closing = tree.registry.begin_close(&parent).unwrap();
        let diagnostic = tree
            .registry
            .diagnostics::<u32>(tree.stmt, HandleType::Stmt)
            .unwrap();
        assert_eq!(
            tree.registry
                .diagnostics::<u32>(tree.stmt, HandleType::Desc)
                .err(),
            Some(RegistryError::WrongType)
        );
        assert_eq!(
            tree.registry
                .diagnostics::<u64>(tree.stmt, HandleType::Stmt)
                .err(),
            Some(RegistryError::WrongType)
        );
        assert_eq!(
            tree.registry
                .acquire::<u32>(tree.stmt, HandleType::Stmt)
                .err(),
            Some(RegistryError::Busy)
        );
        tree.registry.retire(tree.dbc, HandleType::Dbc).unwrap();
        assert_eq!(
            tree.registry
                .diagnostics::<u32>(tree.stmt, HandleType::Stmt)
                .err(),
            Some(RegistryError::NotFound)
        );
        drop(diagnostic);
    }

    struct Reenter {
        registry: Weak<HandleRegistry>,
        probe: HandleId,
        dropped: Sender<Result<Option<HandleType>, RegistryError>>,
    }

    impl Drop for Reenter {
        fn drop(&mut self) {
            let registry = self.registry.upgrade().unwrap();
            self.dropped.send(registry.kind(self.probe)).unwrap();
        }
    }

    fn reenter_payload(
        registry: &Arc<HandleRegistry>,
        probe: HandleId,
        dropped: &Sender<Result<Option<HandleType>, RegistryError>>,
    ) -> Arc<Reenter> {
        Arc::new(Reenter {
            registry: Arc::downgrade(registry),
            probe,
            dropped: dropped.clone(),
        })
    }

    #[test]
    fn retirement_payload_drops_can_reenter_the_registry() {
        for batch in [false, true] {
            let registry = Arc::new(HandleRegistry::new());
            let (probe, _) = register(&registry, HandleType::Env, None);
            let (dropped_tx, dropped_rx) = mpsc::channel();
            let first = registry
                .register(
                    HandleType::Env,
                    reenter_payload(&registry, probe, &dropped_tx),
                    HandleActivity::new(None),
                )
                .unwrap();
            let second = registry
                .register(
                    HandleType::Env,
                    reenter_payload(&registry, probe, &dropped_tx),
                    HandleActivity::new(None),
                )
                .unwrap();
            let worker = thread::spawn(move || {
                if batch {
                    registry
                        .retire_batch(&[(first, HandleType::Env), (second, HandleType::Env)])
                        .unwrap();
                } else {
                    registry.retire(first, HandleType::Env).unwrap();
                    registry.retire(second, HandleType::Env).unwrap();
                }
            });
            assert_eq!(receive(&dropped_rx), Ok(Some(HandleType::Env)));
            assert_eq!(receive(&dropped_rx), Ok(Some(HandleType::Env)));
            worker.join().unwrap();
        }
    }

    #[test]
    fn last_retained_reference_drops_payload_outside_the_registry_lock() {
        let registry = Arc::new(HandleRegistry::new());
        let (probe, _) = register(&registry, HandleType::Env, None);
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let id = registry
            .register(
                HandleType::Dbc,
                reenter_payload(&registry, probe, &dropped_tx),
                HandleActivity::new(None),
            )
            .unwrap();
        let handle = registry.acquire::<Reenter>(id, HandleType::Dbc).unwrap();
        registry.retire(id, HandleType::Dbc).unwrap();
        assert!(dropped_rx.try_recv().is_err());
        let worker = thread::spawn(move || drop(handle));
        assert_eq!(receive(&dropped_rx), Ok(Some(HandleType::Env)));
        worker.join().unwrap();
    }

    struct BlockingDrop {
        entered: Sender<()>,
        finish: Mutex<Receiver<()>>,
    }

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            receive(self.finish.get_mut().unwrap());
        }
    }

    #[test]
    fn ancestor_close_stays_busy_until_final_payload_destruction_finishes() {
        let tree = Tree::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let activity = HandleActivity::new(Some(Arc::clone(&tree.stmt_activity)));
        let id = tree
            .registry
            .register(
                HandleType::Desc,
                Arc::new(BlockingDrop {
                    entered: entered_tx,
                    finish: Mutex::new(finish_rx),
                }),
                Arc::clone(&activity),
            )
            .unwrap();
        let handle = tree
            .registry
            .acquire::<BlockingDrop>(id, HandleType::Desc)
            .unwrap();
        tree.registry.retire(id, HandleType::Desc).unwrap();
        let worker = thread::spawn(move || {
            drop(handle);
            done_tx.send(()).unwrap();
        });
        receive(&entered_rx);
        let ancestors = [
            (tree.env, HandleType::Env, &tree.env_activity),
            (tree.dbc, HandleType::Dbc, &tree.dbc_activity),
            (tree.stmt, HandleType::Stmt, &tree.stmt_activity),
        ];
        let mut blocked_results = Vec::new();
        for (id, kind, activity) in ancestors {
            let ancestor = tree.acquire(id, kind);
            blocked_results.push((tree.registry.begin_close(&ancestor).err(), count(activity)));
        }
        let leaf_count = count(&activity);
        finish_tx.send(()).unwrap();
        receive(&done_rx);
        worker.join().unwrap();

        assert_eq!(leaf_count, 1);
        for (error, active) in blocked_results {
            assert_eq!(error, Some(RegistryError::Busy));
            assert_eq!(active, 2);
        }
        assert_eq!(count(&activity), 0);
        for (id, kind, activity) in ancestors {
            assert_eq!(count(activity), 0);
            let ancestor = tree.acquire(id, kind);
            drop(tree.registry.begin_close(&ancestor).unwrap());
        }
    }

    #[test]
    fn failed_registration_payload_drops_can_reenter_on_every_error_path() {
        for expected in [
            RegistryError::WrongType,
            RegistryError::InvalidActivity,
            RegistryError::Busy,
            RegistryError::NotFound,
            RegistryError::IdSpaceFull,
            RegistryError::Capacity,
        ] {
            let limit = if expected == RegistryError::IdSpaceFull {
                1
            } else {
                usize::MAX
            };
            let registry = Arc::new(HandleRegistry::with_id_limit(limit));
            let (probe, parent_activity) = register(&registry, HandleType::Env, None);
            let mut activity = HandleActivity::new(Some(Arc::clone(&parent_activity)));
            let mut kind = HandleType::Dbc;
            let parent_ref = registry.acquire::<u32>(probe, HandleType::Env).unwrap();
            let mut closing = None;
            match expected {
                RegistryError::WrongType => kind = HandleType::Invalid,
                RegistryError::InvalidActivity => activity = Arc::clone(&parent_activity),
                RegistryError::Busy => {
                    closing = Some(registry.begin_close(&parent_ref).unwrap());
                }
                RegistryError::NotFound => registry.retire(probe, HandleType::Env).unwrap(),
                RegistryError::IdSpaceFull => {}
                RegistryError::Capacity => {
                    registry.state.write().unwrap().reserve_additional = usize::MAX;
                }
                _ => unreachable!("not a registration error"),
            }
            let (dropped_tx, dropped_rx) = mpsc::channel();
            let payload = reenter_payload(&registry, probe, &dropped_tx);
            let (result_tx, result_rx) = mpsc::channel();
            let worker_registry = Arc::clone(&registry);
            let worker = thread::spawn(move || {
                result_tx
                    .send(worker_registry.register(kind, payload, activity))
                    .unwrap();
            });
            let probe_result = match expected {
                RegistryError::NotFound => Ok(None),
                _ => Ok(Some(HandleType::Env)),
            };
            assert_eq!(receive(&dropped_rx), probe_result);
            assert_eq!(receive(&result_rx), Err(expected));
            worker.join().unwrap();
            drop(closing);
        }
    }
}

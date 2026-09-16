// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::any::Any;
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

mod slots;
use slots::{SlotState, SlotTable};

use super::HandleType;
use crate::api::odbc_types::SqlHandle;

const INDEX_BITS: u32 = if usize::BITS == 64 { 32 } else { 20 };
const GENERATION_BITS: u32 = usize::BITS - INDEX_BITS;
const GENERATION_MASK: usize = (1 << GENERATION_BITS) - 1;
const SLOT_CAPACITY: usize = 1 << INDEX_BITS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HandleId(NonZeroUsize);

impl HandleId {
    fn from_parts(index: usize, generation: usize) -> Result<Self, RegistryError> {
        if index >= SLOT_CAPACITY || generation == 0 || generation > GENERATION_MASK {
            return Err(RegistryError::InvalidId);
        }
        NonZeroUsize::new((index << GENERATION_BITS) | generation)
            .map(Self)
            .ok_or(RegistryError::InvalidId)
    }

    fn index(self) -> usize {
        self.0.get() >> GENERATION_BITS
    }

    fn generation(self) -> usize {
        self.0.get() & GENERATION_MASK
    }

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
            Self::IdSpaceFull => "all handle slots are currently in use",
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

const ACTIVITY_BITS: u32 = usize::BITS - 2;
const ACTIVITY_MAX: usize = (1 << ACTIVITY_BITS) - 1;
const ADMISSION_MASK: usize = !ACTIVITY_MAX;
const OPEN: usize = 0;
const CLOSING: usize = 1 << ACTIVITY_BITS;
const RETIRED: usize = 2 << ACTIVITY_BITS;

/// In-flight calls, independent of the Arcs that own handle storage.
#[derive(Debug)]
#[repr(align(128))]
pub(crate) struct HandleActivity {
    parent: Option<Arc<Self>>,
    owner: OnceLock<Arc<()>>,
    registered: AtomicBool,
    state: AtomicUsize,
}

impl HandleActivity {
    pub(crate) fn new(parent: Option<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self {
            parent,
            owner: OnceLock::new(),
            registered: AtomicBool::new(false),
            state: AtomicUsize::new(OPEN),
        })
    }

    fn ancestors(&self) -> impl Iterator<Item = &Self> {
        std::iter::successors(Some(self), |activity| activity.parent.as_deref())
    }

    /// Only direct ENV calls reserve the root: child-before-parent ENV free is
    /// DM-ordered. Descendants still check its admission.
    fn reserved(&self) -> impl Iterator<Item = &Self> {
        self.ancestors()
            .filter(|activity| activity.parent.is_some() || std::ptr::eq(*activity, self))
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
            match activity.admission() {
                OPEN => {}
                CLOSING => return Err(RegistryError::Busy),
                _ => return Err(RegistryError::NotFound),
            }
        }
        Ok(())
    }

    fn admission(&self) -> usize {
        self.state.load(Ordering::Acquire) & ADMISSION_MASK
    }

    fn reserve(&self) -> Result<(), RegistryError> {
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & ADMISSION_MASK == OPEN && state & ACTIVITY_MAX != ACTIVITY_MAX)
                    .then(|| state + 1)
            })
            .map(|_| ())
            .map_err(|state| match state & ADMISSION_MASK {
                OPEN => RegistryError::ActivityOverflow,
                CLOSING => RegistryError::Busy,
                _ => RegistryError::NotFound,
            })
    }

    fn release(&self) {
        self.state.fetch_sub(1, Ordering::Release);
    }

    fn retire(&self) {
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some((state & ACTIVITY_MAX) | RETIRED)
            });
    }

    #[cfg(test)]
    fn set_count_for_test(&self, count: usize) {
        assert!(count <= ACTIVITY_MAX);
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some((state & ADMISSION_MASK) | count)
            });
    }

    #[cfg(test)]
    pub(super) fn exhaust_for_test(self: &Arc<Self>) -> ActivityOverflowGuard {
        let previous = self.state.fetch_or(ACTIVITY_MAX, Ordering::AcqRel) & ACTIVITY_MAX;
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
        self.activity.set_count_for_test(self.previous);
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
            activity.release();
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
        let _ = self
            .activity
            .state
            .fetch_update(Ordering::Release, Ordering::Relaxed, |state| {
                (state & ADMISSION_MASK == CLOSING).then_some(state & ACTIVITY_MAX)
            });
    }
}

struct RegistryEntry {
    kind: HandleType,
    value: Arc<dyn Any + Send + Sync>,
    activity: Arc<HandleActivity>,
}

struct RegistryState {
    next_index: usize,
    free_head: Option<usize>,
    live: usize,
    #[cfg(test)]
    slot_limit: usize,
    #[cfg(test)]
    generation_limit: usize,
    #[cfg(test)]
    reserve_additional: usize,
    #[cfg(test)]
    fail_retirement_after: Option<usize>,
}

impl RegistryState {
    fn slot_limit(&self) -> usize {
        #[cfg(test)]
        {
            self.slot_limit
        }
        #[cfg(not(test))]
        {
            SLOT_CAPACITY
        }
    }

    fn next_generation(&self, previous: usize) -> usize {
        #[cfg(test)]
        let limit = self.generation_limit;
        #[cfg(not(test))]
        let limit = GENERATION_MASK;
        if previous >= limit { 1 } else { previous + 1 }
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
}

impl SlotState {
    fn checked_entry(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<&RegistryEntry, RegistryError> {
        if self.generation != id.generation() {
            return Err(RegistryError::NotFound);
        }
        let entry = self.entry.as_ref().ok_or(RegistryError::NotFound)?;
        if entry.kind != expected {
            return Err(RegistryError::WrongType);
        }
        Ok(entry)
    }
}

pub(crate) struct HandleRegistry {
    state: Mutex<RegistryState>,
    slots: SlotTable,
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
            state: Mutex::new(RegistryState {
                next_index: 0,
                free_head: None,
                live: 0,
                #[cfg(test)]
                slot_limit: SLOT_CAPACITY,
                #[cfg(test)]
                generation_limit: GENERATION_MASK,
                #[cfg(test)]
                reserve_additional: 1,
                #[cfg(test)]
                fail_retirement_after: None,
            }),
            slots: SlotTable::new(),
            identity: Arc::new(()),
        }
    }

    fn write(&self) -> MutexGuard<'_, RegistryState> {
        // This lock only protects allocation/free-list metadata. Payload
        // destructors and business-state locks remain outside it.
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned handle registry");
            self.state.clear_poison();
            poisoned.into_inner()
        })
    }

    #[cfg(test)]
    pub(super) fn poison_for_test(&self) {
        let _ = std::panic::catch_unwind(|| {
            let _state = self.state.lock().unwrap();
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
        state.generation_limit = 1;
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
        let reused = state.free_head.is_some();
        let index = state.free_head.unwrap_or(state.next_index);
        if index >= state.slot_limit() {
            return Err(RegistryError::IdSpaceFull);
        }
        #[cfg(test)]
        if state.reserve_additional == usize::MAX {
            return Err(RegistryError::Capacity);
        }
        let mut slot = self.slots.get_or_create(index)?.write();
        if slot.entry.is_some() {
            return Err(RegistryError::InvalidActivity);
        }
        let generation = state.next_generation(slot.generation);
        let id = HandleId::from_parts(index, generation)?;
        let owner = root.owner.get_or_init(|| Arc::clone(&self.identity));
        if !Arc::ptr_eq(owner, &self.identity) {
            return Err(RegistryError::InvalidActivity);
        }

        activity.registered.store(true, Ordering::Release);
        if reused {
            state.free_head = slot.next_free.take();
        } else {
            state.next_index += 1;
        }
        slot.generation = generation;
        slot.entry = Some(RegistryEntry {
            kind,
            value,
            activity,
        });
        state.live += 1;
        drop(slot);
        drop(state);
        Ok(id)
    }

    pub(crate) fn acquire<T: Any + Send + Sync>(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<HandleRef<T>, RegistryError> {
        let slot = self.slots.get(id.index()).ok_or(RegistryError::NotFound)?;
        let state = slot.read();
        let entry = state.checked_entry(id, expected)?;
        entry.activity.check_open()?;
        let activity = Arc::clone(&entry.activity);
        // Admission and count share one atomic word, so a parent close cannot
        // pass its sole-user test between a child's check and reservation.
        for (reserved, ancestor) in activity.reserved().enumerate() {
            if let Err(error) = ancestor.reserve() {
                for previous in activity.reserved().take(reserved) {
                    previous.release();
                }
                drop(state);
                return Err(error);
            }
        }
        if let Err(error) = activity.check_open() {
            for ancestor in activity.reserved() {
                ancestor.release();
            }
            drop(state);
            return Err(error);
        }
        let lease = ActivityLease { activity };
        let value = match Arc::clone(&entry.value).downcast::<T>() {
            Ok(value) => value,
            Err(value) => {
                drop(state);
                drop(value);
                return Err(RegistryError::WrongType);
            }
        };
        drop(state);
        Ok(HandleRef {
            id,
            kind: expected,
            value,
            lease,
        })
    }

    #[cfg(test)]
    pub(crate) fn kind(&self, id: HandleId) -> Result<Option<HandleType>, RegistryError> {
        let Some(slot) = self.slots.get(id.index()) else {
            return Ok(None);
        };
        let state = slot.read();
        Ok(state
            .entry
            .as_ref()
            .filter(|_| state.generation == id.generation())
            .map(|entry| entry.kind))
    }

    /// Diagnostic access ignores closing admission and activity limits, but
    /// never revives a retired handle or an entry with a retired ancestor.
    pub(super) fn diagnostics<T: Any + Send + Sync>(
        &self,
        id: HandleId,
        expected: HandleType,
    ) -> Result<Arc<T>, RegistryError> {
        let value = {
            let slot = self.slots.get(id.index()).ok_or(RegistryError::NotFound)?;
            let state = slot.read();
            let entry = state.checked_entry(id, expected)?;
            if entry
                .activity
                .ancestors()
                .any(|ancestor| ancestor.admission() == RETIRED)
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
            let slot = self.slots.get(id.index()).ok_or(RegistryError::NotFound)?;
            let state = slot.read();
            let entry = state.checked_entry(id, expected)?;
            Arc::clone(&entry.value)
        };
        value.downcast().map_err(|_| RegistryError::WrongType)
    }

    pub(crate) fn begin_close<T: Any + Send + Sync>(
        &self,
        handle: &HandleRef<T>,
    ) -> Result<CloseGuard, RegistryError> {
        let slot = self
            .slots
            .get(handle.id().index())
            .ok_or(RegistryError::NotFound)?;
        let state = slot.read();
        let entry = state.checked_entry(handle.id(), handle.kind)?;
        if !Arc::ptr_eq(&entry.activity, &handle.lease.activity)
            || !entry
                .value
                .downcast_ref::<T>()
                .is_some_and(|value| std::ptr::eq(value, handle.value.as_ref()))
        {
            return Err(RegistryError::OwnershipMismatch);
        }
        entry.activity.check_open()?;
        entry
            .activity
            .state
            .compare_exchange(1, CLOSING | 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|state| {
                if state & ADMISSION_MASK == RETIRED {
                    RegistryError::NotFound
                } else {
                    RegistryError::Busy
                }
            })?;
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
            let slot = self.slots.get(id.index()).ok_or(RegistryError::NotFound)?;
            let mut slot = slot.write();
            slot.checked_entry(id, expected)?;
            let removed = slot.entry.take().ok_or(RegistryError::NotFound)?;
            removed.activity.retire();
            slot.next_free = state.free_head;
            state.free_head = Some(id.index());
            state.live -= 1;
            removed
        };
        drop(removed);
        Ok(())
    }

    /// Preflights the entire batch before removing anything. No payload
    /// destructor runs while allocation metadata or any slot lock is held.
    pub(crate) fn retire_batch(
        &self,
        handles: &[(HandleId, HandleType)],
    ) -> Result<(), RegistryError> {
        let mut removed = Vec::new();
        removed
            .try_reserve(handles.len())
            .map_err(|_| RegistryError::Capacity)?;
        let mut locked = Vec::new();
        locked
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
            if handles
                .iter()
                .take(index)
                .any(|(previous, _)| previous.index() == id.index())
            {
                return Err(RegistryError::NotFound);
            }
            let slot = self.slots.get(id.index()).ok_or(RegistryError::NotFound)?;
            let slot = slot.write();
            slot.checked_entry(*id, *expected)?;
            locked.push(slot);
        }
        for ((id, _), slot) in handles.iter().zip(&mut locked) {
            if let Some(entry) = slot.entry.take() {
                entry.activity.retire();
                slot.next_free = state.free_head;
                state.free_head = Some(id.index());
                state.live -= 1;
                removed.push(entry);
            }
        }
        drop(locked);
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
        fn with_id_limit(limit: usize) -> Self {
            assert!(limit > 0 && limit <= SLOT_CAPACITY);
            let registry = Self::new();
            {
                let mut state = registry.write();
                state.slot_limit = limit;
                state.generation_limit = 7;
            }
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
        activity.state.load(Ordering::Acquire) & ACTIVITY_MAX
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
    fn packed_ids_round_trip_at_slot_and_generation_boundaries() {
        for index in [0, 1, 255, 256, SLOT_CAPACITY - 1] {
            for generation in [1, 2, GENERATION_MASK] {
                let id = HandleId::from_parts(index, generation).unwrap();
                assert_eq!(id.index(), index);
                assert_eq!(id.generation(), generation);
                assert_eq!(HandleId::from_raw(id.to_raw()), Ok(id));
            }
        }
        assert_eq!(HandleId::from_parts(0, 0), Err(RegistryError::InvalidId));
        assert_eq!(
            HandleId::from_parts(SLOT_CAPACITY, 1),
            Err(RegistryError::InvalidId)
        );
        assert_eq!(
            HandleId::from_parts(0, GENERATION_MASK + 1),
            Err(RegistryError::InvalidId)
        );
    }

    #[test]
    fn slot_addresses_remain_stable_across_segment_boundaries() {
        let table = SlotTable::new();
        let first = std::ptr::from_ref(table.get_or_create(0).unwrap());
        for index in [1, 255, 256, 65535, 65536, SLOT_CAPACITY - 1] {
            let slot = table.get_or_create(index).unwrap();
            assert_eq!(std::ptr::from_ref(slot).addr() % 128, 0);
            assert_eq!(std::ptr::from_ref(table.get(0).unwrap()), first);
        }
        assert!(table.get(257).is_some());
        assert!(table.get(1 << 17).is_none());
    }

    #[test]
    fn activity_words_do_not_share_cache_lines_between_allocations() {
        let activities: Vec<_> = (0..32).map(|_| HandleActivity::new(None)).collect();
        let mut lines = std::collections::HashSet::new();
        for activity in activities {
            assert_eq!(std::ptr::from_ref(&*activity).addr() % 128, 0);
            assert!(lines.insert(std::ptr::from_ref(&activity.state).addr() / 128));
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
            assert_eq!(registry.write().live, 0);
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
    fn last_packed_identity_is_valid_and_generation_wrap_recovers_capacity() {
        let registry = HandleRegistry::new();
        registry.write().next_index = SLOT_CAPACITY - 1;
        registry
            .slots
            .get_or_create(SLOT_CAPACITY - 1)
            .unwrap()
            .write()
            .generation = GENERATION_MASK - 1;
        let (last, _) = register(&registry, HandleType::Env, None);
        assert_eq!(last.to_raw().addr(), usize::MAX);
        assert_eq!(HandleId::from_raw(last.to_raw()), Ok(last));
        registry.retire(last, HandleType::Env).unwrap();
        let (wrapped, _) = register(&registry, HandleType::Env, None);
        assert_eq!(wrapped.index(), last.index());
        assert_eq!(wrapped.generation(), 1);
        assert_eq!(
            registry.acquire::<u32>(last, HandleType::Env).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            *registry.acquire::<u32>(wrapped, HandleType::Env).unwrap(),
            42
        );
        registry.retire(wrapped, HandleType::Env).unwrap();
        assert_eq!(registry.write().live, 0);
        let (next, _) = register(&registry, HandleType::Env, None);
        assert_eq!(next.index(), last.index());
        assert_eq!(next.generation(), 2);
    }

    #[test]
    fn wrap_skips_live_ids_and_reuses_only_retired_entries() {
        let registry = HandleRegistry::with_id_limit(3);
        registry.write().generation_limit = 3;
        let (first, _) = register(&registry, HandleType::Env, None);
        let (second, _) = register(&registry, HandleType::Env, None);
        let (third, _) = register(&registry, HandleType::Env, None);
        let mut current = second;
        for generation in [2, 3, 1] {
            registry.retire(current, HandleType::Env).unwrap();
            assert_eq!(registry.kind(current), Ok(None));
            let (recycled, _) = register(&registry, HandleType::Env, None);
            assert_eq!(recycled.index(), second.index());
            assert_eq!(recycled.generation(), generation);
            assert_live_entries(&registry, &[(first, 42), (recycled, 42), (third, 42)]);
            current = recycled;
        }
        assert_eq!(current, second);
    }

    fn assert_live_entries(registry: &HandleRegistry, live: &[(HandleId, u32)]) {
        let ids: std::collections::HashSet<_> = live.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), live.len());
        assert_eq!(registry.write().live, live.len());
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
            let candidate = registry.write().next_index;
            for _ in 0..2 {
                assert_eq!(
                    registry.register(HandleType::Env, Arc::new(42_u32), Arc::clone(&activity)),
                    Err(RegistryError::IdSpaceFull)
                );
                assert_eq!(registry.write().next_index, candidate);
                assert!(!activity.registered.load(Ordering::Acquire));
                assert!(activity.owner.get().is_none());
                assert_eq!(count(&activity), 0);
                assert_eq!(activity.admission(), OPEN);
                assert_live_entries(&registry, &live);
            }
            let (retired, _) = live.remove(limit / 2);
            registry.retire(retired, HandleType::Env).unwrap();
            assert_live_entries(&registry, &live);
            let recycled = registry
                .register(HandleType::Env, Arc::new(42_u32), Arc::clone(&activity))
                .unwrap();
            assert_eq!(recycled.index(), retired.index());
            assert_ne!(recycled, retired);
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
                assert!(id.index() < limit);
                assert!((1..=7).contains(&id.generation()));
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
            registry.force_wrap_for_test();
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
            assert_eq!(new_activity.admission(), OPEN);
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
            assert_eq!(old_activity.admission(), RETIRED);
            assert_eq!(count(&new_activity), 1);
            assert_eq!(new_activity.admission(), CLOSING);
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
        registry.force_wrap_for_test();
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
        let (_, root) = register(&registry, HandleType::Env, None);
        let (parent, parent_activity) =
            register(&registry, HandleType::Dbc, Some(Arc::clone(&root)));
        let (child, child_activity) = register(
            &registry,
            HandleType::Stmt,
            Some(Arc::clone(&parent_activity)),
        );
        let handle = registry.acquire::<u32>(child, HandleType::Stmt).unwrap();
        assert_eq!(count(&root), 0);
        assert_eq!(count(&parent_activity), 1);
        assert_eq!(count(&child_activity), 1);
        let structural = handle.into_arc();
        assert_eq!(count(&parent_activity), 0);
        assert_eq!(count(&child_activity), 0);
        let parent_ref = registry.acquire::<u32>(parent, HandleType::Dbc).unwrap();
        let closing = registry.begin_close(&parent_ref).unwrap();
        registry
            .retire_batch(&[(parent, HandleType::Dbc), (child, HandleType::Stmt)])
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
    fn closing_a_parent_rejects_a_partially_reserved_child() {
        let tree = Tree::new();
        let parent = tree.acquire(tree.dbc, HandleType::Dbc);
        tree.stmt_activity.reserve().unwrap();
        let closing = tree.registry.begin_close(&parent).unwrap();
        assert_eq!(tree.dbc_activity.reserve(), Err(RegistryError::Busy));
        tree.stmt_activity.release();
        assert_eq!(count(&tree.stmt_activity), 0);
        assert_eq!(count(&tree.dbc_activity), 1);
        drop(closing);
        drop(tree.acquire(tree.stmt, HandleType::Stmt));
    }

    #[test]
    fn cross_slot_acquisition_and_parent_close_cannot_both_succeed() {
        let tree = Tree::new();
        let parent = tree.acquire(tree.dbc, HandleType::Dbc);
        for _ in 0..128 {
            let start = Barrier::new(3);
            let (acquired, closed) = thread::scope(|scope| {
                let acquire = scope.spawn(|| {
                    start.wait();
                    tree.registry.acquire::<u32>(tree.stmt, HandleType::Stmt)
                });
                let close = scope.spawn(|| {
                    start.wait();
                    tree.registry.begin_close(&parent)
                });
                start.wait();
                (acquire.join().unwrap(), close.join().unwrap())
            });
            match (&acquired, &closed) {
                (Ok(_), Err(RegistryError::Busy)) => {}
                (Err(RegistryError::Busy), Ok(_)) => {}
                _ => panic!("acquire and close must have exactly one admitted caller"),
            }
            drop(acquired);
            drop(closed);
            assert_eq!(count(&tree.stmt_activity), 0);
            assert_eq!(count(&tree.dbc_activity), 1);
            assert_eq!(tree.dbc_activity.admission(), OPEN);
        }
    }

    #[test]
    fn implicit_descriptor_call_blocks_statement_and_connection_close() {
        let tree = Tree::new();
        let descriptor = tree.acquire(tree.implicit_desc, HandleType::Desc);
        assert_eq!(count(&tree.stmt_activity), 1);
        assert_eq!(count(&tree.env_activity), 0);
        for (id, kind) in [(tree.stmt, HandleType::Stmt), (tree.dbc, HandleType::Dbc)] {
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
    fn explicit_descriptor_call_blocks_connection_but_not_statement_or_environment() {
        let tree = Tree::new();
        let descriptor = tree.acquire(tree.explicit_desc, HandleType::Desc);
        assert_eq!(count(&tree.stmt_activity), 0);
        assert_eq!(count(&tree.dbc_activity), 1);
        assert_eq!(count(&tree.env_activity), 0);
        let connection = tree.acquire(tree.dbc, HandleType::Dbc);
        assert_eq!(
            tree.registry.begin_close(&connection).err(),
            Some(RegistryError::Busy)
        );
        drop(connection);
        let stmt = tree.acquire(tree.stmt, HandleType::Stmt);
        drop(tree.registry.begin_close(&stmt).unwrap());
        drop(descriptor);
        assert_eq!(count(&tree.dbc_activity), 1);
        drop(stmt);
        assert_eq!(count(&tree.dbc_activity), 0);
    }

    #[test]
    fn in_flight_statement_call_blocks_connection_close_but_not_environment_close() {
        let tree = Tree::new();
        let statement = tree.acquire(tree.stmt, HandleType::Stmt);
        let connection = tree.acquire(tree.dbc, HandleType::Dbc);
        assert_eq!(
            tree.registry.begin_close(&connection).err(),
            Some(RegistryError::Busy)
        );
        assert_eq!(count(&tree.env_activity), 0);

        let environment = tree.acquire(tree.env, HandleType::Env);
        let another_env_call = tree.acquire(tree.env, HandleType::Env);
        assert_eq!(
            tree.registry.begin_close(&environment).err(),
            Some(RegistryError::Busy)
        );
        drop(another_env_call);
        let closing = tree.registry.begin_close(&environment).unwrap();
        assert_eq!(count(&tree.env_activity), 1);
        assert_eq!(
            tree.registry
                .acquire::<u32>(tree.stmt, HandleType::Stmt)
                .err(),
            Some(RegistryError::Busy)
        );
        assert_eq!(*statement, 42);
        drop(closing);
        drop(tree.acquire(tree.stmt, HandleType::Stmt));
        tree.registry.retire(tree.env, HandleType::Env).unwrap();
        assert_eq!(
            tree.registry
                .acquire::<u32>(tree.stmt, HandleType::Stmt)
                .err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(*statement, 42);
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
        assert_eq!(tree.dbc_activity.admission(), RETIRED);
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
        for saturated in [&tree.dbc_activity, &tree.stmt_activity] {
            saturated.set_count_for_test(ACTIVITY_MAX);
            assert_eq!(
                tree.registry
                    .acquire::<u32>(tree.implicit_desc, HandleType::Desc)
                    .err(),
                Some(RegistryError::ActivityOverflow)
            );
            for activity in [&tree.dbc_activity, &tree.stmt_activity] {
                assert_eq!(
                    count(activity),
                    if Arc::ptr_eq(activity, saturated) {
                        ACTIVITY_MAX
                    } else {
                        0
                    }
                );
            }
            assert_eq!(count(&tree.env_activity), 0);
            let state = tree
                .registry
                .slots
                .get(tree.implicit_desc.index())
                .unwrap()
                .read();
            assert_eq!(count(&state.entry.as_ref().unwrap().activity), 0);
            saturated.set_count_for_test(0);
        }
        drop(tree.acquire(tree.implicit_desc, HandleType::Desc));
        assert_eq!(count(&tree.env_activity), 0);
    }

    #[test]
    fn maximum_nonoverflowing_count_can_be_acquired_and_released() {
        let registry = HandleRegistry::new();
        let (id, activity) = register(&registry, HandleType::Env, None);
        activity.set_count_for_test(ACTIVITY_MAX - 1);
        let handle = registry.acquire::<u32>(id, HandleType::Env).unwrap();
        assert_eq!(count(&activity), ACTIVITY_MAX);
        assert_eq!(
            registry.acquire::<u32>(id, HandleType::Env).err(),
            Some(RegistryError::ActivityOverflow)
        );
        drop(handle);
        assert_eq!(count(&activity), ACTIVITY_MAX - 1);
        activity.set_count_for_test(0);
    }

    #[test]
    fn concurrent_readers_cannot_overflow_a_shared_ancestor() {
        let registry = Arc::new(HandleRegistry::new());
        let (_, root) = register(&registry, HandleType::Env, None);
        let (_, parent) = register(&registry, HandleType::Dbc, Some(Arc::clone(&root)));
        let children: Vec<_> = (0..32)
            .map(|_| register(&registry, HandleType::Stmt, Some(Arc::clone(&parent))))
            .collect();
        parent.set_count_for_test(ACTIVITY_MAX - 1);

        let state = registry.write();
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
                        .send(registry.acquire::<u32>(id, HandleType::Stmt))
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
        assert_eq!(count(&root), 0);
        assert_eq!(count(&parent), ACTIVITY_MAX);
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
        assert_eq!(count(&parent), ACTIVITY_MAX - 1);
        assert!(children.iter().all(|(_, activity)| count(activity) == 0));
        parent.set_count_for_test(0);
    }

    #[test]
    fn failed_reservation_does_not_consume_identity_bind_activity_or_insert() {
        for reuse in [false, true] {
            let registry = HandleRegistry::new();
            if reuse {
                let (retired, _) = register(&registry, HandleType::Env, None);
                registry.retire(retired, HandleType::Env).unwrap();
            }
            let before = {
                let state = registry.write();
                (state.next_index, state.free_head, state.live)
            };
            let activity = HandleActivity::new(None);
            registry.write().reserve_additional = usize::MAX;
            assert_eq!(
                registry.register(HandleType::Env, Arc::new(42_u32), Arc::clone(&activity)),
                Err(RegistryError::Capacity)
            );
            assert_live_entries(&registry, &[]);
            let state = registry.write();
            assert_eq!((state.next_index, state.free_head, state.live), before);
            drop(state);
            assert!(!activity.registered.load(Ordering::Acquire));
            assert!(activity.owner.get().is_none());
            assert_eq!(count(&activity), 0);
            registry.write().reserve_additional = 1;
            let id = registry
                .register(HandleType::Env, Arc::new(42_u32), activity)
                .unwrap();
            assert_eq!(id.index(), 0);
            assert_eq!(id.generation(), if reuse { 2 } else { 1 });
            assert_live_entries(&registry, &[(id, 42)]);
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
        assert_eq!(first.write().live, 0);
        assert_eq!(second.write().live, 0);
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
            assert_eq!(activity.admission(), RETIRED);
            assert_eq!(
                registry.register(HandleType::Desc, Arc::new(42_u32), activity),
                Err(RegistryError::InvalidActivity)
            );
        }
        assert!(!parent.registered.load(Ordering::Acquire));
        assert_eq!(registry.write().live, 0);
        let (child, _) = register(&registry, HandleType::Desc, Some(Arc::clone(&parent)));
        let parent_id = registry
            .register(HandleType::Stmt, Arc::new(42_u32), Arc::clone(&parent))
            .unwrap();
        assert_ne!(parent_id, child);
        assert_eq!(parent_id.index(), 1);
        assert_eq!(parent_id.generation(), 2);
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
                assert_eq!(registry.write().live, 0);
                assert_eq!(registry.write().next_index, 0);
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
        let state = registry.write();
        assert_eq!(state.live, 1);
        assert_eq!(state.next_index, 1);
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
        assert_eq!(second_activity.admission(), OPEN);

        let tree = Tree::new();
        let child = tree.acquire(tree.stmt, HandleType::Stmt);
        tree.registry.retire(tree.dbc, HandleType::Dbc).unwrap();
        assert_eq!(
            tree.registry.begin_close(&child).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(count(&tree.stmt_activity), 1);
        assert_eq!(tree.stmt_activity.admission(), OPEN);
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
            assert_eq!(first_activity.admission(), OPEN);
            assert_eq!(second_activity.admission(), OPEN);
            assert_eq!(count(&first_activity), 0);
            assert_eq!(count(&second_activity), 0);
        }
        registry.retire_batch(&[]).unwrap();
        registry
            .retire_batch(&[(first, HandleType::Env), (second, HandleType::Dbc)])
            .unwrap();
        assert_eq!(registry.write().live, 0);
        assert_eq!(first_activity.admission(), RETIRED);
        assert_eq!(second_activity.admission(), RETIRED);
    }

    #[test]
    fn batch_with_two_generations_of_one_slot_does_not_retire_the_live_entry() {
        let registry = HandleRegistry::new();
        let (old, _) = register(&registry, HandleType::Env, None);
        registry.retire(old, HandleType::Env).unwrap();
        let (current, activity) = register(&registry, HandleType::Env, None);
        assert_eq!(old.index(), current.index());
        assert_ne!(old.generation(), current.generation());
        for ids in [[current, old], [old, current]] {
            assert_eq!(
                registry.retire_batch(&ids.map(|id| (id, HandleType::Env))),
                Err(RegistryError::NotFound)
            );
            assert_eq!(registry.kind(current), Ok(Some(HandleType::Env)));
            assert_eq!(activity.admission(), OPEN);
            assert_eq!(registry.write().live, 1);
        }
    }

    #[test]
    fn slot_poison_recovery_preserves_generation_and_retirement_checks() {
        let registry = HandleRegistry::new();
        let (old, _) = register(&registry, HandleType::Env, None);
        let slot = registry.slots.get(old.index()).unwrap();
        slot.poison_for_test();
        assert_eq!(*registry.acquire::<u32>(old, HandleType::Env).unwrap(), 42);
        slot.poison_for_test();
        assert_eq!(
            *registry.diagnostics::<u32>(old, HandleType::Env).unwrap(),
            42
        );
        slot.poison_for_test();
        registry.retire(old, HandleType::Env).unwrap();
        let (current, _) = register(&registry, HandleType::Env, None);
        slot.poison_for_test();
        assert_eq!(
            registry.acquire::<u32>(old, HandleType::Env).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            *registry.acquire::<u32>(current, HandleType::Env).unwrap(),
            42
        );
    }

    fn poison(registry: &Arc<HandleRegistry>) {
        let registry = Arc::clone(registry);
        assert!(
            thread::spawn(move || {
                let _guard = registry.state.lock().unwrap();
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
        assert!(registry.state.is_poisoned());
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
        assert!(!registry.state.is_poisoned());
        registry.retire(other, HandleType::Env).unwrap();
        drop(closing);
        assert_eq!(activity.admission(), OPEN);
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
            (tree.dbc, HandleType::Dbc, &tree.dbc_activity),
            (tree.stmt, HandleType::Stmt, &tree.stmt_activity),
        ];
        let mut blocked_results = Vec::new();
        for (id, kind, activity) in ancestors {
            let ancestor = tree.acquire(id, kind);
            blocked_results.push((tree.registry.begin_close(&ancestor).err(), count(activity)));
        }
        assert_eq!(count(&tree.env_activity), 0);
        let env = tree.acquire(tree.env, HandleType::Env);
        assert!(tree.registry.begin_close(&env).is_ok());
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
                SLOT_CAPACITY
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
                    registry.write().reserve_additional = usize::MAX;
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

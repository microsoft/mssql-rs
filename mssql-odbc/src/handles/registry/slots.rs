// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{RegistryEntry, RegistryError};

const PAGE_BITS: u32 = 8;
const PAGE_LEN: usize = 1 << PAGE_BITS;
const PAGE_MASK: usize = PAGE_LEN - 1;

type Segment = Box<[Slot]>;
type Directory = Box<[OnceLock<Segment>]>;
type Group = Box<[OnceLock<Directory>]>;

pub(super) struct SlotTable {
    groups: [OnceLock<Group>; PAGE_LEN],
}

impl SlotTable {
    pub(super) fn new() -> Self {
        Self {
            groups: [const { OnceLock::new() }; PAGE_LEN],
        }
    }

    pub(super) fn get(&self, index: usize) -> Option<&Slot> {
        self.groups
            .get(index >> (3 * PAGE_BITS))?
            .get()?
            .get((index >> (2 * PAGE_BITS)) & PAGE_MASK)?
            .get()?
            .get((index >> PAGE_BITS) & PAGE_MASK)?
            .get()?
            .get(index & PAGE_MASK)
    }

    // Only the registry's mutation lock may publish pages. Once published,
    // their addresses remain stable for the lifetime of this table.
    pub(super) fn get_or_create(&self, index: usize) -> Result<&Slot, RegistryError> {
        let root = self
            .groups
            .get(index >> (3 * PAGE_BITS))
            .ok_or(RegistryError::IdSpaceFull)?;
        let group = initialize(root, OnceLock::new)?;
        let directory = group
            .get((index >> (2 * PAGE_BITS)) & PAGE_MASK)
            .ok_or(RegistryError::NotFound)?;
        let directory = initialize(directory, OnceLock::new)?;
        let segment = directory
            .get((index >> PAGE_BITS) & PAGE_MASK)
            .ok_or(RegistryError::NotFound)?;
        initialize(segment, Slot::new)?
            .get(index & PAGE_MASK)
            .ok_or(RegistryError::NotFound)
    }
}

fn initialize<T>(cell: &OnceLock<Box<[T]>>, new: fn() -> T) -> Result<&[T], RegistryError> {
    if cell.get().is_none() {
        let mut values = Vec::new();
        values
            .try_reserve_exact(PAGE_LEN)
            .map_err(|_| RegistryError::Capacity)?;
        values.resize_with(PAGE_LEN, new);
        // All page elements are empty metadata; a losing initialization cannot
        // destroy a handle payload. Publication is otherwise writer-serialized.
        let _ = cell.set(values.into_boxed_slice());
    }
    cell.get().map(Box::as_ref).ok_or(RegistryError::Capacity)
}

#[repr(align(128))]
pub(super) struct Slot {
    state: RwLock<SlotState>,
}

pub(super) struct SlotState {
    pub(super) generation: usize,
    pub(super) entry: Option<RegistryEntry>,
    pub(super) next_free: Option<usize>,
}

impl Slot {
    fn new() -> Self {
        Self {
            state: RwLock::new(SlotState {
                generation: 0,
                entry: None,
                next_free: None,
            }),
        }
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, SlotState> {
        self.state.read().unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned handle slot");
            self.state.clear_poison();
            poisoned.into_inner()
        })
    }

    pub(super) fn write(&self) -> RwLockWriteGuard<'_, SlotState> {
        self.state.write().unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned handle slot");
            self.state.clear_poison();
            poisoned.into_inner()
        })
    }

    #[cfg(test)]
    pub(super) fn poison_for_test(&self) {
        let _ = std::panic::catch_unwind(|| {
            let _state = self.state.write().unwrap();
            panic!("poison slot metadata without changing it");
        });
    }
}

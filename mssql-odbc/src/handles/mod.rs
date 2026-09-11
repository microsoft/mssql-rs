// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod bindings;
pub(crate) mod dbc;
pub(crate) mod desc;
mod env;
mod registry;
pub(crate) mod stmt;

pub(crate) use dbc::DbcHandle;
pub(crate) use desc::DescHandle;
pub(crate) use env::{EnvHandle, OdbcVersion, process_is_shutting_down};
pub(crate) use registry::{CloseGuard, HandleActivity, HandleId, HandleRef, RegistryError};
pub(crate) use stmt::StmtHandle;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn};
use registry::HandleRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum HandleType {
    Env = 1,
    Dbc = 2,
    Stmt = 3,
    Desc = 4,
    Invalid = 0xDEADBEEF,
}

static HANDLES: LazyLock<HandleRegistry> = LazyLock::new(HandleRegistry::new);
static LIVE_ENV_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Only handle identity crosses the ABI. Allocation ownership stays in Rust.
pub(crate) trait Handle: Send + Sync + 'static {
    const TYPE: HandleType;

    fn activity(&self) -> &Arc<HandleActivity>;

    fn implicit_ids(&self) -> Option<[SqlHandle; 4]> {
        None
    }
}

impl RegistryError {
    pub(crate) fn sql_return(&self) -> SqlReturn {
        match self {
            Self::InvalidId | Self::NotFound | Self::WrongType => SQL_INVALID_HANDLE,
            _ => SQL_ERROR,
        }
    }
}

pub(crate) fn handle_to_raw<T: Handle>(handle: Arc<T>) -> Result<SqlHandle, RegistryError> {
    let activity = Arc::clone(handle.activity());
    let id = HANDLES.register(T::TYPE, handle, activity)?;
    if T::TYPE == HandleType::Env {
        LIVE_ENV_COUNT.fetch_add(1, Ordering::Release);
    }
    Ok(id.to_raw())
}

/// Pins the allocation and its ancestor activity until the returned guard drops.
pub(crate) fn handle_from_raw<T: Handle>(raw: SqlHandle) -> Result<HandleRef<T>, RegistryError> {
    HANDLES.acquire(HandleId::from_raw(raw)?, T::TYPE)
}

pub(crate) fn begin_close<T: Handle>(handle: &HandleRef<T>) -> Result<CloseGuard, RegistryError> {
    HANDLES.begin_close(handle)
}

/// Retires implicit descriptor IDs with their statement, not with its last Arc.
pub(crate) fn retire_handle<T: Handle>(handle: &T, raw: SqlHandle) -> Result<(), RegistryError> {
    let id = HandleId::from_raw(raw)?;
    if let Some(children) = handle.implicit_ids() {
        let mut ids = [(id, T::TYPE); 5];
        for (slot, child) in ids.iter_mut().skip(1).zip(children) {
            *slot = (HandleId::from_raw(child)?, HandleType::Desc);
        }
        HANDLES.retire_batch(&ids)?;
    } else {
        HANDLES.retire(id, T::TYPE)?;
    }
    if T::TYPE == HandleType::Env {
        LIVE_ENV_COUNT.fetch_sub(1, Ordering::Release);
    }
    Ok(())
}

pub(crate) fn live_env_count() -> usize {
    LIVE_ENV_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn is_live(raw: SqlHandle) -> bool {
    HandleId::from_raw(raw)
        .and_then(|id| HANDLES.kind(id))
        .is_ok_and(|kind| kind.is_some())
}

/// Test-only forced retirement for exercising an acquired-before-free ordering.
#[cfg(test)]
pub(crate) fn free_handle<T: Handle>(raw: SqlHandle) -> Result<(), RegistryError> {
    let handle = handle_from_raw::<T>(raw)?;
    retire_handle(&*handle, raw)
}

macro_rules! get_handle {
    ($ty:ty, $raw:expr) => {{
        match $crate::handles::handle_from_raw::<$ty>($raw) {
            Ok(handle) => handle,
            Err(error) => {
                ::tracing::error!(?error, "ODBC handle acquisition failed");
                return error.sql_return();
            }
        }
    }};
}
pub(crate) use get_handle;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{SQL_ERROR, SQL_HANDLE_DESC, SQL_HANDLE_STMT, SQL_SUCCESS};
    use crate::test_support::TestHandles;

    #[test]
    fn retired_handle_does_not_identify_its_replacement() {
        let mut h = TestHandles::with_env_dbc();
        let old = h.alloc_extra_stmt();
        assert_eq!(h.free_extra_stmt(old), SQL_SUCCESS);
        let replacement = h.alloc_extra_stmt();
        assert_ne!(old, replacement);
        assert!(!is_live(old));
        assert!(is_live(replacement));
        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_STMT, old) },
            SQL_SUCCESS
        );
        assert!(is_live(replacement));
    }

    #[test]
    fn statement_retirement_retires_all_implicit_descriptors() {
        let mut h = TestHandles::with_env_dbc();
        let raw = h.alloc_extra_stmt();
        let stmt = handle_from_raw::<StmtHandle>(raw).unwrap().into_arc();
        let descriptors = [stmt.ard, stmt.apd, stmt.ird, stmt.ipd];
        assert_eq!(h.free_extra_stmt(raw), SQL_SUCCESS);
        assert!(!is_live(raw));
        for desc in descriptors {
            assert!(!is_live(desc));
        }
        assert_eq!(stmt.implicit_ids(), Some(descriptors));
    }

    #[test]
    fn active_implicit_descriptor_prevents_statement_free() {
        let mut h = TestHandles::with_env_dbc();
        let raw = h.alloc_extra_stmt();
        let stmt = handle_from_raw::<StmtHandle>(raw).unwrap().into_arc();
        let descriptor_use = handle_from_raw::<DescHandle>(stmt.ard).unwrap();
        assert_eq!(h.free_extra_stmt(raw), SQL_ERROR);
        assert!(is_live(raw));
        drop(descriptor_use);
        assert_eq!(h.free_extra_stmt(raw), SQL_SUCCESS);
    }

    #[test]
    fn acquired_descriptor_can_outlive_forced_retirement() {
        let mut h = TestHandles::with_env_dbc();
        let raw = h.alloc_explicit_desc();
        let desc = handle_from_raw::<DescHandle>(raw).unwrap();
        free_handle::<DescHandle>(raw).unwrap();
        assert!(!is_live(raw));
        assert!(desc.is_explicit());
        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_DESC, raw) },
            SQL_SUCCESS
        );
        let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap();
        dbc.inner
            .lock()
            .unwrap()
            .descriptors
            .retain(|&id| id != raw);
    }
}

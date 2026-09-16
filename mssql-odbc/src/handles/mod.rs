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
use std::sync::{Arc, LazyLock, Mutex};

use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn};
use crate::error::{DiagRecord, HasDiagnostics};
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

/// Only the current handle ID crosses the ABI. Allocation ownership stays in Rust.
pub(crate) trait Handle: Send + Sync + 'static {
    const TYPE: HandleType;
    type State: HasDiagnostics;

    fn activity(&self) -> &Arc<HandleActivity>;

    fn state(&self) -> &Mutex<Self::State>;

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

    pub(crate) fn post(&self, state: &mut impl crate::error::HasDiagnostics) {
        use crate::api::sqlstate::{
            ERR_FUNCTION_SEQUENCE, ERR_MEMORY_ALLOCATION, SQLSTATE_HY000, post_diag,
        };
        match self {
            Self::Busy => post_diag(state, ERR_FUNCTION_SEQUENCE),
            Self::IdSpaceFull | Self::Capacity | Self::ActivityOverflow => {
                post_diag(state, ERR_MEMORY_ALLOCATION);
            }
            _ => crate::error::post_sql_error(state, SQLSTATE_HY000, 0, self.to_string()),
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

/// Deliberately has no Deref: bypassing admission permits diagnostics, not work.
pub(crate) struct HandleDiagnostics<T: Handle> {
    handle: Arc<T>,
}

impl<T: Handle> HandleDiagnostics<T> {
    pub(crate) fn with_records<R>(&self, f: impl FnOnce(&[DiagRecord]) -> R) -> R {
        crate::error::diag::with_diagnostics(self.handle.state(), |records| f(records))
    }

    fn post(&self, error: RegistryError) {
        crate::error::diag::with_diagnostics(self.handle.state(), |records| {
            crate::error::free_errors(records);
            error.post(records);
        });
    }
}

pub(crate) fn diagnostics_from_raw<T: Handle>(
    raw: SqlHandle,
) -> Result<HandleDiagnostics<T>, RegistryError> {
    HANDLES
        .diagnostics(HandleId::from_raw(raw)?, T::TYPE)
        .map(|handle| HandleDiagnostics { handle })
}

/// SQL_INVALID_HANDLE never posts: null, missing and wrong-type inputs have no
/// valid diagnostic target. A concurrently retired target stays invalid.
pub(crate) fn report_handle_error<T: Handle>(raw: SqlHandle, error: RegistryError) -> SqlReturn {
    if error.sql_return() == SQL_INVALID_HANDLE {
        return SQL_INVALID_HANDLE;
    }
    match diagnostics_from_raw::<T>(raw) {
        Ok(diagnostics) => {
            diagnostics.post(error);
            error.sql_return()
        }
        Err(lookup_error) => {
            tracing::error!(?lookup_error, "cannot access rejected handle diagnostics");
            lookup_error.sql_return()
        }
    }
}

pub(crate) fn post_handle_error<T: Handle>(handle: &HandleRef<T>, error: RegistryError) {
    HandleDiagnostics {
        handle: handle.clone_arc(),
    }
    .post(error);
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
pub(crate) fn probe_slot_install<T: Handle>(index: usize, generation: u64, value: Arc<T>) -> usize {
    let activity = Arc::clone(value.activity());
    registry::HandleRegistry::probe_slot_install(index, generation, T::TYPE, value, activity)
}

#[cfg(test)]
pub(crate) fn probe_slot_lookup<T: Handle>(packed: usize) -> Result<Arc<T>, RegistryError> {
    registry::HandleRegistry::probe_slot_lookup(packed, T::TYPE)
}

#[cfg(test)]
pub(crate) fn probe_lookup<T: Handle>(raw: SqlHandle) -> Result<Arc<T>, RegistryError> {
    HANDLES.probe_lookup(HandleId::from_raw(raw)?, T::TYPE)
}

#[cfg(test)]
pub(crate) fn probe_reserve(activity: &Arc<HandleActivity>) {
    registry::HandleRegistry::probe_reserve(activity);
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
    let handle = HANDLES.inspect_for_test::<T>(HandleId::from_raw(raw)?, T::TYPE)?;
    retire_handle(&*handle, raw)
}

macro_rules! get_handle {
    ($ty:ty, $raw:expr) => {{
        let raw = $raw;
        match $crate::handles::handle_from_raw::<$ty>(raw) {
            Ok(handle) => handle,
            Err(error) => {
                ::tracing::error!(?error, "ODBC handle acquisition failed");
                return $crate::handles::report_handle_error::<$ty>(raw, error);
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

    fn diagnostic_state(raw: SqlHandle, kind: i16) -> [u8; 5] {
        let mut state = [0_u16; 6];
        let mut message = [0_u16; 256];
        assert_eq!(
            unsafe {
                crate::api::SQLGetDiagRecW(
                    kind,
                    raw,
                    1,
                    state.as_mut_ptr(),
                    std::ptr::null_mut(),
                    message.as_mut_ptr(),
                    message.len().try_into().unwrap(),
                    std::ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        std::array::from_fn(|index| u8::try_from(state[index]).unwrap())
    }

    #[test]
    fn rejected_call_diagnostics_remain_readable_while_parent_closes() {
        use crate::api::odbc_types::{
            SQL_DIAG_NUMBER, SQL_DIAG_SQLSTATE, SQL_HANDLE_DBC, SQL_NO_DATA,
        };
        for poisoned in [false, true] {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
            let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap();
            crate::error::post_sql_error(
                &mut stmt.inner.lock().unwrap(),
                *b"HY000",
                0,
                "stale diagnostic",
            );
            if poisoned {
                let stmt = Arc::clone(&stmt);
                assert!(
                    std::thread::spawn(move || {
                        let _state = stmt.inner.lock().unwrap();
                        panic!("poison statement business state");
                    })
                    .join()
                    .is_err()
                );
                HANDLES.poison_for_test();
            }
            let _closing = begin_close(&dbc).unwrap();
            let mut count = -1;
            assert_eq!(
                unsafe { crate::api::SQLNumResultCols(h.stmt, &mut count) },
                SQL_ERROR
            );
            assert_eq!(count, -1);
            assert_eq!(diagnostic_state(h.stmt, SQL_HANDLE_STMT), *b"HY010");
            let mut records = -1_i32;
            assert_eq!(
                unsafe {
                    crate::api::SQLGetDiagFieldW(
                        SQL_HANDLE_STMT,
                        h.stmt,
                        0,
                        SQL_DIAG_NUMBER,
                        (&raw mut records).cast(),
                        0,
                        std::ptr::null_mut(),
                    )
                },
                SQL_SUCCESS
            );
            assert_eq!(records, 1);
            let mut state = [0_u16; 6];
            assert_eq!(
                unsafe {
                    crate::api::SQLGetDiagFieldW(
                        SQL_HANDLE_STMT,
                        h.stmt,
                        1,
                        SQL_DIAG_SQLSTATE,
                        state.as_mut_ptr().cast(),
                        12,
                        std::ptr::null_mut(),
                    )
                },
                SQL_SUCCESS
            );
            assert_eq!(state, [72, 89, 48, 49, 48, 0]);
            assert_eq!(
                unsafe {
                    crate::api::SQLGetDiagRecW(
                        SQL_HANDLE_STMT,
                        h.stmt,
                        2,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                    )
                },
                SQL_NO_DATA
            );
            assert_eq!(
                unsafe {
                    crate::api::SQLGetDiagRecW(
                        SQL_HANDLE_DBC,
                        h.stmt,
                        1,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                    )
                },
                SQL_INVALID_HANDLE
            );
            assert_eq!(stmt.inner.is_poisoned(), poisoned);
            assert!(matches!(
                handle_from_raw::<StmtHandle>(h.stmt),
                Err(RegistryError::Busy)
            ));
        }
    }

    #[test]
    fn activity_overflow_posts_memory_diagnostic_without_needing_another_lease() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let _overflow = stmt.activity.exhaust_for_test();
        let mut count = -1;
        assert_eq!(
            unsafe { crate::api::SQLNumResultCols(h.stmt, &mut count) },
            SQL_ERROR
        );
        assert_eq!(count, -1);
        assert_eq!(diagnostic_state(h.stmt, SQL_HANDLE_STMT), *b"HY001");
    }

    #[test]
    fn invalid_and_wrong_type_calls_do_not_replace_existing_diagnostics() {
        use crate::api::odbc_types::SQL_HANDLE_DBC;
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
        crate::error::post_sql_error(&mut dbc.inner.lock().unwrap(), *b"08003", 0, "existing");
        let mut count = -1;
        assert_eq!(
            unsafe { crate::api::SQLNumResultCols(h.dbc, &mut count) },
            SQL_INVALID_HANDLE
        );
        assert_eq!(
            unsafe { crate::api::SQLNumResultCols(std::ptr::null_mut(), &mut count) },
            SQL_INVALID_HANDLE
        );
        assert_eq!(diagnostic_state(h.dbc, SQL_HANDLE_DBC), *b"08003");
    }

    #[test]
    fn recycled_public_id_does_not_match_a_cached_descriptor_identity() {
        use crate::api::odbc_types::SQL_ATTR_APP_PARAM_DESC;
        use crate::handles::bindings::ParameterBindingKey;
        use crate::handles::stmt::PreparedPlan;
        use mssql_tds::connection::tds_client::{PreparedStatement, StatementId};
        let mut h = TestHandles::with_env_dbc_stmt();
        let old_id = h.alloc_explicit_desc();
        let old = handle_from_raw::<DescHandle>(old_id).unwrap().into_arc();
        let weak_old = Arc::downgrade(&old);
        let ipd = handle_from_raw::<DescHandle>(h.ipd()).unwrap().into_arc();
        let old_key = ParameterBindingKey::new(
            &old,
            &old.inner.lock().unwrap(),
            &ipd,
            &ipd.inner.lock().unwrap(),
        );
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        assert_eq!(
            unsafe { crate::api::SQLSetStmtAttrW(h.stmt, SQL_ATTR_APP_PARAM_DESC, old_id, 0) },
            SQL_SUCCESS
        );
        let prepared_id = StatementId::from_raw_for_test(42);
        {
            let mut state = stmt.inner.lock().unwrap();
            state.refresh_prepared_bindings(old_key.clone());
            state.prepared = Some(PreparedPlan {
                stmt: PreparedStatement::materialized_for_test("SELECT 1", prepared_id),
                marker_count: 0,
                original_sql: String::new(),
            });
        }
        assert_eq!(h.free_explicit_desc(old_id), SQL_SUCCESS);
        drop(old);
        assert!(
            weak_old.upgrade().is_none(),
            "a cached key must not retain descriptor payload"
        );

        HANDLES.force_wrap_for_test();
        let last = h.alloc_explicit_desc();
        assert_eq!(last.addr(), usize::MAX);
        let recycled = h.alloc_explicit_desc();
        assert_eq!(recycled, old_id);
        let new = handle_from_raw::<DescHandle>(recycled).unwrap().into_arc();
        let new_key = ParameterBindingKey::new(
            &new,
            &new.inner.lock().unwrap(),
            &ipd,
            &ipd.inner.lock().unwrap(),
        );
        assert!(!old_key.matches(&new_key));
        assert!(new_key.matches(&new_key));
        assert_eq!(
            unsafe { crate::api::SQLSetStmtAttrW(h.stmt, SQL_ATTR_APP_PARAM_DESC, recycled, 0) },
            SQL_SUCCESS
        );
        let mut state = stmt.inner.lock().unwrap();
        state.refresh_prepared_bindings(new_key);
        assert!(state.prepared.as_ref().unwrap().stmt.id().is_none());
        assert_eq!(state.pending_unprepare, Some(prepared_id));
    }

    #[test]
    fn environment_transaction_reports_a_closing_child_without_relocking_env() {
        use crate::api::odbc_types::{SQL_COMMIT, SQL_HANDLE_ENV};
        let h = TestHandles::with_env_dbc();
        let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap();
        let _closing = begin_close(&dbc).unwrap();
        let (done, result) = std::sync::mpsc::channel();
        let env = h.env.addr();
        let worker = std::thread::spawn(move || {
            let rc = unsafe {
                crate::api::SQLEndTran(
                    SQL_HANDLE_ENV,
                    std::ptr::without_provenance_mut(env),
                    SQL_COMMIT,
                )
            };
            done.send(rc).unwrap();
        });
        assert_eq!(
            result
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            SQL_ERROR
        );
        worker.join().unwrap();
        assert_eq!(diagnostic_state(h.env, SQL_HANDLE_ENV), *b"HY000");
    }

    #[test]
    fn disconnect_retry_does_not_reacquire_already_retired_children() {
        use crate::api::odbc_types::SQL_HANDLE_DBC;
        for successful_retirements in [1, 4] {
            let mut h = TestHandles::with_env_dbc();
            let statements = [
                h.alloc_extra_stmt(),
                h.alloc_extra_stmt(),
                h.alloc_extra_stmt(),
            ];
            let descriptors = [h.alloc_explicit_desc(), h.alloc_explicit_desc()];
            let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
            HANDLES.fail_retirement_after(successful_retirements);
            assert_eq!(unsafe { crate::api::SQLDisconnect(h.dbc) }, SQL_ERROR);
            assert_eq!(diagnostic_state(h.dbc, SQL_HANDLE_DBC), *b"HY001");
            {
                let state = dbc.inner.lock().unwrap();
                assert_eq!(state.connection_state, dbc::ConnectionState::Connected);
                assert_eq!(
                    state.statements.len() + state.descriptors.len(),
                    5 - successful_retirements
                );
                for &raw in state.statements.iter().chain(&state.descriptors) {
                    assert!(is_live(raw), "retry must see only unretired children");
                }
            }
            assert_eq!(unsafe { crate::api::SQLDisconnect(h.dbc) }, SQL_SUCCESS);
            let state = dbc.inner.lock().unwrap();
            assert_eq!(state.connection_state, dbc::ConnectionState::Disconnected);
            assert!(state.statements.is_empty());
            assert!(state.descriptors.is_empty());
            for raw in statements.into_iter().chain(descriptors) {
                assert!(!is_live(raw));
            }
        }
    }

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

    #[test]
    fn disconnect_then_stale_frees_leave_replacements_live() {
        let mut h = TestHandles::with_env_dbc();
        let old_stmt = h.alloc_extra_stmt();
        let old_desc = h.alloc_explicit_desc();
        assert_eq!(unsafe { crate::api::SQLDisconnect(h.dbc) }, SQL_SUCCESS);
        assert!(!is_live(old_stmt));
        assert!(!is_live(old_desc));

        h.mark_dbc_connected();
        let new_stmt = h.alloc_extra_stmt();
        let new_desc = h.alloc_explicit_desc();
        assert_ne!(old_stmt, new_stmt);
        assert_ne!(old_desc, new_desc);
        assert_eq!(h.free_extra_stmt(old_stmt), SQL_SUCCESS);
        assert_eq!(h.free_explicit_desc(old_desc), SQL_SUCCESS);
        assert!(is_live(new_stmt));
        assert!(is_live(new_desc));
    }

    #[test]
    fn disconnect_refuses_an_acquired_child_then_succeeds_after_release() {
        let mut h = TestHandles::with_env_dbc();
        let raw = h.alloc_extra_stmt();
        h.mark_dbc_connected();
        let active = handle_from_raw::<StmtHandle>(raw).unwrap();
        assert_eq!(unsafe { crate::api::SQLDisconnect(h.dbc) }, SQL_ERROR);
        assert!(is_live(raw));
        let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
        assert_eq!(
            dbc.inner.lock().unwrap().diag_records[0].sql_state,
            crate::api::sqlstate::ERR_FUNCTION_SEQUENCE.state
        );
        drop(active);
        assert_eq!(unsafe { crate::api::SQLDisconnect(h.dbc) }, SQL_SUCCESS);
        assert!(!is_live(raw));
    }
}

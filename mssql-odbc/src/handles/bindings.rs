// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Buffer-use lifetimes, separate from registry API-call admission. Admission
//! and binding mutation share the owning DBC lock; dropping a lease never locks.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::dbc::DbcState;
use super::desc::{DescHandle, DescState};
use super::{RegistryError, handle_from_raw};
use crate::api::odbc_types::{SQL_ERROR, SqlHandle, SqlReturn};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_MEMORY_ALLOCATION, SQLSTATE_HY000, post_diag,
};
use crate::error::{HasDiagnostics, post_sql_error};

#[derive(Debug, Default)]
pub(crate) struct BindingUse {
    active: Arc<AtomicUsize>,
}

impl BindingUse {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) != 0
    }

    /// The caller holds the owning DBC gate through snapshot admission or
    /// mutation. A separate check without that gate cannot authorize mutation.
    pub(crate) fn ensure_idle(&self, _gate: &DbcState) -> Result<(), BindingError> {
        if self.is_active() {
            Err(BindingError::InUse)
        } else {
            Ok(())
        }
    }

    pub(crate) fn acquire(&self, _gate: &DbcState) -> Result<BindingUseGuard, BindingError> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| BindingError::Capacity)?;
        Ok(BindingUseGuard {
            active: Arc::clone(&self.active),
        })
    }
}

#[derive(Debug)]
pub(crate) struct BindingUseGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for BindingUseGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Debug)]
pub(crate) struct BindingLease {
    _descriptor: Arc<DescHandle>,
    _use_guard: BindingUseGuard,
}

impl BindingLease {
    /// Call while both the owning DBC and this descriptor are locked, then
    /// snapshot the records before releasing those locks.
    pub(crate) fn acquire(
        descriptor: &Arc<DescHandle>,
        gate: &DbcState,
        _state: &DescState,
    ) -> Result<Self, BindingError> {
        let use_guard = descriptor.binding_use.acquire(gate)?;
        Ok(Self {
            _descriptor: Arc::clone(descriptor),
            _use_guard: use_guard,
        })
    }
}

#[derive(Debug)]
pub(crate) enum BindingError {
    InUse,
    Capacity,
    Registry(RegistryError),
    Poisoned,
    InvalidRecord,
}

impl BindingError {
    pub(crate) fn post(&self, state: &mut impl HasDiagnostics) -> SqlReturn {
        tracing::error!(?self, "binding access failed");
        match self {
            Self::InUse => post_diag(state, ERR_FUNCTION_SEQUENCE),
            Self::Capacity => post_diag(state, ERR_MEMORY_ALLOCATION),
            Self::Registry(error) => post_sql_error(
                state,
                SQLSTATE_HY000,
                0,
                format!("Cannot acquire binding descriptor: {error:?}"),
            ),
            Self::Poisoned | Self::InvalidRecord => {
                post_sql_error(
                    state,
                    SQLSTATE_HY000,
                    0,
                    format!("Binding access failed: {self:?}"),
                );
            }
        }
        SQL_ERROR
    }
}

pub(crate) fn owned_descriptor(raw: SqlHandle) -> Result<Arc<DescHandle>, BindingError> {
    handle_from_raw::<DescHandle>(raw)
        .map(|handle| handle.into_arc())
        .map_err(BindingError::Registry)
}

#[cfg(test)]
pub(crate) mod snapshot_test_hook {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use crate::handles::StmtHandle;

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(crate) enum Phase {
        Fetch,
        Parameters,
        ParameterRows,
    }

    type Key = (usize, Phase);
    type Hook = Box<dyn FnOnce() + Send>;

    fn hooks() -> &'static Mutex<HashMap<Key, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<Key, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(Mutex::default)
    }

    pub(crate) struct Registration(Key);

    impl Drop for Registration {
        fn drop(&mut self) {
            hooks().lock().unwrap().remove(&self.0);
        }
    }

    pub(crate) fn install(
        stmt: &StmtHandle,
        phase: Phase,
        hook: impl FnOnce() + Send + 'static,
    ) -> Registration {
        let key = (std::ptr::from_ref(stmt) as usize, phase);
        assert!(
            hooks()
                .lock()
                .unwrap()
                .insert(key, Box::new(hook))
                .is_none()
        );
        Registration(key)
    }

    pub(crate) fn pause(stmt: &StmtHandle, phase: Phase) {
        let hook = hooks()
            .lock()
            .unwrap()
            .remove(&(std::ptr::from_ref(stmt) as usize, phase));
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::snapshot_test_hook::{self, Phase};
    use super::*;
    use crate::api::odbc_types::*;
    use crate::api::{
        SQLBindCol as sql_bind_col, SQLBindParameter as sql_bind_parameter,
        SQLExecDirectW as sql_exec_direct_w, SQLExecute as sql_execute,
        SQLFetchScroll as sql_fetch_scroll, SQLFreeStmt, SQLGetDescFieldW as sql_get_desc_field_w,
        SQLParamData as sql_param_data, SQLPrepareW as sql_prepare_w, SQLPutData as sql_put_data,
        SQLSetDescFieldW as sql_set_desc_field_w, SQLSetDescRec as sql_set_desc_rec,
        SQLSetStmtAttrW as sql_set_stmt_attr_w,
    };
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
    use crate::handles::{DbcHandle, StmtHandle};
    use crate::test_support::TestHandles;
    use mssql_tds::error::Error as TdsError;
    use mssql_tds::test_client_support::{
        done_no_more, int_columns, tds_client_from_int_rows, tds_client_from_tokens,
    };

    /// # Safety
    /// `stmt` must be a live statement owned by the test fixture.
    unsafe fn sql_free_stmt_unbind(stmt: SqlHandle) -> SqlReturn {
        unsafe { SQLFreeStmt(stmt, SQL_UNBIND) }
    }

    /// # Safety
    /// `stmt` must be a live statement owned by the test fixture.
    unsafe fn sql_free_stmt_reset_params(stmt: SqlHandle) -> SqlReturn {
        unsafe { SQLFreeStmt(stmt, SQL_RESET_PARAMS) }
    }

    fn set_attr(stmt: SqlHandle, attribute: SqlInteger, value: SqlPointer) -> SqlReturn {
        unsafe { sql_set_stmt_attr_w(stmt, attribute, value, 0) }
    }

    fn bind_int(
        stmt: SqlHandle,
        ordinal: SqlUSmallInt,
        value: SqlPointer,
        indicator: *mut SqlLen,
    ) -> SqlReturn {
        unsafe {
            sql_bind_parameter(
                stmt,
                ordinal,
                SQL_PARAM_INPUT,
                SQL_C_SLONG,
                SQL_INTEGER,
                10,
                0,
                value,
                4,
                indicator,
            )
        }
    }

    fn desc_field(
        desc: SqlHandle,
        record: SqlSmallInt,
        field: SqlUSmallInt,
        value: SqlPointer,
    ) -> SqlReturn {
        unsafe { sql_set_desc_field_w(desc, record, field.try_into().unwrap(), value, 0) }
    }

    fn assert_stmt_sequence(stmt: &StmtHandle, rc: SqlReturn) {
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            *b"HY010"
        );
    }

    fn assert_desc_sequence(desc: &DescHandle, rc: SqlReturn) {
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(
            desc.inner.lock().unwrap().diag_records[0].sql_state,
            *b"HY010"
        );
    }

    fn pause_at(
        stmt: &StmtHandle,
        phase: Phase,
    ) -> (
        snapshot_test_hook::Registration,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
    ) {
        let (arrived, ready) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let registration = snapshot_test_hook::install(stmt, phase, move || {
            arrived.send(()).unwrap();
            resume.recv().unwrap();
        });
        (registration, ready, release)
    }

    fn descriptor_data(desc: &DescHandle) -> String {
        let state = desc.inner.lock().unwrap();
        format!("{:?} {:?}", state.header, state.records)
    }

    #[test]
    fn fetch_lease_blocks_shared_descriptor_mutation_through_final_writes() {
        for outcome in [SQL_SUCCESS, SQL_NO_DATA, SQL_ERROR] {
            let mut h = TestHandles::with_env_dbc_stmt();
            h.mark_dbc_connected();
            let other_raw = h.alloc_extra_stmt();
            let desc_raw = h.alloc_explicit_desc();
            let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
            let other = handle_from_raw::<StmtHandle>(other_raw).unwrap().into_arc();
            let desc = owned_descriptor(desc_raw).unwrap();
            let ipd = owned_descriptor(other.ipd).unwrap();
            for raw in [h.stmt, other_raw] {
                assert_eq!(set_attr(raw, SQL_ATTR_APP_ROW_DESC, desc_raw), SQL_SUCCESS);
            }
            assert_eq!(
                set_attr(other_raw, SQL_ATTR_APP_PARAM_DESC, desc_raw),
                SQL_SUCCESS
            );

            let mut values = [91_i32, 0, 92];
            let mut indicators = [93_isize, 0, 94];
            let mut statuses = [95_u16, 0, 96];
            let mut fetched = [97_usize, 0, 98];
            let mut offset = 0_usize;
            let value = (&raw mut values[1]).cast();
            let indicator = &raw mut indicators[1];
            assert_eq!(bind_int(other_raw, 1, value, indicator), SQL_SUCCESS);
            assert_eq!(
                set_attr(
                    h.stmt,
                    SQL_ATTR_ROWS_FETCHED_PTR,
                    (&raw mut fetched[1]).cast()
                ),
                SQL_SUCCESS
            );
            assert_eq!(
                set_attr(
                    h.stmt,
                    SQL_ATTR_ROW_STATUS_PTR,
                    (&raw mut statuses[1]).cast()
                ),
                SQL_SUCCESS
            );
            assert_eq!(
                set_attr(
                    h.stmt,
                    SQL_ATTR_ROW_BIND_OFFSET_PTR,
                    (&raw mut offset).cast()
                ),
                SQL_SUCCESS
            );
            let original = descriptor_data(&desc);
            let original_ipd = descriptor_data(&ipd);
            {
                let mut state = stmt.inner.lock().unwrap();
                state.begin_result_set(int_columns(1));
                state.set_state(STMT_STATE_CURSOR_OPEN);
                state.result_set_exhausted = outcome != SQL_SUCCESS;
                if outcome == SQL_ERROR {
                    state.pending_fetch_error =
                        Some(TdsError::ProtocolError("deferred fetch error".into()));
                }
            }
            if outcome == SQL_SUCCESS {
                let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
                let mut client = tds_client_from_int_rows(vec![vec![42]]);
                dbc.runtime
                    .block_on(client.execute("SELECT 42".to_owned(), ()))
                    .unwrap();
                let mut state = dbc.inner.lock().unwrap();
                state.client = Some(client);
                state.active_stmt = Some(h.stmt);
            }
            let (_registration, ready, release) = pause_at(&stmt, Phase::Fetch);
            let raw = h.stmt as usize;
            std::thread::scope(|scope| {
                let fetch = scope.spawn(move || unsafe {
                    sql_fetch_scroll(raw as SqlHandle, SQL_FETCH_NEXT, 0)
                });
                ready.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(stmt.row_binding_use.is_active());
                assert!(desc.binding_use.is_active());
                assert!(
                    stmt.parent_dbc().inner.try_lock().is_ok(),
                    "snapshot retained DBC gate"
                );
                assert!(stmt.inner.try_lock().is_ok(), "snapshot retained STMT lock");
                assert!(desc.inner.try_lock().is_ok(), "snapshot retained DESC lock");
                for (record, field, value) in [
                    (4, SQL_DESC_DATA_PTR, ptr::null_mut()),
                    (0, SQL_DESC_COUNT, 4_usize as SqlPointer),
                    (0, SQL_DESC_ARRAY_SIZE, 3_usize as SqlPointer),
                    (0, SQL_DESC_BIND_TYPE, 16_usize as SqlPointer),
                    (0, SQL_DESC_BIND_OFFSET_PTR, ptr::null_mut()),
                    (0, SQL_DESC_ARRAY_STATUS_PTR, ptr::null_mut()),
                ] {
                    assert_desc_sequence(&desc, desc_field(desc_raw, record, field, value));
                }
                assert_desc_sequence(&desc, unsafe {
                    sql_set_desc_rec(
                        desc_raw,
                        4,
                        SQL_C_SLONG,
                        0,
                        4,
                        0,
                        0,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                });
                for raw in [h.stmt, other_raw] {
                    let handle = if raw == h.stmt { &stmt } else { &other };
                    assert_stmt_sequence(handle, unsafe {
                        sql_bind_col(raw, 2, SQL_C_SLONG, value, 4, indicator)
                    });
                    assert_stmt_sequence(handle, unsafe {
                        sql_bind_col(raw, 1, SQL_C_SLONG, ptr::null_mut(), 0, ptr::null_mut())
                    });
                    assert_stmt_sequence(handle, unsafe { sql_free_stmt_unbind(raw) });
                    assert_stmt_sequence(
                        handle,
                        set_attr(raw, SQL_ATTR_APP_ROW_DESC, ptr::null_mut()),
                    );
                }
                assert_stmt_sequence(&other, bind_int(other_raw, 2, value, indicator));
                assert_stmt_sequence(&other, unsafe { sql_free_stmt_reset_params(other_raw) });
                assert_stmt_sequence(
                    &other,
                    set_attr(other_raw, SQL_ATTR_APP_PARAM_DESC, ptr::null_mut()),
                );
                for attribute in [
                    SQL_ATTR_ROW_ARRAY_SIZE,
                    SQL_ATTR_ROWS_FETCHED_PTR,
                    SQL_ATTR_ROW_STATUS_PTR,
                    SQL_ATTR_ROW_BIND_OFFSET_PTR,
                    SQL_ATTR_ROW_BIND_TYPE,
                ] {
                    assert_stmt_sequence(&stmt, set_attr(h.stmt, attribute, 1_usize as SqlPointer));
                }
                let mut count = 0_i16;
                assert_eq!(
                    unsafe {
                        sql_get_desc_field_w(
                            desc_raw,
                            0,
                            SQL_DESC_COUNT.try_into().unwrap(),
                            (&raw mut count).cast(),
                            0,
                            ptr::null_mut(),
                        )
                    },
                    SQL_SUCCESS
                );
                assert_eq!(count, 1);
                assert_eq!(descriptor_data(&desc), original);
                assert_eq!(descriptor_data(&ipd), original_ipd);
                assert_eq!(values, [91, 0, 92]);
                assert_eq!(indicators, [93, 0, 94]);
                assert_eq!(statuses, [95, 0, 96]);
                assert_eq!(fetched, [97, 0, 98]);
                release.send(()).unwrap();
                assert_eq!(fetch.join().unwrap(), outcome);
            });
            assert!(!stmt.row_binding_use.is_active());
            assert!(!desc.binding_use.is_active());
            assert_eq!(
                values,
                [91, if outcome == SQL_SUCCESS { 42 } else { 0 }, 92]
            );
            assert_eq!(
                indicators,
                [93, if outcome == SQL_SUCCESS { 4 } else { 0 }, 94]
            );
            assert_eq!(fetched, [97, usize::from(outcome == SQL_SUCCESS), 98]);
            assert_eq!(
                statuses,
                [
                    95,
                    if outcome == SQL_SUCCESS {
                        SQL_ROW_SUCCESS
                    } else {
                        SQL_ROW_NOROW
                    },
                    96
                ]
            );
            assert_eq!(
                unsafe { sql_free_stmt_reset_params(other_raw) },
                SQL_SUCCESS
            );
            assert_eq!(
                set_attr(h.stmt, SQL_ATTR_APP_ROW_DESC, ptr::null_mut()),
                SQL_SUCCESS
            );
            assert_eq!(
                desc_field(desc_raw, 0, SQL_DESC_COUNT, 2_usize as SqlPointer),
                SQL_SUCCESS
            );
        }
    }

    #[test]
    fn parameter_leases_cover_snapshot_reads_and_array_staging() {
        for (phase, array) in [(Phase::Parameters, false), (Phase::ParameterRows, true)] {
            let h = TestHandles::with_env_dbc_stmt();
            h.mark_dbc_connected();
            let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
            let apd = owned_descriptor(h.apd()).unwrap();
            let ipd = owned_descriptor(h.ipd()).unwrap();
            let sql: Vec<u16> = "SELECT ?\0".encode_utf16().collect();
            assert_eq!(
                unsafe { sql_prepare_w(h.stmt, sql.as_ptr(), SQL_NTS) },
                SQL_SUCCESS,
                "{:?}",
                stmt.inner.lock().unwrap().diag_records,
            );
            let mut values = [7_i32, 8];
            let mut indicators = [4_isize; 2];
            let mut operations = [SQL_PARAM_IGNORE; 2];
            let mut statuses = [91_u16, 92];
            let mut processed = 93_usize;
            assert_eq!(
                bind_int(
                    h.stmt,
                    1,
                    values.as_mut_ptr().cast(),
                    indicators.as_mut_ptr()
                ),
                SQL_SUCCESS
            );
            if array {
                assert_eq!(
                    set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 2_usize as SqlPointer),
                    SQL_SUCCESS
                );
                assert_eq!(
                    set_attr(
                        h.stmt,
                        SQL_ATTR_PARAM_OPERATION_PTR,
                        operations.as_mut_ptr().cast()
                    ),
                    SQL_SUCCESS
                );
                assert_eq!(
                    set_attr(
                        h.stmt,
                        SQL_ATTR_PARAM_STATUS_PTR,
                        statuses.as_mut_ptr().cast()
                    ),
                    SQL_SUCCESS
                );
                assert_eq!(
                    set_attr(
                        h.stmt,
                        SQL_ATTR_PARAMS_PROCESSED_PTR,
                        (&raw mut processed).cast()
                    ),
                    SQL_SUCCESS
                );
            }
            let original_apd = descriptor_data(&apd);
            let original_ipd = descriptor_data(&ipd);
            let (_registration, ready, release) = pause_at(&stmt, phase);
            let raw = h.stmt as usize;
            std::thread::scope(|scope| {
                let execute = scope.spawn(move || unsafe { sql_execute(raw as SqlHandle) });
                ready.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(stmt.param_binding_use.is_active());
                assert!(apd.binding_use.is_active());
                assert!(ipd.binding_use.is_active());
                assert_stmt_sequence(&stmt, bind_int(h.stmt, 4, ptr::null_mut(), ptr::null_mut()));
                assert_stmt_sequence(&stmt, unsafe { sql_free_stmt_reset_params(h.stmt) });
                assert_stmt_sequence(
                    &stmt,
                    set_attr(h.stmt, SQL_ATTR_APP_PARAM_DESC, ptr::null_mut()),
                );
                for attribute in [
                    SQL_ATTR_PARAMSET_SIZE,
                    SQL_ATTR_PARAM_BIND_OFFSET_PTR,
                    SQL_ATTR_PARAM_BIND_TYPE,
                    SQL_ATTR_PARAM_OPERATION_PTR,
                    SQL_ATTR_PARAM_STATUS_PTR,
                    SQL_ATTR_PARAMS_PROCESSED_PTR,
                ] {
                    assert_stmt_sequence(&stmt, set_attr(h.stmt, attribute, 1_usize as SqlPointer));
                }
                for descriptor in [h.apd(), h.ipd()] {
                    let desc = if descriptor == h.apd() { &apd } else { &ipd };
                    assert_desc_sequence(
                        desc,
                        desc_field(descriptor, 4, SQL_DESC_PRECISION, 8_usize as SqlPointer),
                    );
                }
                assert_eq!(descriptor_data(&apd), original_apd);
                assert_eq!(descriptor_data(&ipd), original_ipd);
                release.send(()).unwrap();
                assert_eq!(
                    execute.join().unwrap(),
                    if array { SQL_SUCCESS } else { SQL_ERROR }
                );
            });
            assert!(!stmt.param_binding_use.is_active());
            assert!(!apd.binding_use.is_active());
            assert!(!ipd.binding_use.is_active());
            if array {
                assert_eq!(statuses, [SQL_PARAM_UNUSED; 2]);
                assert_eq!(processed, 2);
            }
            assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
        }
    }

    #[test]
    fn fetch_unwind_releases_atomic_use_without_poisoning_state() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let ard = owned_descriptor(h.ard()).unwrap();
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.result_set_exhausted = true;
        }
        let _registration =
            snapshot_test_hook::install(&stmt, Phase::Fetch, || panic!("after snapshot"));
        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_ERROR
        );
        assert!(!stmt.row_binding_use.is_active());
        assert!(!ard.binding_use.is_active());
        assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_SUCCESS);
        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_NO_DATA
        );
    }

    #[test]
    fn shared_fetch_leases_hold_until_the_last_consumer_finishes() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_raw = h.alloc_extra_stmt();
        let desc_raw = h.alloc_explicit_desc();
        let first = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let second = handle_from_raw::<StmtHandle>(other_raw).unwrap().into_arc();
        let desc = owned_descriptor(desc_raw).unwrap();
        for (raw, stmt) in [(h.stmt, &first), (other_raw, &second)] {
            assert_eq!(set_attr(raw, SQL_ATTR_APP_ROW_DESC, desc_raw), SQL_SUCCESS);
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.result_set_exhausted = true;
        }
        let (_first_hook, first_ready, first_release) = pause_at(&first, Phase::Fetch);
        let (_second_hook, second_ready, second_release) = pause_at(&second, Phase::Fetch);
        let first_raw = h.stmt as usize;
        let second_raw = other_raw as usize;
        std::thread::scope(|scope| {
            let first_fetch = scope.spawn(move || unsafe {
                sql_fetch_scroll(first_raw as SqlHandle, SQL_FETCH_NEXT, 0)
            });
            first_ready.recv_timeout(Duration::from_secs(10)).unwrap();
            let second_fetch = scope.spawn(move || unsafe {
                sql_fetch_scroll(second_raw as SqlHandle, SQL_FETCH_NEXT, 0)
            });
            second_ready.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(h.free_explicit_desc(desc_raw), SQL_ERROR);
            first_release.send(()).unwrap();
            assert_eq!(first_fetch.join().unwrap(), SQL_NO_DATA);
            assert!(desc.binding_use.is_active());
            assert_desc_sequence(
                &desc,
                desc_field(desc_raw, 0, SQL_DESC_COUNT, ptr::null_mut()),
            );
            assert_eq!(h.free_explicit_desc(desc_raw), SQL_ERROR);
            second_release.send(()).unwrap();
            assert_eq!(second_fetch.join().unwrap(), SQL_NO_DATA);
        });
        assert!(!desc.binding_use.is_active());
        assert_eq!(h.free_explicit_desc(desc_raw), SQL_SUCCESS);
    }

    #[test]
    fn failed_parameter_snapshot_releases_control_use() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let apd = owned_descriptor(h.apd()).unwrap();
        let ipd = owned_descriptor(h.ipd()).unwrap();
        let poisoned = Arc::clone(&ipd);
        assert!(
            std::thread::spawn(move || {
                let _state = poisoned.inner.lock().unwrap();
                panic!("poison IPD before snapshot");
            })
            .join()
            .is_err()
        );
        let sql: Vec<u16> = "SELECT 1\0".encode_utf16().collect();
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_ERROR
        );
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            *b"HY000"
        );
        assert!(!stmt.param_binding_use.is_active());
        assert!(!apd.binding_use.is_active());
        assert!(!ipd.binding_use.is_active());
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 2_usize as SqlPointer),
            SQL_SUCCESS
        );
    }

    #[test]
    fn ipd_use_preflight_preserves_apd_on_bind_and_reset() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let apd = owned_descriptor(h.apd()).unwrap();
        let ipd = owned_descriptor(h.ipd()).unwrap();
        let mut value = 7_i32;
        let mut indicator = 4_isize;
        assert_eq!(
            bind_int(h.stmt, 1, (&raw mut value).cast(), &raw mut indicator),
            SQL_SUCCESS
        );
        let original_apd = descriptor_data(&apd);
        let original_ipd = descriptor_data(&ipd);
        let lease = {
            let gate = stmt.parent_dbc().inner.lock().unwrap();
            let state = ipd.inner.lock().unwrap();
            BindingLease::acquire(&ipd, &gate, &state).unwrap()
        };
        assert_stmt_sequence(
            &stmt,
            bind_int(h.stmt, 4, (&raw mut value).cast(), &raw mut indicator),
        );
        assert_stmt_sequence(&stmt, unsafe { sql_free_stmt_reset_params(h.stmt) });
        assert_eq!(descriptor_data(&apd), original_apd);
        assert_eq!(descriptor_data(&ipd), original_ipd);
        drop(lease);
        assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
    }

    #[test]
    fn invalid_descriptor_ids_never_become_allocation_pointers() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let stale = h.alloc_explicit_desc();
        assert_eq!(h.free_explicit_desc(stale), SQL_SUCCESS);
        for raw in [ptr::null_mut(), h.dbc, h.stmt, stale] {
            assert_eq!(
                desc_field(raw, 0, SQL_DESC_COUNT, ptr::null_mut()),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                unsafe {
                    sql_set_desc_rec(
                        raw,
                        1,
                        SQL_C_SLONG,
                        0,
                        4,
                        0,
                        0,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                },
                SQL_INVALID_HANDLE
            );
        }
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        for attribute in [SQL_ATTR_APP_ROW_DESC, SQL_ATTR_APP_PARAM_DESC] {
            for raw in [h.dbc, h.stmt, stale] {
                assert_eq!(set_attr(h.stmt, attribute, raw), SQL_ERROR);
                assert_eq!(
                    stmt.inner.lock().unwrap().diag_records[0].sql_state,
                    *b"HY024"
                );
            }
            assert_eq!(set_attr(h.stmt, attribute, ptr::null_mut()), SQL_SUCCESS);
        }
    }

    #[test]
    fn deferred_dae_releases_only_owned_snapshots_and_survives_reset() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
        dbc.inner.lock().unwrap().client = Some(tds_client_from_tokens(vec![done_no_more()]));
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let apd = owned_descriptor(h.apd()).unwrap();
        let ipd = owned_descriptor(h.ipd()).unwrap();
        let mut first = 7_i32;
        let mut first_length = 4_isize;
        let mut token = 8_i32;
        let token_ptr = (&raw mut token).cast();
        let mut dae_length = SQL_DATA_AT_EXEC;
        assert_eq!(
            bind_int(h.stmt, 1, (&raw mut first).cast(), &raw mut first_length),
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    2,
                    SQL_PARAM_INPUT,
                    SQL_C_CHAR,
                    SQL_INTEGER,
                    10,
                    0,
                    token_ptr,
                    1,
                    &raw mut dae_length,
                )
            },
            SQL_SUCCESS
        );
        let sql: Vec<u16> = "SELECT ?, ?\0".encode_utf16().collect();
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_NEED_DATA,
            "{:?}",
            stmt.inner.lock().unwrap().diag_records,
        );
        assert!(!stmt.param_binding_use.is_active());
        assert!(!apd.binding_use.is_active());
        assert!(!ipd.binding_use.is_active());
        assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
        first = 99;
        first_length = SQL_NULL_DATA;
        {
            let state = stmt.inner.lock().unwrap();
            let dae = state.dae.as_ref().unwrap();
            assert!(dae.deferred);
            let expected = mssql_tds::message::parameters::rpc_parameters::RpcParameter::new(
                Some("@P1".into()),
                mssql_tds::message::parameters::rpc_parameters::StatusFlags::NONE,
                mssql_tds::datatypes::sqltypes::SqlType::Int(Some(7)),
            );
            assert_eq!(format!("{:?}", dae.prebuilt[0]), format!("{expected:?}"));
        }
        let mut returned = ptr::null_mut();
        assert_eq!(
            unsafe { sql_param_data(h.stmt, &raw mut returned) },
            SQL_NEED_DATA
        );
        assert_eq!(returned, token_ptr);
        let mut chunk = *b"8";
        assert_eq!(
            unsafe { sql_put_data(h.stmt, chunk.as_mut_ptr().cast(), 1) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_param_data(h.stmt, &raw mut returned) },
            SQL_SUCCESS,
            "{:?}",
            stmt.inner.lock().unwrap().diag_records,
        );
        assert_eq!(first, 99);
        assert_eq!(first_length, SQL_NULL_DATA);
        assert!(!stmt.inner.lock().unwrap().needs_data());
    }

    #[test]
    fn lease_counter_is_checked_and_shared_uses_release_independently() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
        let gate = dbc.inner.lock().unwrap();
        let binding_use = BindingUse::default();
        let first = binding_use.acquire(&gate).unwrap();
        let second = binding_use.acquire(&gate).unwrap();
        drop(first);
        assert!(binding_use.is_active());
        drop(second);
        assert!(!binding_use.is_active());
        binding_use.active.store(usize::MAX, Ordering::Release);
        assert!(matches!(
            binding_use.acquire(&gate),
            Err(BindingError::Capacity)
        ));
        assert_eq!(binding_use.active.load(Ordering::Acquire), usize::MAX);
    }
}

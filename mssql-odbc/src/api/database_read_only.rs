// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Database updateability for SQL_ATTR_ACCESS_MODE and SQL_DATA_SOURCE_READ_ONLY.
//! Access mode is a hint, not a write filter (msodbcsql sqlcmisc.cpp).

use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, TdsClient};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::error::Error as TdsError;
use tracing::error;

use super::odbc_types::{
    SQL_ERROR, SQL_MODE_READ_ONLY, SQL_MODE_READ_WRITE, SQL_SUCCESS, SqlInteger, SqlPointer,
    SqlReturn,
};
use super::secondary_lookup::{Route, cache_is_current, read_on_secondary, select_route};
use super::sqlstate::{
    ERR_CONNECTION_BUSY, ERR_FUNCTION_SEQUENCE, ERR_INVALID_ATTRIBUTE_VALUE,
    ERR_INVALID_NULL_POINTER, SQLSTATE_HY000, post_diag, post_tds_error,
};
use super::txn::try_claim_idle_dbc_client;
use super::util::write_if_some;
use crate::error::free_errors;
use crate::handles::DbcHandle;
use crate::handles::dbc::{CachedDatabaseReadOnly, ConnectionState};
use crate::handles::wire::{LeaseReturn, WireLease};

// A table-free SELECT does not start an implicit transaction.
const READ_ONLY_QUERY: &str = "SELECT CASE CAST(DATABASEPROPERTYEX(DB_NAME(), \
    'Updateability') AS nvarchar(60)) WHEN N'READ_ONLY' THEN N'Y' \
    WHEN N'READ_WRITE' THEN N'N' END";

pub(super) fn set_access_mode(dbc: &DbcHandle, value: u32) -> SqlReturn {
    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLSetConnectAttrW(SQL_ATTR_ACCESS_MODE): dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        debug_assert!(
            matches!(value, SQL_MODE_READ_WRITE | SQL_MODE_READ_ONLY),
            "invalid access mode - DM should have rejected this"
        );
    }

    if value == SQL_MODE_READ_WRITE {
        match database_read_only(dbc) {
            Ok(false) => {}
            Ok(true) => {
                let Ok(mut state) = dbc.inner.lock() else {
                    return SQL_ERROR;
                };
                error!("SQLSetConnectAttrW: database is read-only");
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            Err(ret) => return ret,
        }
    }
    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLSetConnectAttrW(SQL_ATTR_ACCESS_MODE): dbc mutex poisoned");
        return SQL_ERROR;
    };
    state.access_mode = value;
    SQL_SUCCESS
}

pub(super) fn get_access_mode(
    dbc: &DbcHandle,
    value_ptr: SqlPointer,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLGetConnectAttrW(SQL_ATTR_ACCESS_MODE): dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        if value_ptr.is_null() {
            post_diag(&mut state, ERR_INVALID_NULL_POINTER);
            return SQL_ERROR;
        }
    }
    let read_only = match database_read_only(dbc) {
        Ok(value) => value,
        Err(ret) => return ret,
    };
    let Ok(state) = dbc.inner.lock() else {
        error!("SQLGetConnectAttrW(SQL_ATTR_ACCESS_MODE): dbc mutex poisoned");
        return SQL_ERROR;
    };
    let value = if read_only {
        SQL_MODE_READ_ONLY
    } else {
        state.access_mode
    };
    // SAFETY: the entry point requires writable SQLUINTEGER/SQLINTEGER outputs.
    unsafe {
        write_if_some(value_ptr.cast::<u32>(), value);
        write_if_some(string_length_ptr, 4);
    }
    SQL_SUCCESS
}

/// Mirrors sqlcinfo.cpp's lazy SQL_DATA_SOURCE_READ_ONLY query and ExecOption
/// route: use the session when idle, or a separate login while rows are pending.
pub(super) fn database_read_only(dbc: &DbcHandle) -> Result<bool, SqlReturn> {
    let (generation, reset_generation, timeout_secs, route) = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("database updateability lookup: dbc mutex poisoned");
            return Err(SQL_ERROR);
        };
        if state.connection_state != ConnectionState::Connected {
            return Ok(false);
        }
        let Some(client) = state.wire.client() else {
            post_diag(&mut state, ERR_CONNECTION_BUSY);
            return Err(SQL_ERROR);
        };
        if let Some(cache) = &state.database_read_only
            && is_current(cache, client)
        {
            return Ok(cache.read_only);
        }
        let Some(route) = select_route(&state, client) else {
            error!("database updateability lookup: no silent secondary login available");
            post_diag(&mut state, ERR_CONNECTION_BUSY);
            return Err(SQL_ERROR);
        };
        (
            state.session_generation,
            state.reset_generation,
            dbc.connection_timeout(),
            route,
        )
    };

    let (client, lookup) = match route {
        Route::Session => {
            let Some((mut client, claimed_generation, _)) = try_claim_idle_dbc_client(dbc) else {
                let Ok(mut state) = dbc.inner.lock() else {
                    return Err(SQL_ERROR);
                };
                post_diag(&mut state, ERR_CONNECTION_BUSY);
                return Err(SQL_ERROR);
            };
            if claimed_generation != generation {
                let _ = super::txn::release_dbc_client(dbc, client);
                let Ok(mut state) = dbc.inner.lock() else {
                    return Err(SQL_ERROR);
                };
                post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
                return Err(SQL_ERROR);
            }
            let read = dbc
                .runtime
                .block_on(read_updateability(&mut client, timeout_secs));
            let lookup = read.map(|read_only| CachedDatabaseReadOnly {
                catalog: client.database().to_owned(),
                database_change_count: client.database_change_count(),
                read_only,
            });
            let _ = client.take_info_messages();
            (Some(client), lookup)
        }
        Route::Secondary {
            context,
            catalog,
            database_change_count,
        } => {
            let read = dbc.runtime.block_on(read_on_secondary(
                context,
                timeout_secs,
                "Database updateability lookup timed out",
                read_updateability,
            ));
            (
                None,
                read.map(|read_only| CachedDatabaseReadOnly {
                    catalog,
                    database_change_count,
                    read_only,
                }),
            )
        }
    };
    publish(dbc, client, generation, reset_generation, lookup)
}

fn is_current(cache: &CachedDatabaseReadOnly, client: &TdsClient) -> bool {
    cache_is_current(&cache.catalog, cache.database_change_count, client)
}

fn publish(
    dbc: &DbcHandle,
    client: Option<WireLease>,
    generation: u64,
    reset_generation: u64,
    lookup: TdsResult<CachedDatabaseReadOnly>,
) -> Result<bool, SqlReturn> {
    let Ok(mut state) = dbc.inner.lock() else {
        error!("database updateability lookup: dbc mutex poisoned");
        return Err(SQL_ERROR);
    };
    if state.session_generation != generation
        || state.connection_state != ConnectionState::Connected
    {
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return Err(SQL_ERROR);
    }
    if let Some(client) = client
        && !state.wire.restore(client, LeaseReturn::Release)
    {
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return Err(SQL_ERROR);
    }
    if state.reset_generation != reset_generation {
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return Err(SQL_ERROR);
    }
    match lookup {
        Ok(cache) => {
            if !state
                .wire
                .client()
                .is_some_and(|client| is_current(&cache, client))
            {
                post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
                return Err(SQL_ERROR);
            }
            let read_only = cache.read_only;
            state.database_read_only = Some(Box::new(cache));
            Ok(read_only)
        }
        Err(e) => {
            error!(%e, "database updateability lookup failed");
            post_tds_error(&mut state, &e, SQLSTATE_HY000);
            Err(SQL_ERROR)
        }
    }
}

async fn read_updateability(client: &mut TdsClient, timeout_secs: u32) -> TdsResult<bool> {
    let read = async {
        client
            .execute(
                READ_ONLY_QUERY.to_string(),
                ExecuteOptions::new().timeout_secs(timeout_secs),
            )
            .await?;
        if !client.on_rows() && client.has_open_batch() {
            client.advance_to_rows().await?;
        }
        let row = client.next_row().await?;
        match row.as_ref().and_then(|row| row.first()) {
            Some(ColumnValues::String(value)) => match value.to_utf8_string().as_str() {
                "Y" => Ok(true),
                "N" => Ok(false),
                _ => Err(invalid_updateability()),
            },
            _ => Err(invalid_updateability()),
        }
    }
    .await;
    let closed = if client.has_open_batch() {
        client.close_query().await
    } else {
        Ok(())
    };
    let value = read?;
    closed?;
    Ok(value)
}

fn invalid_updateability() -> TdsError {
    TdsError::ProtocolError("Database updateability lookup returned no valid Y/N value".to_string())
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::time::{Duration, Instant};

    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, LeadingError, QueryResponse, Row, SqlDataType,
    };

    use super::*;
    use crate::api::odbc_types::{
        SQL_ATTR_ACCESS_MODE, SQL_ATTR_RESET_CONNECTION, SQL_C_SLONG, SQL_DATA_SOURCE_READ_ONLY,
        SQL_NO_DATA, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlSmallInt,
    };
    use crate::api::{
        SQLExecDirectW, SQLFetch, SQLGetConnectAttrW, SQLGetData, SQLGetInfoW, SQLSetConnectAttrW,
    };
    use crate::error::post_sql_error;
    use crate::handles::handle_from_raw;
    use crate::handles::wire::{ClaimPolicy, WireOwner};
    use crate::test_support::{MockServer, TestHandles, connect_mock_server};

    fn response(value: &str) -> QueryResponse {
        QueryResponse::new(
            vec![ColumnDefinition::new("", SqlDataType::NVarChar)],
            vec![Row::new(vec![ColumnValue::NVarChar(value.to_string())])],
        )
    }

    fn dbc(h: &TestHandles) -> &DbcHandle {
        unsafe { handle_from_raw::<DbcHandle>(h.dbc) }
    }

    fn fixture(response: QueryResponse) -> (TestHandles, MockServer) {
        let h = TestHandles::with_env_dbc_stmt();
        let server = connect_mock_server(dbc(&h), READ_ONLY_QUERY, response);
        (h, server)
    }

    fn set_mode(h: &TestHandles, value: u32) -> SqlReturn {
        unsafe {
            SQLSetConnectAttrW(
                h.dbc,
                SQL_ATTR_ACCESS_MODE,
                usize::try_from(value).unwrap() as SqlPointer,
                0,
            )
        }
    }

    fn mode(handle: SqlHandle) -> (SqlReturn, u32, SqlInteger) {
        let mut value = u32::MAX;
        let mut length = -1;
        let rc = unsafe {
            SQLGetConnectAttrW(
                handle,
                SQL_ATTR_ACCESS_MODE,
                ptr::from_mut(&mut value).cast(),
                0,
                &mut length,
            )
        };
        (rc, value, length)
    }

    fn info(handle: SqlHandle) -> (SqlReturn, String, SqlSmallInt) {
        let mut value = [0u16; 2];
        let mut length = -1;
        let rc = unsafe {
            SQLGetInfoW(
                handle,
                SQL_DATA_SOURCE_READ_ONLY,
                value.as_mut_ptr().cast(),
                4,
                &mut length,
            )
        };
        (rc, String::from_utf16_lossy(&value[..1]), length)
    }

    fn assert_diag(h: &TestHandles, expected: &[u8; 5]) {
        assert_eq!(
            dbc(h).inner.lock().unwrap().diag_records[0].sql_state,
            *expected
        );
    }

    fn assert_row(h: &TestHandles, expected: i32) {
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_SUCCESS);
        let mut value = 0i32;
        let mut length = 0;
        assert_eq!(
            unsafe {
                SQLGetData(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    ptr::from_mut(&mut value).cast(),
                    4,
                    &mut length,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, expected);
        assert_eq!(length, 4);
    }

    fn open_pending_result(h: &TestHandles, server: &MockServer) {
        const QUERY: &str = "SELECT value FROM pending_rows";
        server.register_query(
            QUERY,
            QueryResponse::new(
                vec![ColumnDefinition::new("value", SqlDataType::Int)],
                (1..=3)
                    .map(|value| Row::new(vec![ColumnValue::Int(value)]))
                    .collect(),
            ),
        );
        let sql: Vec<u16> = QUERY.encode_utf16().collect();
        assert_eq!(
            unsafe { SQLExecDirectW(h.stmt, sql.as_ptr(), sql.len().try_into().unwrap()) },
            SQL_SUCCESS
        );
        assert_row(h, 1);
        let state = dbc(h).inner.lock().unwrap();
        assert_eq!(state.wire.statement_owner(), Some(h.stmt));
        assert!(state.wire.client().unwrap().has_open_batch());
    }

    #[test]
    fn disconnected_mode_defaults_and_round_trips_without_a_lookup() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(mode(h.dbc), (SQL_SUCCESS, SQL_MODE_READ_WRITE, 4));
        for requested in [SQL_MODE_READ_ONLY, SQL_MODE_READ_WRITE] {
            assert_eq!(set_mode(&h, requested), SQL_SUCCESS);
            assert_eq!(mode(h.dbc), (SQL_SUCCESS, requested, 4));
        }
        assert_eq!(info(h.dbc), (SQL_SUCCESS, "N".to_string(), 2));
        assert!(dbc(&h).inner.lock().unwrap().database_read_only.is_none());
    }

    #[test]
    fn writable_database_preserves_the_hint_but_info_reports_actual_updateability() {
        let (h, server) = fixture(response("N"));
        assert_eq!(set_mode(&h, SQL_MODE_READ_ONLY), SQL_SUCCESS);
        assert!(dbc(&h).inner.lock().unwrap().database_read_only.is_none());
        assert_eq!(mode(h.dbc), (SQL_SUCCESS, SQL_MODE_READ_ONLY, 4));
        assert_eq!(info(h.dbc), (SQL_SUCCESS, "N".to_string(), 2));
        server.register_query(READ_ONLY_QUERY, response("Y"));
        assert_eq!(set_mode(&h, SQL_MODE_READ_WRITE), SQL_SUCCESS);
        assert_eq!(mode(h.dbc), (SQL_SUCCESS, SQL_MODE_READ_WRITE, 4));
        assert_eq!(server.connection_count(), 1);
    }

    #[test]
    fn read_only_database_overrides_the_hint_and_rejects_read_write_without_mutation() {
        let (h, _server) = fixture(response("Y"));
        assert_eq!(mode(h.dbc), (SQL_SUCCESS, SQL_MODE_READ_ONLY, 4));
        assert_eq!(info(h.dbc), (SQL_SUCCESS, "Y".to_string(), 2));
        assert_eq!(
            dbc(&h).inner.lock().unwrap().access_mode,
            SQL_MODE_READ_WRITE
        );
        assert_eq!(set_mode(&h, SQL_MODE_READ_ONLY), SQL_SUCCESS);
        assert_eq!(set_mode(&h, SQL_MODE_READ_WRITE), SQL_ERROR);
        assert_diag(&h, b"HY024");
        assert_eq!(
            dbc(&h).inner.lock().unwrap().access_mode,
            SQL_MODE_READ_ONLY
        );
        assert_eq!(mode(h.dbc), (SQL_SUCCESS, SQL_MODE_READ_ONLY, 4));
        assert!(dbc(&h).inner.lock().unwrap().diag_records.is_empty());
    }

    #[test]
    fn read_write_set_validates_the_database_before_the_first_get() {
        for (answer, expected) in [("Y", SQL_ERROR), ("N", SQL_SUCCESS)] {
            let (h, _server) = fixture(response(answer));
            assert_eq!(set_mode(&h, SQL_MODE_READ_ONLY), SQL_SUCCESS);
            assert_eq!(set_mode(&h, SQL_MODE_READ_WRITE), expected);
            let state = dbc(&h).inner.lock().unwrap();
            assert_eq!(
                state.access_mode,
                if expected == SQL_SUCCESS {
                    SQL_MODE_READ_WRITE
                } else {
                    SQL_MODE_READ_ONLY
                }
            );
            assert_eq!(
                state.database_read_only.as_ref().unwrap().read_only,
                answer == "Y"
            );
        }
    }

    #[test]
    fn all_three_entry_points_propagate_a_lookup_error_and_retry() {
        let (h, server) = fixture(response("Y").with_leading_error(LeadingError::new(
            229,
            14,
            "Lookup permission denied",
        )));
        assert_eq!(set_mode(&h, SQL_MODE_READ_ONLY), SQL_SUCCESS);
        assert_eq!(mode(h.dbc), (SQL_ERROR, u32::MAX, -1));
        assert_eq!(info(h.dbc).0, SQL_ERROR);
        assert_eq!(set_mode(&h, SQL_MODE_READ_WRITE), SQL_ERROR);
        {
            let state = dbc(&h).inner.lock().unwrap();
            assert_eq!(state.diag_records[0].native_error, 229);
            assert_eq!(state.access_mode, SQL_MODE_READ_ONLY);
            assert!(state.database_read_only.is_none());
            assert!(state.wire.client().is_some());
            assert!(!state.wire.client().unwrap().has_open_batch());
        }
        server.register_query(READ_ONLY_QUERY, response("N"));
        assert_eq!(set_mode(&h, SQL_MODE_READ_WRITE), SQL_SUCCESS);
        assert_eq!(mode(h.dbc), (SQL_SUCCESS, SQL_MODE_READ_WRITE, 4));
    }

    #[test]
    fn absent_null_and_invalid_answers_are_errors_not_writable_defaults() {
        let (h, server) = fixture(response("unexpected"));
        for answer in [
            response("unexpected"),
            QueryResponse::new(
                vec![ColumnDefinition::new("", SqlDataType::NVarChar)],
                vec![],
            ),
            QueryResponse::new(
                vec![ColumnDefinition::new("", SqlDataType::NVarCharMax)],
                vec![Row::new(vec![ColumnValue::NVarCharMaxNull])],
            ),
        ] {
            server.register_query(READ_ONLY_QUERY, answer);
            assert_eq!(mode(h.dbc).0, SQL_ERROR);
            assert_diag(&h, b"HY000");
            assert!(dbc(&h).inner.lock().unwrap().database_read_only.is_none());
        }
        server.register_query(READ_ONLY_QUERY, response("Y"));
        assert_eq!(info(h.dbc), (SQL_SUCCESS, "Y".to_string(), 2));
    }

    #[test]
    fn cache_is_shared_and_refreshed_for_both_parts_of_its_database_key() {
        let (h, server) = fixture(response("Y"));
        assert_eq!(info(h.dbc).1, "Y");
        server.register_query(READ_ONLY_QUERY, response("N"));
        assert_eq!(
            mode(h.dbc).1,
            SQL_MODE_READ_ONLY,
            "the first answer is cached"
        );
        dbc(&h)
            .inner
            .lock()
            .unwrap()
            .database_read_only
            .as_mut()
            .unwrap()
            .catalog = "another_database".to_string();
        assert_eq!(mode(h.dbc).1, SQL_MODE_READ_WRITE);
        server.register_query(READ_ONLY_QUERY, response("Y"));
        dbc(&h)
            .inner
            .lock()
            .unwrap()
            .database_read_only
            .as_mut()
            .unwrap()
            .database_change_count = u64::MAX;
        assert_eq!(info(h.dbc).1, "Y");
    }

    #[test]
    fn busy_session_uses_a_secondary_login_without_touching_the_cursor() {
        let (h, server) = fixture(response("Y"));
        let dbc = dbc(&h);
        {
            let mut state = dbc.inner.lock().unwrap();
            dbc.runtime
                .block_on(
                    state
                        .wire
                        .client_mut_for_test()
                        .unwrap()
                        .execute("SELECT 1".to_string(), ()),
                )
                .unwrap();
            state.wire.set_owner_for_test(Some(h.stmt));
        }
        assert_eq!(mode(h.dbc).1, SQL_MODE_READ_ONLY);
        assert_eq!(info(h.dbc).1, "Y");
        assert_eq!(set_mode(&h, SQL_MODE_READ_WRITE), SQL_ERROR);
        assert_diag(&h, b"HY024");
        assert_eq!(server.connection_count(), 2);
        let mut state = dbc.inner.lock().unwrap();
        assert_eq!(state.wire.statement_owner(), Some(h.stmt));
        let client = state.wire.client_mut_for_test().unwrap();
        assert!(client.has_open_batch());
        assert!(dbc.runtime.block_on(client.next_row()).unwrap().is_some());
        dbc.runtime.block_on(client.close_query()).unwrap();
        state.wire.set_owner_for_test(None);
    }

    #[test]
    fn claimed_session_is_an_explicit_error_not_a_writable_answer() {
        let (h, _server) = fixture(response("Y"));
        let (client, _, _) = try_claim_idle_dbc_client(dbc(&h)).unwrap();
        assert_eq!(info(h.dbc).0, SQL_ERROR);
        assert_diag(&h, b"HY000");
        assert!(super::super::txn::release_dbc_client(dbc(&h), client));
        assert_eq!(info(h.dbc).1, "Y");
    }

    #[test]
    fn invalid_buffers_do_not_issue_or_cache_a_lookup() {
        let (h, _server) = fixture(response("Y").with_delay(Duration::from_secs(8)));
        let started = Instant::now();
        assert_eq!(
            unsafe {
                SQLGetConnectAttrW(
                    h.dbc,
                    SQL_ATTR_ACCESS_MODE,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                )
            },
            SQL_ERROR
        );
        assert_diag(&h, b"HY009");
        assert_eq!(
            unsafe {
                SQLGetInfoW(
                    h.dbc,
                    SQL_DATA_SOURCE_READ_ONLY,
                    ptr::null_mut(),
                    -1,
                    ptr::null_mut(),
                )
            },
            SQL_ERROR
        );
        assert_diag(&h, b"HY090");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(dbc(&h).inner.lock().unwrap().database_read_only.is_none());
    }

    #[test]
    fn info_truncation_and_length_queries_keep_the_shared_answer() {
        let (h, _server) = fixture(response("Y"));
        let mut terminator = 0xffffu16;
        let mut length = -1;
        assert_eq!(
            unsafe {
                SQLGetInfoW(
                    h.dbc,
                    SQL_DATA_SOURCE_READ_ONLY,
                    ptr::from_mut(&mut terminator).cast(),
                    2,
                    &mut length,
                )
            },
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(terminator, 0);
        assert_eq!(length, 2);
        assert_diag(&h, b"01004");
        assert_eq!(
            unsafe {
                SQLGetInfoW(
                    h.dbc,
                    SQL_DATA_SOURCE_READ_ONLY,
                    ptr::null_mut(),
                    0,
                    &mut length,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(length, 2);
    }

    #[test]
    fn connection_timeout_bounds_idle_lookup_and_the_next_attempt_recovers() {
        let (h, server) = fixture(response("Y").with_delay(Duration::from_secs(8)));
        dbc(&h).set_connection_timeout(1);
        let started = Instant::now();
        assert_eq!(mode(h.dbc).0, SQL_ERROR);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(dbc(&h).inner.lock().unwrap().database_read_only.is_none());
        assert!(!dbc(&h).inner.lock().unwrap().diag_records.is_empty());
        server.register_query(READ_ONLY_QUERY, response("N"));
        assert_eq!(mode(h.dbc).1, SQL_MODE_READ_WRITE);
    }

    #[test]
    fn connection_reset_invalidates_a_cached_answer() {
        let (h, _server) = fixture(response("Y"));
        assert_eq!(info(h.dbc).1, "Y");
        assert_eq!(
            unsafe {
                SQLSetConnectAttrW(h.dbc, SQL_ATTR_RESET_CONNECTION, 1usize as SqlPointer, 0)
            },
            SQL_SUCCESS
        );
        assert!(dbc(&h).inner.lock().unwrap().database_read_only.is_none());
    }

    #[test]
    fn secondary_login_timeout_is_reported_and_original_session_remains_usable() {
        let (h, server) = fixture(response("Y"));
        open_pending_result(&h, &server);
        dbc(&h).set_connection_timeout(1);
        server.set_login_delay(Duration::from_secs(8));
        let started = Instant::now();
        assert_eq!(info(h.dbc).0, SQL_ERROR);
        assert!(started.elapsed() < Duration::from_secs(5));
        {
            let state = dbc(&h).inner.lock().unwrap();
            assert!(state.wire.client().is_some());
            assert_eq!(state.wire.statement_owner(), Some(h.stmt));
            assert!(state.database_read_only.is_none());
            assert!(!state.diag_records.is_empty());
        }
        assert_row(&h, 2);
        assert_row(&h, 3);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
    }

    #[test]
    fn lookup_replaces_stale_diagnostics() {
        let (h, _server) = fixture(response("N"));
        post_sql_error(&mut *dbc(&h).inner.lock().unwrap(), *b"HY000", 42, "old");
        assert_eq!(mode(h.dbc).0, SQL_SUCCESS);
        assert!(dbc(&h).inner.lock().unwrap().diag_records.is_empty());
    }

    #[test]
    fn lookup_does_not_leak_internal_info_messages() {
        use mssql_mock_tds::InfoMessage;

        let (h, _server) = fixture(response("N").with_info_tokens(vec![InfoMessage::new(
            5701,
            10,
            "internal lookup message",
        )]));
        assert_eq!(mode(h.dbc).0, SQL_SUCCESS);
        let mut state = dbc(&h).inner.lock().unwrap();
        assert!(state.diag_records.is_empty());
        assert!(
            state
                .wire
                .client_mut_for_test()
                .unwrap()
                .take_info_messages()
                .is_empty()
        );
    }

    #[test]
    fn reset_during_lookup_discards_the_answer_but_returns_the_client() {
        let (h, _server) = fixture(response("Y"));
        let (client, generation, reset_generation) = {
            let mut state = dbc(&h).inner.lock().unwrap();
            let reset_generation = state.reset_generation;
            state.reset_generation = reset_generation.wrapping_add(1);
            (
                state
                    .wire
                    .claim(WireOwner::Connection, ClaimPolicy::IdleOnly)
                    .unwrap(),
                state.session_generation,
                reset_generation,
            )
        };
        let cache = CachedDatabaseReadOnly {
            catalog: client.database().to_string(),
            database_change_count: client.database_change_count(),
            read_only: true,
        };
        assert_eq!(
            publish(
                dbc(&h),
                Some(client),
                generation,
                reset_generation,
                Ok(cache)
            ),
            Err(SQL_ERROR)
        );
        assert_diag(&h, b"HY010");
        let state = dbc(&h).inner.lock().unwrap();
        assert!(state.wire.client().is_some());
        assert!(state.database_read_only.is_none());
    }

    #[test]
    fn secondary_answer_for_a_replaced_session_or_database_is_not_cached() {
        let (h, _server) = fixture(response("Y"));
        for replace_session in [false, true] {
            let (generation, reset_generation, cache) = {
                let mut state = dbc(&h).inner.lock().unwrap();
                let client = state.wire.client().unwrap();
                let cache = CachedDatabaseReadOnly {
                    catalog: if replace_session {
                        client.database().to_string()
                    } else {
                        "replaced_database".to_string()
                    },
                    database_change_count: client.database_change_count(),
                    read_only: true,
                };
                let generation = state.session_generation;
                if replace_session {
                    state.session_generation = generation.wrapping_add(1);
                }
                (generation, state.reset_generation, cache)
            };
            assert_eq!(
                publish(dbc(&h), None, generation, reset_generation, Ok(cache)),
                Err(SQL_ERROR)
            );
            let state = dbc(&h).inner.lock().unwrap();
            assert!(state.database_read_only.is_none());
            assert!(state.wire.client().is_some());
        }
    }

    #[test]
    fn secondary_read_timeout_leaves_the_original_session_untouched() {
        let (h, server) = fixture(response("Y").with_delay(Duration::from_secs(8)));
        open_pending_result(&h, &server);
        dbc(&h).set_connection_timeout(1);
        let started = Instant::now();
        assert_eq!(mode(h.dbc).0, SQL_ERROR);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(server.connection_count(), 2);
        {
            let state = dbc(&h).inner.lock().unwrap();
            assert!(state.wire.client().is_some());
            assert_eq!(state.wire.statement_owner(), Some(h.stmt));
            assert!(state.database_read_only.is_none());
        }
        assert_row(&h, 2);
        assert_row(&h, 3);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
    }

    #[test]
    fn secondary_lookup_future_stays_small() {
        let size = std::mem::size_of_val(&read_on_secondary(
            Box::default(),
            1,
            "Database updateability lookup timed out",
            read_updateability,
        ));
        assert!(size <= 4096, "secondary lookup future is {size} bytes");
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Alias type names for `SQL_DESC_TYPE_NAME` and `SQL_DESC_LOCAL_TYPE_NAME`,
//! from msodbcsql's per-connection user-type cache
//! (`sqlccmd.cpp::RefreshShilohUDTCache`).
//!
//! `COLMETADATA` identifies an alias type (`sp_addtype`, `CREATE TYPE ...
//! FROM`) only by its database-scoped `UserType`, so the names are read from
//! `systypes` when an alias-typed column is first described and again after the
//! session's database changes, the event that sets msodbcsql's
//! `CONN_ST_REFRESH_UDT` on a supported server (`sqlctokn.cpp:2866-2882`).
//! msodbcsql also re-reads after an informational message reading exactly
//! `Type added.` (`sqlcerr.cpp`), which no supported server sends: neither
//! `CREATE TYPE` nor `sp_addtype` reports anything on SQL Server 2025. A type
//! created after the read is therefore described by its base type until the
//! database changes, in both drivers.
//!
//! msodbcsql reads these names in the same batch as `USER_NAME()`. They are
//! looked up separately here because the two differ while a cursor is open: a
//! column is usually first described with its rows still pending, so this
//! lookup then logs in separately, as msodbcsql does, while `SQL_USER_NAME`
//! keeps to the session (AB#48942). A separate login's `USER_NAME()` need not
//! be the session's principal.

use std::collections::HashMap;
use std::time::Duration;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, StatementResult, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::error::{Error as TdsError, TimeoutErrorType};
use tracing::{debug, error};

use super::txn::try_claim_idle_dbc_client;
use crate::handles::StmtHandle;
use crate::handles::dbc::{AliasTypeCache, ConnectionState, DbcHandle};

/// msodbcsql's `MIN_REAL_USER_DATATYPE`: lower `UserType` values are system
/// types.
const MIN_ALIAS_USER_TYPE: u32 = 256;

/// The alias half of msodbcsql's `g_szSqlUdtQuery` (`sqlcstr.cpp`), with
/// `MIN_REAL_USER_DATATYPE` substituted. On the idle route it runs on the
/// application's session, where reading `systypes` would begin a transaction
/// if the application had set `IMPLICIT_TRANSACTIONS ON`. As in msodbcsql, the
/// option is turned off only inside `sp_executesql`, which restores it when
/// the batch returns.
const ALIAS_TYPE_QUERY: &str = "EXEC sp_executesql N'SET IMPLICIT_TRANSACTIONS OFF; \
                                SELECT usertype, name FROM systypes WHERE usertype >= 256'";

/// Where the lookup runs.
enum Route {
    /// The application's own session, which has no results pending.
    Session,
    /// A separate login, because the session has results pending. The names
    /// are keyed to the session's state when the login was decided on.
    Secondary {
        context: Box<ClientContext>,
        catalog: String,
        database_change_count: u64,
    },
}

/// A handle mutex poisoned by an earlier panic, which the entry point reports
/// as `SQL_ERROR` like any other lock failure.
pub(super) struct Poisoned;

/// An alias type name and the result set it was read for.
pub(super) struct ColumnAlias {
    pub(super) name: String,
    pub(super) result_generation: u64,
}

/// Returns the alias name of a result column whose `UserType` names an alias
/// type, or `None` to report its base type. The lookup runs without the
/// statement lock, so the caller applies the name only while
/// `result_generation` is still current.
pub(super) fn alias_type_name_for_column(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    column_number: usize,
) -> Result<Option<ColumnAlias>, Poisoned> {
    let (user_type, result_generation) = {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("SQLColAttributeW: stmt mutex poisoned during alias-type lookup");
            return Err(Poisoned);
        };
        let Some(column) = column_number
            .checked_sub(1)
            .and_then(|index| stmt_state.column_metadata.get(index))
        else {
            return Ok(None);
        };
        (column.user_type, stmt_state.result_generation)
    };
    if user_type < MIN_ALIAS_USER_TYPE {
        return Ok(None);
    }

    refresh_alias_types(dbc);

    let Ok(state) = dbc.inner.lock() else {
        error!("SQLColAttributeW: dbc mutex poisoned during alias-type lookup");
        return Err(Poisoned);
    };
    // While another call has the session checked out, the key cannot be
    // checked: a `USE` in that call may have moved the session to a database
    // where the same `UserType` names another type.
    let (Some(cache), Some(client)) = (state.alias_types.as_ref(), state.client.as_ref()) else {
        return Ok(None);
    };
    if !is_current(cache, client) {
        return Ok(None);
    }
    Ok(cache.names.get(&user_type).map(|name| ColumnAlias {
        name: name.clone(),
        result_generation,
    }))
}

fn is_current(cache: &AliasTypeCache, client: &TdsClient) -> bool {
    client.database() == cache.catalog
        && client.database_change_count() == cache.database_change_count
}

/// Reads the alias type names unless the cache is current for the session.
/// Best effort, as in msodbcsql: a failed read is logged, changes nothing, and
/// is retried by the next description of an alias-typed column.
fn refresh_alias_types(dbc: &DbcHandle) {
    let (generation, timeout_secs, route) = {
        let Ok(state) = dbc.inner.lock() else {
            error!("alias-type lookup: dbc mutex poisoned");
            return;
        };
        if state.connection_state != ConnectionState::Connected {
            return;
        }
        // Absent only while another call has the session checked out.
        let Some(client) = state.client.as_ref() else {
            return;
        };
        if state
            .alias_types
            .as_ref()
            .is_some_and(|cache| is_current(cache, client))
        {
            return;
        }
        // msodbcsql queries the session itself unless results are pending on
        // it, and only then logs in separately.
        let route = if state.active_stmt.is_none() {
            Route::Session
        } else {
            match client.secondary_client_context() {
                Some(context) if !prompts_for_sign_in(&context) => Route::Secondary {
                    context: Box::new(context),
                    catalog: client.database().to_owned(),
                    database_change_count: client.database_change_count(),
                },
                _ => {
                    debug!("alias-type lookup: no silent secondary login; using base type names");
                    return;
                }
            }
        };
        // The connection timeout bounds the whole lookup on either route,
        // including the secondary login. msodbcsql bounds its read on the
        // session the same way (`GetNetIOTimeOut`), but on a spawned
        // connection it reads that connection's timeout, which is unset, so
        // its lookup can wait forever behind the session's own uncommitted
        // type change (parity-deviations entry 32).
        (state.session_generation, state.connection_timeout, route)
    };

    match route {
        Route::Session => {
            let Some((mut client, generation, _)) = try_claim_idle_dbc_client(dbc) else {
                return;
            };
            let names = dbc
                .runtime
                .block_on(read_alias_types(&mut client, timeout_secs));
            // Keyed to where the names came from: `execute` can reconnect a
            // dropped session onto the login's default database.
            let catalog = client.database().to_owned();
            let database_change_count = client.database_change_count();
            // Internal, like the diagnostics msodbcsql frees with its own
            // statement handle.
            let _ = client.take_info_messages();
            let lookup = names.map(|names| AliasTypeCache {
                catalog,
                database_change_count,
                names,
            });
            publish(dbc, Some(client), generation, lookup);
        }
        Route::Secondary {
            context,
            catalog,
            database_change_count,
        } => {
            let names = dbc
                .runtime
                .block_on(read_on_secondary(context, timeout_secs));
            let lookup = names.map(|names| AliasTypeCache {
                catalog,
                database_change_count,
                names,
            });
            publish(dbc, None, generation, lookup);
        }
    }
}

/// Hands a claimed session back and installs the names in one critical
/// section, for the reasons `get_info::publish_lookup` gives. A session that
/// turned over during the lookup keeps neither.
fn publish(
    dbc: &DbcHandle,
    client: Option<TdsClient>,
    generation: u64,
    lookup: TdsResult<AliasTypeCache>,
) {
    let mut state = match dbc.inner.lock() {
        Ok(state) => state,
        Err(mut poisoned) => {
            error!("alias-type lookup: dbc mutex poisoned");
            // A claimed client cannot be stored, so the DBC must not keep
            // claiming to be connected, as in `release_dbc_client`.
            if client.is_some() {
                poisoned.get_mut().connection_state = ConnectionState::Disconnected;
            }
            return;
        }
    };
    if state.session_generation != generation
        || state.connection_state != ConnectionState::Connected
    {
        debug!("alias-type lookup: session replaced during the lookup; discarding it");
        return;
    }
    if client.is_some() {
        state.client = client;
    }
    match lookup {
        Ok(cache) => state.alias_types = Some(cache),
        Err(e) => debug!(%e, "alias-type lookup failed; using base type names"),
    }
}

/// msodbcsql's `SpawnConnection` (`sqlcinfo.cpp`) does not copy the session's
/// `Authentication` keyword, so its secondary login raises no sign-in prompt
/// mid-call. Methods that may prompt are not replayed.
fn prompts_for_sign_in(context: &ClientContext) -> bool {
    matches!(
        context.tds_authentication_method,
        TdsAuthenticationMethod::ActiveDirectoryInteractive
            | TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow
    )
}

/// One deadline covers the login and the read. Without one, the login keeps
/// the login timeout it inherits, which `SpawnConnection` also copies.
///
/// The caller polls this on the application's thread, so the login and read
/// are boxed, as `create_client` boxes its connection, to leave room on small
/// application stacks such as musl's 128 KiB threads.
async fn read_on_secondary(
    context: Box<ClientContext>,
    timeout_secs: u32,
) -> TdsResult<HashMap<u32, String>> {
    let lookup = Box::pin(async move {
        let data_source = context.data_source.clone();
        let mut client = TdsConnectionProvider::new()
            .create_client(*context, &data_source, None)
            .await?;
        debug!(
            client_connection_id = %client.client_connection_id(),
            "alias-type lookup: logged in separately"
        );
        read_alias_types(&mut client, 0).await
    });
    if timeout_secs == 0 {
        return lookup.await;
    }
    tokio::time::timeout(Duration::from_secs(timeout_secs.into()), lookup)
        .await
        .unwrap_or_else(|_| {
            Err(TdsError::TimeoutError(TimeoutErrorType::String(
                "alias-type lookup timed out".to_string(),
            )))
        })
}

/// Runs the lookup and leaves no batch open, even when it fails part-way, for
/// the reasons `fetch_database_user_name` gives.
async fn read_alias_types(
    client: &mut TdsClient,
    timeout_secs: u32,
) -> TdsResult<HashMap<u32, String>> {
    let first = match client
        .execute(
            ALIAS_TYPE_QUERY.to_string(),
            ExecuteOptions::new().timeout_secs(timeout_secs),
        )
        .await
    {
        Ok(first) => first,
        Err(error) => {
            if client.has_open_batch() {
                let _ = client.close_query().await;
            }
            return Err(error);
        }
    };

    let read = async {
        let on_rows = match first {
            StatementResult::Rows => true,
            StatementResult::NoRows { .. } => client.advance_to_rows().await?,
            StatementResult::End => false,
        };
        let mut names = HashMap::new();
        if on_rows {
            while let Some(row) = client.next_row().await? {
                if let [user_type, name] = row.as_slice()
                    && let (Some(user_type), Some(name)) = (type_id(user_type), string_value(name))
                {
                    names.insert(user_type, name);
                }
            }
        }
        Ok::<_, TdsError>(names)
    }
    .await;

    let closed = if client.has_open_batch() {
        client.close_query().await
    } else {
        Ok(())
    };
    let names = read?;
    closed?;
    Ok(names)
}

fn string_value(value: &ColumnValues) -> Option<String> {
    match value {
        ColumnValues::String(value) => Some(value.to_utf8_string()),
        _ => None,
    }
}

fn type_id(value: &ColumnValues) -> Option<u32> {
    match value {
        ColumnValues::SmallInt(value) => u32::try_from(*value).ok(),
        ColumnValues::Int(value) => u32::try_from(*value).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::time::{Duration, Instant};

    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, LeadingError, QueryResponse, Row, SqlDataType,
    };
    use mssql_tds::test_client_support::int_columns;

    use super::*;
    use crate::api::col_attribute::sql_col_attribute_w;
    use crate::api::odbc_types::{
        SQL_DESC_LOCAL_TYPE_NAME, SQL_DESC_TYPE_NAME, SQL_ERROR, SQL_SUCCESS, SqlHandle,
        SqlPointer, SqlSmallInt, SqlUSmallInt,
    };
    use crate::api::sqlstate::ERR_INVALID_DESCRIPTOR_INDEX;
    use crate::api::txn::release_dbc_client;
    use crate::handles::handle_from_raw;
    use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;
    use crate::test_support::{TestHandles, connect_mock_server};

    /// `ALIAS_TYPE_QUERY`'s result: one `(usertype, name)` row per alias type.
    fn lookup_response(alias_types: &[(i16, &str)]) -> QueryResponse {
        QueryResponse::new(
            vec![
                ColumnDefinition::new("usertype", SqlDataType::SmallInt),
                ColumnDefinition::new("name", SqlDataType::NVarChar),
            ],
            alias_types
                .iter()
                .map(|(user_type, name)| {
                    Row::new(vec![
                        ColumnValue::SmallInt(*user_type),
                        ColumnValue::NVarChar(name.to_string()),
                    ])
                })
                .collect(),
        )
    }

    fn dbc(h: &TestHandles) -> &DbcHandle {
        unsafe { handle_from_raw::<DbcHandle>(h.dbc) }
    }

    /// Positions `h.stmt` on `int` columns carrying the given `UserType`s.
    fn describe_columns(h: &TestHandles, user_types: &[u32]) {
        let mut columns = int_columns(user_types.len());
        for (column, user_type) in columns.iter_mut().zip(user_types) {
            column.user_type = *user_type;
        }
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut stmt_state = stmt.inner.lock().unwrap();
        stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata = columns;
    }

    /// Leaves an application cursor open on the session with its row unread,
    /// so a lookup sent on the session would be refused.
    fn hold_session(h: &TestHandles) {
        let dbc = dbc(h);
        let mut state = dbc.inner.lock().unwrap();
        let client = state.client.as_mut().unwrap();
        dbc.runtime
            .block_on(client.execute("SELECT 1".to_string(), ()))
            .unwrap();
        state.active_stmt = Some(h.stmt);
    }

    fn type_name(h: &TestHandles, col: SqlUSmallInt, field: SqlUSmallInt) -> String {
        type_name_of(h.stmt, col, field)
    }

    fn type_name_of(stmt: SqlHandle, col: SqlUSmallInt, field: SqlUSmallInt) -> String {
        let mut buf = [0u16; 128];
        let mut written: SqlSmallInt = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                stmt,
                col,
                field,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * 2) as SqlSmallInt,
                &mut written,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "field {field}");
        String::from_utf16_lossy(&buf[..(written as usize) / 2])
    }

    /// A cache entry keyed to the session's current state.
    fn current_cache(dbc: &DbcHandle, names: &[(u32, &str)]) -> AliasTypeCache {
        let state = dbc.inner.lock().unwrap();
        let client = state.client.as_ref().unwrap();
        AliasTypeCache {
            catalog: client.database().to_owned(),
            database_change_count: client.database_change_count(),
            names: names
                .iter()
                .map(|(user_type, name)| (*user_type, name.to_string()))
                .collect(),
        }
    }

    #[test]
    fn alias_columns_report_the_alias_name_and_others_their_base_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let server = connect_mock_server(
            dbc(&h),
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]),
        );
        describe_columns(&h, &[257, 258, 0]);

        for field in [SQL_DESC_TYPE_NAME, SQL_DESC_LOCAL_TYPE_NAME] {
            assert_eq!(type_name(&h, 1, field), "phone_number", "field {field}");
            assert_eq!(type_name(&h, 2, field), "int", "field {field}");
            assert_eq!(type_name(&h, 3, field), "int", "field {field}");
        }

        assert_eq!(server.connection_count(), 1, "the idle session answers");
        let expected = current_cache(dbc(&h), &[(257, "phone_number")]);
        let state = dbc(&h).inner.lock().unwrap();
        assert_eq!(state.alias_types, Some(expected));
        assert!(state.active_stmt.is_none());
    }

    #[test]
    fn busy_session_is_left_alone_and_a_secondary_login_reads_the_names() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]),
        );
        hold_session(&h);
        describe_columns(&h, &[257]);

        assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "phone_number");

        assert_eq!(server.connection_count(), 2);
        let expected = current_cache(dbc, &[(257, "phone_number")]);
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.alias_types, Some(expected));
        assert_eq!(state.active_stmt, Some(h.stmt));
        let client = state.client.as_ref().unwrap();
        assert!(client.has_open_batch());
        assert_eq!(
            dbc.trace.client_connection_id(),
            Some(client.client_connection_id()),
            "SQL_COPT_SS_CLIENT_CONNECTION_ID keeps naming the session"
        );
        assert_eq!(
            state.database_user_name, None,
            "SQL_USER_NAME keeps to the session"
        );
    }

    #[test]
    fn names_read_for_the_current_session_state_are_not_read_again() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let _server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]),
        );
        let kept = current_cache(dbc, &[(257, "kept")]);
        dbc.inner.lock().unwrap().alias_types = Some(kept.clone());
        describe_columns(&h, &[257]);

        assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "kept");
        assert_eq!(dbc.inner.lock().unwrap().alias_types, Some(kept));
    }

    /// The same `UserType` can name different types in different databases, so
    /// names read under another catalog or before a database change are read
    /// again rather than reported.
    #[test]
    fn names_read_under_other_session_state_are_read_again() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let _server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]),
        );
        describe_columns(&h, &[257]);
        let current = current_cache(dbc, &[(257, "phone_number")]);

        let mut other_catalog = current_cache(dbc, &[(257, "invoice_number")]);
        other_catalog.catalog = "sales".to_string();
        let mut earlier_change = current_cache(dbc, &[(257, "invoice_number")]);
        earlier_change.database_change_count = earlier_change.database_change_count.wrapping_add(1);

        for stale in [other_catalog, earlier_change] {
            dbc.inner.lock().unwrap().alias_types = Some(stale);
            assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "phone_number");
            assert_eq!(dbc.inner.lock().unwrap().alias_types, Some(current.clone()));
        }
    }

    /// Another call holding the session may have run a `USE`, after which the
    /// same `UserType` can name a different type, so names that cannot be
    /// checked against the session are not reported.
    #[test]
    fn names_are_not_reported_while_another_call_holds_the_session() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]),
        );
        let mut stale = current_cache(dbc, &[(257, "invoice_number")]);
        stale.catalog = "sales".to_string();
        dbc.inner.lock().unwrap().alias_types = Some(stale.clone());
        describe_columns(&h, &[257]);
        let (client, _, _) = try_claim_idle_dbc_client(dbc).expect("the session is idle");

        assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "int");

        assert_eq!(server.connection_count(), 1, "nothing is read meanwhile");
        assert_eq!(dbc.inner.lock().unwrap().alias_types, Some(stale));
        release_dbc_client(dbc, client);
    }

    /// The lookup runs without the statement lock, so another call can replace
    /// the result while it waits; the name read for the old result is not
    /// applied to the new one.
    #[test]
    fn a_name_read_for_a_replaced_result_is_not_applied() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let _server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]).with_delay(Duration::from_secs(2)),
        );
        describe_columns(&h, &[257]);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let raw = h.stmt as usize;

        let name = std::thread::scope(|scope| {
            let describe =
                scope.spawn(move || type_name_of(raw as SqlHandle, 1, SQL_DESC_TYPE_NAME));
            let deadline = Instant::now() + Duration::from_secs(2);
            while dbc.inner.lock().unwrap().client.is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                dbc.inner.lock().unwrap().client.is_none(),
                "the lookup holds the session"
            );
            stmt.inner.lock().unwrap().begin_batch(int_columns(1));
            describe.join().unwrap()
        });

        assert_eq!(name, "int");
        let expected = current_cache(dbc, &[(257, "phone_number")]);
        assert_eq!(dbc.inner.lock().unwrap().alias_types, Some(expected));
    }

    #[test]
    fn a_poisoned_connection_fails_an_alias_columns_type_name() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        describe_columns(&h, &[257]);
        let type_name_rc = |col: SqlUSmallInt| {
            let mut buf = [0u16; 16];
            unsafe {
                sql_col_attribute_w(
                    h.stmt,
                    col,
                    SQL_DESC_TYPE_NAME,
                    buf.as_mut_ptr() as SqlPointer,
                    (buf.len() * 2) as SqlSmallInt,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            }
        };
        assert_eq!(type_name_rc(2), SQL_ERROR);
        assert_eq!(
            stmt.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            ERR_INVALID_DESCRIPTOR_INDEX.state
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = dbc.inner.lock().unwrap();
            panic!("poison the dbc lock");
        }));

        assert_eq!(type_name_rc(1), SQL_ERROR);

        assert!(
            stmt.inner.lock().unwrap().diag_records.is_empty(),
            "the previous call's 07009 is not reported for this one"
        );
        dbc.inner.clear_poison();
    }

    #[test]
    fn failed_lookup_changes_nothing_and_leaves_the_session_usable() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let _server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]).with_leading_error(LeadingError::new(
                229,
                14,
                "The SELECT permission was denied on the object 'systypes'.",
            )),
        );
        let mut stale = current_cache(dbc, &[(257, "invoice_number")]);
        stale.catalog = "sales".to_string();
        dbc.inner.lock().unwrap().alias_types = Some(stale.clone());
        describe_columns(&h, &[257]);

        // Names read for another database never describe this column.
        assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "int");

        let mut state = dbc.inner.lock().unwrap();
        assert_eq!(state.alias_types, Some(stale));
        assert!(state.active_stmt.is_none());
        let client = state.client.as_mut().unwrap();
        assert!(!client.has_open_batch());
        assert!(matches!(
            dbc.runtime
                .block_on(client.execute("SELECT 1".to_string(), ())),
            Ok(StatementResult::Rows)
        ));
    }

    #[test]
    fn connection_timeout_bounds_the_lookup() {
        let h = TestHandles::with_env_dbc_stmt();
        let dbc = dbc(&h);
        let _server = connect_mock_server(
            dbc,
            ALIAS_TYPE_QUERY,
            lookup_response(&[(257, "phone_number")]).with_delay(Duration::from_secs(10)),
        );
        dbc.inner.lock().unwrap().connection_timeout = 1;
        describe_columns(&h, &[257]);

        let started = Instant::now();
        assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "int");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.alias_types, None);
        assert!(state.client.is_some());
    }

    #[test]
    fn connection_timeout_bounds_a_secondary_login_and_its_read() {
        let names = || lookup_response(&[(257, "phone_number")]);
        for (stalled, response, login_delay) in [
            ("login", names(), Some(Duration::from_secs(10))),
            ("read", names().with_delay(Duration::from_secs(10)), None),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let dbc = dbc(&h);
            let server = connect_mock_server(dbc, ALIAS_TYPE_QUERY, response);
            hold_session(&h);
            if let Some(delay) = login_delay {
                server.set_login_delay(delay);
            }
            dbc.inner.lock().unwrap().connection_timeout = 1;
            describe_columns(&h, &[257]);

            let started = Instant::now();
            assert_eq!(type_name(&h, 1, SQL_DESC_TYPE_NAME), "int", "{stalled}");

            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{stalled}: {:?}",
                started.elapsed()
            );
            let state = dbc.inner.lock().unwrap();
            assert_eq!(state.alias_types, None, "{stalled}");
            assert_eq!(state.active_stmt, Some(h.stmt), "{stalled}");
            assert!(state.client.as_ref().unwrap().has_open_batch(), "{stalled}");
        }
    }

    #[test]
    fn a_session_replaced_during_the_lookup_keeps_nothing() {
        let h = TestHandles::with_env_dbc();
        let dbc = dbc(&h);
        let _server = connect_mock_server(dbc, ALIAS_TYPE_QUERY, lookup_response(&[]));
        let names = current_cache(dbc, &[(257, "phone_number")]);
        let generation = {
            let mut state = dbc.inner.lock().unwrap();
            let generation = state.session_generation;
            state.session_generation = generation.wrapping_add(1);
            generation
        };

        publish(dbc, None, generation, Ok(names));

        assert_eq!(dbc.inner.lock().unwrap().alias_types, None);
    }

    /// The caller polls the secondary login on the application's thread, so
    /// its future stays within the bound `create_client`'s connection future
    /// keeps for small application stacks.
    #[test]
    fn secondary_lookup_future_stays_small() {
        const MAX: usize = 4096;
        let size = std::mem::size_of_val(&read_on_secondary(Box::default(), 1));
        assert!(
            size <= MAX,
            "secondary lookup future is {size} B, expected <= {MAX} B"
        );
    }

    #[test]
    fn methods_that_may_prompt_are_never_replayed() {
        let mut context = ClientContext::default();
        for (method, prompts) in [
            (TdsAuthenticationMethod::Password, false),
            (TdsAuthenticationMethod::SSPI, false),
            (
                TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
                false,
            ),
            (TdsAuthenticationMethod::ActiveDirectoryInteractive, true),
            (TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow, true),
        ] {
            context.tds_authentication_method = method.clone();
            assert_eq!(prompts_for_sign_in(&context), prompts, "{method:?}");
        }
    }
}

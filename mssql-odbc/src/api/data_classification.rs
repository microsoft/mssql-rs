// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `SQL_CA_SS_DATA_CLASSIFICATION` payload.
//!
//! msodbcsql hands the application the DATACLASSIFICATION token's payload,
//! every byte after the `0xA3` type, exactly as the server sent it
//! (`Parse.cpp` `ProcessDataClassification`, `sqlctokn.cpp`
//! `OnDataClassification`). mssql-tds exposes only validated metadata, which
//! keeps both dictionaries in wire order with the raw indexes and ranks, so
//! [`payload`] writes it back in the [MS-TDS] 2.2.7.5 layout. For any token the
//! parser accepts, the bytes match the server's, except that an unpaired
//! surrogate the parser replaced with U+FFFD is returned as U+FFFD.

use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::token::data_classification::{
    DataClassificationMetadata, InformationTypeReference, SensitivityLabelReference,
    SensitivityRank,
};
use tracing::error;

/// The wire index of an unspecified label or information type.
const UNSPECIFIED_INDEX: u16 = 0xFFFF;

/// Captures the classification of the result set `client` is positioned on,
/// as the payload the IRD reports.
pub(super) fn capture(client: &TdsClient) -> Option<Box<[u8]>> {
    let metadata = client.data_classification()?;
    let payload = payload(metadata);
    if payload.is_none() {
        error!("data classification metadata does not fit the DATACLASSIFICATION layout");
    }
    payload
}

/// Serializes `metadata` in the DATACLASSIFICATION payload layout. `None` only
/// for metadata no parsed token can produce.
pub(super) fn payload(metadata: &DataClassificationMetadata) -> Option<Box<[u8]>> {
    let mut out = Vec::with_capacity(capacity(metadata));
    write_count(&mut out, metadata.sensitivity_labels().len())?;
    for label in metadata.sensitivity_labels() {
        write_b_varchar(&mut out, label.name())?;
        write_b_varchar(&mut out, label.id())?;
    }
    write_count(&mut out, metadata.information_types().len())?;
    for information_type in metadata.information_types() {
        write_b_varchar(&mut out, information_type.name())?;
        write_b_varchar(&mut out, information_type.id())?;
    }
    // The parser reads a rank exactly when the negotiated version is 2, so its
    // presence reproduces the version-dependent layout.
    if let Some(rank) = metadata.rank() {
        out.extend_from_slice(&rank_value(rank)?.to_le_bytes());
    }
    write_count(&mut out, metadata.columns().len())?;
    for column in metadata.columns() {
        write_count(&mut out, column.properties().len())?;
        for property in column.properties() {
            let label = match property.sensitivity_label() {
                SensitivityLabelReference::Unspecified => UNSPECIFIED_INDEX,
                SensitivityLabelReference::Index(index) => index,
            };
            let information_type = match property.information_type() {
                InformationTypeReference::Unspecified => UNSPECIFIED_INDEX,
                InformationTypeReference::Index(index) => index,
            };
            out.extend_from_slice(&label.to_le_bytes());
            out.extend_from_slice(&information_type.to_le_bytes());
            if let Some(rank) = property.rank() {
                out.extend_from_slice(&rank_value(rank)?.to_le_bytes());
            }
        }
    }
    Some(out.into_boxed_slice())
}

/// An upper bound on the payload size, so it is built with one allocation.
/// Twice a string's UTF-8 length bounds its UTF-16 byte length.
fn capacity(metadata: &DataClassificationMetadata) -> usize {
    let strings: usize =
        metadata
            .sensitivity_labels()
            .iter()
            .map(|label| label.name().len() + label.id().len())
            .chain(metadata.information_types().iter().map(|information_type| {
                information_type.name().len() + information_type.id().len()
            }))
            .sum();
    let dictionary_entries =
        metadata.sensitivity_labels().len() + metadata.information_types().len();
    let properties: usize = metadata
        .columns()
        .iter()
        .map(|column| column.properties().len())
        .sum();
    10 + 2 * strings + 2 * dictionary_entries + 2 * metadata.columns().len() + 8 * properties
}

fn write_count(out: &mut Vec<u8>, count: usize) -> Option<()> {
    out.extend_from_slice(&u16::try_from(count).ok()?.to_le_bytes());
    Some(())
}

/// A `B_VARCHAR`: a one-byte UTF-16 code-unit count, then the units.
fn write_b_varchar(out: &mut Vec<u8>, value: &str) -> Option<()> {
    out.push(u8::try_from(value.encode_utf16().count()).ok()?);
    for unit in value.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    Some(())
}

fn rank_value(rank: SensitivityRank) -> Option<i32> {
    // `SensitivityRank` is non-exhaustive, so a cast is not available here.
    match rank {
        SensitivityRank::NotDefined => Some(-1),
        SensitivityRank::None => Some(0),
        SensitivityRank::Low => Some(10),
        SensitivityRank::Medium => Some(20),
        SensitivityRank::High => Some(30),
        SensitivityRank::Critical => Some(40),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, ConnectionStore, DataClassificationAck, MockTdsServer,
        QueryRegistry, QueryResponse, ResultSetResponse, Row, SqlDataType, TerminalError,
    };
    use tokio::sync::Mutex;

    use crate::api::close_cursor::sql_close_cursor;
    use crate::api::col_attribute::sql_col_attribute_w;
    use crate::api::disconnect::sql_disconnect;
    use crate::api::driver_connect::sql_driver_connect_w;
    use crate::api::exec_direct::sql_exec_direct_w;
    use crate::api::execute::sql_execute;
    use crate::api::get_connect_attr::sql_get_connect_attr_w;
    use crate::api::get_desc_field::sql_get_desc_field_w;
    use crate::api::more_results::sql_more_results;
    use crate::api::odbc_types::{
        SQL_CA_SS_DATA_CLASSIFICATION, SQL_CA_SS_DATA_CLASSIFICATION_VERSION,
        SQL_COPT_SS_DATACLASSIFICATION_VERSION, SQL_DRIVER_NOPROMPT, SQL_ERROR, SQL_NO_DATA,
        SQL_NTS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
        SqlSmallInt, SqlWChar,
    };
    use crate::api::prepare::sql_prepare_w;
    use crate::api::set_connect_attr::sql_set_connect_attr_w;
    use crate::api::set_desc_field::sql_set_desc_field_w;
    use crate::api::sqlstate::{
        ERR_CANNOT_MODIFY_IRD, ERR_INVALID_ATTRIBUTE_VALUE, ERR_INVALID_DESCRIPTOR_FIELD,
        WARN_STRING_TRUNCATION,
    };
    use crate::error::diag::DiagRecord;
    use crate::handles::{DbcHandle, DescHandle, StmtHandle, handle_from_raw};
    use crate::test_support::{TestHandles, assert_last_diag, cs};

    const CLASSIFIED: &str = "SELECT SSN, PHONE, ID FROM PEOPLE";
    const UNCLASSIFIED: &str = "SELECT ID FROM PEOPLE";
    const THREE_RESULT_SETS: &str = "SELECT SSN FROM A; SELECT ID FROM B; SELECT EMAIL FROM C";
    const UNSPECIFIED: u16 = 0xFFFF;
    const DATA_CLASSIFICATION: SqlSmallInt = SQL_CA_SS_DATA_CLASSIFICATION as SqlSmallInt;
    const DATA_CLASSIFICATION_VERSION: SqlSmallInt =
        SQL_CA_SS_DATA_CLASSIFICATION_VERSION as SqlSmallInt;

    /// `(label index, information-type index, rank)`; ranks are written only for v2.
    type Property = (u16, u16, i32);

    fn b_varchar(bytes: &mut Vec<u8>, value: &str) {
        let units: Vec<u16> = value.encode_utf16().collect();
        bytes.push(u8::try_from(units.len()).unwrap());
        units
            .iter()
            .for_each(|unit| bytes.extend_from_slice(&unit.to_le_bytes()));
    }

    /// A DATACLASSIFICATION token as SQL Server sends it, `0xA3` included.
    fn token(
        v2: bool,
        labels: &[(&str, &str)],
        information_types: &[(&str, &str)],
        rank: i32,
        columns: &[&[Property]],
    ) -> Vec<u8> {
        let mut bytes = vec![0xA3];
        for dictionary in [labels, information_types] {
            bytes.extend_from_slice(&u16::try_from(dictionary.len()).unwrap().to_le_bytes());
            for (name, id) in dictionary {
                b_varchar(&mut bytes, name);
                b_varchar(&mut bytes, id);
            }
        }
        if v2 {
            bytes.extend_from_slice(&rank.to_le_bytes());
        }
        bytes.extend_from_slice(&u16::try_from(columns.len()).unwrap().to_le_bytes());
        for properties in columns {
            bytes.extend_from_slice(&u16::try_from(properties.len()).unwrap().to_le_bytes());
            for (label, information_type, rank) in *properties {
                bytes.extend_from_slice(&label.to_le_bytes());
                bytes.extend_from_slice(&information_type.to_le_bytes());
                if v2 {
                    bytes.extend_from_slice(&rank.to_le_bytes());
                }
            }
        }
        bytes
    }

    /// SSN has one property; PHONE has two, each leaving one dictionary
    /// unspecified; ID is unclassified. Non-ASCII names, including a surrogate
    /// pair, check the UTF-16 round trip.
    fn classified_token(v2: bool) -> Vec<u8> {
        token(
            v2,
            &[("Confidentiel – données", "L1"), ("Public", "L0")],
            &[("National ID 🔒", "I1"), ("Contact Info", "I2")],
            40,
            &[
                &[(0, 0, 30)],
                &[(1, UNSPECIFIED, 10), (UNSPECIFIED, 1, -1)],
                &[],
            ],
        )
    }

    fn single_label_token(label: &str) -> Vec<u8> {
        token(false, &[(label, "L")], &[], 0, &[&[(0, UNSPECIFIED, 0)]])
    }

    fn int_response(columns: &[&str]) -> QueryResponse {
        QueryResponse::new(
            columns
                .iter()
                .map(|name| ColumnDefinition::new(*name, SqlDataType::Int))
                .collect(),
            vec![Row::new(
                columns.iter().map(|_| ColumnValue::Int(1)).collect(),
            )],
        )
    }

    struct Server {
        runtime: tokio::runtime::Runtime,
        address: std::net::SocketAddr,
        registry: Arc<Mutex<QueryRegistry>>,
        connections: Arc<Mutex<ConnectionStore>>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Server {
        fn start(ack: DataClassificationAck, register: impl FnOnce(&mut QueryRegistry)) -> Self {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let (address, registry, connections, shutdown) = runtime.block_on(async {
                let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
                let address = server.local_addr();
                let registry = server.query_registry();
                let connections = server.connection_store();
                {
                    let mut registry = registry.lock().await;
                    registry.set_data_classification_ack(ack);
                    register(&mut registry);
                }
                let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let _ = server.run_with_shutdown(shutdown_rx).await;
                });
                tokio::time::sleep(Duration::from_millis(50)).await;
                (address, registry, connections, shutdown)
            });
            Self {
                runtime,
                address,
                registry,
                connections,
                shutdown: Some(shutdown),
            }
        }

        fn set_ack(&self, ack: DataClassificationAck) {
            self.runtime.block_on(async {
                self.registry.lock().await.set_data_classification_ack(ack);
            });
        }

        fn register(&self, query: &str, response: QueryResponse) {
            self.runtime.block_on(async {
                self.registry.lock().await.register(query, response);
            });
        }

        fn connect(&self, dbc: SqlHandle) {
            let text: Vec<u16> = cs(&format!(
                "Server=tcp:{},{};UID=sa;<PW>=unused;Database=master;Encrypt=no;\
                 TrustServerCertificate=yes",
                self.address.ip(),
                self.address.port()
            ))
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
            let ret = unsafe {
                sql_driver_connect_w(
                    dbc,
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    SQL_NTS,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    SQL_DRIVER_NOPROMPT,
                )
            };
            assert!(
                matches!(ret, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
                "connect: {ret}"
            );
        }

        /// The version each login requested, in connection order.
        fn requested_versions(&self) -> Vec<Option<u8>> {
            self.runtime.block_on(async {
                let store = self.connections.lock().await;
                store
                    .all()
                    .keys()
                    .map(|id| {
                        store
                            .login_details(*id)
                            .and_then(|details| details.data_classification_requested)
                    })
                    .collect()
            })
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    /// Connects `h.dbc`, executes `sql` on a new statement, and returns it.
    fn execute(server: &Server, h: &mut TestHandles, sql: &str) -> SqlHandle {
        server.connect(h.dbc);
        let stmt = h.alloc_extra_stmt();
        exec(stmt, sql);
        stmt
    }

    fn exec(stmt: SqlHandle, sql: &str) {
        let text: Vec<SqlWChar> = sql.encode_utf16().collect();
        let ret = unsafe {
            sql_exec_direct_w(
                stmt,
                text.as_ptr(),
                SqlInteger::try_from(text.len()).unwrap(),
            )
        };
        assert!(
            matches!(ret, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
            "execute {sql}: {ret}"
        );
    }

    fn ird(stmt: SqlHandle) -> SqlHandle {
        unsafe { handle_from_raw::<StmtHandle>(stmt) }.ird
    }

    fn desc_diags(desc: SqlHandle) -> Vec<DiagRecord> {
        let desc = unsafe { handle_from_raw::<DescHandle>(desc) };
        desc.inner.lock().unwrap().diag_records.clone()
    }

    fn dbc_diags(dbc: SqlHandle) -> Vec<DiagRecord> {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        dbc.inner.lock().unwrap().diag_records.clone()
    }

    /// `SQLGetDescFieldW(SQL_CA_SS_DATA_CLASSIFICATION)` into a large buffer.
    fn classification(desc: SqlHandle) -> (SqlReturn, Vec<u8>) {
        let mut buffer = vec![0u8; 4096];
        let mut length: SqlInteger = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                desc,
                0,
                DATA_CLASSIFICATION,
                buffer.as_mut_ptr().cast(),
                SqlInteger::try_from(buffer.len()).unwrap(),
                &mut length,
            )
        };
        buffer.truncate(usize::try_from(length).unwrap_or(0));
        (ret, buffer)
    }

    fn classification_version(desc: SqlHandle) -> SqlInteger {
        let mut version: SqlInteger = -1;
        let mut length: SqlInteger = 0;
        let ret = unsafe {
            sql_get_desc_field_w(
                desc,
                0,
                DATA_CLASSIFICATION_VERSION,
                (&mut version as *mut SqlInteger).cast(),
                0,
                &mut length,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(length, 4);
        version
    }

    fn requested_version_attribute(dbc: SqlHandle) -> u32 {
        let mut value: u32 = 0xAAAA_AAAA;
        let mut length: SqlInteger = 0;
        let ret = unsafe {
            sql_get_connect_attr_w(
                dbc,
                SQL_COPT_SS_DATACLASSIFICATION_VERSION,
                (&mut value as *mut u32).cast(),
                4,
                &mut length,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(length, 4);
        value
    }

    fn set_requested_version(dbc: SqlHandle, value: usize) -> SqlReturn {
        unsafe {
            sql_set_connect_attr_w(
                dbc,
                SQL_COPT_SS_DATACLASSIFICATION_VERSION,
                value as SqlPointer,
                0,
            )
        }
    }

    #[test]
    fn a_default_connection_requests_version_1_and_returns_the_payload_verbatim() {
        let wire = classified_token(false);
        let registered = wire.clone();
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                registered,
            );
        });
        let mut h = TestHandles::with_env_dbc();
        let stmt = execute(&server, &mut h, CLASSIFIED);

        assert_eq!(server.requested_versions(), [Some(1)]);
        assert_eq!(classification_version(ird(stmt)), 1);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, wire[1..].to_vec()));
    }

    #[test]
    fn a_version_2_request_returns_the_ranks_verbatim() {
        let wire = classified_token(true);
        let registered = wire.clone();
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                registered,
            );
        });
        let mut h = TestHandles::with_env_dbc();
        assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
        let stmt = execute(&server, &mut h, CLASSIFIED);

        assert_eq!(server.requested_versions(), [Some(2)]);
        assert_eq!(classification_version(ird(stmt)), 2);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, wire[1..].to_vec()));
    }

    #[test]
    fn a_lower_acknowledgement_is_the_version_reported_and_parsed() {
        let wire = classified_token(false);
        let registered = wire.clone();
        let server = Server::start(DataClassificationAck::Version(1), |registry| {
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                registered,
            );
        });
        let mut h = TestHandles::with_env_dbc();
        assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
        let stmt = execute(&server, &mut h, CLASSIFIED);

        assert_eq!(server.requested_versions(), [Some(2)]);
        assert_eq!(requested_version_attribute(h.dbc), 2);
        assert_eq!(classification_version(ird(stmt)), 1);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, wire[1..].to_vec()));
    }

    #[test]
    fn an_unacknowledged_request_reports_version_0_and_no_classification() {
        let server = Server::start(DataClassificationAck::Omitted, |registry| {
            registry.register(UNCLASSIFIED, int_response(&["id"]));
        });
        let mut h = TestHandles::with_env_dbc();
        let stmt = execute(&server, &mut h, UNCLASSIFIED);

        assert_eq!(classification_version(ird(stmt)), 0);
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
        assert_last_diag(&desc_diags(ird(stmt)), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    #[test]
    fn each_result_set_reports_only_its_own_classification() {
        let first = single_label_token("First");
        let third = single_label_token("Third");
        let (registered_first, registered_third) = (first.clone(), third.clone());
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_result_sets(
                THREE_RESULT_SETS,
                vec![
                    ResultSetResponse::new(int_response(&["ssn"]), registered_first),
                    ResultSetResponse::new(int_response(&["id"]), Vec::new()),
                    ResultSetResponse::new(int_response(&["email"]), registered_third),
                ],
            );
        });
        let mut h = TestHandles::with_env_dbc();
        let stmt = execute(&server, &mut h, THREE_RESULT_SETS);

        assert_eq!(
            classification(ird(stmt)),
            (SQL_SUCCESS, first[1..].to_vec())
        );
        assert_eq!(unsafe { sql_more_results(stmt) }, SQL_SUCCESS);
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
        assert_eq!(unsafe { sql_more_results(stmt) }, SQL_SUCCESS);
        assert_eq!(
            classification(ird(stmt)),
            (SQL_SUCCESS, third[1..].to_vec())
        );
    }

    #[test]
    fn a_short_buffer_gets_the_prefix_and_01004_and_a_null_buffer_only_the_length() {
        let wire = classified_token(false);
        let registered = wire.clone();
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                registered,
            );
        });
        let mut h = TestHandles::with_env_dbc();
        let stmt = execute(&server, &mut h, CLASSIFIED);
        let payload = &wire[1..];
        let full = SqlInteger::try_from(payload.len()).unwrap();

        let mut buffer = [0u8; 8];
        let mut length: SqlInteger = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                ird(stmt),
                0,
                DATA_CLASSIFICATION,
                buffer.as_mut_ptr().cast(),
                8,
                &mut length,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(length, full);
        assert_eq!(buffer, payload[..8]);
        assert_last_diag(&desc_diags(ird(stmt)), WARN_STRING_TRUNCATION);

        let mut length: SqlInteger = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                ird(stmt),
                0,
                DATA_CLASSIFICATION,
                std::ptr::null_mut(),
                0,
                &mut length,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(length, full);

        let mut exact = vec![0u8; payload.len()];
        let ret = unsafe {
            sql_get_desc_field_w(
                ird(stmt),
                0,
                DATA_CLASSIFICATION,
                exact.as_mut_ptr().cast(),
                full,
                &mut length,
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "an exact fit is not truncated");
        assert_eq!(exact, payload);
        assert!(desc_diags(ird(stmt)).is_empty());
    }

    /// Prepares and executes `sql` on a new statement of the connected `dbc`.
    fn prepare_and_execute(h: &mut TestHandles, sql: &str) -> SqlHandle {
        let stmt = h.alloc_extra_stmt();
        let text: Vec<SqlWChar> = sql.encode_utf16().collect();
        let len = SqlInteger::try_from(text.len()).unwrap();
        assert_eq!(
            unsafe { sql_prepare_w(stmt, text.as_ptr(), len) },
            SQL_SUCCESS
        );
        let ret = unsafe { sql_execute(stmt) };
        assert!(
            matches!(ret, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
            "execute {sql}: {ret}"
        );
        stmt
    }

    fn classified_server() -> (Server, Vec<u8>) {
        let wire = classified_token(false);
        let registered = wire.clone();
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                registered,
            );
            registry.register(UNCLASSIFIED, int_response(&["id"]));
        });
        (server, wire[1..].to_vec())
    }

    /// msodbcsql's `SQLGetDescField` reads the session's version, so it
    /// answers before any result set (`sqlcdesc.cpp:5220`).
    #[test]
    fn the_version_is_the_acknowledged_one_before_any_result_set() {
        for (ack, expected) in [
            (DataClassificationAck::Requested, 2),
            (DataClassificationAck::Version(1), 1),
            (DataClassificationAck::Omitted, 0),
        ] {
            let server = Server::start(ack, |_| {});
            let mut h = TestHandles::with_env_dbc();
            assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
            server.connect(h.dbc);
            let stmt = h.alloc_extra_stmt();
            assert_eq!(classification_version(ird(stmt)), expected, "{ack:?}");
        }
    }

    /// Measured on msodbcsql 18.07.0001: closing a cursor drops the
    /// classification unless the statement is prepared.
    #[test]
    fn closing_drops_the_classification_unless_the_statement_is_prepared() {
        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        let direct = execute(&server, &mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(direct) }, SQL_SUCCESS);
        assert_eq!(classification(ird(direct)).0, SQL_ERROR);
        assert_last_diag(&desc_diags(ird(direct)), ERR_INVALID_DESCRIPTOR_FIELD);

        let prepared = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(prepared) }, SQL_SUCCESS);
        assert_eq!(classification(ird(prepared)), (SQL_SUCCESS, payload));
    }

    /// msodbcsql clears it at batch end even for a prepared statement.
    #[test]
    fn the_end_of_the_batch_drops_the_classification() {
        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));
        assert_eq!(unsafe { sql_more_results(stmt) }, SQL_NO_DATA);
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
    }

    /// A cancellation that lands after the last read turns the call into a
    /// failure (`HY008`), so even a prepared statement drops its classification.
    #[test]
    fn a_late_cancellation_drops_the_classification() {
        use std::time::{Duration, Instant};

        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let raw = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(classification(ird(raw)), (SQL_SUCCESS, payload));

        let stmt = unsafe { handle_from_raw::<StmtHandle>(raw) };
        let operation = stmt.begin_operation().unwrap();
        let address = raw as usize;
        let rc = std::thread::scope(|scope| {
            let cancel =
                scope.spawn(move || unsafe { crate::api::SQLCancel(address as SqlHandle) });
            let deadline = Instant::now() + Duration::from_secs(2);
            while !stmt.cancellation_state_for_test().1 {
                assert!(Instant::now() < deadline, "cancel did not signal");
                std::thread::yield_now();
            }
            let rc = super::super::exec_common::finish_operation(operation, stmt, raw, SQL_SUCCESS);
            assert_eq!(cancel.join().unwrap(), SQL_SUCCESS);
            rc
        });

        assert_eq!(rc, SQL_ERROR);
        assert_eq!(classification(ird(raw)).0, SQL_ERROR);
        assert_last_diag(&desc_diags(ird(raw)), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    /// A failed execution reports no classification, not the one its
    /// statement had before.
    #[test]
    fn a_failed_execution_does_not_report_the_previous_classification() {
        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));

        server.register(
            CLASSIFIED,
            QueryResponse::error_only(TerminalError::new(208, 16, "Invalid object name.")),
        );
        assert_eq!(unsafe { sql_execute(stmt) }, SQL_ERROR);
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
        assert_last_diag(&desc_diags(ird(stmt)), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    #[test]
    fn the_fields_are_read_only_ird_header_fields() {
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                classified_token(false),
            );
        });
        let mut h = TestHandles::with_env_dbc();
        let stmt = execute(&server, &mut h, CLASSIFIED);
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };

        let mut value = [0u8; 64];
        let mut length: SqlInteger = 0;
        for desc in [stmt_handle.ard, stmt_handle.apd, stmt_handle.ipd] {
            for field in [DATA_CLASSIFICATION, DATA_CLASSIFICATION_VERSION] {
                let ret = unsafe {
                    sql_get_desc_field_w(desc, 0, field, value.as_mut_ptr().cast(), 64, &mut length)
                };
                assert_eq!(ret, SQL_ERROR, "field {field}");
                assert_last_diag(&desc_diags(desc), ERR_INVALID_DESCRIPTOR_FIELD);
            }
        }

        for field in [
            SQL_CA_SS_DATA_CLASSIFICATION,
            SQL_CA_SS_DATA_CLASSIFICATION_VERSION,
        ] {
            let ret = unsafe {
                sql_col_attribute_w(
                    stmt,
                    1,
                    field,
                    value.as_mut_ptr().cast(),
                    64,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(ret, SQL_ERROR, "SQLColAttribute({field})");
            let diags = stmt_handle.inner.lock().unwrap().diag_records.clone();
            assert_last_diag(&diags, ERR_INVALID_DESCRIPTOR_FIELD);
        }

        let ret = unsafe {
            sql_set_desc_field_w(
                ird(stmt),
                0,
                DATA_CLASSIFICATION,
                value.as_mut_ptr().cast(),
                64,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(ird(stmt)), ERR_CANNOT_MODIFY_IRD);
    }

    /// A v1 payload SQL Server 2022 sent for a table with one classified
    /// column, captured from msodbcsql 18.7.1.1's `SQL_CA_SS_DATA_CLASSIFICATION`.
    const SQL_SERVER_V1_PAYLOAD: &str = "0100134800690067006800\
        6c00790020004300\
        6f006e0066006900\
        640065006e007400\
        690061006c00024c\
        00310001000b4e00\
        610074006900\
        6f006e0061006c00\
        2000490044000249\
        00310002000000010000000000";

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_payload_captured_from_sql_server_is_returned_byte_for_byte() {
        let payload = hex(SQL_SERVER_V1_PAYLOAD);
        assert_eq!(payload.len(), 86);
        let mut wire = vec![0xA3];
        wire.extend_from_slice(&payload);
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register_with_data_classification(
                UNCLASSIFIED,
                int_response(&["id", "ssn"]),
                wire,
            );
        });
        let mut h = TestHandles::with_env_dbc();
        let stmt = execute(&server, &mut h, UNCLASSIFIED);

        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));
    }

    #[test]
    fn the_attribute_defaults_to_1_and_rejects_values_outside_1_to_2() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(requested_version_attribute(h.dbc), 1);

        for invalid in [0, 3, 255] {
            assert_eq!(set_requested_version(h.dbc, invalid), SQL_ERROR);
            assert_last_diag(&dbc_diags(h.dbc), ERR_INVALID_ATTRIBUTE_VALUE);
            assert_eq!(requested_version_attribute(h.dbc), 1, "after {invalid}");
        }

        assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
        assert_eq!(requested_version_attribute(h.dbc), 2);
        assert_eq!(set_requested_version(h.dbc, 1), SQL_SUCCESS);
        assert_eq!(requested_version_attribute(h.dbc), 1);
    }

    /// msodbcsql validates `static_cast<DWORD>(vParam)`, so only the low 32
    /// bits of a pointer-sized value count.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_attribute_validates_the_low_32_bits() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_requested_version(h.dbc, (1 << 32) | 2), SQL_SUCCESS);
        assert_eq!(requested_version_attribute(h.dbc), 2);
        assert_eq!(set_requested_version(h.dbc, 1 << 32), SQL_ERROR);
        assert_eq!(requested_version_attribute(h.dbc), 2);
    }

    #[test]
    fn a_version_set_while_connected_is_requested_at_the_next_login() {
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.register(UNCLASSIFIED, int_response(&["id"]));
        });
        let h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
        assert_eq!(unsafe { sql_disconnect(h.dbc) }, SQL_SUCCESS);
        server.connect(h.dbc);

        assert_eq!(server.requested_versions(), [Some(1), Some(2)]);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(dbc.negotiated_data_classification_version(), 2);
        assert_eq!(unsafe { sql_disconnect(h.dbc) }, SQL_SUCCESS);
        assert_eq!(dbc.negotiated_data_classification_version(), 0);
    }

    /// Session recovery logs in again, and the reconnect may enable a lower
    /// version; the IRD must report and parse that one.
    #[test]
    fn session_recovery_reports_the_version_the_reconnect_acknowledges() {
        const KILL_SESSION: &str = "KILL SESSION";
        let wire = classified_token(false);
        let registered = wire.clone();
        let server = Server::start(DataClassificationAck::Requested, |registry| {
            registry.set_session_recovery(true);
            registry.register(KILL_SESSION, QueryResponse::select_one());
            registry.close_connection_after(KILL_SESSION);
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone", "id"]),
                registered,
            );
        });
        let mut h = TestHandles::with_env_dbc();
        assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
        let stmt = execute(&server, &mut h, KILL_SESSION);
        assert_eq!(classification_version(ird(stmt)), 2);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);

        server.set_ack(DataClassificationAck::Version(1));
        exec(stmt, CLASSIFIED);

        assert_eq!(server.requested_versions(), [Some(2), Some(2)]);
        assert_eq!(classification_version(ird(stmt)), 1);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, wire[1..].to_vec()));
    }
}

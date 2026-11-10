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
    let capacity = capacity(metadata);
    let mut out = Vec::with_capacity(capacity);
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
        out.extend_from_slice(&rank_value(rank).to_le_bytes());
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
                out.extend_from_slice(&rank_value(rank).to_le_bytes());
            }
        }
    }
    debug_assert!(out.len() <= capacity, "capacity() undercounts the payload");
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
    // Three u16 counts and the v2 payload rank; each property holds two u16
    // indexes and its v2 rank.
    3 * 2 + 4 + 2 * strings + 2 * dictionary_entries + 2 * metadata.columns().len() + 8 * properties
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

/// The wire value: `SensitivityRank` is `#[repr(i32)]` with the [MS-TDS]
/// values as its discriminants, so a rank mssql-tds adds later keeps its own.
fn rank_value(rank: SensitivityRank) -> i32 {
    rank as i32
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
        SQL_NTS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlLen, SqlPointer,
        SqlReturn, SqlSmallInt, SqlWChar,
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

    /// A B_VARCHAR length is an unsigned byte. SQL Server sends names of up to
    /// 128 UTF-16 units, where msodbcsql 18.07.0001 times out (parity deviation
    /// 42); every length up to 255 round-trips here. ASCII names make
    /// `capacity` exact, so its debug assertion checks every v2 rank is counted.
    #[test]
    fn names_up_to_255_units_are_returned_verbatim() {
        let (n127, n128, n255) = ("a".repeat(127), "b".repeat(128), "c".repeat(255));
        for v2 in [false, true] {
            let wire = token(
                v2,
                &[(&n127, "L127"), (&n128, &n128)],
                &[(&n255, "I255")],
                30,
                &[&[(0, UNSPECIFIED, 10)], &[(1, 0, 20)]],
            );
            let registered = wire.clone();
            let server = Server::start(DataClassificationAck::Requested, |registry| {
                registry.register_with_data_classification(
                    CLASSIFIED,
                    int_response(&["ssn", "phone"]),
                    registered,
                );
            });
            let mut h = TestHandles::with_env_dbc();
            if v2 {
                assert_eq!(set_requested_version(h.dbc, 2), SQL_SUCCESS);
            }
            let stmt = execute(&server, &mut h, CLASSIFIED);

            assert_eq!(
                classification(ird(stmt)),
                (SQL_SUCCESS, wire[1..].to_vec()),
                "v2: {v2}"
            );
        }
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

    /// A fetch error closes the cursor but keeps the classification; closing
    /// afterwards still drops it unless the statement is prepared (measured on
    /// msodbcsql 18.07.0001 for all three close functions).
    #[test]
    fn closing_after_a_fetch_error_drops_the_classification_unless_prepared() {
        for close in ["SQLFreeStmt", "SQLCancel", "SQLCloseCursor"] {
            for prepared in [false, true] {
                let (server, payload) = classified_server();
                let mut h = TestHandles::with_env_dbc();
                server.connect(h.dbc);
                let stmt = if prepared {
                    prepare_and_execute(&mut h, CLASSIFIED)
                } else {
                    let stmt = h.alloc_extra_stmt();
                    exec(stmt, CLASSIFIED);
                    stmt
                };
                while unsafe { crate::api::SQLFetch(stmt) } == SQL_SUCCESS {}
                // What a server error on a later fetch leaves behind.
                super::super::close_cursor::reset_cursor_state(
                    &mut unsafe { handle_from_raw::<StmtHandle>(stmt) }
                        .inner
                        .lock()
                        .unwrap(),
                );
                assert_eq!(classification(ird(stmt)).0, SQL_SUCCESS, "{close}");

                unsafe {
                    match close {
                        "SQLFreeStmt" => {
                            crate::api::SQLFreeStmt(stmt, crate::api::odbc_types::SQL_CLOSE)
                        }
                        "SQLCancel" => crate::api::SQLCancel(stmt),
                        _ => sql_close_cursor(stmt),
                    }
                };
                if prepared {
                    assert_eq!(
                        classification(ird(stmt)),
                        (SQL_SUCCESS, payload),
                        "{close} prepared"
                    );
                } else {
                    assert_eq!(classification(ird(stmt)).0, SQL_ERROR, "{close} direct");
                }
            }
        }
    }

    /// msodbcsql's commit sweep strips `KEEP_COLUMN_INFO`, so a prepared
    /// statement whose cursor is already closed loses its classification; an
    /// open one keeps it, and with no transaction started there is no sweep
    /// (measured on msodbcsql 18.07.0001).
    #[test]
    fn ending_a_transaction_drops_a_closed_prepared_statements_classification() {
        for started in [false, true] {
            let (server, payload) = classified_server();
            let mut h = TestHandles::with_env_dbc();
            server.connect(h.dbc);
            let closed = prepare_and_execute(&mut h, CLASSIFIED);
            assert_eq!(unsafe { sql_close_cursor(closed) }, SQL_SUCCESS);
            let open = prepare_and_execute(&mut h, CLASSIFIED);
            unsafe { handle_from_raw::<crate::handles::dbc::DbcHandle>(h.dbc) }
                .inner
                .lock()
                .unwrap()
                .local_tran_started = started;

            let rc = unsafe {
                crate::api::SQLEndTran(
                    crate::api::odbc_types::SQL_HANDLE_DBC,
                    h.dbc,
                    crate::api::odbc_types::SQL_COMMIT,
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "started: {started}");
            if started {
                assert_eq!(classification(ird(closed)).0, SQL_ERROR);
            } else {
                assert_eq!(classification(ird(closed)), (SQL_SUCCESS, payload.clone()));
            }
            assert_eq!(
                classification(ird(open)),
                (SQL_SUCCESS, payload),
                "started: {started}"
            );
        }
    }

    /// msodbcsql's `FreeStmt` keeps a prepared statement's classification
    /// through the commit sweep only while its cursor still has rows to read:
    /// unread or on its last row it stays, after `SQL_NO_DATA` or on an empty
    /// result it goes (`sqlcfunc.cpp:494-500`, measured on 18.07.0001). With no
    /// transaction started there is no sweep, so it stays either way.
    #[test]
    fn ending_a_transaction_drops_a_prepared_classification_with_no_rows_left() {
        const EMPTY: &str = "SELECT SSN, PHONE, ID FROM NOBODY";
        for (query, fetches, started, kept) in [
            (CLASSIFIED, 0, true, true),
            (CLASSIFIED, 1, true, true),
            (CLASSIFIED, 2, true, false),
            (EMPTY, 0, true, false),
            (CLASSIFIED, 2, false, true),
        ] {
            let (server, payload) = classified_server();
            let registered = classified_token(false);
            server.runtime.block_on(async {
                server
                    .registry
                    .lock()
                    .await
                    .register_with_data_classification(
                        EMPTY,
                        QueryResponse::new(
                            ["ssn", "phone", "id"]
                                .iter()
                                .map(|name| ColumnDefinition::new(*name, SqlDataType::Int))
                                .collect(),
                            Vec::new(),
                        ),
                        registered,
                    );
            });
            let mut h = TestHandles::with_env_dbc();
            server.connect(h.dbc);
            let stmt = prepare_and_execute(&mut h, query);
            for _ in 0..fetches {
                unsafe { crate::api::SQLFetch(stmt) };
            }
            unsafe { handle_from_raw::<DbcHandle>(h.dbc) }
                .inner
                .lock()
                .unwrap()
                .local_tran_started = started;

            let rc = unsafe {
                crate::api::SQLEndTran(
                    crate::api::odbc_types::SQL_HANDLE_DBC,
                    h.dbc,
                    crate::api::odbc_types::SQL_COMMIT,
                )
            };
            let case = format!("{query}, {fetches} fetches, started: {started}");
            assert_eq!(rc, SQL_SUCCESS, "{case}");
            if kept {
                assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload), "{case}");
            } else {
                assert_eq!(classification(ird(stmt)).0, SQL_ERROR, "{case}");
            }
        }
    }

    /// Turning autocommit back on runs `CommitAbortTran`'s sweep
    /// (`SetCommitModeOption`), so a transaction holding user work ends like
    /// `SQLEndTran`, including for a cursor left open with no rows left; with no
    /// user work the classification stays (measured on msodbcsql 18.07.0001).
    #[test]
    fn enabling_autocommit_ends_the_transaction_for_the_classification() {
        use crate::api::odbc_types::{SQL_ATTR_AUTOCOMMIT, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON};

        let autocommit = |dbc: SqlHandle, value: u32| unsafe {
            sql_set_connect_attr_w(dbc, SQL_ATTR_AUTOCOMMIT, value as usize as SqlPointer, 0)
        };
        // `None` closes the cursor first; `Some(n)` leaves it open after `n`
        // fetches: unread (0), on the last row (1), or past it with `SQL_NO_DATA`
        // (2).
        for (user_work, fetches, kept) in [
            (true, None, false),
            (false, None, true),
            (true, Some(0), true),
            (true, Some(1), true),
            (true, Some(2), false),
        ] {
            let (server, payload) = classified_server();
            let mut h = TestHandles::with_env_dbc();
            server.connect(h.dbc);
            if user_work {
                assert_eq!(autocommit(h.dbc, SQL_AUTOCOMMIT_OFF), SQL_SUCCESS);
            }
            let stmt = prepare_and_execute(&mut h, CLASSIFIED);
            match fetches {
                None => assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS),
                Some(n) => {
                    for _ in 0..n {
                        unsafe { crate::api::SQLFetch(stmt) };
                    }
                }
            }
            if !user_work {
                assert_eq!(autocommit(h.dbc, SQL_AUTOCOMMIT_OFF), SQL_SUCCESS);
            }
            assert_eq!(
                unsafe { handle_from_raw::<DbcHandle>(h.dbc) }
                    .inner
                    .lock()
                    .unwrap()
                    .local_tran_started,
                user_work
            );

            let case = format!("user work: {user_work}, fetches: {fetches:?}");
            let rc = autocommit(h.dbc, SQL_AUTOCOMMIT_ON);
            assert!(
                matches!(rc, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
                "{case}: {rc}"
            );
            if kept {
                assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload), "{case}");
            } else {
                assert_eq!(classification(ird(stmt)).0, SQL_ERROR, "{case}");
            }
        }
    }

    /// Another statement can add user work to the driver-begun transaction
    /// after the switch's first snapshot and before it claims the client. The
    /// switch must still commit that work, not roll it back as an empty
    /// transaction, and leave each classification as one transaction-ending
    /// sweep would: a closed prepared statement drops it, a prepared result
    /// the late work left unread or on its last row keeps it, even on a
    /// statement the first sweep found closed, and one it read to
    /// `SQL_NO_DATA` drops it, whether that happened before the sweep or after.
    #[test]
    fn user_work_added_during_the_autocommit_switch_is_committed() {
        use crate::api::odbc_types::{SQL_ATTR_AUTOCOMMIT, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON};

        let autocommit = |dbc: SqlHandle, value: u32| unsafe {
            sql_set_connect_attr_w(dbc, SQL_ATTR_AUTOCOMMIT, value as usize as SqlPointer, 0)
        };
        let prepare_and_run = |stmt: SqlHandle, fetches: usize| {
            let text: Vec<SqlWChar> = CLASSIFIED.encode_utf16().collect();
            let len = SqlInteger::try_from(text.len()).unwrap();
            unsafe {
                assert_eq!(sql_prepare_w(stmt, text.as_ptr(), len), SQL_SUCCESS);
                assert_eq!(sql_execute(stmt), SQL_SUCCESS);
                for _ in 0..fetches {
                    crate::api::SQLFetch(stmt);
                }
            }
        };
        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let closed = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(closed) }, SQL_SUCCESS);
        let reused = h.alloc_extra_stmt();
        prepare_and_run(reused, 0);
        assert_eq!(unsafe { sql_close_cursor(reused) }, SQL_SUCCESS);
        assert_eq!(autocommit(h.dbc, SQL_AUTOCOMMIT_OFF), SQL_SUCCESS);
        let dbc = h.dbc;
        let exhausted = h.alloc_extra_stmt();
        let unread = h.alloc_extra_stmt();
        let late = h.alloc_extra_stmt();
        crate::api::txn::test_hooks::set_after_autocommit_snapshot(move || {
            prepare_and_run(exhausted, 2);
            prepare_and_run(unread, 0);
            assert!(
                unsafe { handle_from_raw::<DbcHandle>(dbc) }
                    .inner
                    .lock()
                    .unwrap()
                    .local_tran_started
            );
        });
        crate::api::txn::test_hooks::set_after_autocommit_sweep(move || {
            prepare_and_run(late, 2);
            prepare_and_run(reused, 1);
        });

        assert_eq!(autocommit(h.dbc, SQL_AUTOCOMMIT_ON), SQL_SUCCESS_WITH_INFO);
        assert_last_diag(
            &dbc_diags(h.dbc),
            crate::api::sqlstate::WARN_TRANSACTION_COMMITTED,
        );
        assert_eq!(classification(ird(closed)).0, SQL_ERROR, "closed");
        assert_eq!(classification(ird(exhausted)).0, SQL_ERROR, "exhausted");
        assert_eq!(
            classification(ird(unread)),
            (SQL_SUCCESS, payload.clone()),
            "unread"
        );
        assert_eq!(
            classification(ird(reused)),
            (SQL_SUCCESS, payload),
            "reused"
        );
        assert_eq!(classification(ird(late)).0, SQL_ERROR, "late");
    }
    /// Preparing new text drops the classification before anything executes
    /// (measured on msodbcsql 18.07.0001).
    #[test]
    fn preparing_new_text_drops_the_classification() {
        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));

        let text: Vec<SqlWChar> = UNCLASSIFIED.encode_utf16().collect();
        let len = SqlInteger::try_from(text.len()).unwrap();
        assert_eq!(
            unsafe { sql_prepare_w(stmt, text.as_ptr(), len) },
            SQL_SUCCESS
        );
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
        assert_last_diag(&desc_diags(ird(stmt)), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    /// A parameter that fails conversion rejects the execution before anything
    /// is sent, so the previous classification stays (measured on msodbcsql
    /// 18.07.0001).
    #[test]
    fn a_parameter_conversion_failure_keeps_the_classification() {
        use crate::api::odbc_types::{
            SQL_C_CHAR, SQL_C_SLONG, SQL_INTEGER, SQL_NTS, SQL_PARAM_INPUT,
        };

        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = h.alloc_extra_stmt();
        let mut id: i32 = 1;
        let mut id_len: SqlLen = 0;
        let bind = |c_type, value: SqlPointer, len: *mut SqlLen| unsafe {
            crate::api::SQLBindParameter(
                stmt,
                1,
                SQL_PARAM_INPUT,
                c_type,
                SQL_INTEGER,
                0,
                0,
                value,
                0,
                len,
            )
        };
        assert_eq!(
            bind(SQL_C_SLONG, (&raw mut id).cast(), &raw mut id_len),
            SQL_SUCCESS
        );
        let text: Vec<SqlWChar> = format!("{CLASSIFIED} WHERE ID = ?")
            .encode_utf16()
            .collect();
        let len = SqlInteger::try_from(text.len()).unwrap();
        assert_eq!(
            unsafe { sql_prepare_w(stmt, text.as_ptr(), len) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { sql_execute(stmt) }, SQL_SUCCESS);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);

        let mut bad = *b"not-an-int\0";
        let mut bad_len = SqlLen::from(SQL_NTS);
        assert_eq!(
            bind(SQL_C_CHAR, bad.as_mut_ptr().cast(), &raw mut bad_len),
            SQL_SUCCESS
        );
        assert_eq!(unsafe { sql_execute(stmt) }, SQL_ERROR);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));
    }

    /// Prepares and executes `{CLASSIFIED} WHERE ID = ?` with `id` bound as an
    /// integer, then closes the cursor, which a prepared statement survives with
    /// its classification.
    fn prepared_with_parameter(
        h: &mut TestHandles,
        id: &mut i32,
        id_len: &mut SqlLen,
    ) -> SqlHandle {
        use crate::api::odbc_types::{SQL_C_SLONG, SQL_INTEGER, SQL_PARAM_INPUT};

        let stmt = h.alloc_extra_stmt();
        let rc = unsafe {
            crate::api::SQLBindParameter(
                stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_SLONG,
                SQL_INTEGER,
                0,
                0,
                std::ptr::from_mut(id).cast(),
                0,
                id_len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        let text: Vec<SqlWChar> = format!("{CLASSIFIED} WHERE ID = ?")
            .encode_utf16()
            .collect();
        let len = SqlInteger::try_from(text.len()).unwrap();
        assert_eq!(
            unsafe { sql_prepare_w(stmt, text.as_ptr(), len) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { sql_execute(stmt) }, SQL_SUCCESS);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
        stmt
    }

    /// msodbcsql drops the classification only once a request reaches the
    /// server, so a call refused because another owner holds the connection
    /// keeps it (measured on 18.07.0001 for SQLExecute, SQLExecDirect and
    /// SQLTables).
    #[test]
    fn a_busy_connection_keeps_the_classification() {
        use crate::api::odbc_types::SQL_ALL_TYPES;
        use crate::api::sqlstate::ERR_CONNECTION_BUSY;
        use crate::handles::wire::{ClaimPolicy, LeaseReturn, WireOwner};

        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let lease = dbc
            .inner
            .lock()
            .unwrap()
            .wire
            .claim(WireOwner::Connection, ClaimPolicy::IdleOnly)
            .unwrap();
        let unclassified: Vec<SqlWChar> = UNCLASSIFIED.encode_utf16().collect();
        let calls: [(&str, &dyn Fn() -> SqlReturn); 4] = [
            ("SQLExecute", &|| unsafe { sql_execute(stmt) }),
            ("SQLExecDirect", &|| unsafe {
                crate::api::SQLExecDirectW(
                    stmt,
                    unclassified.as_ptr(),
                    SqlInteger::try_from(unclassified.len()).unwrap(),
                )
            }),
            ("SQLTables", &|| unsafe {
                crate::api::SQLTablesW(
                    stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                )
            }),
            ("SQLGetTypeInfo", &|| unsafe {
                crate::api::SQLGetTypeInfoW(stmt, SQL_ALL_TYPES)
            }),
        ];
        for (name, call) in calls {
            assert_eq!(call(), SQL_ERROR, "{name}");
            assert_last_diag(
                &unsafe { handle_from_raw::<StmtHandle>(stmt) }
                    .inner
                    .lock()
                    .unwrap()
                    .diag_records,
                ERR_CONNECTION_BUSY,
            );
            assert_eq!(
                classification(ird(stmt)),
                (SQL_SUCCESS, payload.clone()),
                "{name}"
            );
        }
        dbc.return_lease(lease, LeaseReturn::Release);
    }

    /// A parameter array that sends no set keeps the classification, whether
    /// every set is ignored or every set fails conversion (measured on
    /// msodbcsql 18.07.0001).
    #[test]
    fn a_parameter_array_that_sends_nothing_keeps_the_classification() {
        use crate::api::odbc_types::{
            SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAMSET_SIZE, SQL_C_CHAR, SQL_INTEGER, SQL_NTS,
            SQL_PARAM_IGNORE, SQL_PARAM_INPUT, SqlUSmallInt,
        };

        for ignored in [true, false] {
            let (server, payload) = classified_server();
            let mut h = TestHandles::with_env_dbc();
            server.connect(h.dbc);
            let (mut id, mut id_len) = (1, 0);
            let stmt = prepared_with_parameter(&mut h, &mut id, &mut id_len);
            let mut operations: [SqlUSmallInt; 2] = [SQL_PARAM_IGNORE; 2];
            let mut values = *b"not-an-int\0nope\0\0\0\0\0\0\0";
            let mut lengths = [SqlLen::from(SQL_NTS); 2];
            unsafe {
                assert_eq!(
                    crate::api::SQLSetStmtAttrW(stmt, SQL_ATTR_PARAMSET_SIZE, 2 as SqlPointer, 0),
                    SQL_SUCCESS
                );
                if ignored {
                    assert_eq!(
                        crate::api::SQLSetStmtAttrW(
                            stmt,
                            SQL_ATTR_PARAM_OPERATION_PTR,
                            operations.as_mut_ptr().cast(),
                            0,
                        ),
                        SQL_SUCCESS
                    );
                } else {
                    assert_eq!(
                        crate::api::SQLBindParameter(
                            stmt,
                            1,
                            SQL_PARAM_INPUT,
                            SQL_C_CHAR,
                            SQL_INTEGER,
                            0,
                            0,
                            values.as_mut_ptr().cast(),
                            12,
                            lengths.as_mut_ptr(),
                        ),
                        SQL_SUCCESS
                    );
                }
            }
            let rc = unsafe { sql_execute(stmt) };
            assert_eq!(rc, if ignored { SQL_SUCCESS } else { SQL_ERROR });
            assert_eq!(
                classification(ird(stmt)),
                (SQL_SUCCESS, payload),
                "ignored: {ignored}"
            );
        }
    }

    /// `SQLExecute` stops at `SQL_NEED_DATA` before anything runs, so a cancel
    /// there keeps the classification; the last `SQLParamData` sends the
    /// request and drops it even when the server then fails it (measured on
    /// msodbcsql 18.07.0001). Covers the buffered and the streamed sequence.
    #[test]
    fn data_at_execution_drops_the_classification_only_when_it_runs() {
        use crate::api::odbc_types::{
            SQL_C_CHAR, SQL_DATA_AT_EXEC, SQL_INTEGER, SQL_NEED_DATA, SQL_NTS, SQL_PARAM_INPUT,
            SQL_VARCHAR,
        };

        for sql_type in [SQL_INTEGER, SQL_VARCHAR] {
            let (server, payload) = classified_server();
            let mut h = TestHandles::with_env_dbc();
            server.connect(h.dbc);
            let (mut id, mut id_len) = (1, 0);
            let stmt = prepared_with_parameter(&mut h, &mut id, &mut id_len);
            let mut dae = SQL_DATA_AT_EXEC;
            let rc = unsafe {
                crate::api::SQLBindParameter(
                    stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_CHAR,
                    sql_type,
                    0,
                    0,
                    1 as SqlPointer,
                    0,
                    &raw mut dae,
                )
            };
            assert_eq!(rc, SQL_SUCCESS);

            assert_eq!(unsafe { sql_execute(stmt) }, SQL_NEED_DATA, "{sql_type}");
            assert_eq!(unsafe { crate::api::SQLCancel(stmt) }, SQL_SUCCESS);
            assert_eq!(
                classification(ird(stmt)),
                (SQL_SUCCESS, payload.clone()),
                "{sql_type}"
            );

            server.register(
                CLASSIFIED,
                QueryResponse::error_only(TerminalError::new(208, 16, "Invalid object name.")),
            );
            assert_eq!(unsafe { sql_execute(stmt) }, SQL_NEED_DATA, "{sql_type}");
            let mut token: SqlPointer = std::ptr::null_mut();
            assert_eq!(
                unsafe { crate::api::SQLParamData(stmt, &raw mut token) },
                SQL_NEED_DATA
            );
            assert_eq!(
                classification(ird(stmt)),
                (SQL_SUCCESS, payload),
                "{sql_type}"
            );
            let mut value = *b"1\0";
            assert_eq!(
                unsafe {
                    crate::api::SQLPutData(stmt, value.as_mut_ptr().cast(), SqlLen::from(SQL_NTS))
                },
                SQL_SUCCESS
            );
            assert_eq!(
                unsafe { crate::api::SQLParamData(stmt, &raw mut token) },
                SQL_ERROR,
                "{sql_type}"
            );
            assert_eq!(classification(ird(stmt)).0, SQL_ERROR, "{sql_type}");
        }
    }
    /// An execute on a connection already known dead, with no session recovery
    /// to reconnect it, fails before anything is sent, so the classification
    /// stays: msodbcsql's `GetBatchCtxOrRecover` fails before `ClearColinfo`
    /// (`sqlccmd.cpp:1870`, `sqlcfunc.cpp:641`).
    #[test]
    fn a_failed_reconnect_keeps_the_classification() {
        const KILL_SESSION: &str = "KILL SESSION";
        let (server, payload) = classified_server();
        server.register(KILL_SESSION, QueryResponse::select_one());
        server.runtime.block_on(async {
            server
                .registry
                .lock()
                .await
                .close_connection_after(KILL_SESSION);
        });
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);

        let killer = h.alloc_extra_stmt();
        exec(killer, KILL_SESSION);
        assert_eq!(unsafe { sql_close_cursor(killer) }, SQL_SUCCESS);
        let text: Vec<SqlWChar> = UNCLASSIFIED.encode_utf16().collect();
        let len = SqlInteger::try_from(text.len()).unwrap();
        while unsafe { sql_exec_direct_w(killer, text.as_ptr(), len) } != SQL_ERROR {
            assert_eq!(unsafe { sql_close_cursor(killer) }, SQL_SUCCESS);
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(
            dbc.inner
                .lock()
                .unwrap()
                .wire
                .client()
                .is_some_and(|client| client.is_connection_dead()),
            "the failed execute marks the connection dead"
        );

        let unclassified: Vec<SqlWChar> = UNCLASSIFIED.encode_utf16().collect();
        let calls: [(&str, &dyn Fn() -> SqlReturn); 4] = [
            ("SQLExecute", &|| unsafe { sql_execute(stmt) }),
            ("SQLTables", &|| unsafe {
                crate::api::SQLTablesW(
                    stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                )
            }),
            ("SQLGetTypeInfo", &|| unsafe {
                crate::api::SQLGetTypeInfoW(stmt, crate::api::odbc_types::SQL_ALL_TYPES)
            }),
            ("SQLExecDirect", &|| unsafe {
                crate::api::SQLExecDirectW(
                    stmt,
                    unclassified.as_ptr(),
                    SqlInteger::try_from(unclassified.len()).unwrap(),
                )
            }),
        ];
        for (name, call) in calls {
            assert_eq!(call(), SQL_ERROR, "{name}");
            assert_eq!(
                classification(ird(stmt)),
                (SQL_SUCCESS, payload.clone()),
                "{name}"
            );
        }
    }

    /// A connection not yet known dead does not stop the send, so a request
    /// that then fails on the closed socket drops the classification, as
    /// msodbcsql's does once `GetBatchCtxOrRecover` passes.
    #[test]
    fn a_send_that_finds_the_connection_closed_drops_the_classification() {
        const CLOSE_AFTER: &str = "CLOSE AFTER";
        let (server, payload) = classified_server();
        server.register(CLOSE_AFTER, QueryResponse::select_one());
        server.runtime.block_on(async {
            server
                .registry
                .lock()
                .await
                .close_connection_after(CLOSE_AFTER);
        });
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
        let closer = h.alloc_extra_stmt();
        exec(closer, CLOSE_AFTER);
        assert_eq!(unsafe { sql_close_cursor(closer) }, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(
            dbc.inner
                .lock()
                .unwrap()
                .wire
                .client()
                .is_some_and(|client| !client.is_connection_dead()),
            "the closed socket is not yet observed"
        );
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));

        assert_eq!(unsafe { sql_execute(stmt) }, SQL_ERROR);
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
    }

    /// With session recovery on, a request sent on a healthy connection, or on
    /// one known dead that recovery reconnects, drops the classification when
    /// the server fails it, including a fatal error that leaves the new
    /// connection dead too.
    #[test]
    fn a_send_after_a_successful_reconnect_drops_the_classification() {
        for (dead, severity) in [(false, 16), (true, 16), (true, 20)] {
            let (server, payload) = classified_server();
            server.runtime.block_on(async {
                server.registry.lock().await.set_session_recovery(true);
            });
            let mut h = TestHandles::with_env_dbc();
            server.connect(h.dbc);
            let stmt = prepare_and_execute(&mut h, CLASSIFIED);
            assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
            assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));
            server.register(
                CLASSIFIED,
                QueryResponse::error_only(TerminalError::new(208, severity, "Server failure.")),
            );
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            if dead {
                dbc.inner
                    .lock()
                    .unwrap()
                    .wire
                    .client_mut_for_test()
                    .unwrap()
                    .mark_connection_dead();
            }
            let case = format!("dead: {dead}, severity {severity}");

            assert_eq!(unsafe { sql_execute(stmt) }, SQL_ERROR, "{case}");
            assert_eq!(
                dbc.inner
                    .lock()
                    .unwrap()
                    .wire
                    .client()
                    .is_some_and(|client| client.is_connection_dead()),
                severity >= 20,
                "{case}"
            );
            assert_eq!(classification(ird(stmt)).0, SQL_ERROR, "{case}");
        }
    }
    /// With session recovery on, an idle socket can die before the client has
    /// seen it. mssql-tds then finds it dead while checking the connection and,
    /// when the reconnect fails, rejects the request before sending anything,
    /// so the classification stays.
    #[test]
    fn a_failed_reconnect_on_an_unobserved_dead_socket_keeps_the_classification() {
        const CLOSE_AFTER: &str = "CLOSE AFTER";
        let (server, payload) = classified_server();
        server.register(CLOSE_AFTER, QueryResponse::select_one());
        server.runtime.block_on(async {
            let mut registry = server.registry.lock().await;
            registry.set_session_recovery(true);
            registry.close_connection_after(CLOSE_AFTER);
        });
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = prepare_and_execute(&mut h, CLASSIFIED);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
        let closer = h.alloc_extra_stmt();
        exec(closer, CLOSE_AFTER);
        assert_eq!(unsafe { sql_close_cursor(closer) }, SQL_SUCCESS);
        // The reconnect's login is no longer acknowledged for recovery, so
        // mssql-tds rejects the recovered session.
        server.runtime.block_on(async {
            server.registry.lock().await.set_session_recovery(false);
        });
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(
            dbc.inner
                .lock()
                .unwrap()
                .wire
                .client()
                .is_some_and(|client| !client.is_connection_dead()),
            "the closed socket is not yet observed"
        );

        assert_eq!(unsafe { sql_execute(stmt) }, SQL_ERROR);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));
    }

    /// With two streamed data-at-execution parameters, closing the first sends
    /// no complete request, so the classification stays through that
    /// `SQLParamData` and a cancel after it; the last one drops it even when
    /// the server then fails the request.
    #[test]
    fn closing_a_streamed_parameter_before_the_last_keeps_the_classification() {
        use crate::api::odbc_types::{
            SQL_C_CHAR, SQL_C_SLONG, SQL_DATA_AT_EXEC, SQL_INTEGER, SQL_NEED_DATA, SQL_NTS,
            SQL_PARAM_INPUT, SQL_VARCHAR,
        };

        let (server, payload) = classified_server();
        let mut h = TestHandles::with_env_dbc();
        server.connect(h.dbc);
        let stmt = h.alloc_extra_stmt();
        let mut id: i32 = 1;
        let mut id_len: SqlLen = 0;
        let mut dae = SQL_DATA_AT_EXEC;
        let bind = |streamed: bool, id: &mut i32, id_len: &mut SqlLen, dae: &mut SqlLen| {
            for number in 1..=2u16 {
                let rc = unsafe {
                    if streamed {
                        crate::api::SQLBindParameter(
                            stmt,
                            number,
                            SQL_PARAM_INPUT,
                            SQL_C_CHAR,
                            SQL_VARCHAR,
                            0,
                            0,
                            usize::from(number) as SqlPointer,
                            0,
                            dae,
                        )
                    } else {
                        crate::api::SQLBindParameter(
                            stmt,
                            number,
                            SQL_PARAM_INPUT,
                            SQL_C_SLONG,
                            SQL_INTEGER,
                            0,
                            0,
                            std::ptr::from_mut(id).cast(),
                            0,
                            id_len,
                        )
                    }
                };
                assert_eq!(rc, SQL_SUCCESS);
            }
        };
        bind(false, &mut id, &mut id_len, &mut dae);
        let text: Vec<SqlWChar> = format!("{CLASSIFIED} WHERE ID = ? OR ID = ?")
            .encode_utf16()
            .collect();
        let len = SqlInteger::try_from(text.len()).unwrap();
        assert_eq!(
            unsafe { sql_prepare_w(stmt, text.as_ptr(), len) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { sql_execute(stmt) }, SQL_SUCCESS);
        assert_eq!(unsafe { sql_close_cursor(stmt) }, SQL_SUCCESS);
        bind(true, &mut id, &mut id_len, &mut dae);

        let mut token: SqlPointer = std::ptr::null_mut();
        let mut value = *b"1\0";
        let mut send_first_parameter = || unsafe {
            assert_eq!(sql_execute(stmt), SQL_NEED_DATA);
            assert_eq!(
                crate::api::SQLParamData(stmt, &raw mut token),
                SQL_NEED_DATA
            );
            assert_eq!(
                crate::api::SQLPutData(stmt, value.as_mut_ptr().cast(), SqlLen::from(SQL_NTS)),
                SQL_SUCCESS
            );
            assert_eq!(
                crate::api::SQLParamData(stmt, &raw mut token),
                SQL_NEED_DATA
            );
        };
        send_first_parameter();
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload.clone()));
        assert_eq!(unsafe { crate::api::SQLCancel(stmt) }, SQL_SUCCESS);
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload.clone()));

        server.register(
            CLASSIFIED,
            QueryResponse::error_only(TerminalError::new(208, 16, "Invalid object name.")),
        );
        send_first_parameter();
        assert_eq!(classification(ird(stmt)), (SQL_SUCCESS, payload));
        let mut value = *b"2\0";
        unsafe {
            assert_eq!(
                crate::api::SQLPutData(stmt, value.as_mut_ptr().cast(), SqlLen::from(SQL_NTS)),
                SQL_SUCCESS
            );
            assert_eq!(crate::api::SQLParamData(stmt, &raw mut token), SQL_ERROR);
        }
        assert_eq!(classification(ird(stmt)).0, SQL_ERROR);
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

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A table-valued parameter streamed by row produces the same request as the
//! materialized table, sends packets before it closes, and keeps its row and
//! cell sequencing rules.

use super::*;
use crate::datatypes::sql_tvp::{
    TVP_END_TOKEN, TVP_ROW_TOKEN, TvpColumnDef, TvpTableData, TvpTypeName,
};
use crate::error::TvpParameterRef;

const SQL: &str = "SELECT COUNT(*) FROM @t";

fn type_name() -> TvpTypeName {
    TvpTypeName::new(Some("dbo".to_string()), "Items".to_string())
}

fn columns() -> Vec<TvpColumnDef> {
    vec![
        TvpColumnDef::new(SqlType::Int(None)),
        TvpColumnDef::new(SqlType::NVarchar(None, 20)),
        TvpColumnDef::new(SqlType::VarBinaryMax(None)),
    ]
}

fn text(value: &str) -> SqlType {
    SqlType::NVarchar(Some(SqlString::from_utf8_string(value.to_string())), 20)
}

fn rows() -> Vec<Vec<SqlType>> {
    vec![
        vec![
            SqlType::Int(Some(1)),
            text("one"),
            SqlType::VarBinaryMax(Some(vec![1, 2, 3])),
        ],
        vec![
            SqlType::Int(None),
            SqlType::NVarchar(None, 20),
            SqlType::VarBinaryMax(None),
        ],
        vec![
            SqlType::Int(Some(3)),
            text(""),
            SqlType::VarBinaryMax(Some(Vec::new())),
        ],
    ]
}

fn streamed_table(columns: Vec<TvpColumnDef>) -> RpcParameter {
    RpcParameter::data_at_exec_table(
        Some("@t".to_string()),
        StatusFlags::NONE,
        type_name(),
        columns,
    )
}

fn take_sent(sent: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u8> {
    reassemble_sent(&std::mem::take(&mut *sent.lock().unwrap()))
}

async fn materialized_request(rows: Vec<Vec<SqlType>>) -> Vec<u8> {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    let table = SqlType::Table(type_name(), Some(TvpTableData::new(columns(), rows)));
    client
        .execute_sp_executesql(
            SQL.to_string(),
            vec![RpcParameter::new(
                Some("@t".to_string()),
                StatusFlags::NONE,
                table,
            )],
            (),
        )
        .await
        .unwrap();
    take_sent(&sent)
}

async fn begin_table(client: &mut TdsClient) {
    let status = client
        .begin_sp_executesql(SQL.to_string(), vec![streamed_table(columns())], ())
        .await
        .unwrap();
    assert!(matches!(status, StreamedParamStatus::NeedData { param_name } if param_name == "@t"));
}

fn assert_active(client: &TdsClient) {
    assert!(matches!(
        client.streamed_write_state,
        StreamedWriteState::Active(_)
    ));
}

fn ends_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && &haystack[haystack.len() - needle.len()..] == needle
}

#[tokio::test]
async fn streamed_rows_match_the_materialized_request() {
    let expected = materialized_request(rows()).await;
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;

    // Calls deliberately split rows to prove rows may span calls.
    let cells: Vec<SqlType> = rows().into_iter().flatten().collect();
    client.write_streamed_tvp_cells(&cells[..2]).await.unwrap();
    client.write_streamed_tvp_cells(&cells[2..7]).await.unwrap();
    client.write_streamed_tvp_cells(&cells[7..]).await.unwrap();
    let status = client.end_streamed_param().await.unwrap();

    assert!(matches!(status, StreamedParamStatus::Complete(_)));
    assert_eq!(take_sent(&sent), expected);
    assert!(matches!(
        client.streamed_write_state,
        StreamedWriteState::Idle
    ));
}

#[tokio::test]
async fn a_table_closed_without_rows_is_an_empty_table() {
    let expected = materialized_request(Vec::new()).await;
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;
    client.write_streamed_tvp_cells(&[]).await.unwrap();

    client.end_streamed_param().await.unwrap();

    let payload = take_sent(&sent);
    assert_eq!(payload, expected);
    assert!(ends_with(&payload, &[TVP_END_TOKEN, TVP_END_TOKEN]));
}

#[tokio::test]
async fn plp_cells_stream_values_nulls_and_empty_values() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;

    for (id, chunks) in [
        (1, Some(vec![vec![0xAA, 0xBB], vec![0xCC]])),
        (2, None),
        (3, Some(Vec::new())),
    ] {
        client
            .write_streamed_tvp_cells(&[SqlType::Int(Some(id)), text("x")])
            .await
            .unwrap();
        client.begin_streamed_tvp_cell().await.unwrap();
        match &chunks {
            Some(chunks) => {
                for chunk in chunks {
                    client.write_streamed_chunk(chunk).await.unwrap();
                }
            }
            None => client.write_streamed_null().unwrap(),
        }
        client.end_streamed_tvp_cell().await.unwrap();
    }
    client.end_streamed_param().await.unwrap();

    let int = |value: i32| {
        let mut bytes = vec![4];
        bytes.extend(value.to_le_bytes());
        bytes
    };
    let x = [2, 0, b'x', 0];
    let mut expected = Vec::new();
    for (id, body) in [
        (
            1,
            [
                &PLP_UNKNOWN_LEN_BYTES[..],
                &2u32.to_le_bytes(),
                &[0xAA, 0xBB],
                &1u32.to_le_bytes(),
                &[0xCC],
                &PLP_TERMINATOR_BYTES,
            ]
            .concat(),
        ),
        (2, PLP_NULL_BYTES.to_vec()),
        (
            3,
            [&PLP_UNKNOWN_LEN_BYTES[..], &PLP_TERMINATOR_BYTES].concat(),
        ),
    ] {
        expected.push(TVP_ROW_TOKEN);
        expected.extend(int(id));
        expected.extend(x);
        expected.extend(body);
    }
    expected.push(TVP_END_TOKEN);
    assert!(ends_with(&take_sent(&sent), &expected));
}

/// Only the rows in flight are held: full packets leave while the
/// application is still supplying rows, as msodbcsql's `PushTVPRowData` does.
#[tokio::test]
async fn full_packets_are_sent_before_the_table_closes() {
    let many: Vec<Vec<SqlType>> = (0..2_000)
        .map(|id| {
            vec![
                SqlType::Int(Some(id)),
                text("payload"),
                SqlType::VarBinaryMax(Some(vec![0x5A; 16])),
            ]
        })
        .collect();
    let expected = materialized_request(many.clone()).await;
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;

    for row in &many[..many.len() / 2] {
        client.write_streamed_tvp_cells(row).await.unwrap();
    }
    let sent_mid_table = sent.lock().unwrap().len();
    assert!(
        sent_mid_table > 4096,
        "half the rows must already be on the wire, sent {sent_mid_table} bytes"
    );
    for row in &many[many.len() / 2..] {
        client.write_streamed_tvp_cells(row).await.unwrap();
    }
    client.end_streamed_param().await.unwrap();

    assert_eq!(take_sent(&sent), expected);
}

#[tokio::test]
async fn closing_mid_row_is_rejected_and_the_row_can_finish() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;
    let row: Vec<SqlType> = rows().remove(0);
    client.write_streamed_tvp_cells(&row[..2]).await.unwrap();

    let error = client.end_streamed_param().await.unwrap_err();
    assert!(matches!(error, UsageError(_)), "{error:?}");
    assert_active(&client);

    client.write_streamed_tvp_cells(&row[2..]).await.unwrap();
    client.end_streamed_param().await.unwrap();
    assert_eq!(
        take_sent(&sent),
        materialized_request(rows()[..1].to_vec()).await
    );
}

#[tokio::test]
async fn closing_with_an_open_cell_is_rejected() {
    let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;
    client
        .write_streamed_tvp_cells(&[SqlType::Int(Some(1)), text("x")])
        .await
        .unwrap();
    client.begin_streamed_tvp_cell().await.unwrap();

    assert!(matches!(
        client.end_streamed_param().await,
        Err(UsageError(_))
    ));
    assert!(matches!(
        client
            .write_streamed_tvp_cells(&[SqlType::Int(Some(1))])
            .await,
        Err(UsageError(_))
    ));
    assert!(matches!(
        client.begin_streamed_tvp_cell().await,
        Err(UsageError(_))
    ));
    assert_active(&client);

    client.end_streamed_tvp_cell().await.unwrap();
    client.end_streamed_param().await.unwrap();
}

#[tokio::test]
async fn chunks_and_nulls_need_an_open_cell() {
    let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;

    assert!(matches!(
        client.write_streamed_chunk(&[1]).await,
        Err(UsageError(_))
    ));
    assert!(matches!(client.write_streamed_null(), Err(UsageError(_))));
    assert!(matches!(
        client.end_streamed_tvp_cell().await,
        Err(UsageError(_))
    ));
    assert_active(&client);
    client.end_streamed_param().await.unwrap();
}

#[tokio::test]
async fn only_max_columns_can_be_streamed_by_cell() {
    let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;

    let error = client.begin_streamed_tvp_cell().await.unwrap_err();
    assert!(matches!(&error, UsageError(message) if message.contains("column 1")));
    assert_active(&client);
    client.end_streamed_param().await.unwrap();
}

#[tokio::test]
async fn table_calls_need_an_open_table() {
    let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
    client
        .begin_sp_executesql(SQL.to_string(), vec![streamed_varbinary("@v")], ())
        .await
        .unwrap();

    assert!(matches!(
        client
            .write_streamed_tvp_cells(&[SqlType::Int(Some(1))])
            .await,
        Err(UsageError(_))
    ));
    assert!(matches!(
        client.begin_streamed_tvp_cell().await,
        Err(UsageError(_))
    ));
    assert!(matches!(
        client.end_streamed_tvp_cell().await,
        Err(UsageError(_))
    ));
    assert_active(&client);
    client.end_streamed_param().await.unwrap();

    assert!(matches!(
        client.write_streamed_tvp_cells(&[]).await,
        Err(UsageError(_))
    ));
}

/// A cell that does not match its column is refused before any byte of the
/// call is written, so the stream stays usable.
#[tokio::test]
async fn a_mismatched_cell_rejects_the_whole_call() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;
    let first = rows().remove(0);
    client.write_streamed_tvp_cells(&first).await.unwrap();

    let error = client
        .write_streamed_tvp_cells(&[SqlType::Int(Some(2)), SqlType::Int(Some(2))])
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        Error::TvpCellSerialization {
            parameter: TvpParameterRef::Name(name),
            row: 2,
            column: 2,
            ..
        } if name == "@t"
    ));
    assert_active(&client);
    client.end_streamed_param().await.unwrap();
    assert_eq!(take_sent(&sent), materialized_request(vec![first]).await);
}

/// A cell rejected while it is being written may already have earlier rows
/// on the wire, so the request is withdrawn rather than resumed.
#[tokio::test]
async fn a_cell_failing_mid_write_withdraws_the_request() {
    let narrow =
        |value: &str| SqlType::Varchar(Some(SqlString::from_utf8_string(value.to_string())), 2);
    let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
    client
        .begin_sp_executesql(
            SQL.to_string(),
            vec![streamed_table(vec![
                TvpColumnDef::new(SqlType::Int(None)),
                TvpColumnDef::new(SqlType::Varchar(None, 2)),
            ])],
            (),
        )
        .await
        .unwrap();
    client
        .write_streamed_tvp_cells(&[SqlType::Int(Some(1)), narrow("ok")])
        .await
        .unwrap();

    let error = client
        .write_streamed_tvp_cells(&[SqlType::Int(Some(2)), narrow("too long")])
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        Error::TvpCellSerialization {
            parameter: TvpParameterRef::Name(name),
            row: 2,
            column: 2,
            source,
        } if name == "@t" && matches!(**source, Error::EncodedValueTooLong { .. })
    ));
    assert!(matches!(
        client.streamed_write_state,
        StreamedWriteState::Idle
    ));
    assert!(matches!(
        client.end_streamed_param().await,
        Err(UsageError(_))
    ));
}

#[tokio::test]
async fn a_send_failure_inside_a_table_aborts_the_stream() {
    let (mut client, fail) = create_failing_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;
    fail.store(true, std::sync::atomic::Ordering::SeqCst);

    let row = vec![
        SqlType::Int(Some(1)),
        text("x"),
        SqlType::VarBinaryMax(Some(vec![0; 10_000])),
    ];
    assert!(client.write_streamed_tvp_cells(&row).await.is_err());

    assert!(matches!(
        client.streamed_write_state,
        StreamedWriteState::Idle
    ));
    assert!(client.transport.connection_known_dead());
}

/// Streamed scalars, a streamed table and a trailing materialized value keep
/// their order: the table's end token precedes the value written after it.
#[tokio::test]
async fn a_table_between_streamed_and_materialized_values_keeps_order() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    let trailing = RpcParameter::new(
        Some("@n".to_string()),
        StatusFlags::NONE,
        SqlType::Int(Some(0x5A5A_5A5A)),
    );
    let status = client
        .begin_sp_executesql(
            "SELECT @v, @n FROM @t".to_string(),
            vec![
                streamed_varbinary("@v"),
                streamed_table(columns()),
                trailing,
            ],
            (),
        )
        .await
        .unwrap();
    assert!(matches!(status, StreamedParamStatus::NeedData { param_name } if param_name == "@v"));
    assert!(matches!(
        client.write_streamed_tvp_cells(&[]).await,
        Err(UsageError(_))
    ));

    client.write_streamed_chunk(&[0xEE]).await.unwrap();
    let status = client.end_streamed_param().await.unwrap();
    assert!(matches!(status, StreamedParamStatus::NeedData { param_name } if param_name == "@t"));
    client
        .write_streamed_tvp_cells(&rows().remove(0))
        .await
        .unwrap();
    let status = client.end_streamed_param().await.unwrap();
    assert!(matches!(status, StreamedParamStatus::Complete(_)));

    let payload = take_sent(&sent);
    let value = 0x5A5A_5A5Au32.to_le_bytes();
    assert!(ends_with(&payload, &value));
    let declaration = utf16le("[dbo].[Items] READONLY");
    assert!(
        payload.windows(declaration.len()).any(|w| w == declaration),
        "the table is declared by its type name"
    );
    let streamed_scalar = [1, 0, 0, 0, 0xEE, 0, 0, 0, 0];
    let row_tail = [3, 0, 0, 0, 1, 2, 3, 0, 0, 0, 0, TVP_END_TOKEN];
    let scalar_at = find_last(&payload, &streamed_scalar).expect("@v is written");
    let table_end = find_last(&payload, &row_tail).expect("the row and table end are written");
    assert!(scalar_at < table_end, "@v precedes the table");
    assert!(
        table_end + row_tail.len() < payload.len() - value.len(),
        "@n follows the table end"
    );
}

#[tokio::test]
async fn a_table_without_columns_is_rejected_before_sending() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

    let error = client
        .begin_sp_executesql(SQL.to_string(), vec![streamed_table(Vec::new())], ())
        .await
        .unwrap_err();

    assert!(matches!(error, UsageError(_)));
    assert!(sent.lock().unwrap().is_empty());
    assert!(matches!(
        client.streamed_write_state,
        StreamedWriteState::Idle
    ));
}

#[tokio::test]
async fn cancelling_inside_a_table_discards_the_request() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    begin_table(&mut client).await;
    client
        .write_streamed_tvp_cells(&rows().remove(0))
        .await
        .unwrap();

    client.cancel_streamed_write().await;

    assert!(matches!(
        client.streamed_write_state,
        StreamedWriteState::Idle
    ));
    assert!(sent.lock().unwrap().is_empty(), "nothing had left yet");
}

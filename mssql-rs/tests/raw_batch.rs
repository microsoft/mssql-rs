// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[expect(
    clippy::indexing_slicing,
    reason = "fixed wire offsets in mock packets"
)]
mod wire {
    use mssql_mock_tds::protocol::{
        PACKET_HEADER_SIZE, PacketHeader, PacketType, build_done_token, build_login_ack,
        build_prelogin_response, build_query_result, parse_sql_batch,
    };
    use mssql_mock_tds::{ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    pub async fn read(socket: &mut TcpStream, expected: PacketType) -> Vec<u8> {
        let mut message = Vec::new();
        loop {
            let mut header = [0; PACKET_HEADER_SIZE];
            socket.read_exact(&mut header).await.expect("packet header");
            let header = PacketHeader::parse(&mut header.as_slice()).expect("valid packet header");
            assert_eq!(header.packet_type, expected);
            let offset = message.len();
            message.resize(offset + usize::from(header.length) - PACKET_HEADER_SIZE, 0);
            socket
                .read_exact(&mut message[offset..])
                .await
                .expect("packet body");
            if header.status.is_end_of_message() {
                return message;
            }
        }
    }

    pub async fn expect_batch(socket: &mut TcpStream, sql: &str) {
        let body = read(socket, PacketType::SqlBatch).await;
        assert_eq!(parse_sql_batch(&body).expect("valid SQL batch"), sql);
    }

    pub async fn login(socket: &mut TcpStream) {
        read(socket, PacketType::PreLogin).await;
        socket
            .write_all(&build_prelogin_response())
            .await
            .expect("prelogin response");
        read(socket, PacketType::Login7).await;
        let mut tokens = build_login_ack();
        tokens.extend_from_slice(&build_done_token(0));
        send(socket, &tokens, 1, true).await;
    }

    pub async fn send(socket: &mut TcpStream, tokens: &[u8], id: u8, eom: bool) {
        let length = u16::try_from(PACKET_HEADER_SIZE + tokens.len()).expect("test packet fits");
        let [hi, lo] = length.to_be_bytes();
        let mut packet = vec![
            PacketType::TabularResult as u8,
            u8::from(eom),
            hi,
            lo,
            0,
            0,
            id,
            0,
        ];
        packet.extend_from_slice(tokens);
        socket
            .write_all(&packet)
            .await
            .expect("write response packet");
    }

    pub fn mixed_response() -> Vec<u8> {
        let mut count = build_done_token(7);
        count[1] = 0x11; // DONE_MORE | DONE_COUNT
        count[3] = 0xc5; // UPDATE
        let mut empty = rowset("empty", &[]);
        let done = empty.len() - build_done_token(0).len();
        empty[done + 1] = 0x11; // DONE_MORE | DONE_COUNT
        let mut nonempty = rowset("n", &[42]);
        nonempty.truncate(nonempty.len() - build_done_token(0).len());
        [count.as_ref(), &empty, &nonempty].concat()
    }

    pub fn rowset(name: &str, values: &[i32]) -> Vec<u8> {
        let response = QueryResponse::new(
            vec![ColumnDefinition::new(name, SqlDataType::Int)],
            values
                .iter()
                .map(|&n| Row::new(vec![ColumnValue::Int(n)]))
                .collect(),
        );
        build_query_result(&response)[PACKET_HEADER_SIZE..].to_vec()
    }
}

use std::time::Duration;

use mssql_mock_tds::TerminalError;
use mssql_mock_tds::protocol::{
    PacketType, build_attention_ack_packet, build_done_token, build_terminal_error_tokens,
};
use mssql_rs::{
    AuthMethod, BatchErrorMode, BatchItem, Client, Config, EncryptionLevel, Error, ExecuteOptions,
};
use mssql_tds::error::Error as TdsError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const BATCH: &str = "UPDATE t SET n = 42; SELECT n AS empty FROM t WHERE 1 = 0; SELECT 42 AS n";
const FOLLOW_UP: &str = "SELECT 9 AS n";

#[derive(Clone, Copy)]
enum Tail {
    Complete,
    Cancel,
    BufferedError,
    MissingAck,
    RequestTimeout,
}

async fn exercise_batch(tail: Tail) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let mut config = Config::new();
    config
        .host("127.0.0.1")
        .port(listener.local_addr().expect("mock server address").port())
        .authentication(AuthMethod::sql_server("mock", String::new()))
        .encryption(EncryptionLevel::Off);

    let server = async {
        let (mut socket, _) = listener.accept().await.expect("accept client");
        wire::login(&mut socket).await;
        wire::expect_batch(&mut socket, BATCH).await;
        let mut tokens = wire::mixed_response();
        if matches!(tail, Tail::BufferedError) {
            tokens.extend_from_slice(&build_terminal_error_tokens(&TerminalError::new(
                50000,
                16,
                "unread statement error",
            )));
            wire::send(&mut socket, &tokens, 1, true).await;
        } else {
            wire::send(&mut socket, &tokens, 1, false).await;
            if matches!(tail, Tail::Complete) {
                wire::send(&mut socket, &build_done_token(0), 2, true).await;
            } else {
                wire::read(&mut socket, PacketType::Attention).await;
                if matches!(tail, Tail::MissingAck) {
                    assert_eq!(socket.read(&mut [0]).await.expect("client disconnect"), 0);
                    return;
                }
                socket
                    .write_all(&build_attention_ack_packet())
                    .await
                    .expect("send ATTENTION ACK");
            }
        }
        wire::expect_batch(&mut socket, FOLLOW_UP).await;
        wire::send(&mut socket, &wire::rowset("n", &[9]), 1, true).await;
    };

    let consumer = async {
        let mut client = Client::connect(&config)
            .await
            .expect("connect to mock server");
        // Cancelling an unstarted batch must not send it.
        client
            .simple_query_batch("unused", ())
            .cancel(1)
            .await
            .expect("cancel unstarted batch");
        let mut options = ExecuteOptions::new();
        if matches!(tail, Tail::BufferedError) {
            options.on_error = BatchErrorMode::Continue;
        } else if matches!(tail, Tail::RequestTimeout) {
            options = options.timeout_secs(1);
        }
        let mut batch = client.simple_query_batch(BATCH, options);

        assert!(matches!(
            batch.next_item().await.expect("read affected-row count"),
            Some(BatchItem::NoRows {
                rows_affected: Some(7)
            })
        ));
        for (index, name) in ["empty", "n"].into_iter().enumerate() {
            let item = batch.next_item().await.expect("read metadata");
            assert!(matches!(&item, Some(BatchItem::ResultSetStart(meta))
                if meta.result_index() == index
                    && meta.columns().first().map(|column| column.name()) == Some(name)));
            if index == 0 {
                assert!(matches!(
                    batch.next_item().await.expect("finish empty rowset"),
                    Some(BatchItem::ResultSetEnd { row_count: Some(0) })
                ));
            }
        }
        assert!(
            matches!(batch.next_item().await.expect("read row"), Some(BatchItem::Row(row))
            if row.get::<i32, _>("n") == Some(42) && row.result_index() == 1)
        );

        match tail {
            Tail::Complete => {
                assert!(matches!(
                    batch.next_item().await.expect("finish rowset"),
                    Some(BatchItem::ResultSetEnd { row_count: None })
                ));
                assert!(batch.next_item().await.expect("finish batch").is_none());
                batch.cancel(1).await.expect("cancel completed batch");
            }
            Tail::Cancel | Tail::BufferedError => {
                batch.cancel(1).await.expect("settle explicit cancellation")
            }
            Tail::MissingAck => {
                assert!(batch.cancel(1).await.is_err());
                assert!(client.is_connection_dead());
                return;
            }
            Tail::RequestTimeout => {
                assert!(matches!(
                    batch.next_item().await,
                    Err(Error::Tds(TdsError::TimeoutError(_)))
                ));
                assert!(
                    batch
                        .next_item()
                        .await
                        .expect("batch ended after timeout")
                        .is_none()
                );
                drop(batch);
            }
        }
        assert!(!client.is_connection_dead());
        let result = client
            .simple_query(FOLLOW_UP)
            .await
            .expect("query after batch cleanup")
            .into_first_result();
        assert_eq!(
            result.first().expect("follow-up row").get::<i32, _>("n"),
            Some(9)
        );
    };
    tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(server, consumer)
    })
    .await
    .expect("scripted batch exchange did not complete");
}

#[tokio::test]
async fn streams_statement_counts_and_rowset_boundaries() {
    exercise_batch(Tail::Complete).await;
}

#[tokio::test]
async fn row_limit_sends_attention_and_preserves_connection() {
    exercise_batch(Tail::Cancel).await;
}

#[tokio::test]
async fn cancel_discards_buffered_statement_error_without_attention() {
    exercise_batch(Tail::BufferedError).await;
}

#[tokio::test]
async fn missing_attention_ack_retires_connection() {
    exercise_batch(Tail::MissingAck).await;
}

#[tokio::test]
async fn native_request_timeout_applies_while_reading_rows() {
    exercise_batch(Tail::RequestTimeout).await;
}

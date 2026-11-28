// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Commit/rollback-and-begin responses, without a live server.

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::message::transaction_management::{CreateTxnParams, TransactionIsolationLevel};
use mssql_tds::test_client_support::{
    ScriptedToken, done_no_more, env_change_begin_transaction, env_change_commit_transaction,
    env_change_rollback_transaction, tds_client_from_tokens_capturing,
};

const NEW_DESCRIPTOR: u64 = 0x0102_0304_0506_0708;
const PACKET_HEADER_LEN: usize = 8;
// ALL_HEADERS total length, then the descriptor header's length and type.
const DESCRIPTOR_OFFSET: usize = PACKET_HEADER_LEN + 4 + 4 + 2;

fn begin_new() -> Option<CreateTxnParams> {
    Some(CreateTxnParams {
        level: TransactionIsolationLevel::NoChange,
        name: None,
    })
}

fn packets(sent: &[u8]) -> Vec<&[u8]> {
    let mut packets = Vec::new();
    let mut rest = sent;
    while !rest.is_empty() {
        let len = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let (packet, tail) = rest.split_at(len);
        packets.push(packet);
        rest = tail;
    }
    packets
}

fn descriptor(packet: &[u8]) -> u64 {
    u64::from_le_bytes(
        packet[DESCRIPTOR_OFFSET..DESCRIPTOR_OFFSET + 8]
            .try_into()
            .unwrap(),
    )
}

async fn assert_end_and_begin_switches_descriptor(
    end_ack: ScriptedToken,
    end_and_begin: impl AsyncFnOnce(&mut TdsClient),
) {
    let (mut client, sent) = tds_client_from_tokens_capturing(vec![
        end_ack,
        env_change_begin_transaction(NEW_DESCRIPTOR),
        done_no_more(),
        env_change_rollback_transaction(),
        done_no_more(),
    ]);

    end_and_begin(&mut client).await;
    assert!(client.has_active_transaction());
    client.rollback_transaction(None, None).await.unwrap();
    assert!(!client.has_active_transaction());

    let sent = sent.lock().unwrap();
    let packets = packets(&sent);
    assert_eq!(packets.len(), 2);
    assert_eq!(descriptor(packets[0]), 0);
    assert_eq!(descriptor(packets[1]), NEW_DESCRIPTOR);
}

#[tokio::test]
async fn commit_and_begin_adopts_new_descriptor() {
    assert_end_and_begin_switches_descriptor(
        env_change_commit_transaction(),
        async |client: &mut TdsClient| {
            client.commit_transaction(None, begin_new()).await.unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn rollback_and_begin_adopts_new_descriptor() {
    assert_end_and_begin_switches_descriptor(
        env_change_rollback_transaction(),
        async |client: &mut TdsClient| {
            client
                .rollback_transaction(None, begin_new())
                .await
                .unwrap();
        },
    )
    .await;
}

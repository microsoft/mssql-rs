// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RPC request text is written from UTF-8 straight into the packet: each
//! text is one `nvarchar(max)` PLP chunk, and the `@params` declaration
//! uses msodbcsql's separators.

use super::*;
use crate::connection::client_context::ExecutionColumnEncryptionSetting;

const UNICODE_SQL: &str = "SELECT @P1 AS [é你好𐐀], N'𐐀'; SET @P2 = 1";
const EXECUTESQL: [u8; 6] = [0xFF, 0xFF, 10, 0, 0, 0];
const PREPEXEC: [u8; 6] = [0xFF, 0xFF, 13, 0, 0, 0];
const PREPARE: [u8; 6] = [0xFF, 0xFF, 11, 0, 0, 0];

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn take_sent(sent: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u8> {
    reassemble_sent(&std::mem::take(&mut *sent.lock().unwrap()))
}

fn int_param(name: &str, flags: StatusFlags) -> RpcParameter {
    RpcParameter::new(Some(name.to_string()), flags, SqlType::Int(Some(7)))
}

fn handle_token(handle: i32) -> Tokens {
    let mut token = ae_return_value_token("", ColumnValues::Int(handle), None);
    token.status = crate::token::tokenitems::ReturnValueStatus::OutputParam;
    Tokens::ReturnValue(token)
}

/// The positional `nvarchar(max)` parameter carrying `text`, as the
/// materialized UTF-16 value used to be sent.
fn text_param(client: &TdsClient, text: &str) -> Vec<u8> {
    let collation = client.negotiated_settings.database_collation;
    let mut bytes = vec![0, 0, 0xE7, 0xFF, 0xFF];
    bytes.extend(collation.info.to_le_bytes());
    bytes.push(collation.sort_id);
    bytes.extend(PLP_UNKNOWN_LEN_BYTES);
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    if !utf16.is_empty() {
        bytes.extend(u32::try_from(utf16.len()).unwrap().to_le_bytes());
        bytes.extend(utf16);
    }
    bytes.extend(PLP_TERMINATOR_BYTES);
    bytes
}

/// The same text as a named `nvarchar(max)` parameter, as the
/// `sp_describe_parameter_encryption` request sends `@tsql` and `@params`.
fn named_text_param(client: &TdsClient, name: &str, text: &str) -> Vec<u8> {
    let mut bytes = vec![u8::try_from(name.len()).unwrap()];
    bytes.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
    bytes.extend(&text_param(client, text)[1..]);
    bytes
}

fn enable_always_encrypted(client: &mut TdsClient) {
    use crate::message::features::always_encrypted::AlwaysEncryptedFeature;
    use crate::message::login::Feature;

    let mut feature = AlwaysEncryptedFeature::default();
    feature.set_acknowledged(true);
    client
        .negotiated_settings
        .session_settings
        .supported_features
        .push(Box::new(feature));
}

/// An `sp_describe_parameter_encryption` response reporting no CEKs and
/// no encrypted parameters, so the values then go out in plaintext.
fn empty_describe_response() -> [Tokens; 4] {
    [
        int_col_metadata(1),
        done_more(),
        int_col_metadata(1),
        done_no_more(),
    ]
}

/// Splits the captured bytes at the RPC that follows the describe request.
fn split_at_rpc<'b>(bytes: &'b [u8], proc_header: &[u8]) -> (&'b [u8], &'b [u8]) {
    let at = bytes
        .windows(proc_header.len())
        .position(|window| window == proc_header)
        .expect("RPC follows the describe request");
    bytes.split_at(at)
}

#[tokio::test]
async fn sp_executesql_without_parameters_sends_an_empty_declaration() {
    let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
    client
        .execute_sp_executesql("SELECT 1".to_string(), Vec::new(), ())
        .await
        .unwrap();
    let expected = [
        EXECUTESQL.to_vec(),
        text_param(&client, "SELECT 1"),
        text_param(&client, ""),
    ]
    .concat();
    assert!(take_sent(&sent).ends_with(&expected));
}

#[tokio::test]
async fn sp_prepare_sends_declaration_then_statement() {
    let (mut client, sent) = create_capturing_client(vec![handle_token(55), done_no_more()]);
    client
        .execute_sp_prepare(
            UNICODE_SQL.to_string(),
            vec![int_param("@P1", StatusFlags::NONE)],
            false,
            (),
        )
        .await
        .unwrap();
    let bytes = take_sent(&sent);
    assert!(contains(&bytes, &PREPARE));
    let expected = [
        text_param(&client, "@P1 int"),
        text_param(&client, UNICODE_SQL),
    ]
    .concat();
    assert!(bytes.ends_with(&expected));
}

#[tokio::test]
async fn streamed_prepexec_declares_the_streamed_value_last() {
    let (mut client, sent) = create_capturing_client(vec![handle_token(55), done_no_more()]);
    let mut statement = PreparedStatement::new(UNICODE_SQL);
    let mut orphan = None;
    client
        .begin_execute_prepared(
            &mut statement,
            vec![
                streamed_varbinary("@P1"),
                int_param("@P2", StatusFlags::NONE),
            ],
            &mut orphan,
            (),
        )
        .await
        .unwrap();
    client.write_streamed_chunk(&[1, 2]).await.unwrap();
    client
        .end_execute_prepared_param(&mut statement, &mut orphan)
        .await
        .unwrap();
    let expected = [
        text_param(&client, "@P2 int,@P1 varbinary(max)"),
        text_param(&client, UNICODE_SQL),
    ]
    .concat();
    assert!(contains(&take_sent(&sent), &expected));
}

/// Always Encrypted sends the declaration to
/// `sp_describe_parameter_encryption`, then sends the identical text in
/// the RPC it describes. Output parameters bypass the describe cache, so
/// both executions make a round trip.
#[tokio::test]
async fn always_encrypted_describes_and_sends_the_same_declaration() {
    const DECLARATION: &str = "@P1 int,@P2 int OUTPUT";
    let mut tokens = Vec::from(empty_describe_response());
    tokens.push(done_no_more());
    tokens.extend(empty_describe_response());
    tokens.extend([handle_token(55), done_no_more()]);
    let (mut client, sent) = create_capturing_client(tokens);
    enable_always_encrypted(&mut client);
    let options =
        || ExecuteOptions::new().column_encryption(ExecutionColumnEncryptionSetting::Enabled);
    let params = || {
        vec![
            int_param("@P1", StatusFlags::NONE),
            int_param("@P2", StatusFlags::BY_REF_VALUE),
        ]
    };
    let described = |client: &TdsClient| {
        [
            named_text_param(client, "@tsql", UNICODE_SQL),
            named_text_param(client, "@params", DECLARATION),
        ]
        .concat()
    };

    client
        .execute_sp_executesql(UNICODE_SQL.to_string(), params(), options())
        .await
        .unwrap();
    client.close_query().await.unwrap();
    assert_eq!(client.describe_round_trips(), 1);
    let bytes = take_sent(&sent);
    let (describe, rpc) = split_at_rpc(&bytes, &EXECUTESQL);
    assert!(contains(describe, &described(&client)));
    let expected = [
        EXECUTESQL.to_vec(),
        text_param(&client, UNICODE_SQL),
        text_param(&client, DECLARATION),
    ]
    .concat();
    assert!(rpc.starts_with(&expected));

    let mut statement = PreparedStatement::new(UNICODE_SQL);
    client
        .execute_prepared(&mut statement, params(), &mut None, options())
        .await
        .unwrap();
    assert_eq!(client.describe_round_trips(), 2);
    let id = statement.id().expect("the @handle was captured");
    assert!(client.prepared_param_encryption.contains_key(&id));
    let bytes = take_sent(&sent);
    let (describe, rpc) = split_at_rpc(&bytes, &PREPEXEC);
    assert!(contains(describe, &described(&client)));
    let expected = [
        text_param(&client, DECLARATION),
        text_param(&client, UNICODE_SQL),
    ]
    .concat();
    assert!(contains(rpc, &expected));
}

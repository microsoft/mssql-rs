# Data Classification

`mssql-tds` can request SQL Server's Data Classification metadata and expose it
for each result set: the sensitivity labels, information types, and (with
version 2) ranks that `ADD SENSITIVITY CLASSIFICATION` assigns to columns.

Work items: feature [AB#42847](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/42847),
tasks [AB#49357](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/49357) and
[AB#49358](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/49358).
The ODBC surface (`SQL_CA_SS_DATA_CLASSIFICATION`) is tracked separately under
Feature 42845; see [Follow-up for the ODBC layer](#follow-up-for-the-odbc-layer).

## Using it

The feature is off by default. Request a version in the client context, then
read the classification of the positioned result set:

```rust,ignore
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::token::data_classification::DataClassificationVersion;

context.requested_data_classification_version = Some(DataClassificationVersion::Version2);
let mut client = TdsConnectionProvider {}.create_client(context, &datasource, None).await?;

client.execute("SELECT id, ssn FROM dbo.people".to_string(), ()).await?;
if let Some(classification) = client.data_classification() {
    for (column, sensitivity) in classification.columns().iter().enumerate() {
        for property in sensitivity.properties() {
            let label = classification.resolve_label(property.sensitivity_label());
            let information_type =
                classification.resolve_information_type(property.information_type());
            println!(
                "column {column}: {:?} {:?} {:?}",
                label.map(|label| label.name()),
                information_type.map(|information_type| information_type.name()),
                property.rank(),
            );
        }
    }
}
```

- `TdsClient::negotiated_data_classification_version()` reports the version in
  effect for the connection, or `None` when the server did not enable it.
- `ResultSet::data_classification()` is available as soon as a result set is
  positioned, before any row is read, and until the client moves to the next
  result set, starts another request, cancels, or reconnects.
- `columns()` covers the result set's leading columns. It is empty when the
  server sent no per-column data, and one entry short for a server cursor (see
  [Result sets](#result-sets)). Look entries up with `columns().get(i)` and treat a
  missing entry as unclassified.
- A dictionary reference of `0xFFFF` is `Unspecified` and resolves to `None`.
- Ranks are `None` unless version 2 is in effect.

## Protocol flow

```text
LOGIN7 FeatureExt 0x09 (requested version)
  -> FEATUREEXTACK 0x09 (version, IsEnabled)
  -> version in effect for the connection
  -> COLMETADATA
  -> DATACLASSIFICATION 0xA3 (dictionaries, rank, per-column properties)
  -> attached to that result set
```

### Negotiation

The LOGIN7 block is `09 01 00 00 00 <version>`. It is added only when
`requested_data_classification_version` is set; otherwise LOGIN7 is unchanged.

The acknowledgement is `version | IsEnabled | version-specific data`:

| Acknowledgement | Result |
|---|---|
| None | Feature off. A `0xA3` that arrives anyway is rejected. |
| Fewer than 2 bytes | Login fails with a protocol error. |
| Version other than 1 or 2 | Login fails with a protocol error. |
| `IsEnabled = 0` | Feature off. |
| Version 1 or 2, enabled | On. The layout is the lower of the requested and acknowledged versions. |

The token carries no version of its own. [MS-TDS] 2.2.7.5 sends
`SensitivityRank` only when both the request and the acknowledgement are
version 2, so the layout is `min(requested, acknowledged)`. A higher
acknowledgement is accepted, as msodbcsql, SqlClient and JDBC do, but does not
change the layout.

### Token parsing

The token has no length prefix, so it is parsed field by field
(`token/parsers/data_classification_parser.rs`):

- Both dictionaries, then the result-set rank (version 2), then per-column
  property lists, each property a label index, an information-type index and
  (version 2) a rank.
- Indexes must be in range or `0xFFFF`; ranks must be -1, 0, 10, 20, 30 or 40.
- Every count is checked against the remaining byte budget before anything is
  allocated, and initial capacity is capped at 1,024 entries per list.
- The token may not exceed 16 MiB. The worst case SQL Server produces is
  4,251,658 bytes: 4,096 columns, each with its own label and information type,
  all four strings at the 128-character `sysname` maximum. Live, 252 such
  columns produce 261,586 bytes, matching the formula.
- An unpaired UTF-16 surrogate in a name or id is replaced with U+FFFD. SQL
  Server stores such strings as given.

### Result sets

The client reads `0xA3` only immediately after COLMETADATA, and only when the
connection negotiated the feature. The classification is paired with that
COLMETADATA (`Arc::ptr_eq`), so it cannot surface on another result set. It is
cleared when the client advances, starts a new request, closes or cancels the
response, or adopts a recovered session. A `0xA3` anywhere else belongs to no
result set and is skipped.

The per-column list may cover fewer columns than the result set. For a STATIC
or FAST_FORWARD server cursor, SQL Server classifies the selected columns but
not the trailing ROWSTAT column it adds (live: 4 metadata columns, 3
classified). Those trailing columns are unclassified. A list longer than the
result set cannot be mapped, so it is dropped with a warning. In both cases the
token was read whole, so the connection is unaffected.

### Errors and connection safety

| Situation | Result | Connection |
|---|---|---|
| Out-of-range index, unknown rank, budget exceeded | Protocol error | Retired; session recovery disarmed |
| `0xA3` on a connection that did not negotiate it | Protocol error | Retired; session recovery disarmed |
| Transport error or truncation inside the token | The reader error | Retired; recovery kept armed for a lost transport |
| Timeout or cancellation during the read | Timeout or cancellation error | Settled through ATTENTION; usable afterwards |
| Fewer classified columns than result columns | Accepted | Unaffected |
| More classified columns than result columns | Classification dropped, warning logged | Unaffected |
| Unpaired surrogate in a string | Replaced with U+FFFD | Unaffected |

A rejected token leaves the rest of it unread, and those bytes can pass for
other tokens, a final DONE among them. The connection is therefore retired in
both batch error modes, so a later `close_query` cannot report success over a
desynchronized stream.

### Session recovery

The reconnect LOGIN7 requests the feature again from the client context. The
recovered session parses with its own acknowledgement, so a server that
downgrades or drops the feature on reconnect is honored, and the version the
dead session negotiated is never reused.

## Unknown tokens

An unrecognized token type is a hard protocol error, and the connection is
retired. This is unchanged, and Data Classification does not rely on skipping:
`0xA3` is a known token type with its own parser.

[MS-TDS] 2.2.4.2.1 assigns each token a framing class by bits 5–4 of its type
byte: zero length (`01`), fixed length (`11`, 1/2/4/8 bytes), variable length
(`10`, followed by a length) and variable count (`00`). A generic skip would
have to trust that class for a token it does not know. The known tokens show it
cannot:

| Token | Class from the type byte | Actual framing |
|---|---|---|
| ROW `0xD1`, NBCROW `0xD2`, ALTROW `0xD3` | Zero length | Column data follows |
| DONE `0xFD`, DONEPROC `0xFE`, DONEINPROC `0xFF` | Fixed, 8 bytes | 12 bytes |
| DATACLASSIFICATION `0xA3`, RETURNVALUE `0xAC`, FEATUREEXTACK `0xAE` | Variable length | No length prefix |
| SESSIONSTATE `0xE4`, FEDAUTHINFO `0xEE` | Variable length | 4-byte length, not 2 |
| COLMETADATA `0x81`, ALTMETADATA `0x88` | Variable count | Needs type parsing |

Only 10 of the 23 known token types could be skipped from the class alone. The
spec itself notes that `0xA3` "does not start with a length specification"
(2.2.4.2.1.3). Skipping an unknown token risks reading its payload as tokens,
which is worse than failing.

Blast radius of keeping the error: an unknown token can appear only if the
server sends a token this client never asked for, since servers introduce new
tokens behind a LOGIN7 feature or TDS version. The query that meets it fails
with `Unknown token type: 0x..` (or `No parser implemented` for a known type
without a parser), the connection is retired with session recovery disarmed,
and a pool discards it. The reference drivers do the same:

- msodbcsql: `default: // Unknown token, out of sync!` raises a protocol error
  (`Parse.cpp:2002`).
- SqlClient: `IsValidTdsToken` (`TdsParser.cs:2447`) fails, the connection is
  broken and `InvalidTdsTokenReceived` is thrown.
- JDBC: `throwUnexpectedTokenException` (`tdsparser.java:141`) terminates the
  connection.

Regression coverage: `tokens_without_a_parser_are_fatal_on_every_read_path`
(`io/token_stream.rs`), `a_token_without_a_parser_fails_closed`
(`tests/test_unknown_token.rs`), and
`an_unknown_token_in_the_attention_drain_retires_the_connection`
(`connection/transport/network_transport.rs`).

## Differences from the reference drivers

| Aspect | msodbcsql | SqlClient / JDBC | mssql-tds |
|---|---|---|---|
| Requested by default | Version 1 on every login | Opt-in | Opt-in |
| `IsEnabled = 0` | Ignored | Feature off | Feature off |
| Layout | Acknowledged version | Acknowledged, bounded by 2 | `min(requested, acknowledged)` |
| Exposed as | Raw token bytes | Typed objects | Typed model |
| Invalid index or rank | Passed through raw | Error | Error; connection retired |
| Token not negotiated | Parsed as version 1 | Error | Error; connection retired |
| Column count | Not checked | Not checked | Fewer accepted, more dropped |
| Unpaired surrogate | Raw bytes | U+FFFD (SqlClient) | U+FFFD |
| Token size | Unbounded | Unbounded | 16 MiB |
| After a no-row result | Previous buffer may remain | — | `None` |

## Validation

### Mock server

`tests/test_data_classification.rs` runs the client against `mssql-mock-tds`.
Parser, negotiation and transport behaviour is also covered by unit tests next
to the code.

| Criterion | Tests |
|---|---|
| Feature disabled; LOGIN7 omits `0x09` | `disabled_feature_is_not_advertised_and_queries_run_as_before`; `data_classification_is_not_requested_by_default` |
| V1 negotiation and parsing | `v1_classification_is_attached_to_its_result_set` |
| V2 negotiation and ranks | `v2_classification_carries_ranks`, `decodes_every_rank_from_signed_little_endian_wire_bytes` |
| V2 request, V1 acknowledgement | `a_v2_request_acknowledged_as_v1_parses_the_v1_layout`, `recovered_session_parses_with_the_version_it_negotiated`, `mixed_versions_parse_the_v1_layout` |
| Missing acknowledgement | `a_missing_acknowledgement_leaves_the_feature_off`, `recovered_session_without_the_feature_rejects_classification` |
| Classified and unclassified columns | `a_result_set_mixes_classified_and_unclassified_columns`, `unclassified_result_has_no_classification` |
| Multiple result sets, both orders | `each_result_set_carries_only_its_own_classification`, `consecutive_queries_do_not_share_classification`, `rpc_result_sets_carry_their_classification` |
| Cleanup on close and cancel | `closing_a_classified_result_early_leaves_no_classification`, `cancelled_classified_results_leave_no_classification` |
| Column count | `classification_covers_at_most_the_result_columns` |
| Malformed `0xA3` | `malformed_classification_on_the_first_result_set_retires_the_connection`, `invalid_references_and_ranks_are_protocol_errors`, `an_unrequested_classification_token_retires_the_connection` |
| Invalid references, counts, lengths, ranks | `rejects_invalid_label_index`, `rejects_invalid_information_type_index`, `rejects_unknown_result_set_rank`, `rejects_unknown_property_rank`, `rejects_counts_that_exceed_parser_budget_before_allocating`, `string_content_counts_toward_the_parser_budget`, `every_truncated_prefix_passes_the_reader_error_through` |
| Invalid acknowledgement | `an_unsupported_acknowledged_version_fails_the_login`, `rejects_short_acknowledgement`, `rejects_invalid_acknowledged_version` |
| Session recovery | `session_recovery_renegotiates_data_classification` |

Each test added for this work was mutation-checked: the guarded code was broken
and the test confirmed to fail.

### Live SQL Server

`tests/test_data_classification_live.rs` runs against a real server. Each test
creates its own table in tempdb (`dc_live_<random>`), applies
`ADD SENSITIVITY CLASSIFICATION`, and drops the table even when the test fails.
The login needs permission to create tables in tempdb, add classifications,
and (for the recovery test) `KILL` its own session.

```powershell
$env:DB_HOST = 'localhost'; $env:DB_PORT = '1433'
$env:DB_USERNAME = '<login>'; $env:SQL_PASSWORD = '<password>'
$env:TRUST_SERVER_CERTIFICATE = 'true'
cargo nextest run -p mssql-tds --test test_data_classification_live
```

| Test | Checks |
|---|---|
| `live_v1_classification_matches_the_server`, `live_v2_…` | Negotiation; label, information type and ids; rank at V2; unclassified `id` and `note`; row values; a following unclassified result set has none |
| `live_v1_partial_shared_and_derived_classifications_match_the_server`, `live_v2_…` | Label-only and type-only columns; a shared label sent once; derived columns with one property per source |
| `live_v1_static_cursor_classification_skips_rowstat`, `live_v2_…` | STATIC cursor open and fetch; classification maps to the data columns |
| `live_unrequested_connection_receives_no_classification` | No `0xA3` when not requested |
| `live_session_recovery_keeps_classification_working` | Real `KILL`; reconnect renegotiates |

Result on 2026-11-04 against SQL Server 2022 16.0.1210.3 (Developer Edition,
Windows): 8 of 8 passed, and no `dc_live_` tables were left in tempdb. The PR
validation pipeline runs the same tests against SQL Server 2025 on Linux,
Linux ARM, Windows and macOS.

### Fuzzing

`fuzz/fuzz_targets/fuzz_data_classification.rs` feeds arbitrary bytes to the
parser at both versions. The scheduled fuzz job runs it for 300 seconds.

A 300-second run on 2026-11-04 with the CI options (`-timeout=10 -fork=1
-ignore_crashes=1`, nightly 1.101) executed 3,678,521 inputs and reached 758
coverage points, with no crashes, timeouts or out-of-memory errors.

## Limitations and assumptions

- Only versions 1 and 2 exist; any other acknowledgement fails the login.
- The token is validated strictly. A malformed or unexpected token retires the
  connection where msodbcsql would pass it through.
- The per-column count is not required to match COLMETADATA, because SQL
  Server does not match it for server cursors.
- Tokens above 16 MiB are rejected. SQL Server's single-classification worst
  case is about a quarter of that; a derived column with many classified
  sources adds 8 bytes per source and has no documented upper bound.
- Unpaired surrogates are replaced, so the strings may differ from the bytes
  the server sent.
- Unknown token types stay fatal.

## Follow-up for the ODBC layer

Feature 42845 owns the ODBC surface. To match msodbcsql it needs to:

- Request version 1 on every connection by default, as msodbcsql does
  (`sqlcfunc.cpp:3487`), and map `SQL_COPT_SS_DATACLASSIFICATION_VERSION`
  (1 or 2; anything else is `HY024`, `sqlcmisc.cpp:2871`).
- Serve `SQL_CA_SS_DATA_CLASSIFICATION` from the IRD by re-serializing
  `DataClassificationMetadata` in the negotiated layout, keeping `0xFFFF`
  references. Strings with replaced surrogates will differ from msodbcsql's raw
  bytes.
- Decide what `SQL_CA_SS_DATA_CLASSIFICATION_VERSION` reports. msodbcsql
  reports the acknowledged version; this client reports the version in effect.
- Decide the behavior after `SQLMoreResults` moves to a row-count result, where
  this client returns no classification.
- Account for the unclassified ROWSTAT column on server cursors.
- Add parity tests against msodbcsql with classified tables.

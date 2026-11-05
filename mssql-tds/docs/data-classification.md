# Data Classification

`mssql-tds` can request SQL Server's Data Classification metadata and expose it
for each result set: the sensitivity labels, information types, and (with
version 2) ranks that `ADD SENSITIVITY CLASSIFICATION` assigns to columns.

Work items: feature [AB#42847](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/42847),
tasks [AB#49357](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/49357) and
[AB#49358](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/49358).

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
  positioned, before any row is read. Its lifetime and clearing rules are
  described under [Result sets](#result-sets).
- `columns()` covers the result set's leading columns. It is empty when the
  server sent no per-column data. Look entries up with `columns().get(i)` and
  treat a missing entry as unclassified.
- A dictionary reference of `0xFFFF` is `Unspecified` and resolves to `None`.
- Ranks are `None` unless version 2 is in effect.

## Protocol flow

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

The token carries no version of its own. Under [MS-TDS] 2.2.7.5,
`SensitivityRank` is present only when both versions are 2. A higher
acknowledgement is accepted, as in msodbcsql, SqlClient and JDBC, without
changing the layout.

Session recovery requests the feature again from the client context and uses
the new acknowledgement, honoring a downgrade or disabled feature rather than
reusing the dead session's version.

### Token parsing

The token has no length prefix, so it is parsed field by field
(`token/parsers/data_classification_parser.rs`):

- Both dictionaries, then the result-set rank (version 2), then per-column
  property lists, each property a label index, an information-type index and
  (version 2) a rank.
- Indexes must be in range or `0xFFFF`; ranks must be -1, 0, 10, 20, 30 or 40.
- Every count is checked against the remaining byte budget before anything is
  allocated, and initial capacity is capped at 1,024 entries per list.
- The token may not exceed 16 MiB. When each column references one classified
  source, the worst case SQL Server produces is 4,251,658 bytes: 4,096
  columns, each with its own label and information type, all four strings at
  the 128-character `sysname` maximum. Derived columns add 8 bytes per extra
  source, plus up to 514 bytes for each label or information type no other column
  uses; their source count has no documented upper bound, so the 16 MiB limit
  can reject a sufficiently large derived-column classification.
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
not the trailing ROWSTAT column it adds. Those trailing columns are
unclassified. A list longer than the result set cannot be mapped, so it is
dropped with a warning. In both cases the token was read whole, so the
connection is unaffected.

## Connection safety

| Situation | Result | Connection |
|---|---|---|
| Out-of-range index, unknown rank, budget exceeded | Protocol error | Retired; session recovery disarmed |
| `0xA3` on a connection that did not negotiate it | Protocol error | Retired; session recovery disarmed |
| Transport error or truncation inside the token | The reader error | Retired; recovery kept armed for a lost transport |
| Request timeout or `CancelHandle` during the read | Timeout or cancellation error | Settled through ATTENTION and usable if settlement succeeds; otherwise retired with the same error, so check `is_connection_dead()` |
| Caller drops the future during the read | None | Not settled; must not be reused |
| Unknown token type or known type without a parser | Protocol error | Retired at the read; session recovery disarmed |

A rejected or unreadable token leaves bytes that could pass for other tokens,
including a final DONE. Retirement happens at the failing read in both
`BatchErrorMode::Abort` and `Continue`, including prepared-batch and ATTENTION
drains, so a later `close_query()` cannot succeed over a desynchronized stream.

Dropping a read future, for example with `tokio::select!` or an outer
`tokio::time::timeout`, skips the ATTENTION settlement. As with a row, column
metadata or PLP value split across packets, the reader can be left inside the
token, so drop the client rather than reuse it. With the request timeout or a
`CancelHandle`, reuse is safe only when the interrupted token and ATTENTION
acknowledgement both complete in time.

Unknown tokens are not skipped: the framing class in the type byte is not
sufficient to determine their length ([MS-TDS] 2.2.4.2.1). `0xA3` is a known
token parsed without a length prefix. msodbcsql (`Parse.cpp:2002`), SqlClient
(`TdsParser.cs:2447`) and JDBC (`tdsparser.java:141`) likewise break the
connection on an unknown token.

## Differences from the reference drivers

| Aspect | msodbcsql | SqlClient / JDBC | mssql-tds |
|---|---|---|---|
| Requested by default | Version 1 on every login | Version 2 on every login | Opt-in |
| `IsEnabled = 0` | Ignored | Feature off | Feature off |
| Layout | Acknowledged version | Acknowledged, bounded by 2 | `min(requested, acknowledged)` |
| Exposed as | Raw token bytes | Typed objects | Typed model |
| Invalid index or rank | Passed through raw | Error | Error; connection retired |
| Token not negotiated | Parsed as version 1 | Error | Error; connection retired |
| Column count | Not checked | Not checked | Fewer accepted, more dropped |
| Unpaired surrogate | Raw bytes | U+FFFD (SqlClient) | U+FFFD |
| Token size | Unbounded | Unbounded | 16 MiB |
| After a no-row result | Previous buffer may remain | — | `None` |

mssql-tds is the only one of these clients that does not request the feature
on every login (msodbcsql `sqlcfunc.cpp:3487`, SqlClient
`SqlConnectionInternal.cs:3423`, JDBC `SQLServerConnection.java:7809`). It
stays opt-in so existing callers keep their LOGIN7 and expected token stream
unchanged. Classification validation applies whenever a token arrives,
including an unrequested one.

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

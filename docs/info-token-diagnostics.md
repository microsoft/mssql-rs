# Capturing all server INFO tokens as retrievable diagnostics

Work item: [mssql-rs#46285](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46285)
— *"mssql-tds | All INFO tokens from a token stream are captured and retrievable"*
(child task [#46403](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46403)).

## Problem

SQL Server emits **INFO tokens** (`0xAB`) for informational and low-severity
messages: `PRINT` output, `RAISERROR`/`THROW` with severity ≤ 10, and context
notices such as *"Changed database context to 'master'."* (msg 5701) or
*"Changed language setting…"* (msg 5703). These arrive interleaved with the
rest of the token stream for a batch, RPC, or login.

`mssql-tds` already *parsed* INFO tokens (`token/parsers/info_parser.rs` →
`InfoToken`), but every token-consuming loop in `TdsClient` logged and then
**discarded** them. Nothing accumulated them, and there was no public API to
retrieve them.

This blocks ODBC parity. ODBC surfaces server informational/warning messages as
diagnostic records via `SQLGetDiagRec` / `SQLGetDiagField`, and returns
`SQL_SUCCESS_WITH_INFO` when a successful call produced them. Because
`mssql-tds` dropped INFO tokens, `mssql-odbc` had no way to expose them.

A second, related gap: on **login failure** the login parser kept only the
*last* ERROR token (`tds_error: Option<TdsError>`) and, once INFO capture was
added, discarded INFO messages entirely because a failed `create_client`
returns an `Err` with no `TdsClient` to drain. `msodbcsql` accumulates *all*
login errors and informational messages and makes them retrievable.

## Goals

1. Capture **every** INFO token (any severity — we do not filter on `< 10`)
   from all token streams: batch execution, RPC/stored-procedure execution,
   result-set draining, and login.
2. Expose them through a public `mssql-tds` API.
3. On **failure**, carry the complete server diagnostic set (all ERROR tokens
   plus any INFO messages) out with the error so callers can post them even
   when no `TdsClient` exists (login).
4. Wire `mssql-odbc` to post these as diagnostic records and return
   `SQL_SUCCESS_WITH_INFO` on successful calls that observed server messages.

## Design

Errors were already modeled as a first-class, retrievable collection
(`Error::SqlServerError { errors: Vec<SqlErrorInfo> }`, drained via
`drain_stream`). We mirror that for INFO and unify both categories under one
"server diagnostics" payload so a failed operation carries the full set.

- A public **`SqlInfoMessage`** value type, parallel to `SqlErrorInfo`, carries
  a single INFO message with its server fields (number, state, class/severity,
  message, server/procedure name, line number).
- A public **`SqlServerDiagnostics`** wrapper groups `errors` and
  `info_messages`. This is what a failed SQL-Server-originated operation
  carries, and it keeps the `Error::SqlServerError` variant from growing a new
  field each time a diagnostic category is added.
- `TdsClient` retains INFO messages seen on the **success** path in a buffer,
  exposed via `info_messages()` / `take_info_messages()`. This mirrors how
  `return_values` (output parameters) are retained and read after a command.
- The **login** path accumulates all ERROR and INFO tokens across the
  (possibly multi-step: SSPI / FedAuth) handshake and, on failure, returns them
  as `Error::SqlServerError { diagnostics }`.

Client-originated errors (bad configuration, transport, TLS, timeout, protocol
violations) are **not** SQL-Server diagnostics; they remain their own `Error`
variants and continue to map to a single consumer diagnostic record. Only
tokens the server actually sent go into `SqlServerDiagnostics`.

## Public API changes (`mssql-tds`)

All in `mssql_tds::error` unless noted.

### Added

```rust
/// One informational/warning message returned by SQL Server.
pub struct SqlInfoMessage {
    pub message: String,
    pub state: u8,
    pub class: i32,          // TDS severity/class
    pub number: u32,
    pub server_name: Option<String>,
    pub proc_name: Option<String>,
    pub line_number: Option<i32>,
}

/// Server-reported diagnostics for one operation.
pub struct SqlServerDiagnostics {
    pub errors: Vec<SqlErrorInfo>,
    pub info_messages: Vec<SqlInfoMessage>,
}

impl SqlServerDiagnostics {
    pub fn new(errors: Vec<SqlErrorInfo>, info_messages: Vec<SqlInfoMessage>) -> Self;
    pub fn has_errors(&self) -> bool;
    pub fn is_empty(&self) -> bool;
}

impl Error {
    /// Build a SqlServerError from a full diagnostics set.
    pub fn from_sql_diagnostics(diagnostics: SqlServerDiagnostics) -> Self;
}
```

On `TdsClient` (`mssql_tds::connection::tds_client`):

```rust
/// Informational messages captured from the current (or most recent) command.
pub fn info_messages(&self) -> &[SqlInfoMessage];

/// Drain and return the captured informational messages.
pub fn take_info_messages(&mut self) -> Vec<SqlInfoMessage>;
```

### Changed (breaking)

```rust
// Before
Error::SqlServerError { errors: Vec<SqlErrorInfo> }

// After
Error::SqlServerError { diagnostics: SqlServerDiagnostics }
```

The ergonomic constructors are **retained** with unchanged signatures, so
producers do not change:

```rust
Error::from_sql_error(SqlErrorInfo) -> Error         // unchanged
Error::from_sql_errors(Vec<SqlErrorInfo>) -> Error   // unchanged
```

Consumers that pattern-match the variant must destructure `{ diagnostics }` and
read `diagnostics.errors` (and, new, `diagnostics.info_messages`).

### Removed (internal)

- `mssql_tds::message::messages::TdsError` — a thin single-error wrapper used
  only by the login path. Superseded by collecting `SqlErrorInfo` directly.

## Behavior changes

1. **INFO tokens are retained, not discarded.** Every `Tokens::Info` branch in
   `TdsClient` (batch done-consume, `drain_stream`, `move_to_column_metadata`,
   row read, transaction-response consume, and the bulk-load `consume_done_token`
   loop) now records the message.

2. **`Error::SqlServerError` carries errors *and* info.** Failed
   SQL-Server-originated operations expose the full diagnostic set. In-repo
   consumers were updated; the ergonomic constructors keep producers unchanged.

3. **Login surfaces all diagnostics.** Login now collects **every** ERROR token
   (previously only the last was kept) plus any INFO messages, and a failed
   login returns them as `Error::SqlServerError { diagnostics }`. This closes
   the gap where a failed `create_client` dropped login INFO messages.

4. **Per-command reset.** Each `execute*` entry point calls an internal
   `begin_command()` that clears the info buffer, so `info_messages()` reflects
   only the current command. It runs *before* reconnect, so a transparent
   session recovery's login messages remain visible as part of the command that
   triggered them.

5. **`close_query` intentionally preserves the info buffer.** It clears
   `return_values` but not `info_messages`, because draining the trailing token
   stream can surface INFO (e.g. a `PRINT` after the last result set) that the
   caller drains via `take_info_messages()` afterward. This is documented at the
   call site to prevent a future "cleanup" from regressing it.

6. **Bulk copy accumulates INFO across the whole operation.**
   `execute_bulk_load_streaming_zerocopy` calls `begin_command()` like the other
   token-consuming entry points, and its `consume_done_token` loop captures INFO.
   Because `BulkCopy::write_to_server_zerocopy` is a *composite* (metadata
   retrieval + optional internal transaction + one or more bulk-load batches),
   the internal `commit_transaction` would otherwise reset the buffer. So the
   composite drains each batch's INFO before the commit and restores the full
   set once the loop finishes, making all bulk-load INFO retrievable via
   `info_messages()` after the operation returns.

## Consumer changes (`mssql-odbc`)

- `post_tds_error` (`api/sqlstate.rs`) matches `SqlServerError { diagnostics }`
  and posts one diagnostic record per error (SQLSTATE from the server-error map,
  falling back to the caller's default), **then** one per INFO message. Errors
  are posted first so record 1 remains the primary failure.
- New `post_tds_info_messages` helper posts INFO messages as diagnostic
  records: SQLSTATE from the server map or `01000` fallback, native error = the
  server message number, message text from the wire. Returns whether any record
  was posted.
- These entry points drain `client.take_info_messages()` and post them, and
  return `SQL_SUCCESS_WITH_INFO` when informational records were posted on an
  otherwise successful call:
  `SQLDriverConnectW`, `SQLExecDirectW`, `SQLFetch`, `SQLMoreResults`,
  `SQLCloseCursor` / `SQLFreeStmt(SQL_CLOSE)`.
- **End-of-rowset INFO is deferred, not posted under `SQL_NO_DATA`.** `SQLFetch`
  returning `SQL_NO_DATA` (and `SQLMoreResults` returning `SQL_NO_DATA` for an
  exhausted batch) cannot be upgraded to `SQL_SUCCESS_WITH_INFO` per the ODBC
  cursor contract, so those return codes carry no "read diagnostics" hint that
  many applications rely on. Rather than post end-of-rowset INFO under
  `SQL_NO_DATA` (where it may never be read) and consume it so nothing else can
  re-surface it, `SQLFetch`'s end-of-rowset path leaves the captured INFO on the
  client buffer. It is then surfaced *with* a hint by the next boundary call:
  `SQLMoreResults` advancing to a further result set (`SQL_SUCCESS_WITH_INFO`),
  or `SQLCloseCursor` / `SQLFreeStmt(SQL_CLOSE)` (`SQL_SUCCESS_WITH_INFO` via
  `DrainOutcome::InfoPosted`). If the batch is exhausted and the application
  calls `SQLMoreResults` rather than closing the cursor, that call still posts
  the records (under `SQL_NO_DATA`) so they are never dropped — matching
  msodbcsql's between-result surfacing.
- `SQLDriverConnect` failure now fans out the full login diagnostics (all
  errors + info) via `post_tds_error`, matching `msodbcsql`.

## Cross-driver alignment

- **SqlClient**: `SqlConnection.InfoMessage` event + `SqlError`/`SqlErrorCollection`;
  errors and warnings are cached separately then surfaced. Our
  `SqlServerDiagnostics { errors, info_messages }` mirrors that error/warning
  split.
- **JDBC**: INFO tokens become `SQLServerInfoMessage` and are chained as
  `SQLWarning`s; each retains the server number/severity/state/message. Our
  `SqlInfoMessage` carries the same fields.
- **ODBC (msodbcsql)**: accumulates all messages on the handle and exposes them
  via `SQLGetDiagRec`/`SQLGetDiagField`, returning `SQL_SUCCESS_WITH_INFO`.
  `mssql-odbc` now matches this.

## Testing

- **Unit (`mssql-tds`)**: `SqlServerDiagnostics` helpers, `from_sql_diagnostics`
  round-trip and `Display`, transient-error detection through the wrapper,
  `consume_done_token` INFO capture, and `begin_command` clearing stale info.
- **Unit (`mssql-odbc`)**: `post_tds_info_messages` record shape, and
  `post_tds_error` fanning out errors-then-info.
- **Integration (`mssql-tds/tests/test_info_messages.rs`)**: INFO before a
  result set, INFO-only batches, INFO drained after result rows via
  `close_query`, and per-command reset (a prior command's info does not bleed
  into the next).
- **Integration (`mssql-tds/tests/test_bulk_copy.rs`)**: bulk copy surfaces INFO
  emitted during the bulk load (an `AFTER INSERT` trigger `PRINT`, fired via
  `fire_triggers`) through `info_messages()` after the operation completes.
- **ODBC e2e (`mssql-odbc/tests/e2e/tests/exec_direct_test.cpp`)**: `PRINT` +
  low-severity `RAISERROR` yields `SQL_SUCCESS_WITH_INFO` with two retrievable
  diagnostic records.
- **ODBC e2e (`mssql-odbc/tests/e2e/tests/more_results_test.cpp`)**: an INFO
  message between two result sets surfaces on the `SQLMoreResults` advance with a
  `SQL_SUCCESS_WITH_INFO` hint and a retrievable diagnostic record.

## Open items / future work

- **Statement vs. login info carriage is not unified.** Statement-execution
  errors keep info on the client (drained separately by the consumer), while
  login errors carry info inside the `SqlServerError` diagnostics. Both are
  correct; unifying would mean threading info into `drain_stream`'s return so
  statement errors also carry a full `SqlServerDiagnostics`.
- **Live end-to-end validation pending.** Unit and integration tests are in
  place; a run against a live SQL Server (and building the ODBC C++ e2e suite)
  is still needed to confirm on-wire behavior.

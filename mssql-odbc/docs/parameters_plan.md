# Parameterized execution - `SQLBindParameter` / `SQLExecute` / `SQLExecDirect`

Status, behavior, and known gaps for parameterized prepared-statement execution
in the ODBC Driver 18 (Rust). Updated 2026-08-11.

---

## Implemented

`SQLPrepare(W)`, `SQLBindParameter`, `SQLExecute`, and parameterized
`SQLExecDirect(W)`, including managed prepared-handle invalidation across
transparent reconnects.

- **Managed prepared statement** - `StmtState` stores a
  `mssql_tds::PreparedStatement` containing the rewritten SQL and, once
  materialized, an opaque client-issued `StatementId`. The live server handle
  lives in the `TdsClient`, keyed by that id. `SQLExecute` moves the statement
  out while executing and writes it back afterward. The first execute runs
  `sp_prepexec`; subsequent executes reuse the live handle via `sp_execute`.
- **`SQLExecDirect`** - parameterized text runs `sp_executesql` (direct, no
  cached handle); unparameterized text runs as a plain language batch.
- **Prepared-handle capture** - for a result-returning `sp_prepexec`, the
  `@handle` arrives after the result set. It is written straight into the
  client's `StatementId -> handle` map by `push_return_value` when the token
  lands, in the same token funnel that pins Always Encrypted metadata — no
  caller-side capture step, so no drain path can drop it.
- **`sp_unprepare` (handle release)** - a handle superseded by a re-prepare or
  rebind is deferred in `pending_unprepare` and released at the next
  `SQLExecute` by piggybacking onto `sp_prepexec`: the superseded handle is sent
  as that call's in/out `@handle`, so the server drops the old plan and prepares
  the new one in one round trip. `SQLExecDirect` supersede and
  `SQLFreeHandle(STMT)` use standalone `sp_unprepare` because they have no
  `sp_prepexec` on which to piggyback.
- **`sp_prepexec` failure ownership** - the pending handle remains in ODBC
  through reconnect, validation, parameter construction, and Always Encrypted
  setup. `mssql-tds` consumes it only when the prepexec RPC is ready to
  serialize, so definite pre-send failures restore it for a later cleanup.
  Serialization, send, and response failures remain ambiguous: the server may
  already have consumed the handle, so retrying cleanup could target an invalid
  or reused id. This matches msodbcsql after its `ExecRPCImmediate` boundary.
- **Stale-handle invalidation after transparent reconnect** - the client's
  `StatementId -> handle` map is cleared on every reconnect, alongside the
  Always Encrypted describe cache. `TdsClient::execute_prepared` performs
  recovery first, then resolves the statement's id against the (possibly
  cleared) map: a hit reuses the handle, a miss re-prepares the SQL. "Stale"
  therefore collapses to "absent from the map", and a superseded pending drop
  that the reconnect discarded is likewise absent and skipped. `unprepare`
  applies the same lookup. If ODBC cannot claim the connection before execution,
  it restores both the moved prepared statement and pending orphan to
  `StmtState`.
- **Lifecycle** - `SQL_RESET_PARAMS` clears bindings; `SQLCloseCursor` and
  `SQLFreeStmt(SQL_CLOSE)` preserve the handle; re-`SQLPrepare` and rebind
  orphan it for release.
- **Placeholder rewrite** - `SQLPrepare` rewrites `?` to `@P1...@Pn` once,
  skipping string literals, quoted identifiers, and comments. It stores the
  rewritten SQL and marker count, so repeated `SQLExecute` calls do not re-scan
  the text.
- **Types** - `SQL_C_CHAR` maps to varchar and `SQL_C_WCHAR` to nvarchar; other
   Invalid C types return  HY003 ; unsupported C/SQL conversions return  07006. Indicators support `SQL_NULL_DATA`, `SQL_NTS`, and explicit byte length.

## `mssql-tds` prepared API

- `PreparedStatement` stores SQL plus an optional opaque `StatementId`; the
  server handle lives in the client's `StatementId -> handle` map.
- `execute_prepared` owns recovery, timeout deduction, live-handle reuse,
  stale-handle invalidation, reprepare, and live-orphan piggyback planning.
  `unprepare` sends `sp_unprepare` only when the client still holds a handle for
  the statement in the live session.
- `sp_prepexec` captures its `@handle` RETURNVALUE separately from user output
  parameters. Always Encrypted describe metadata is retained until capture and
  pinned under the returned handle, allowing the next managed `sp_execute` to
  encrypt parameters without another describe. When `sp_prepexec` replaces a
  prior handle, successful replacement capture also removes the superseded
  handle's metadata; failed or incomplete capture leaves it untouched.
- Focused coverage includes handle-map reuse/re-prepare planning, unprepare
  behavior, in-funnel handle capture, wire-byte assertions for piggybacked
  drops, claim-failure restoration, and Always Encrypted metadata pinning.

### Tracked follow-ups

- **Cross-client ownership:** closed structurally by the opaque `StatementId`.
  Ids are unique to the issuing client, so a `PreparedStatement` carried to a
  different client resolves to "not materialized here" and is re-prepared rather
  than aliasing an unrelated server handle. (Formerly tracked as ADO 47098.)
- **Enabled reconnect e2e:** session-recovery baseline state is being fixed in
  [ADO 46631](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46631).
  Enable `StaleHandleAfterReconnectIsInvalidatedAndReprepared` afterward under
  [ADO 47099](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47099).

## Remaining work

- **Stream marker rewriting without an intermediate SQL string.** `SQLPrepare`
  already scans and rewrites once. A future allocation optimization could store
  the original SQL plus `Vec<usize>` marker offsets, then stream SQL chunks and
  `@P{n}` names directly to the TDS writer. Execute-time binding state would
  also allow `OUTPUT` and `?=` handling. This is no longer a repeated-parsing
  correctness issue.
- **Phase-2 type matrix:** widen beyond `SQL_C_CHAR` / `SQL_C_WCHAR` as
  `SQLGetData` grows (numeric, binary, and date C types).
- **Drive the RPC parameter's TDS type from `ParameterType`, not the C type.**
  `params::convert` currently ignores `sql_type` and emits `(n)varchar(max)`,
  relying on SQL Server implicit conversion. Map the ODBC SQL type to the wire
  TDS type; use the C type only to read and convert the application buffer.
  This avoids incorrect plan declarations and conversion differences for
  binary, `uniqueidentifier`, money, decimal, and date/time values.
- **Deferred features:** output parameters (`SQL_PARAM_OUTPUT`, `SQL_PARAM_INPUT_OUTPUT`), data-at-exec
  (`SQLParamData` / `SQLPutData`), parameter arrays
  (`SQL_ATTR_PARAMSET_SIZE`), and TVPs. Data-at-exec requires an
  `sp_prepare` + `sp_execute` branch because `sp_prepexec` cannot carry streamed
  values.
- **Canonical procedure calls / `sp_prepexecrpc`:** support ODBC canonical
  calls (`{call proc(?)}`) with the appropriate parameter-count and single-row
  parameter-set guards. Ad-hoc T-SQL currently uses `sp_prepexec`.
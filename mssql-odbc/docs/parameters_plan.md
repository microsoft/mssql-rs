# Parameterized execution - `SQLBindParameter` / `SQLExecute` / `SQLExecDirect`

Status, behavior, and known gaps for parameterized prepared-statement execution
in the ODBC Driver 18 (Rust). Updated 2026-08-07.

---

## Implemented

`SQLPrepare(W)`, `SQLBindParameter`, `SQLExecute`, and parameterized
`SQLExecDirect(W)`, including managed prepared-handle invalidation across
transparent reconnects.

- **Managed prepared statement** - `StmtState` stores a
  `mssql_tds::PreparedStatement` containing the rewritten SQL and its optional
  `PreparedHandle`. `SQLExecute` moves it out while executing and writes it back
  afterward. The first execute runs `sp_prepexec`; subsequent executes reuse the
  live handle via `sp_execute`.
- **`SQLExecDirect`** - parameterized text runs `sp_executesql` (direct, no
  cached handle); unparameterized text runs as a plain language batch.
- **Prepared-handle capture** - for a result-returning `sp_prepexec`, the
  `@handle` arrives after the result set and is captured at drain time
  (`SQLCloseCursor` / DDL finish) via `capture_prepared_handle_into()`. The
  explicit post-drain capture remains for now: replacing it with shared
  completion state adds clone aliasing, stale delivery, cancellation, and
  lock-order hazards.
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
- **Stale-handle invalidation after transparent reconnect** - every cached
  handle carries the connection's recovery epoch at capture.
  `TdsClient::execute_prepared` performs recovery first, then compares handles
  with the post-recovery epoch. A handle from an old physical session is dropped
  and its SQL is prepared again; a stale pending drop is skipped because the
  old session already discarded it. `unprepare` applies the same liveness rule.
  If ODBC cannot claim the connection before execution, it restores both the
  moved prepared statement and pending orphan to `StmtState`.
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

- `PreparedHandle` stores the server id and recovery epoch;
  `PreparedStatement` stores SQL plus an optional handle.
- `execute_prepared` owns recovery, timeout deduction, live-handle reuse,
  stale-handle invalidation, reprepare, and live-orphan piggyback planning.
  `unprepare` sends `sp_unprepare` only for a handle from the current session.
- The low-level RPC methods are named `execute_sp_*_raw`. They remain public for
  protocol integration tests and the prepared-query benchmark, but their
  contract makes callers responsible for bare-handle/session validation.
  `execute_sp_prepare_raw` is the recovery exception: it can reconnect safely
  because it accepts no existing handle.
- `sp_prepexec` captures its `@handle` RETURNVALUE separately from user output
  parameters. Always Encrypted describe metadata is retained until capture and
  pinned under the returned handle, allowing the next managed `sp_execute` to
  encrypt parameters without another describe. When `sp_prepexec` replaces a
  prior handle, successful replacement capture also removes the superseded
  handle's metadata; failed or incomplete capture leaves it untouched.
- Focused coverage includes recovery-epoch planning, unprepare behavior, handle
  capture, wire-byte assertions for piggybacked drops, claim-failure restoration,
  and Always Encrypted metadata pinning.

### Tracked follow-ups

- **Cross-client ownership:** an epoch distinguishes reconnect generations of
  one client but does not identify different clients. Direct `mssql-tds`
  consumers can move a `PreparedStatement` between clients with equal epochs.
  Normal ODBC ownership cannot do this because an HSTMT has one parent HDBC.
  Track the direct-client hardening in
  [ADO 47098](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47098).
- **Enabled reconnect e2e:** session-recovery baseline state is being fixed in
  [ADO 46631](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46631).
  Enable `StaleHandleAfterReconnectIsInvalidatedAndReprepared` afterward under
  [ADO 47099](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47099).
- **Raw API visibility:** changing the four raw methods to `pub(crate)` is
  mechanically trivial but currently breaks 34 external-crate call sites: 29
  in three `mssql-tds` integration-test files and 5 in the separate
  `mssql-tds-bench` crate. A hard privacy boundary requires relocating or
  reworking those protocol tests and deciding whether the raw prepared
  benchmark is removed, changed to the managed API, or supported through a
  deliberately unstable low-level surface. Treat this as separate breaking API
  work, not part of the ODBC migration.

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
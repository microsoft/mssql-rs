# Parameterized execution — `SQLBindParameter` / `SQLExecute` / `SQLExecDirect`

Status, behavior, and known gaps for parameterized prepared-statement execution
in the ODBC Driver 18 (Rust).

---

## Implemented (Phase 1)

`SQLPrepare(W)`, `SQLBindParameter`, `SQLExecute`, and parameterized
`SQLExecDirect(W)`. Validated by 177 unit tests + live e2e against SQL Server
2025 (`tests/e2e/tests/execute_test.cpp`).

- **`SQLExecute`** — first execute runs `sp_prepexec` (prepare + execute in one
  round trip); subsequent executes reuse the cached `prepared_handle` via
  `sp_execute`.
- **`SQLExecDirect`** — parameterized text runs `sp_executesql` (direct, no
  cached handle); unparameterized text runs as a plain language batch.
- **Prepared-handle capture** — for a result-returning `sp_prepexec` the
  `@handle` arrives *after* the result set, so it is captured at drain time
  (`SQLCloseCursor` / DDL finish) via `take_prepared_statement_handle()`.
- **`sp_unprepare` (handle release)** — a handle superseded by a re-prepare or
  rebind is deferred (`pending_unprepare`) and released at the next `SQLExecute`
  by **piggybacking onto `sp_prepexec`**: the superseded handle is sent as that
  call's `@handle` **input**, so the server drops the old plan and prepares the
  new one in one round trip (no separate `sp_unprepare`). `SQLExecDirect`
  supersede and `SQLFreeHandle(STMT)` still issue a standalone `sp_unprepare`
  (`flush_pending_unprepare` / best-effort), since neither runs an `sp_prepexec`
  to ride along.
- **Stale-handle invalidation after transparent reconnect** — every cached
  handle (`prepared_handle` and `pending_unprepare`) carries the connection's
  `session_epoch` at capture. `SQLExecute` performs the single recovery up front
  (`check_and_reconnect`, mirroring msodbcsql `GetBatchCtxOrRecover`), then
  predictively re-evaluates handle liveness against the post-recovery epoch
  (`plan_execution`, mirroring `FIsReprepareRequired`): a handle from a
  superseded session is dropped — the cached handle is re-prepared fresh and a
  stale piggyback drop is skipped (the server already discarded it). No RPC
  recovers again after the decision, so the send is bound to the same session
  and can never fire a handle from a dead one (no reliance on server error 8179).
  The standalone `sp_unprepare` paths (`flush_pending_unprepare`,
  `SQLFreeHandle(STMT)`) skip a stale handle the same way.
- **Lifecycle** — `SQL_RESET_PARAMS` clears bindings; `SQLCloseCursor` /
  `SQLFreeStmt(SQL_CLOSE)` preserve the handle; re-`SQLPrepare` and rebind
  orphan it for release.
- **Placeholder rewrite** — `?` → `@P1…`, skipping string literals, quoted
  identifiers, and comments.
- **Types** — `SQL_C_CHAR` → varchar, `SQL_C_WCHAR` → nvarchar; all other C
  types → `HYC00`. Indicator honored: `SQL_NULL_DATA` → typed NULL, `SQL_NTS`,
  and explicit byte length.

---

## `mssql-tds` changes

Crate-level delta backing the behavior above (commits `d344abe6`, `ad9c7b9a`,
plus working-tree edits) — see **Implemented (Phase 1)** for the ODBC-level
semantics. Files: `src/connection/tds_client.rs`, `tests/test_rpc_results.rs`,
`tests/test_always_encrypted.rs`.

- `execute_sp_prepexec` gained a `drop_handle: Option<i32>` argument — the handle
  sent as the by-reference `@handle` input for the piggybacked unprepare (per the
  `sp_unprepare` bullet above).
- `connection_recovery_count()` doubles as the connection's session epoch, and
  `check_and_reconnect()` is now public so the caller can own the single
  recovery point. `execute_sp_execute` / `execute_sp_prepexec` no longer recover
  internally: the ODBC layer reconnects first, decides handle liveness against
  `connection_recovery_count()`, then sends — so no reconnect can slip between
  the decision and the send and turn the chosen handle stale (Bug 2 fix).
- Handle capture via new `expecting_prepare_handle` / `prepared_statement_handle`
  fields and `take_prepared_statement_handle()`. The `@handle` RETURNVALUE is
  routed to that field and **not** surfaced through `get_return_values()` /
  `retrieve_output_params()`, matching msodbcsql's `OnReturnValue` (handle goes to
  `hPrepCurrent`, not the user output-param path).
- Tests: byte-capturing mock transport (`create_capturing_client`) asserting the
  `@handle` INTN input (`Some(h)` vs NULL), `push_return_value` unit tests, and a
  live `sp_prepexec` piggyback test; existing callers updated for the new arg.

---

## Remaining work (TODO)

- **Cache parameter-marker offsets at prepare time.**
  Today `rewrite_param_markers` re-scans the SQL text and builds a new rewritten
  string on every `SQLExecute`. Split this into two phases:

  **Phase 1 — parse once at `SQLPrepare` time:**
  Scan the SQL for `?` markers (skipping string literals, quoted identifiers, and
  comments) and record each marker's byte offset in an offset table on
  `StmtState`. This is the only thing that *can* be done at prepare time: the
  application hasn't bound parameters yet, so the driver doesn't know whether each
  `?` is `INPUT`, `OUTPUT`, or a `?=` return-status marker.

  **Phase 2 — rewrite at execute time:**
  Walk the offset table and stream SQL chunks interleaved with `@P{n}` names
  directly onto the wire. At this point bound-param state is available, so the
  driver can:
  - Append `OUTPUT` to the `@P{n}` slot when the parameter is bound as output.
  - Handle `?=` syntax by adjusting the marker position by one character.
  No intermediate buffer is materialized — the original SQL serves as the source.

  **Why this matters:**
  - The expensive parse (comments, quotes) runs once per prepare, not once per
    execute — good for frequently re-executed statements.
  - The rewrite can stream directly into the TDS buffer instead of materializing
    an intermediate string.
  - The offset count is the natural primitive for `SQLNumParams`.
  - Defers semantics (output params, `?=`) to execute time where binding info is
    available, which output-param support will require.

  **Proposed change:** store `Vec<usize>` of marker byte offsets on `StmtState`
  (computed during `SQLPrepare`), replace `rewrite_param_markers` with a
  streaming emitter that consumes the offset table at execute time.
- **Phase-2 type matrix:** widen beyond `SQL_C_CHAR` / `SQL_C_WCHAR` once
  `SQLGetData` grows (numeric, binary, date C types).
- **Drive the RPC parameter's TDS type from `ParameterType`, not the C type.**
  Today `params::convert` ignores `sql_type` entirely: it branches only on the
  C type and always emits `(n)varchar(max)`, relying on SQL Server's implicit
  conversion to coerce the value into the target type. Instead, map the ODBC SQL
  type (`SQL_DESC_CONCISE_TYPE` on the IPD) to the wire TDS type; the C type
  should only govern how the application buffer is read, with a client-side
  C→SQL conversion before sending. The shortcut works for common
  `varchar → int/date/...` cases but diverges for binary / `uniqueidentifier` /
  `money` / exact `decimal` / locale-sensitive `datetime`, declares the wrong
  prepared-plan param types (`@P1 nvarchar(max)` vs `@P1 int`, hurting parameter
  sniffing), and can throw server-side conversion errors. Phase 2: build the
  `RpcParameter` from `ParameterType`, converting the C value to that SQL/TDS
  type. This pairs with the type-matrix work above and lets `is_valid_conversion`
  finally use its `_sql_type` argument.
- **Deferred features:** output params (`SQL_PARAM_OUTPUT`), data-at-exec
  (`SQLParamData` / `SQLPutData`), parameter arrays (`SQL_ATTR_PARAMSET_SIZE`),
  and TVPs. For DAE specifically, fall back to `sp_prepare` + `sp_execute` since
  `sp_prepexec` can't carry data-at-exec values — add that branch when DAE lands.
- **Canonical procedure calls / `sp_prepexecrpc`.** ODBC canonical calls
  (`{call proc(?)}`) and single-row RPC batches can be prepared via
  `sp_prepexecrpc`, gated on a single parameter row (`SQL_ATTR_PARAMSET_SIZE ==
  1`) and a server parameter-count limit. We only handle ad-hoc T-SQL via
  `sp_prepexec` today; add the canonical-RPC branch with the same param-count /
  array-size guards when procedure-call syntax is supported.

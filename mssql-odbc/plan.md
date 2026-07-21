# Implementation Plan: ODBC Driver 18 for SQL Server in Rust

---

## Overview

This project delivers a **production-ready ODBC Driver 18 for SQL Server written in Rust**, designed as a drop-in replacement for Microsoft's `msodbcsql18` driver. Applications using `Driver={ODBC Driver 18 for SQL Server}` work without code changes.

The driver wraps the `mssql-tds` library (TDS protocol implementation, workspace-local path dependency) with a full ODBC 3.x API layer — ~78 exported functions (including W wide-char variants) covering connectivity, query execution, data types, authentication, encryption, bulk operations, and more.

**Target platforms**: Windows, Linux, macOS (x64 and ARM64) - Same as the platforms currently supported by msodbcsql
**Binary output**: `msodbcsql18.dll` (Windows), `libmsodbcsql-18.so.1.1` (Linux), `libmsodbcsql.18.dylib` (macOS)

---

## Architecture

The driver follows a three-layer architecture with strict separation of concerns:

```
┌─────────────────────────────────────────────────────┐
│  FFI Layer  (api/exports.rs — extern "C" symbols)   │
│  Each entry point: wrapper → ffi_entry! panic       │
│  boundary → unsafe shim (raw-pointer validation)   │
│  → safe core (business logic).                     │
├─────────────────────────────────────────────────────┤
│  ODBC Layer  (api/, handles/, connection/, error/)  │
│  Handles, state machines, type conversions,         │
│  diagnostics, connection-string parsing, etc.       │
├─────────────────────────────────────────────────────┤
│  TDS Layer  (mssql-tds — local path dependency)     │
│  TDS protocol, authentication, token parsing,       │
│  TLS, Azure AD, SSPI/Kerberos                       │
└─────────────────────────────────────────────────────┘
```

**No layer skipping**: FFI calls ODBC layer, ODBC layer calls TDS layer. Type boundaries are enforced at each level.

### Crate Structure

A single crate (`mssql-odbc/`) producing the cdylib. Internal separation is via modules:

```
mssql-odbc/
├── src/
│   ├── lib.rs              # cdylib root — declares modules, hosts ffi_entry! macro and init_tracing()
│   ├── test_support.rs     # TestHandles helper for unit tests
│   ├── api/                # ODBC API layer — one file per SQLXxx (or family)
│   │   ├── mod.rs
│   │   ├── exports.rs        # All #[unsafe(no_mangle)] pub extern "C" symbols
│   │   ├── odbc_types.rs     # ODBC C type aliases (SqlHandle, SqlReturn, ...) and constants
│   │   ├── sqlstate.rs       # SQLSTATE constants
│   │   ├── util.rs           # Shared helpers (write_if_some, read_utf16, copy_with_nul, ...)
│   │   ├── alloc_handle.rs   # SQLAllocHandle
│   │   ├── free_handle.rs    # SQLFreeHandle
│   │   ├── set_env_attr.rs   # SQLSetEnvAttr
│   │   ├── driver_connect.rs # SQLDriverConnect[W]
│   │   ├── disconnect.rs     # SQLDisconnect
│   │   ├── exec_direct.rs    # SQLExecDirect[W]
│   │   ├── prepare.rs        # SQLPrepare[W] (deferred prepare)
│   │   ├── execute.rs        # SQLExecute (sp_prepexec / sp_execute)
│   │   ├── exec_common.rs    # Shared claim/execute/finish helpers for exec paths
│   │   ├── bind_param.rs     # SQLBindParameter / SQLFreeStmt(SQL_RESET_PARAMS)
│   │   ├── fetch.rs          # SQLFetch
│   │   ├── get_data.rs       # SQLGetData
│   │   ├── num_result_cols.rs# SQLNumResultCols
│   │   ├── describe_col.rs   # SQLDescribeCol[W]
│   │   ├── more_results.rs   # SQLMoreResults
│   │   ├── close_cursor.rs   # SQLCloseCursor / SQLFreeStmt(SQL_CLOSE)
│   │   └── get_diag.rs       # SQLGetDiagRec[W] / SQLGetDiagField[W]
│   ├── handles/            # Env, DBC, STMT handle structs and state
│   │   ├── mod.rs            # HandleType, handle_to_raw / handle_from_raw conversions
│   │   ├── env.rs            # EnvHandle, EnvState, OdbcVersion
│   │   ├── dbc.rs            # DbcHandle, DbcState, ConnectionState
│   │   └── stmt.rs           # StmtHandle, StmtState
│   ├── params/            # Bound parameter storage and C→TDS value conversion
│   │   ├── mod.rs            # BoundParam
│   │   ├── bound_param.rs    # BoundParam struct
│   │   └── convert.rs        # C-type → SqlType conversion, type/conversion validity
│   ├── connection/         # Connection-string parsing and connect orchestration
│   │   └── mod.rs
│   └── error/              # Diagnostic record posting (post_sql_error, post_tds_error)
│       ├── mod.rs            # free_errors, error posters, SQLSTATE mapping table
│       └── diag.rs           # DiagRec storage, SQLGetDiagRec / SQLGetDiagField backing
├── tests/
│   └── e2e/                # CMake-built Google Test C++ suite that loads the driver
│                           # via the ODBC Driver Manager (run_e2e.sh / .ps1)
├── build.rs                # Platform-specific linker config (soname, install_name, .def)
├── README.md
├── plan.md                 # This document
└── Cargo.toml
```

New SQLXxx implementations follow the layered shape from
[.github/instructions/mssql-odbc.instructions.md](../.github/instructions/mssql-odbc.instructions.md):
thin `extern "C"` wrapper in `exports.rs` → `ffi_entry!` panic boundary →
`unsafe fn sql_xxx_impl` shim (raw-pointer validation) → safe `fn sql_xxx_safe`
core (business logic). Each new SQLXxx generally lives in its own file under
`api/`. If the crate grows large enough to warrant splitting (e.g. separating
FFI from logic), that restructuring is a follow-up task.

---

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **TDS dependency** | `mssql-tds` via local path (`../mssql-tds`) | TDS 7.4 + 8.0 support, co-developed in same workspace |
| **Driver version** | Driver 18 only | TDS 7.4 + TDS 8.0 strict encryption, SQL Server 2016+ alignment |
| **TLS** | Delegated to mssql-tds (`native-tls`) | SChannel (Win), Security.framework (macOS), OpenSSL (Linux) — no separate TLS crates needed |
| **Authentication** | Delegated to mssql-tds | Full SSPI, Kerberos, and all 9 Azure AD modes built-in |
| **Localization** | English (en_US) only | Simplifies initial release; resource bundles extensible later |
| **Test targets** | SQL Server 2022 + Azure SQL Database | Latest features + primary cloud target; older versions work via TDS backward compat |

---

## Roadmap

The project is organized into 15 phases grouped by priority. We start with foundational infrastructure and the MVP, then layer on data types, authentication, encryption, and advanced features.

### Phase 1: Setup
- Cargo workspace initialization, CI/CD pipelines, build infrastructure
- Export definition file, platform-specific build.rs, GitHub Actions matrix

### Phase 2: Foundation
- ODBC type system (SQLRETURN, SQLHANDLE, SQL_C_* types)
- SQLAllocHandle / SQLFreeHandle for all handle types (Env, Connection, Statement, Descriptor)
- Diagnostics (SQLGetDiagRec, SQLGetDiagField) and SQLSTATE mapping from mssql-tds errors
- FFI boundary with panic catching and pointer validation
- State machine transitions with HY010 enforcement
- SQLGetInfo, SQLGetFunctions, SQLGetTypeInfo for Driver Manager integration

### Phase 3: Connectivity — MVP
- Connection string parsing (Server, Database, UID, PWD, Encrypt, Trusted_Connection, Authentication, etc.)
- SQLConnect, SQLDriverConnect with SQL authentication
- Map auth keywords to mssql-tds: `Trusted_Connection=Yes` → integrated, `Authentication=ActiveDirectory*` → corresponding mode (mostly keyword passthrough)
- SQLExecDirect, SQLFetch, SQLGetData
- **Milestone**: Connect to SQL Server, execute `SELECT 1`, fetch result

### Phase 4: Result Handling & Prepared Statements
- SQLBindCol
- SQLNumResultCols, SQLDescribeCol, SQLRowCount
- SQLMoreResults for multi-statement batches
- SQLCancel / SQLCancelHandle for query cancellation and timeout handling
- SQLPrepare / SQLExecute via **deferred prepare**: the first `SQLExecute`
  prepares and runs in one round trip with `sp_prepexec`, caches the returned
  handle, and subsequent executes reuse it via `sp_execute` (a rebind or
  re-prepare invalidates the handle). Matches msodbcsql's deferred-prepare path.
- SQLBindParameter — input parameters. Phase-1 subset: `SQL_C_CHAR` →
  `char`/`varchar`/`longvarchar` and `SQL_C_WCHAR` →
  `wchar`/`wvarchar`/`wlongvarchar`; bind-time type/conversion validation
  (HY003 / HY004 / 07006). Output params, data-at-exec, parameter arrays, and
  the wider C↔SQL type matrix are deferred — see
  [parameters_plan.md](parameters_plan.md).
- Statement reuse with different parameter values
- Batch execution with multiple result sets

### Phase 5: All SQL Server Data Types
- All 45+ SQL Server types with correct C type conversions
- Numeric, character, binary, date/time, special types (BIT, GUID, XML)
- LOB streaming via chunked SQLGetData
- Data-at-execution parameters (SQLPutData/SQLParamData)
- Table-Valued Parameters (TVP)
- Cross-type automatic conversions per ODBC specification

### Phase 6: Authentication E2E Validation
- E2E tests for each auth mode against SQL Server 2022 + Azure SQL
- Keyword mapping already implemented in Phase 3; this phase is test coverage and edge-case hardening
- Validate: SQL auth, Windows Integrated (SSPI/Kerberos), all 9 Azure AD modes (Password, Interactive, DeviceCodeFlow, ServicePrincipal, ManagedIdentity, Default, MSI, WorkloadIdentity, Integrated)
- Linux/macOS Kerberos via GSS-API (delegated to mssql-tds)
- Token refresh, expiry, and retry behavior under real Azure AD

### Phase 7: TLS Encryption
- Map Encrypt=Yes/No/Strict, TrustServerCertificate, HostNameInCertificate to mssql-tds
- TLS 1.2/1.3 via `native-tls` (delegated to mssql-tds)
- TDS 8.0 strict encryption mode for SQL Server 2022

### Phase 8: Cross-Platform Installation
- Windows MSI (WiX), Linux DEB/RPM, macOS PKG installers
- Driver registration as "ODBC Driver 18 for SQL Server"
- DSN configuration (ConfigDSN, SQLConfigDataSource)
- CI/CD release pipeline

### Phase 9: Always Encrypted
- SQLSetConnectAttr(SQL_COPT_SS_COLUMN_ENCRYPTION) to enable AE
- Transparent encrypt on SQLBindParameter, decrypt on SQLFetch/SQLGetData for encrypted columns
- Column Encryption Key (CEK) caching and Column Master Key (CMK) resolution
- Keystore providers: Windows Certificate Store, Azure Key Vault
- Secure enclave attestation (SQL Server 2019+) for LIKE, range, and pattern queries on encrypted columns

### Phase 10: Scrollable Cursors
- Static, keyset-driven, and dynamic cursor types
- SQLFetchScroll (ABSOLUTE, RELATIVE, PRIOR, FIRST, LAST)
- Positioned updates/deletes via SQLSetPos, SQLBulkOperations

### Phase 11: Catalog Functions
- Each catalog function is a canned T-SQL query with a fixed result-set column contract — ship incrementally
- SQLTables, SQLColumns, SQLPrimaryKeys, SQLForeignKeys, SQLStatistics, SQLSpecialColumns, SQLProcedures, SQLProcedureColumns
- Results must match msodbcsql column names, types, and ordering exactly
- Filter arguments (catalog, schema, table name patterns) with proper LIKE/escape handling

### Phase 12: Polish & Cross-Cutting Concerns
- Transaction management: SQLEndTran (COMMIT/ROLLBACK), SQLSetConnectAttr for isolation levels (READ COMMITTED, SNAPSHOT, etc.), autocommit on/off
- Unicode (W) FFI entry points: string-accepting exports duplicated as wide-char variants (SQLConnectW, SQLExecDirectW, etc.) for Driver Manager integration
- Data type conversion compliance: validate all SQL_C_* ↔ SQL_* conversion pairs, overflow/truncation behavior, SQL_C_DEFAULT resolution
- SQLSTATE audit: verify every error path returns the correct SQLSTATE per ODBC 3.x spec (HY000, HY001, HY010, 42000, 08001, etc.)
- Connection pooling: SQLSetEnvAttr(SQL_ATTR_CONNECTION_POOLING), state reset on pool return, dead connection detection
- Connection resiliency: auto-reconnect via ConnectRetryCount / ConnectRetryInterval connection string keywords
- Async execution: see **Phase 15: Asynchronous Execution** (dedicated phase)
- DTC support: Windows ITransactionDispenser integration for distributed transactions (platform_windows)
- DSN configuration GUI: ConfigDSNW dialog for Windows ODBC Data Source Administrator (platform_windows)
- Error message resource files (.rll for Windows localized messages)
- Multi-subnet failover: MultiSubnetFailover=Yes connection string keyword, parallel connection attempts
- 24-hour stress test, memory leak validation (Valgrind/ASAN)

### Phase 13: Bulk Copy Program (BCP)
- All 25+ BCP API functions: `bcp_init`, `bcp_bind`, `bcp_sendrow`, `bcp_batch`, `bcp_exec`
- High-performance bulk import/export targeting >50K rows/sec
- FFI wiring for all BCP entry points

### Phase 14: MARS
- SQLSetConnectAttr(SQL_COPT_SS_MARS_ENABLED) to enable/disable MARS
- Multiple active SQLExecDirect / SQLFetch calls on separate statements sharing one connection
- TDS session multiplexing — delegated to mssql-tds; ODBC layer routes results to correct statement handle
- When MARS is off, return HY000 if a second statement executes while results are pending

### Phase 15: Asynchronous Execution (Polling Method)

**Scope decision: mirror msodbcsql — statement-level async only.** Verified against
the reference driver's observable behavior:
the reference driver advertises `SQL_ASYNC_MODE = SQL_AM_STATEMENT`,
`SQL_MAX_ASYNC_CONCURRENT_STATEMENTS = 1`, and `SQL_ASYNC_DBC_FUNCTIONS = 0`.
We match this exactly so we remain a drop-in replacement.

**What we support (statement-level):**
- `SQLSetStmtAttr(SQL_ATTR_ASYNC_ENABLE, SQL_ASYNC_ENABLE_ON/OFF)` — writable per
  statement, toggleable between operations. Default is `SQL_ASYNC_ENABLE_OFF`
  (set at statement allocation).
- `SQLGetInfo` advertises: `SQL_ASYNC_MODE = SQL_AM_STATEMENT`,
  `SQL_MAX_ASYNC_CONCURRENT_STATEMENTS = 1`, `SQL_ASYNC_DBC_FUNCTIONS = 0`.
- Async-capable statement functions return `SQL_STILL_EXECUTING` while the
  operation is in flight; the app polls by re-calling the *same* function with
  the *same* arguments until it returns the final code. Target set (match
  msodbcsql): `SQLExecDirect[W]`, `SQLExecute`, `SQLFetch`, `SQLFetchScroll`,
  `SQLMoreResults`, `SQLGetData`, `SQLNumResultCols`, `SQLDescribeCol[W]`,
  `SQLColAttribute[W]`, `SQLPrepare[W]`, and the catalog functions.
- Cancellation of an in-flight async op via `SQLCancel` / `SQLCancelHandle`
  (wires into the existing Phase 4 cancel path / mssql-tds `CancelHandle`).

**What we explicitly do NOT support (matching msodbcsql):**
- **No connection-level async** (`SQL_AM_CONNECTION`). The `SQL_ATTR_ASYNC_ENABLE`
  statement attribute is therefore *not* read-only and is *not* settable via
  `SQLSetConnectAttr` to fan out to statements.
- **No async connection functions** (`SQL_ATTR_ASYNC_DBC_FUNCTIONS_ENABLE`):
  `SQLConnect`/`SQLDriverConnect`/`SQLDisconnect`/`SQLEndTran` stay synchronous.
  Return the appropriate error if an app tries to enable DBC-level async.

**State-machine rules:**
- Setting `SQL_ATTR_ASYNC_ENABLE` while that statement has an async op in
  progress returns **HY010** (function sequence error) — the *only* timing
  restriction (mirrors msodbcsql's in-progress async guard).
- At most one async op per statement handle; with
  `SQL_MAX_ASYNC_CONCURRENT_STATEMENTS = 1`, only one statement per connection
  may have an async op in flight (non-MARS). A second async op on the connection
  returns HY010.
- While an op is `SQL_STILL_EXECUTING`, only the original function plus
  `SQLCancel`/`SQLCancelHandle`, `SQLGetDiagRec`/`SQLGetDiagField`, and the
  read-only info functions may be called on that handle; anything else → HY010.
- Each repeated (polling) call clears the previous diagnostic records, per spec.

**Runtime implications (see `tokio.md`):**
- True async ODBC requires a **`multi_thread`** Tokio runtime: `SQLExecDirect`
  spawns the work via `runtime.spawn(...)`, stores the `JoinHandle` on the
  statement, and returns `SQL_STILL_EXECUTING`. A worker thread drives the future
  while the app thread is outside the driver; the polling re-call checks
  `JoinHandle::is_finished()`. A `current_thread` runtime cannot do this (the
  spawned task freezes once `block_on` returns).
- `StmtState` gains a `pending: Option<JoinHandle<...>>` (or equivalent) plus the
  async-enabled flag stored in the per-statement attribute table.
- Ownership of the `TdsClient` must move into the spawned future and back on
  completion (same take/return dance as the sync path, but across the spawn).

**Out of scope for this phase (future):** the ODBC 3.8 notification method
(`SQL_ATTR_ASYNC_STMT_EVENT`) — polling method only, matching the initial
msodbcsql parity bar.

**Milestone**: `SQLExecDirect` on an async-enabled statement returns
`SQL_STILL_EXECUTING`, polls to completion on a worker thread, and `SQLCancel`
interrupts an in-flight query.

---

## Phase Dependencies

```
  Phase 1: Setup
      │
      ▼
  Phase 2: Foundation  ◄── BLOCKS all user stories
      │
      ├───► Phase 3: Connectivity (MVP)
      │         │
      │         ├───► Phase 4: Result Handling & Prepared Statements
      │         │         │
      │         │         ├───► Phase 5: Data Types
      │         │         │         │
      │         │         │         └───► Phase 9: Always Encrypted
      │         │         │
      │         │         ├───► Phase 14: MARS
      │         │         └───► Phase 15: Async Execution
      │         │
      │         ├───► Phase 6: Auth E2E Validation
      │         ├───► Phase 7: TLS Encryption
      │         ├───► Phase 8: Installation
      │         ├───► Phase 10: Cursors
      │         ├───► Phase 11: Catalog Functions
      │         └───► Phase 13: BCP
      │
      └───► Phase 12: Cross-Cutting (alongside any phase)
```

Phases **6, 7, 8, 10, 11, and 13** can proceed in parallel once Phase 3 is complete. Phase 9 (Always Encrypted) requires Phases 4 and 5. Phase 11 (Catalog) can ship incrementally — each function is independent. Phase 12 cross-cutting items can be picked up alongside any phase. Phases 13 (BCP) and 14 (MARS) are deferred — not required for initial releases. Phase 15 (Async Execution) requires Phase 4 (cancellation path) and a `multi_thread` Tokio runtime; it is deferred — not required for initial releases.

---

## Risks & Mitigations

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| **mssql-tds API gaps** — pre-release library may lack features | High | Return HYC00 for unsupported features; document gaps; contribute upstream PRs |
| **ODBC conformance failures** — spec edge cases | Medium | Run conformance tests early; use ec-mini-odbc MCP server to verify msodbcsql behavior |
| **BCP performance below 50K rows/sec** | Low | Profile with criterion; optimize type conversions and buffer copies |
| **Azure AD complexity** — 9 auth modes with platform differences | Medium | Delegate to mssql-tds (supports all modes); test incrementally |
| **Cross-platform build matrix** — 3 OS × 2+ architectures | Medium | GitHub Actions matrix builds; Docker for Linux reproducibility |

---

## Success Metrics

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | ODBC Core Level 1 conformance | 100% pass |
| SC-002 | Drop-in replacement | Zero app code changes from msodbcsql18 |
| SC-003 | Authentication | All auth methods connect to SQL Server 2022 + Azure SQL |
| SC-004 | Query overhead | <5ms vs. msodbcsql |
| SC-005 | BCP throughput | >50K rows/sec (10 INT columns, gigabit network) |
| SC-006 | Stability | 24-hour stress test (100 queries/sec), zero memory leaks |
| SC-007 | Installer | Completes <60s, registers as "ODBC Driver 18 for SQL Server" |
| SC-008 | Catalog parity | Identical results to msodbcsql |
| SC-009 | Encryption | TLS verified (no plaintext SQL in Wireshark capture) |
| SC-010 | Data type accuracy | All 45+ types round-trip with zero corruption |
| SC-011 | MARS | 5 concurrent queries on single connection |
| SC-012 | Cross-platform | Windows/Linux/macOS install succeeds |
| SC-013 | Error messages | Format matches msodbcsql `[Microsoft][ODBC Driver 18 for SQL Server]...` |
| SC-014 | Connection pooling | 1000 connections (10 threads, >90% reuse rate) |
| SC-015 | Always Encrypted | Transparent encrypt/decrypt with keystore provider |

---

## Appendix: ODBC APIs Used by mssql-python

Audit of [microsoft/mssql-python](https://github.com/microsoft/mssql-python) — 38 ODBC functions dynamically loaded via `GetFunctionPointer` in `ddbc_bindings.cpp`. This is the minimum API surface required for mssql-python compatibility.

| Category | Functions |
|----------|-----------|
| **Handle Management** (2) | `SQLAllocHandle`, `SQLFreeHandle` |
| **Environment** (1) | `SQLSetEnvAttr` |
| **Connection Attrs** (2) | `SQLSetConnectAttrW`, `SQLGetConnectAttrW` |
| **Statement Attrs** (2) | `SQLSetStmtAttrW`, `SQLGetStmtAttrW` |
| **Connection** (2) | `SQLDriverConnectW`, `SQLDisconnect` |
| **Execution** (4) | `SQLExecDirectW`, `SQLPrepareW`, `SQLExecute`, `SQLRowCount` |
| **Parameters** (4) | `SQLBindParameter`, `SQLDescribeParam`, `SQLParamData`, `SQLPutData` |
| **Data Retrieval** (7) | `SQLFetch`, `SQLFetchScroll`, `SQLGetData`, `SQLNumResultCols`, `SQLBindCol`, `SQLDescribeColW`, `SQLMoreResults` |
| **Result Metadata** (1) | `SQLColAttributeW` |
| **Catalog** (7) | `SQLTablesW`, `SQLColumnsW`, `SQLPrimaryKeysW`, `SQLForeignKeysW`, `SQLSpecialColumnsW`, `SQLStatisticsW`, `SQLProceduresW` |
| **Info** (2) | `SQLGetInfoW`, `SQLGetTypeInfoW` |
| **Descriptor** (1) | `SQLSetDescFieldW` |
| **Transaction** (1) | `SQLEndTran` |
| **Diagnostics** (1) | `SQLGetDiagRecW` |
| **Cleanup** (1) | `SQLFreeStmt` |

**Not used by mssql-python**: `SQLConnect`, `SQLBrowseConnect`, `SQLGetFunctions`, `SQLSetPos`, `SQLBulkOperations`, `SQLCloseCursor`, `SQLCancel`, `SQLProcedureColumns`, `SQLGetDiagField`, `SQLGetDescField`, all `bcp_*` functions.

---

## Appendix: ODBC APIs used by pyodbc but not mssql-python

Audit of [mkleehammer/pyodbc](https://github.com/mkleehammer/pyodbc) (source call sites) and diffed against the mssql-python API list above.

| ODBC APIs used by pyodbc but not mssql-python |
|----------|
| `SQLCancel` |
| `SQLColAttribute` |
| `SQLDataSources` |
| `SQLDataSourcesW` |
| `SQLDescribeCol` |
| `SQLDrivers` |
| `SQLExecDirect` |
| `SQLForeignKeys` |
| `SQLGetDiagField` |
| `SQLGetInfo` |
| `SQLGetStmtAttr` |
| `SQLGetTypeInfo` |
| `SQLNumParams` |
| `SQLPrepare` |
| `SQLPrimaryKeys` |
| `SQLProcedureColumns` |
| `SQLProcedures` |
| `SQLSetConnectAttr` |
| `SQLSetDescField` |
| `SQLSetStmtAttr` |
| `SQLSpecialColumns` |
| `SQLStatistics` |
| `SQLTables` |

---

## Appendix: Known Issues

### Disconnect Lifetime Race Condition

**Issue**: A use-after-free (UAF) can occur if a thread calls `SQLDisconnect` while another thread is mid-execute (during `SQLExecDirectW` network I/O), even though the ODBC spec and Driver Manager should prevent this scenario in correct usage.

**Root Cause**:
- In `SQLExecDirectW` ([exec_direct.rs line ~150](src/api/exec_direct.rs#L148-L150)), the DBC lock is released before I/O begins to avoid holding the lock during network operations.
- Thread A (executing) takes the TdsClient, releases the DBC lock, and begins `block_on(client.execute(...))`.
- Thread B (disconnecting) locks the DBC, sees the connection is connected, then frees all STMT handles in a loop by taking the STMT lock and immediately deallocating the `Box<StmtHandle>`.
- Thread A's I/O completes and attempts to lock the STMT handle again to post error diagnostics, but the memory has already been freed by Thread B → UAF.

**Why It Doesn't Fire in Practice**:
The ODBC spec explicitly gates `SQLDisconnect` via the Driver Manager with `HY010` (function sequence error) if:
- any statement is still executing **asynchronously** (returns `SQL_STILL_EXECUTING`), or
- any statement/connection is in a `SQL_NEED_DATA` flow (data-at-execution parameters).

([SQLDisconnect diagnostics](https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldisconnect-function?view=sql-server-ver17#diagnostics))
([Statement Transitions](https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/statement-transitions?view=sql-server-ver17#sqldisconnect))

For **synchronous** in-flight operations, the spec does not explicitly gate `SQLDisconnect` at the DM level, but the intended usage pattern is: app drains all results via `SQLFetch`/`SQLGetData`, calls `SQLCloseCursor` or `SQLFreeStmt(SQL_CLOSE)`, *then* `SQLDisconnect`. In correct ODBC usage, this ordering ensures no concurrent synchronous I/O exists when `SQLDisconnect` is called.

**How msodbcsql Handles It:**
msodbcsql uses **batch-context refcounting** — a separate batch execution context independent of the STMT handle lifetime:
1. When a statement starts executing, it attaches a batch context with a reference count.
2. Thread A (executing) holds a strong reference to the batch context, incrementing its refcount.
3. When `SQLDisconnect` frees a STMT handle, it does **not** free the batch context; only the STMT handle box is deallocated.
4. Thread A's in-flight I/O still holds a reference to the batch context; when the operation completes, the refcount drops and cleanup happens cleanly.
5. Thread B (disconnecting) can safely free STMT handles because the underlying batch state remains alive while referenced.

**Fix (Phase 3+)** (Not fixed now — does not affect correct ODBC usage):
Introduce an `Arc<BatchContext>` wrapper so each TdsClient operation holds a strong reference across I/O. The `StmtHandle` can be deallocated immediately by `SQLDisconnect`, but the arc keeps the underlying state alive until all in-flight operations complete. This mirrors msodbcsql's batch-context pattern but using Rust's Arc for automatic refcounting.

See [disconnect.rs line 63](src/api/disconnect.rs#L63) for the TODO comment noting this exact issue.

---

## Appendix: Authentication Method Support Comparison

| Authentication Method | mssql-tds | mssql-python | mssql-odbc (target) | Notes |
|---|---|---|---|---|
| **SQL Server Auth** |
| Password | ✅ | ✅ | ✅ | Username + password |
| **Integrated / Domain** |
| SSPI / Integrated | ✅ | ✅ | ✅ | Windows SSPI or Unix GSSAPI/Kerberos |
| **Entra ID / Azure AD** |
| ActiveDirectoryPassword | ✅ | ✅ | ✅ | Username + password. Deprecated by the drivers — uses the Azure AD ROPC flow, which Microsoft discourages; prefer Interactive / Default / ServicePrincipal |
| ActiveDirectoryInteractive | ✅ | ✅ | ✅ | Browser-based interactive sign-in |
| ActiveDirectoryDeviceCode | ✅ | ✅ | ✅ | Device code flow (headless/CLI environments) |
| ActiveDirectoryServicePrincipal | ✅ | ✅ | ✅ | Client ID + secret or certificate |
| ActiveDirectoryManagedIdentity / MSI | ✅ | ✅ | ✅ | System-assigned or user-assigned identity |
| ActiveDirectoryDefault | ✅ | ✅ | ✅ | Auto-detect auth method from environment |
| ActiveDirectoryIntegrated | ✅ | ❌ | ✅ | Entra with current user's Kerberos ticket |
| **Advanced** |
| ActiveDirectoryWorkloadIdentity | ✅ | ❌ | ✅ | Kubernetes workload identity |
| AccessToken (JWT) | ✅ | ❌ | ✅ | Pre-acquired bearer access token |

**Summary**:
- **mssql-tds** supports all 12 methods (foundation layer)
- **mssql-python** supports 9 core methods (Password, Integrated, and 7 Entra flows)
- **mssql-odbc** targets full parity with mssql-tds (all 12 methods delegated via tds layer)

# Implementation Plan: ODBC Driver 18 for SQL Server in Rust

---

## Overview

This project is developing an ODBC 3.x driver for SQL Server in Rust, with
Microsoft ODBC Driver 18 compatibility as the target. The driver is alpha
software and is not yet a general-purpose, drop-in replacement for
`msodbcsql18`.

The driver wraps the workspace-local `mssql-tds` protocol library. The current
export manifest defines 50 ODBC entry points covering handles, connectivity,
statement execution, parameters, result retrieval, catalogs, descriptors, and
diagnostics. `SQLSetConnectAttr` is exported only on non-Windows targets; the
other character APIs currently expose their wide variants.

**Target platforms**: Windows, Linux, macOS (x64 and ARM64) - Same as the platforms currently supported by msodbcsql
**Binary output**: `mssqlodbc.dll` (Windows), `mssqlodbc.so` (Linux), `mssqlodbc.dylib` (macOS)

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

`mssql-odbc` is a single crate producing the native driver library. Its current
top-level structure is:

```
mssql-odbc/
├── src/
│   ├── lib.rs             # Crate root and FFI panic boundary
│   ├── tracing_init.rs    # Process-wide tracing initialization
│   ├── api/               # ODBC exports and implementations by API/family
│   ├── auth/              # SQL, integrated, access-token, and Entra adapters
│   ├── connection/        # Connection-string parsing and orchestration
│   ├── conversion/        # Parameter and result value conversion
│   ├── handles/           # ENV, DBC, STMT, and DESC state
│   ├── params/            # Bound parameters and conversion legality matrix
│   └── error/             # Diagnostic records and SQLSTATE mapping
├── tests/e2e/             # Driver Manager C++ end-to-end suite
├── docs/                  # Subsystem plans and parity records
├── build.rs               # Platform linker and artifact configuration
├── README.md
├── plan.md
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
| **Authentication** | ODBC adapters over mssql-tds | SQL password, SSPI, access token, service principal, managed identity, and Windows interactive are wired; other recognized Entra modes currently return `HYC00` |
| **Localization** | English (en_US) only | Simplifies initial release; resource bundles extensible later |
| **Test targets** | SQL Server 2022 + Azure SQL Database | Latest features + primary cloud target; older versions work via TDS backward compat |

---

## Roadmap

This roadmap records both implemented milestones and target capabilities. Phase
status describes the current crate, not merely whether supporting code exists in
`mssql-tds`.

### Phase 1: Setup — Implemented
- Cargo workspace initialization, CI/CD pipelines, build infrastructure
- Export definition file, platform-specific build.rs, GitHub Actions matrix

### Phase 2: Foundation — Implemented
- ODBC type system (SQLRETURN, SQLHANDLE, SQL_C_* types)
- SQLAllocHandle / SQLFreeHandle for all handle types (Env, Connection, Statement, Descriptor)
- Diagnostics (SQLGetDiagRec, SQLGetDiagField) and SQLSTATE mapping from mssql-tds errors
- FFI boundary with panic catching and pointer validation
- State machine transitions with HY010 enforcement
- SQLGetInfo, SQLGetFunctions, SQLGetTypeInfo for Driver Manager integration

### Phase 3: Connectivity — MVP — Implemented
- Connection string parsing (Server, Database, UID, PWD, Encrypt, Trusted_Connection, Authentication, etc.)
- SQLConnect, SQLDriverConnect with SQL authentication
- Map auth keywords to mssql-tds: `Trusted_Connection=Yes` → integrated, `Authentication=ActiveDirectory*` → corresponding mode (mostly keyword passthrough)
- SQLExecDirect, SQLFetch, SQLGetData
- **Milestone**: Connect to SQL Server, execute `SELECT 1`, fetch result

### Phase 4: Result Handling & Prepared Statements — Implemented with limitations
- SQLBindCol
- SQLNumResultCols, SQLDescribeCol, SQLRowCount
- SQLMoreResults for multi-statement batches
- SQLCancel for query cancellation and timeout handling
- SQLPrepare / SQLExecute via **deferred prepare**: the first `SQLExecute`
  prepares and runs in one round trip with `sp_prepexec`, caches the returned
  handle, and subsequent executes reuse it via `sp_execute` (a rebind or
  re-prepare invalidates the handle). Matches msodbcsql's deferred-prepare path.
- SQLBindParameter with input, output, input/output, and return-value
  parameters; broad C↔SQL conversion validation; data-at-execution input; and
  prepared parameter arrays. Streamed output parameters, data-at-execution
  inside arrays, and parameter arrays through `SQLExecDirect` remain unsupported.
  See [parameters_plan.md](docs/parameters_plan.md).
- Statement reuse with different parameter values
- Batch execution with multiple result sets

### Phase 5: SQL Server Data Types — In progress
- Complete the C↔SQL conversion matrix and overflow/truncation behavior
- Numeric, character, binary, date/time, and special types (BIT, GUID, XML)
- LOB streaming via chunked SQLGetData
- Table-Valued Parameters (TVP)
- Cross-type automatic conversions per ODBC specification

### Phase 6: Authentication E2E Validation — In progress
- E2E tests for each auth mode against SQL Server 2022 + Azure SQL
- Complete the one remaining driver-reachable adapter,
  `ActiveDirectoryIntegrated` (AB#46068). `ActiveDirectoryPassword` is an
  accepted out-of-scope deviation (AB#45486) and stays `HYC00`.
- Validate what the driver itself performs: SQL auth, Windows Integrated
  (SSPI/Kerberos), Windows Interactive, ServicePrincipal, ManagedIdentity/MSI,
  and pre-acquired access tokens
- `ActiveDirectoryDefault` and `ActiveDirectoryDeviceCode` are not msodbcsql
  `Authentication=` keywords; mssql-python maps both and hands the driver
  `SQL_COPT_SS_ACCESS_TOKEN`, so validating them means covering the
  access-token path rather than a driver-side mode.
- Any `Authentication=` value mssql-python does not map survives into the
  connection string and reaches this driver. That includes
  `ActiveDirectoryWorkloadIdentity` and the `ActiveDirectoryDeviceCodeFlow`
  spelling, both of which this driver recognizes and validates before
  `configure_auth` returns `HYC00`. They are reachable unsupported
  pass-through modes, not unreachable ones. Neither is an msodbcsql keyword,
  so implementing them would exceed parity - decide that explicitly rather
  than by default.
- Linux/macOS Kerberos via GSS-API (delegated to mssql-tds)
- Token refresh, expiry, and retry behavior under real Azure AD

### Phase 7: TLS Encryption — In progress
- Map Encrypt, TrustServerCertificate, and HostNameInCertificate to mssql-tds
- Validate TLS behavior and certificate diagnostics on each target platform
- Complete and verify TDS 8.0 strict encryption against SQL Server 2022

### Phase 8: Distribution and Installation — In progress
- **Shipped**: the driver is distributed inside the `mssql-python-rs` Python
  wheels on PyPI, for Windows (x64, ARM64), macOS (universal2), and Linux
  (`manylinux_2_28`, `manylinux_2_34`, `musllinux_1_2`; x86_64 and aarch64),
  built and verified by the containerized pipeline under `.pipeline/` and
  `scripts/`. This replaced OS-native packaging as the initial release vehicle.
- Windows MSI (WiX), Linux DEB/RPM, macOS PKG installers
- Driver Manager registration as "ODBC Driver 18 for SQL Server" outside the
  test harnesses. The e2e and benchmark scripts register a distinct
  "ODBC Driver 18 for SQL Server (Rust)" name so both drivers can coexist, and
  mssql-python loads the library directly rather than through a registration.
- DSN configuration: `ConfigDSN` and `SQLConfigDataSource` are not exported

### Phase 9: Always Encrypted — Planned
- SQLSetConnectAttr(SQL_COPT_SS_COLUMN_ENCRYPTION) to enable AE
- Transparent encrypt on SQLBindParameter, decrypt on SQLFetch/SQLGetData for encrypted columns
- Column Encryption Key (CEK) caching and Column Master Key (CMK) resolution
- Keystore providers: Windows Certificate Store, Azure Key Vault
- Secure enclave attestation (SQL Server 2019+) for LIKE, range, and pattern queries on encrypted columns

### Phase 10: Scrollable Cursors — Planned
- Static, keyset-driven, and dynamic cursor types
- SQLFetchScroll (ABSOLUTE, RELATIVE, PRIOR, FIRST, LAST)
- Positioned updates/deletes via SQLSetPos, SQLBulkOperations

### Phase 11: Catalog Functions — In progress
- Each catalog function dispatches to the matching SQL Server system stored
  procedure via RPC, renames its ODBC 2.x column names to ODBC 3.x, and clears
  the ODBC-mandated NOT NULL flags — matching msodbcsql (`sqlcdd.cpp` `DoDD()`)
- **Implemented**: SQLTables, SQLColumns, SQLPrimaryKeys, SQLForeignKeys,
  SQLStatistics, SQLSpecialColumns, SQLProcedures (AB#46380)
- **Not yet implemented**: SQLProcedureColumns, SQLTablePrivileges,
  SQLColumnPrivileges
- Results match msodbcsql column names, types, and ordering exactly
- Filter arguments (catalog, schema, table name patterns) flow through
  unmodified to the stored procedures, which already implement ODBC's
  pattern/exact-match argument semantics server-side

### Phase 12: Polish & Cross-Cutting Concerns — Ongoing
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

### Phase 13: Bulk Copy Program (BCP) — Deferred
- All 25+ BCP API functions: `bcp_init`, `bcp_bind`, `bcp_sendrow`, `bcp_batch`, `bcp_exec`
- High-performance bulk import/export targeting >50K rows/sec
- FFI wiring for all BCP entry points

### Phase 14: MARS — Deferred
- SQLSetConnectAttr(SQL_COPT_SS_MARS_ENABLED) to enable/disable MARS
- Multiple active SQLExecDirect / SQLFetch calls on separate statements sharing one connection
- TDS session multiplexing — delegated to mssql-tds; ODBC layer routes results to correct statement handle
- When MARS is off, return HY000 if a second statement executes while results are pending

### Phase 15: Asynchronous Execution (Polling Method) — Planned

**Planned scope: mirror msodbcsql — statement-level async only.** The reference
driver advertises `SQL_ASYNC_MODE = SQL_AM_STATEMENT`,
`SQL_MAX_ASYNC_CONCURRENT_STATEMENTS = 1`, and `SQL_ASYNC_DBC_FUNCTIONS = 0`.
This phase is not implemented yet: `SQLGetInfo(SQL_ASYNC_MODE)` truthfully
returns `SQL_AM_NONE`. See `docs/sql-get-info-plan.md` for the measured
AB#48149 capability ledger.

**What this phase will support (statement-level):**
- `SQLSetStmtAttr(SQL_ATTR_ASYNC_ENABLE, SQL_ASYNC_ENABLE_ON/OFF)` — writable per
  statement, toggleable between operations. Default is `SQL_ASYNC_ENABLE_OFF`
  (set at statement allocation).
- Once implemented, `SQLGetInfo` will advertise: `SQL_ASYNC_MODE = SQL_AM_STATEMENT`,
  `SQL_MAX_ASYNC_CONCURRENT_STATEMENTS = 1`, `SQL_ASYNC_DBC_FUNCTIONS = 0`.
- Async-capable statement functions return `SQL_STILL_EXECUTING` while the
  operation is in flight; the app polls by re-calling the *same* function with
  the *same* arguments until it returns the final code. Target set (match
  msodbcsql): `SQLExecDirect[W]`, `SQLExecute`, `SQLFetch`, `SQLFetchScroll`,
  `SQLMoreResults`, `SQLGetData`, `SQLNumResultCols`, `SQLDescribeCol[W]`,
  `SQLColAttribute[W]`, `SQLPrepare[W]`, and the catalog functions.
- Cancellation of an in-flight async op via `SQLCancel`
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
  `SQLCancel`, `SQLGetDiagRec`/`SQLGetDiagField`, and the
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

## Risks & Mitigations

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| **mssql-tds API gaps** — pre-release library may lack features | High | Return HYC00 for unsupported features; document gaps; contribute upstream PRs |
| **ODBC conformance failures** — spec edge cases | Medium | Run the e2e suite against both drivers through the same Driver Manager (`run_e2e.sh --compare-with-msodbcsql`) and treat any parity-table difference as a defect |
| **BCP performance below 50K rows/sec** | Low | Profile with criterion; optimize type conversions and buffer copies |

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

Audit of [microsoft/mssql-python](https://github.com/microsoft/mssql-python) at
commit `29fa5546eb7f24df0d5d42276549aa789937880d`: 40 ODBC functions are
dynamically resolved via `GetFunctionPointer` in `ddbc_bindings.cpp`. The
loader's mandatory composite check covers 38; `SQLCancel` and
`SQLGetDiagFieldW` are optional but used when available.

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
| **Cancellation** (1) | `SQLCancel` |
| **Diagnostics** (2) | `SQLGetDiagRecW`, `SQLGetDiagFieldW` |
| **Cleanup** (1) | `SQLFreeStmt` |

**Not used by mssql-python**: `SQLConnect`, `SQLBrowseConnect`, `SQLGetFunctions`, `SQLSetPos`, `SQLBulkOperations`, `SQLCloseCursor`, `SQLProcedureColumns`, `SQLGetDescField`, all `bcp_*` functions.

---

## Appendix: ODBC APIs used by pyodbc but not mssql-python

Audit of [mkleehammer/pyodbc](https://github.com/mkleehammer/pyodbc) source call
sites, diffed against the mssql-python list above.

pyodbc chooses the ANSI or the wide entry point at runtime from the connection
encoding (the `isWide` branches in `cursor.cpp`), so it calls both spellings of
most families. Treating each A/W pair as one family, three families are absent
from the mssql-python list:

| Additional API | pyodbc use | Current mssql-odbc status |
|---|---|---|
| `SQLNumParams` | Parameter-marker count in `params.cpp` | Exported |
| `SQLProcedureColumns` | `Cursor.procedureColumns` | Not implemented |
| `SQLTablePrivileges` | `Cursor.tablePrivileges` | Not implemented |

The other unsuffixed names pyodbc calls — `SQLColAttribute`, `SQLDescribeCol`,
`SQLExecDirect`, `SQLForeignKeys`, `SQLGetInfo`, `SQLGetStmtAttr`,
`SQLGetTypeInfo`, `SQLPrepare`, `SQLPrimaryKeys`, `SQLProcedures`,
`SQLSetConnectAttr`, `SQLSetDescField`, `SQLSetStmtAttr`, `SQLSpecialColumns`,
`SQLStatistics`, and `SQLTables` — are ANSI spellings of families mssql-python
already uses in `W` form. They are not additional APIs, and they raise the same
ANSI routing question as the PHP appendix below.

`SQLDataSources` and `SQLDrivers` back `pyodbc.dataSources()` and
`pyodbc.drivers()`. Both are Driver Manager enumeration APIs called on the
environment handle, so they are not driver exports.

### Required feature work beyond API availability

| Area | pyodbc path | Required work |
|------|-------------|---------------|
| **Table-valued parameters** | `SQL_SS_TABLE` parameters bind their constituent columns under `SQL_SOPT_SS_PARAM_FOCUS`, each column sent as DAE | Implement TVP binding. The crate rejects `SQL_SOPT_SS_PARAM_FOCUS`, so this fails at the first call. |
| **DAE inside parameter arrays** | `fast_executemany` drives a `SQLParamData`/`SQLPutData` loop within a single paramset | Support data-at-execution within parameter arrays. The crate rejects the combination, so `fast_executemany` fails once any value is DAE-sized. |
| **Row-wise parameter arrays** | Binds at a synthetic base address with `SQL_ATTR_PARAM_BIND_TYPE` set to the row length and `SQL_ATTR_PARAM_BIND_OFFSET_PTR` rebasing each batch | Implemented; validate against pyodbc's offset technique. |
| **APD descriptor writes** | `SQL_C_NUMERIC` parameters set `SQL_DESC_TYPE`, `SQL_DESC_PRECISION`, `SQL_DESC_SCALE`, and `SQL_DESC_DATA_PTR` on the application parameter descriptor | Validate `SQLSetDescField`, including `SQL_DESC_DATA_PTR` writes. |
| **Retrieval and sizing metadata** | No `SQLBindCol`; every value is read through `SQLGetData`, and DAE thresholds come from `SQL_NEED_LONG_DATA_LEN` and `SQLGetTypeInfo` column sizes | Validate chunked `SQLGetData` and the reported type metadata, which decide when pyodbc switches to DAE. |
| **ANSI routing** | `setencoding`/`setdecoding` can select `SQL_C_CHAR`, after which pyodbc calls the unsuffixed entry points | Same ANSI routing question as the API table above. |

pyodbc does not need scrollable cursors, output parameters, or `SQLBindCol`:
`Cursor.skip` deliberately calls `SQLFetchScroll` with `SQL_FETCH_NEXT` to avoid
scrollable cursors, and both parameter paths bind `SQL_PARAM_INPUT` only.

The API gaps are `SQLProcedureColumns` and `SQLTablePrivileges`; the feature
gaps are TVPs and data-at-execution inside parameter arrays.

---

## Appendix: Additional ODBC support required by the PHP drivers

Audit of [microsoft/msphpsql](https://github.com/microsoft/msphpsql) `dev` at
commit `a218e2382c5d2698f84ed762044e7ab0bfe3b9fc`. Both `sqlsrv` and
`pdo_sqlsrv` use the shared ODBC layer under `source/shared/`.

Treating ANSI and wide variants as one API family, msphpsql calls one family
that is absent from the mssql-python list above. It already exists in
`mssql-odbc`:

| Additional API | PHP use | Current mssql-odbc status |
|----------|---------|---------------------------|
| `SQLNumParams` | Parameter-marker count | Exported |

Msphpsql also uses unsuffixed calls for `SQLColAttribute`, `SQLColumns`,
`SQLDescribeCol`, `SQLGetConnectAttr`, `SQLGetInfo`, `SQLGetStmtAttr`,
`SQLPrepare`, `SQLSetConnectAttr`, `SQLSetStmtAttr`, and `SQLTables`, while
`mssql-odbc` mainly exports the corresponding `W` symbols. Because msphpsql
calls through an ODBC Driver Manager, test these paths through the Windows
Driver Manager and unixODBC before adding ANSI exports.

`SQLGetInstalledDrivers` is not a driver requirement. It is a Driver Manager
installer API; installation only needs to register the expected driver name.

### Required feature work beyond mssql-python compatibility

| Area | PHP path | Required work |
|------|----------|---------------|
| **Server cursors** | Static, dynamic, and keyset cursor options; PRIOR, FIRST, LAST, ABSOLUTE, and RELATIVE fetches | Implement non-forward cursor types and orientations. The PHP-layer client-buffered cursor does not require a new ODBC feature. |
| **Table-valued parameters** | `SQL_SS_TABLE`, descriptor fields, and `SQL_SOPT_SS_PARAM_FOCUS` for constituent columns | Implement TVP binding and RPC serialization. The crate currently rejects `SQL_SOPT_SS_PARAM_FOCUS`. |
| **Always Encrypted and data classification** | Column-encryption options and `SQL_COPT_SS_DATACLASSIFICATION_VERSION` negotiation | Implement the existing Phase 9 work plus classification metadata. |
| **Connection options** | PHP exposes MARS, failover partner, attach-file, language, workstation ID, quoted-ID, and transparent-network-resolution options | These keywords are currently recognized but ignored; implement each option required for the intended PHP compatibility level. |
| **Consumer validation** | ANSI Driver Manager routing, output parameters, DAE input streams, metadata/catalog calls, transactions, timeouts, and the PHP C/SQL type matrix | Run both PHP extension suites through each target Driver Manager. Treat failures as concrete behavior gaps; these paths do not currently imply new API families. |

The largest feature gaps are server cursors and TVPs. Full PHP feature
parity additionally requires the optional SQL Server features above.

---

## Appendix: Known Issues

### Disconnect Lifetime Race Condition

Concurrent `SQLDisconnect` and statement I/O can cause a use-after-free. The
execution path releases the DBC lock during network I/O, while disconnect can
free the statement handle before the execution path reacquires it.

The handle lifetime must be refcounted so in-flight operations retain valid
state independently of ODBC handle ownership. Until that is implemented,
callers must serialize disconnect against all operations on the connection.
See the lifetime TODO in [disconnect.rs](src/api/disconnect.rs).

---

## Appendix: Authentication Method Support Comparison

`mssql-python` acquires tokens itself through Azure Identity for Default,
DeviceCode, MSI, and non-Windows Interactive, then passes the result to the
driver as `SQL_COPT_SS_ACCESS_TOKEN`; for those modes the driver only needs
access-token support. It leaves Windows Interactive and ServicePrincipal to the
driver, and any keyword it does not map stays in the connection string for the
driver to resolve.

| Authentication method | mssql-tds | Driver support mssql-python needs | mssql-odbc | Notes |
|---|---|---|---|---|
| Password (SQL auth) | ✅ | Native | Implemented | Username + password |
| SSPI / Integrated | ✅ | Native | Implemented | Windows SSPI or Unix GSSAPI/Kerberos |
| AccessToken (JWT) | ✅ | Native | Implemented | Pre-acquired bearer token via `SQL_COPT_SS_ACCESS_TOKEN` |
| ActiveDirectoryServicePrincipal | ✅ | Native | Implemented | Client ID + secret |
| ActiveDirectoryManagedIdentity | ✅ | Access token | Implemented | System- or user-assigned identity |
| ActiveDirectoryInteractive | ✅ | Native on Windows, access token elsewhere | Windows only | Browser sign-in; non-Windows resolves to Integrated and returns `HYC00` |
| ActiveDirectoryPassword | ✅ | Native | Out of scope (`HYC00`) | ROPC sends plaintext credentials to Entra, supports neither MFA nor conditional access, and is deprecated by the Microsoft identity platform. Excluded by signed-off design deviation (AB#45486) |
| ActiveDirectoryDeviceCodeFlow | ✅ | Access token | `HYC00` | Not an msodbcsql18 keyword. mssql-python maps the `ActiveDirectoryDeviceCode` spelling and acquires the token itself; the `...Flow` spelling it does not map passes through and reaches this refusal |
| ActiveDirectoryDefault | ✅ | Access token | `HYC00` | Not an msodbcsql18 keyword. mssql-python maps it and acquires the token itself, so it never sends the keyword; this driver recognizes it, so another ODBC consumer can still reach this refusal |
| ActiveDirectoryIntegrated | ✅ | Native | `HYC00` | Entra with the current user's Kerberos ticket |
| ActiveDirectoryWorkloadIdentity | ✅ | Pass-through | `HYC00` | Not an msodbcsql18 keyword and not mapped by mssql-python, so the keyword survives into the connection string and reaches this driver, which recognizes and validates it before refusing. Reachable but unsupported; implementing it would exceed parity |

`ActiveDirectoryMSI` is accepted as a connection-string alias and resolves to
`ActiveDirectoryManagedIdentity`; mssql-tds has no separate MSI workflow.

msodbcsql18 accepts exactly six `Authentication=` values (`dlgattr.h`):
`SqlPassword`, `ActiveDirectoryIntegrated`, `ActiveDirectoryPassword`,
`ActiveDirectoryInteractive`, `ActiveDirectoryMSI`, and
`ActiveDirectoryServicePrincipal`. `ActiveDirectoryDefault`,
`ActiveDirectoryDeviceCode`, and `ActiveDirectoryWorkloadIdentity` are not
driver keywords at all — mssql-python implements the first two in Python.

**Summary**: `mssql-odbc` currently wires six methods — SQL password, SSPI,
access token, service principal, managed identity, and Windows interactive.
AB#45484 closed against exactly that scope (T0–T3). Measured against msodbcsql18
there are only two real gaps: `ActiveDirectoryPassword`, an accepted
out-of-scope deviation (AB#45486) and a known regression for the pass-through
case; and `ActiveDirectoryIntegrated`, tracked by AB#46068 and still open.
Non-Windows interactive was cut (AB#46683).

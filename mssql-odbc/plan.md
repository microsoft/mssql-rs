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
│  C API exports, panic catching, pointer validation  │
├─────────────────────────────────────────────────────┤
│  ODBC Layer  (api/, handles/, state/, etc.)          │
│  Handles, state machines, type conversions,         │
│  diagnostics, catalog, connection str parsing etc.  │
├─────────────────────────────────────────────────────┤
│  TDS Layer  (mssql-tds — local path dependency)     │
│  TDS protocol, authentication, token parsing,       │
│  TLS, Azure AD, SSPI/Kerberos                       │
└─────────────────────────────────────────────────────┘
```

**No layer skipping**: FFI calls ODBC layer, ODBC layer calls TDS layer. Type boundaries are enforced at each level.

### Crate Structure

Today this lives as a single crate (`mssql-odbc/`) producing the cdylib. Internal separation is via modules:

```
mssql-odbc/
├── src/
│   ├── lib.rs             # cdylib root — declares top-level modules
│   ├── api/               # ODBC API layer
│   │   ├── exports.rs     # All #[unsafe(no_mangle)] pub extern "C" symbols (driver export manifest)
│   │   ├── alloc_handle.rs # SQLAllocHandle implementation
│   │   └── odbc_types.rs  # ODBC C type aliases (SqlHandle, SqlReturn, etc.) and constants
│   ├── handles/           # Env, Connection, Statement, Descriptor handles
│   ├── state/             # State machines, HY010 enforcement (planned)
│   ├── types/             # SQL_C_* ↔ SQL_* type conversions (planned)
│   ├── catalog/           # SQLTables, SQLColumns, etc. (planned)
│   ├── diagnostics/       # SQLGetDiagRec, SQLSTATE mapping (planned)
│   ├── platform_windows/  # DTC, DSN config GUI (cfg(windows)) (planned)
│   └── platform_unix/     # Placeholder (cfg(unix)) (planned)
├── tests/                 # Integration / E2E tests via ODBC Driver Manager
├── benches/               # criterion benchmarks
├── build.rs               # Platform-specific linker config, .def/.map exports
└── Cargo.toml
```

If the crate grows large enough to warrant splitting (e.g. separating FFI from logic), that restructuring is a Phase 1 task.

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

The project is organized into 14 phases grouped by priority. We start with foundational infrastructure and the MVP, then layer on data types, authentication, encryption, and advanced features.

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
- SQLPrepare / SQLExecute with `sp_executesql` approach
- SQLBindParameter for common types (INT, VARCHAR, NVARCHAR, DATE, NULL)
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
- Async execution: SQLSetStmtAttr(SQL_ATTR_ASYNC_ENABLE), polling via repeated SQLExecDirect calls returning SQL_STILL_EXECUTING
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
      │         │         └───► Phase 14: MARS
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

Phases **6, 7, 8, 10, 11, and 13** can proceed in parallel once Phase 3 is complete. Phase 9 (Always Encrypted) requires Phases 4 and 5. Phase 11 (Catalog) can ship incrementally — each function is independent. Phase 12 cross-cutting items can be picked up alongside any phase. Phases 13 (BCP) and 14 (MARS) are deferred — not required for initial releases.

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

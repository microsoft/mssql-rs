# Implementation Plan — Typed & Columnar Fetch (mssql-odbc)

Tracking: ADO User Story [46375](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46375) (`mssql-odbc | Typed & columnar fetch`), under Feature [42845](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/42845).

## Goal

Make the Rust `mssql-odbc` driver (a drop-in replacement for `msodbcsql18` that wraps `mssql-tds`) implement the result-fetch ODBC ABI that the `mssql-python` C++ pybind layer calls, so `mssql-odbc` can replace the bundled `msodbcsql18` underneath `mssql-python`.

Reference: `rust_odbc_for_python_driver.docx` §4.5.1 (fetch type map) and §4.8 (batch fetch / insert).

## The real consumer / contract

- `mssql-python` is a C++ pybind layer (`mssql_python/pybind/ddbc_bindings.cpp`) that dynamically loads an ODBC driver and exposes DB-API 2.0. It is **not** built on `mssql-py-core` (the separate pure-Rust pyo3 driver on `mssql-tds`).
- The exact fetch behavior `mssql-odbc` must provide is defined by what `ddbc_bindings.cpp` calls; `msodbcsql` is only the behavioral reference.
- Driver load in `ddbc_bindings.cpp` **requires** these function pointers to be non-null or it aborts: `SQLFetchScroll`, `SQLGetData`, `SQLNumResultCols`, `SQLBindCol`, `SQLDescribeColW`, `SQLMoreResults`, `SQLColAttributeW`, `SQLSetStmtAttrW`. Several of these are missing today, so `mssql-odbc` would not even load under `mssql-python` before this work.

## Two fetch paths mssql-python drives

1. **Columnar / bound** (`fetchmany` / `fetchall`, §4.8): `SQLSetStmtAttr(SQL_ATTR_ROW_ARRAY_SIZE = N)` + `SQL_ATTR_ROWS_FETCHED_PTR`, then `SQLBindCol` per column into typed C arrays (column-wise, default `SQL_BIND_BY_COLUMN`), then one `SQLFetchScroll(SQL_FETCH_NEXT)` per block. Forward-only, read-only. It calls `SQLFreeStmt(SQL_UNBIND)` before each fetch.
2. **Row-by-row** (`fetchone`, LOB, `sql_variant`, §4.7): `SQLFetch` + typed `SQLGetData` per column.

Both paths must share one conversion core: `ColumnValues -> requested SQL_C_* target`.

## Fetch type map (§4.5.1) — SQL type → C type mssql-python requests

| SQL type | C type | Python |
| --- | --- | --- |
| `CHAR`, `VARCHAR`, `LONGVARCHAR` | `SQL_C_WCHAR` (default) or `SQL_C_CHAR` | `str` |
| `WCHAR`, `WVARCHAR`, `WLONGVARCHAR` | `SQL_C_WCHAR` | `str` |
| `SS_XML` | `SQL_C_WCHAR` (streamed) | `str` |
| `TINYINT` | `SQL_C_TINYINT` | `int` |
| `SMALLINT` | `SQL_C_SSHORT` | `int` |
| `INTEGER` | `SQL_C_SLONG` | `int` |
| `BIGINT` | `SQL_C_SBIGINT` | `int` |
| `BIT` | `SQL_C_BIT` | `bool` |
| `REAL` | `SQL_C_FLOAT` | `float` |
| `FLOAT`, `DOUBLE` | `SQL_C_DOUBLE` | `float` |
| `DECIMAL`, `NUMERIC` | `SQL_C_CHAR` (parsed) | `decimal.Decimal` |
| `TYPE_DATE` | `SQL_C_TYPE_DATE` | `datetime.date` |
| `TYPE_TIME`, `SS_TIME2` | `SQL_C_SS_TIME2` | `datetime.time` |
| `TIMESTAMP`, `TYPE_TIMESTAMP`, `DATETIME` | `SQL_C_TYPE_TIMESTAMP` | `datetime.datetime` |
| `SS_TIMESTAMPOFFSET` | `SQL_C_SS_TIMESTAMPOFFSET` | `datetime` (tz-aware) |
| `BINARY`, `VARBINARY`, `LONGVARBINARY`, `SS_UDT` | `SQL_C_BINARY` | `bytes` |
| `GUID` | `SQL_C_GUID` | `uuid.UUID` |
| `SS_VARIANT` | probe via `SQL_C_BINARY`, then map to underlying type | base type |

### sql_variant handling

`ddbc_bindings.cpp` first calls `SQLGetData(col, SQL_C_BINARY, NULL, 0, &ind)` as a probe (detects NULL and initializes variant metadata), then `SQLColAttribute(col, SQL_CA_SS_VARIANT_TYPE, ..., &ctype)`. So `SQLColAttributeW` **must** support `SQL_CA_SS_VARIANT_TYPE` (`1215`) — it is the only `SQLColAttribute` field `mssql-python` uses for fetch.

### Relevant SQL Server constants

`SQL_SS_XML = -152`, `SQL_SS_UDT = -151`, `SQL_SS_VARIANT = -150`, `SQL_SS_TIME2 = -154`, `SQL_SS_TIMESTAMPOFFSET = -155`, `SQL_CA_SS_VARIANT_TYPE = 1215`, `SQL_C_SS_TIME2 = 0x4000`, `SQL_C_SS_TIMESTAMPOFFSET = 0x4001`. Note: `mssql-python` does not set `SQL_ATTR_ROW_STATUS_PTR`; it relies on `SQL_ATTR_ROWS_FETCHED_PTR` plus per-column indicators.

## Starting state (before this work)

- `SQLGetData` (`get_data.rs`): only `SQL_C_CHAR` / `SQL_C_WCHAR`, text conversion of a **subset** of `ColumnValues` (`TinyInt`, `SmallInt`, `Int`, `BigInt`, `Real`, `Float`, `Bit`, `String`, `Uuid`). Missing: `Decimal`/`Numeric`, all date/time types, `Bytes`, `Money`/`SmallMoney`, `Xml`, `Json`, `Vector`. No chunked-offset streaming (repeated calls return the same prefix).
- `SQLFetch` (`fetch.rs`): row-by-row firehose only (`client.next_row` via `block_on`), stores `stmt_state.current_row`.
- `SQLBindCol`, `SQLFetchScroll`, `SQLColAttributeW`: not implemented / not exported.
- `SQLSetStmtAttrW` / `SQLGetStmtAttrW`: no-op stubs returning `SQL_SUCCESS`, so `SQL_ATTR_ROW_ARRAY_SIZE` / `ROWS_FETCHED_PTR` / `PARAMSET_SIZE` were ignored.
- `SQLDescribeColW` (`describe_col.rs`): implemented, with type mapping + column size / decimal digits.

## Phased plan

### P0 — Prerequisites & plumbing — Task [46577](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46577)

- Add all `SQL_C_*` constants + SQL Server extension type ids and the C interop structs (`SQL_DATE_STRUCT`, `SQL_TIME_STRUCT`, `SQL_TIMESTAMP_STRUCT`, `SQL_SS_TIME2_STRUCT`, `SQL_SS_TIMESTAMPOFFSET_STRUCT`, `SQLGUID`, `SQL_NUMERIC_STRUCT`) to `api/odbc_types.rs`.
- Extend `StmtState` (`handles/stmt.rs`) with the block-fetch controls: `row_array_size`, `rows_fetched_ptr`, `row_status_ptr`, `row_bind_type`. (The column-bindings vector and rowset buffer land with P3, and the per-column `SQLGetData` offset with P1, where they are consumed.)
- Implement real `SQLSetStmtAttrW` / `SQLGetStmtAttrW` that honor the rowset controls and accept cursor / concurrency / param-set attributes as no-ops (the driver is forward-only, read-only — exactly what `mssql-python` requests).
- Covered by unit tests only (safe-core logic at ~97% line coverage). E2e tests are intentionally deferred to P1: the P0 statement attributes are ARD/APD descriptor-backed and intercepted by the unixODBC Driver Manager, so they are not meaningfully observable through the DM until a fetch path consumes them.

### P1 — Typed SQLGetData (row-by-row) — Task [46578](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46578)

- Build the shared `ColumnValues -> requested SQL_C_*` conversion core (reused by `SQLBindCol` in P3).
- Start with int types (existing child task [46404](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46404)) and int→char/wchar (existing child task [46405](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46405)), then floats, decimal/numeric (→ `SQL_C_CHAR`), strings, binary, guid, date/time/timestamp/time2/timestampoffset, money, xml, json/vector.
- Add chunked-offset streaming so repeated `SQLGetData` calls advance (`01004` + `SQL_SUCCESS_WITH_INFO` reporting remaining length).
- Implement `sql_variant` probe semantics (`SQL_C_BINARY` NULL detection + variant metadata init).
- Add the e2e coverage deferred from P0: a `set_stmt_attr_test.cpp` (parity with `set_env_attr_test.cpp`) plus a live-connection test that drives typed `SQLGetData` through the Driver Manager. P0's statement attributes (`SQL_ATTR_ROW_ARRAY_SIZE`, etc.) are descriptor-backed and intercepted by unixODBC, so they are only meaningfully observable end-to-end once a fetch path consumes them here (block fetch lands in P3).

### P2 — SQLColAttributeW — Task [46579](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46579)

- Required minimum: `SQL_CA_SS_VARIANT_TYPE` so the `sql_variant` underlying C type resolves after the `SQL_C_BINARY` probe.
- Plus common descriptor fields (type / concise type, length, octet length, precision, scale, name, unsigned, nullable, display size) reusing the `SQLDescribeColW` metadata mapping.
- Export `SQLColAttributeW` (driver-load requires the pointer non-null).

### P3 — SQLBindCol + block SQLFetchScroll — Task [46580](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46580)

- `SQLBindCol`: store per-column binding (col, target C type, buffer ptr, buffer len, indicator ptr); support unbind (null ptr) and `SQLFreeStmt(SQL_UNBIND)`.
- `SQLFetchScroll(SQL_FETCH_NEXT)`: fetch up to `row_array_size` rows into a rowset; fill each bound-column array + indicator array (default column-wise); set `*rows_fetched_ptr`; return `SQL_NO_DATA` at end with partial-rowset handling. Forward-only.
- Reuse the P1 conversion core. Ensure `SQLGetData` still works after a bound fetch (mixed access).
- Export `SQLBindCol` and `SQLFetchScroll`.

### P4 — Exports & driver-load compatibility — Task [46581](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46581)

- Export `SQLBindCol`, `SQLFetchScroll`, `SQLColAttributeW` (exact names incl. the `W` variant) so `ddbc_bindings.cpp` `GetFunctionPointer` succeeds.
- Verify the full required symbol set is present and the driver loads under `mssql-python`.

### P5 — Testing & end-to-end validation — Task [46582](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46582)

- Unit tests per conversion (`ColumnValues` → each `SQL_C_*` target; NULL / indicator / truncation).
- Integration tests against SQL Server (Docker) exercising every type via the ODBC C ABI, both bound (`SQLFetchScroll`) and row-by-row (`SQLGetData`) paths.
- End-to-end: run the `mssql-python` test suite against a locally-swapped `mssql-odbc` build.

## Scope boundary — batch insert

§4.8 also covers **batch insert** (`executemany` via `SQL_ATTR_PARAMSET_SIZE` array binding), which is a **write** path and out of scope for this story. It is tracked separately as User Story [46576](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46576) (`mssql-odbc | Batch insert (executemany array binding)`), with dependencies on Parameter completeness ([46373](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46373)), Descriptors ([46374](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46374)), Connection & statement attributes ([46377](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46377)), and Streaming ([46378](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46378)).

## Status

| Phase | Task | State |
| --- | --- | --- |
| P0 — Prerequisites & plumbing | 46577 | Implemented (build + clippy clean, 332 tests pass) |
| P1 — Typed SQLGetData | 46578 | Not started |
| P2 — SQLColAttributeW | 46579 | Not started |
| P3 — SQLBindCol + SQLFetchScroll | 46580 | Not started |
| P4 — Exports & driver-load compat | 46581 | Not started |
| P5 — Testing & end-to-end | 46582 | Not started |

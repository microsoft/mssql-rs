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
- Implement real `SQLSetStmtAttrW` / `SQLGetStmtAttrW` that honor the rowset controls. `SQL_ATTR_CURSOR_TYPE` / `SQL_ATTR_CONCURRENCY` accept only the supported forward-only / read-only values and substitute+warn (`01S02`) otherwise; `SQL_ATTR_PARAMSET_SIZE` accepts 1 and rejects larger batches; unknown identifiers fail with `HY092`. `SQL_ATTR_APP_PARAM_DESC` requires a descriptor subsystem and is deferred (see scope boundary below).
- Covered by unit tests only (safe-core logic at ~97% line coverage). E2e tests are intentionally deferred to P1: the P0 statement attributes are ARD/APD descriptor-backed and intercepted by the unixODBC Driver Manager, so they are not meaningfully observable through the DM until a fetch path consumes them.

### P1 — Typed SQLGetData (row-by-row) — Task [46578](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46578)

- Build the shared `ColumnValues -> requested SQL_C_*` conversion core (reused by `SQLBindCol` in P3).
- Start with int types (existing child task [46404](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46404)) and int→char/wchar (existing child task [46405](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46405)), then floats, decimal/numeric (→ `SQL_C_CHAR`), strings, binary, guid, date/time/timestamp/time2/timestampoffset, money, xml, json/vector.
- Add chunked-offset streaming so repeated `SQLGetData` calls advance (`01004` + `SQL_SUCCESS_WITH_INFO` reporting remaining length). **Moved out of P1** — chunked retrieval and incremental PLP streaming are owned by the fetch rework in [#153](https://github.com/microsoft/mssql-rs/pull/153) (column-wise fetch + incremental PLP), which uses ODBC wire-stream state rather than an offset over a materialized value. P1 returns each value in a single call and reports truncation with `01004`.
- Implement `sql_variant` probe semantics (`SQL_C_BINARY` NULL detection + variant metadata init).
- **Binary targets regressed out of P1 and are not implemented.** `SQL_C_BINARY` was implemented alongside the chunked-offset streaming, so removing that streaming (ceding it to [#153](https://github.com/microsoft/mssql-rs/pull/153)) took `SQL_C_BINARY` with it; the target gate in `write_captured_column` now rejects it with `HYC00`. Separately, binary → character (hex rendering) has never been implemented in any phase: `ColumnValues::Bytes` has no `column_value_to_text` arm. Note that the e2e test `GetDataLiveTest.UnsupportedColumnTypeHyc00PreservesValue` is currently anchored on `VARBINARY(8)` being unconvertible, so implementing binary → char requires re-pointing it (see the maintenance note in that test).
- Add the e2e coverage deferred from P0: a `set_stmt_attr_test.cpp` (parity with `set_env_attr_test.cpp`) plus a live-connection test that drives typed `SQLGetData` through the Driver Manager. P0's statement attributes (`SQL_ATTR_ROW_ARRAY_SIZE`, etc.) are descriptor-backed and intercepted by unixODBC, so they are only meaningfully observable end-to-end once a fetch path consumes them here (block fetch lands in P3).

### P1a — Mandatory source-type conversions — Task [47107](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47107)

ODBC Appendix D requires a driver to support conversions to **all** ODBC C types from every SQL type it supports. P1 implemented the integer, floating-point, GUID and date/time targets, but only from a subset of sources. P1a added the missing source types, delivered in PR #217:

- `decimal` / `numeric` → the numeric C targets (`SQL_C_DOUBLE`, `SQL_C_FLOAT`, `SQL_C_SLONG`, `SQL_C_SBIGINT`, …). A `NumericSource` abstraction keeps the exact-decimal types exact instead of routing them through `f64`, so an integer target can report truncation rather than silently dropping a fraction.
- `money` / `smallmoney` → the numeric C targets, from their 10^4-scaled wire value.
- Character sources (`char` / `varchar` / `nchar` / `nvarchar`) → numeric and date/time C targets (`'123'` → `SQL_C_SLONG`, `'2023-06-15'` → `SQL_C_TYPE_DATE`). Decimal literals parse exactly, with an `f64` fallback for exponent forms; the `date` / `time` / `datetime2` / `datetimeoffset` character forms are all accepted. Text that is not a valid literal for the requested target returns `22018`, including a literal that parses as a different temporal shape (`'12:00'` into `SQL_C_TYPE_DATE`) and impossible calendar dates (`'2023-02-31'`).
- Lossy **numeric** conversions report fractional truncation with `01S07` + `SQL_SUCCESS_WITH_INFO` (`float` `1234.99` → `SQL_C_SLONG` yields `1234` + `01S07`), reusing the `ConvOk::Truncated` plumbing P1 introduced for date/time targets that discard a component.

A source with no interpretation for the requested target (binary, guid) is `07006`, since that pairing is illegal rather than unimplemented.

`tinyint` → `SQL_C_TINYINT` moves the byte, so a column value above 127 arrives intact rather than as `22003`. `sqlext.h` gives `SQL_C_TINYINT` no sign offset, and a same-width transfer has no narrowing to range-check; msodbcsql usually does not reach its converter at all here, since `sqlcdata.cpp` maps `SQLINT1` to `SQL_C_UTINYINT` and clears `fConvNeeded`. Every other source narrows against the `SCHAR` bounds, so a character or decimal value above 127 is still `22003`, as is any source into `SQL_C_STINYINT`. Pinned by `TinyintColumnAbove127FetchesIntoTinyintCType`, which compares on both legs of the parity run. The parameter direction reaches the same rule by a different route and is described in `parameters_plan.md`.

Max-length character sources (`varchar(max)` / `nvarchar(max)`) into the numeric and date/time targets are **excluded** from P1a and tracked as Task [47238](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47238). They arrive as PLP, so parsing needs the ODBC layer to accumulate chunks, which inverts the "never buffer the full PLP payload" invariant that `stream_active_plp_chunk` documents. That work is sequenced after #204 and #215, which are both rewriting the same read path, and needs a bounded-prefix policy agreed first so a 2 GB column cannot be drained to produce a `SQL_C_SLONG`.

#### Known divergences from msodbcsql

These were found by reading `Sql/Ntdbms/sqlncli/odbc/sqlccnvt.cpp` while reviewing P1a. They are recorded here because `GetDataLiveTest` skips the msodbcsql comparison leg for these cases, so the parity run will not surface them.

| Case | msodbcsql | mssql-odbc | Status |
| --- | --- | --- | --- |
| A UTC offset in a literal, for any target other than `SQL_C_SS_TIMESTAMPOFFSET` | shifts the value into the client's local zone (`ConvertOffsetToLocal`) | validates the offset, then delivers the wall-clock fields as written | **Deliberate.** Matching would make the returned value depend on the client machine's time zone. Locked in by `offset_is_ignored_for_non_offset_targets`. |
| `YYYY/MM/DD` and the ODBC escape literals `{d '...'}` / `{t '...'}` / `{ts '...'}` | accepted (`rgbECODE_DATE_SLASH` retry, and the `FindECode` branch) | `22018` | Gap — Task [47246](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47246). |
| `T` separator, `HH:MM` without seconds, unpadded fields such as `2023-6-5` | rejected (fixed-length token grammar) | accepted | Permissive. Low risk, same task. |
| A time-only value into `SQL_C_TYPE_TIMESTAMP` | fills in the current date and succeeds, per Appendix D | `22018` from a character source, `07006` from a `time` column | Gap — Task [47247](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47247). Needs a platform-specific local-date helper, so it is not a one-line fix. |
| Any source into `SQL_C_NUMERIC` | converts, per Appendix D | `HYC00` | **Deliberate, and permanent.** Decimal is delivered as character data, which is what mssql-python requests, so `SQL_NUMERIC_STRUCT` is not scheduled to become supported. Anchored by `UnsupportedCTypeReturnsHyc00ThenValueReadable`. |
| `SQL_CA_SS_VARIANT_TYPE` for a variant holding `decimal` / `numeric` / `money` | `SQL_C_NUMERIC` | `SQL_C_CHAR` | **Deliberate**, and follows from the row above: reporting `SQL_C_NUMERIC` would make the caller request a `SQL_NUMERIC_STRUCT` this driver refuses. Character is how those values are actually delivered. |
| `SQL_CA_SS_VARIANT_TYPE` on a column that is not `sql_variant` | `SQL_SUCCESS` | `HY113` | **Deliberate.** msodbcsql prepares `IDS_S1_113` and then `break`s without returning it, where the adjacent `SQL_CA_SS_VARIANT_SERVER_TYPE` case does `SETRC_SERR_GOTO` with the same error — so its success looks like an oversight rather than a contract. Telling the caller it asked the wrong question is more useful than answering it. |
| `SQL_DESC_DATETIME_INTERVAL_CODE` through `SQLColAttribute` | rejected — the field is not in the `GetIRDField` switch | `SQL_CODE_TIMESTAMP` for the `datetime`/`smalldatetime`/`datetime2` family, `0` otherwise | **Deliberate**, and additive. Having collapsed `SQL_DESC_TYPE` to the verbose `SQL_DATETIME` to match msodbcsql, refusing to say which member it was leaves the caller with strictly less information than before. Anchored by `DatetimeSubtypeAccompaniesTheVerboseType`, which skips the comparison leg. |

##### Descriptor fields verified against msodbcsql

The per-type tables behind `SQL_DESC_DISPLAY_SIZE`, `SQL_DESC_OCTET_LENGTH`, `SQL_DESC_PRECISION`, `SQL_DESC_UNSIGNED` and `SQL_DESC_SEARCHABLE` were taken from `GetIRDField` in `Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp` and are asserted on *both* legs of the parity run, so they are checked against the real driver rather than against a reading of it. Three are easy to get wrong:

- **Display size is not the column size.** An `int` is 11 (sign plus ten digits), a GUID is 36, binary is two hex characters per byte, and the national character types report characters while their octet length reports bytes.
- **Octet length is the ODBC transfer size, not the TDS payload width.** A `date` is 3 bytes on the wire but transfers as a 6-byte `SQL_DATE_STRUCT`; `time` is 12, the `datetime` family 16, and `datetimeoffset` 20. Reporting the wire width would have callers allocate short.
- **`SQL_DESC_UNSIGNED` keys off the ODBC type, not the TDS type.** msodbcsql's `IsUnsigned()` is a bitmask over the *SQL* type, so it is `SQL_FALSE` only for the signed numerics and `SQL_TRUE` for every nonnumeric column. `money` therefore comes out **signed**, because it is reported as `SQL_DECIMAL` — which is the one case a TDS-type-based implementation gets backwards.

### P2 — SQLColAttributeW — Task [46579](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46579)

- Required minimum: `SQL_CA_SS_VARIANT_TYPE` so the `sql_variant` underlying C type resolves after the `SQL_C_BINARY` probe.
- Plus common descriptor fields (type / concise type, length, octet length, precision, scale, name, unsigned, nullable, display size) reusing the `SQLDescribeColW` metadata mapping.
- Export `SQLColAttributeW` (driver-load requires the pointer non-null).

Reading a `sql_variant` column takes three things, not one, and mssql-python needs all of them before it will produce a value — on any failure it logs and yields `None` for the column, so a missing link shows up as silently empty data rather than an error:

1. `SQLDescribeCol` must report `SQL_SS_VARIANT`. mssql-python branches on that exact type; while the column was reported as `SQL_VARCHAR` it never entered the variant path at all.
2. `SQLGetData(col, SQL_C_BINARY, NULL, 0, &indicator)` must succeed. This is a length/NULL probe, not a data read; it is admitted while binary delivery stays unimplemented (a real buffer is still `HYC00`, tracked as Task [47239](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47239)).
3. `SQLColAttribute(SQL_CA_SS_VARIANT_TYPE)` returns the C type of the value just probed.

The underlying type is a property of the **value**, not the column — a variant column can hold a different type in every row — so it is carried up from the decoder rather than derived from metadata: `RowWriter` gained a defaulted `write_variant_base_type`, `CursorColumn::Value` carries the base type alongside the value, and `StmtState` clears it with the rest of the row-stream state. `ColumnValues` is deliberately untouched, which is what keeps this change out of the Python and Node bindings.

### P3 — SQLBindCol + block SQLFetchScroll — Task [46580](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46580)

- `SQLBindCol`: store per-column binding (col, target C type, buffer ptr, buffer len, indicator ptr); support unbind (null ptr) and `SQLFreeStmt(SQL_UNBIND)`.
- `SQLFetchScroll(SQL_FETCH_NEXT)`: fetch up to `row_array_size` rows into a rowset; fill each bound-column array + indicator array (default column-wise); set `*rows_fetched_ptr`; return `SQL_NO_DATA` at end with partial-rowset handling. Forward-only.
- Reuse the P1 conversion core. Ensure `SQLGetData` still works after a bound fetch (mixed access).
- Export `SQLBindCol` and `SQLFetchScroll`, and advertise both in `SQLGetFunctions` — the Windows DM returns `IM001` for an entry point it has not been told about even when the export exists, which cost P2 a CI cycle.

`SQLFreeStmt(SQL_UNBIND)` is part of this, not an extra: `mssql-python` calls it before every fetch, so the columnar path does not work without it.

#### Deliberate limits

Each of these is an error rather than a guess, because a wrong value delivered silently into an application buffer is worse than a reported failure.

| Case | Behaviour | Why |
| --- | --- | --- |
| A scrolling `FetchOrientation` | `HY106` | The cursor is forward-only; treating `SQL_FETCH_PRIOR` as `NEXT` would return the wrong rows |
| Row-wise binding (`SQL_ATTR_ROW_BIND_TYPE` ≠ `SQL_BIND_BY_COLUMN`) | `HYC00` | Filling a struct array as if it were column-wise would corrupt application memory |
| A bound PLP / LOB column | `SQL_ROW_ERROR` | Draining a LOB into a fixed buffer needs machinery `SQLGetData` owns — Task [47361](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47361) |
| NULL at a column bound with no indicator | `22002`, `SQL_ROW_ERROR` | There is nowhere to report the NULL, and the slot would read back as the previous row's value |
| `SQLGetData` after a rowset wider than one row | cursor left unpositioned | ODBC expects `SQLSetPos` to nominate the current row, and that is not implemented; mixed access still works at `row_array_size` 1 |
| `SQL_C_DEFAULT` at bind time | `HY003` | **Divergence from msodbcsql**, which accepts it and resolves it at fetch time from the IRD (`sqlcfunc.cpp` `BindOffset` → `Sql2CDefault`). Deferring needs the column's SQL type threaded into the fill loop, which `ColumnBinding` does not carry. No known consumer binds `SQL_C_DEFAULT` on the fetch path — mssql-python uses it only for parameters — so this is deferred rather than blocking |

Two details worth keeping in mind when extending this:

- **`BufferLength` is ignored for fixed-width C targets.** The stride comes from the C type; only the character and binary targets are sized by the application. Honouring a caller's `sizeof(array)` would place later rows outside it.
- **`SQL_ATTR_ROW_BIND_OFFSET_PTR` is read per fetch, not per bind,** and displaces the data *and* indicator bases by the same byte count — so the offset has to keep both naturally aligned.

### P4 — Exports & driver-load compatibility — Task [46581](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46581)

- Export `SQLBindCol`, `SQLFetchScroll`, `SQLColAttributeW` (exact names incl. the `W` variant) so `ddbc_bindings.cpp` `GetFunctionPointer` succeeds.
- Verify the full required symbol set is present and the driver loads under `mssql-python`.

### P5 — Testing & end-to-end validation — Task [46582](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46582)

- Unit tests per conversion (`ColumnValues` → each `SQL_C_*` target; NULL / indicator / truncation).
- Integration tests against SQL Server (Docker) exercising every type via the ODBC C ABI, both bound (`SQLFetchScroll`) and row-by-row (`SQLGetData`) paths.
- End-to-end: run the `mssql-python` test suite against a locally-swapped `mssql-odbc` build.

## Scope boundary — batch insert

§4.8 also covers **batch insert** (`executemany` via `SQL_ATTR_PARAMSET_SIZE` array binding), which is a **write** path and out of scope for this story. It is tracked separately as User Story [46576](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46576) (`mssql-odbc | Batch insert (executemany array binding)`), with dependencies on Parameter completeness ([46373](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46373)), Descriptors ([46374](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46374)), Connection & statement attributes ([46377](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46377)), and Streaming ([46378](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46378)).

## Scope boundary — descriptors (`SQL_ATTR_APP_PARAM_DESC` / `SQL_C_NUMERIC` input binding)

Descriptor handles are not implemented in the crate yet, so `SQLGetStmtAttrW(SQL_ATTR_APP_PARAM_DESC)` currently returns `HY092`. `mssql-python`'s `ddbc_bindings.cpp` calls this (then `SQLSetDescField` on the returned handle) only when binding a `SQL_C_NUMERIC` **input parameter**, and aborts the bind if it fails — so numeric input-parameter binding will not work until a descriptor subsystem exists. There is no correct minimal shim (a non-null fake handle would crash `SQLSetDescField`). This is deferred to the Descriptors work item [46374](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46374) (APD handle alloc/free, `SQLGetStmtAttr` returning it, and `SQLSetDescField` support for `SQL_DESC_TYPE`/`SQL_DESC_CONCISE_TYPE`/precision/scale), rather than expanding this P0 plumbing PR.

## Scope boundary — chunked retrieval and incremental PLP (LOB) streaming

`SQLGetData` in P1 returns each value in a single call from the materialized `ColumnValues` that `TdsClient::next_row` produces, reporting truncation with `01004`. It does **not** advance a per-call offset, and it does not stream LOBs off the wire.

Both of those landed with the fetch rework in [#153](https://github.com/microsoft/mssql-rs/pull/153) (column-wise fetch + incremental PLP support), which carries ODBC wire-stream state and builds on the PLP reader added in [#109](https://github.com/microsoft/mssql-rs/pull/109) (`PlpChunkStreamReader`, `receive_row_into` / `resume_row_into` / `read_active_plp_bytes`). Those primitives are `pub(crate)` to `mssql-tds`, and consuming them from the ODBC crate additionally requires a public `TdsClient` streaming API plus an ODBC connection-ownership change (today `SQLFetch` returns the TDS client to the DBC, whereas streaming requires the statement to hold it across `SQLGetData` calls). P1 therefore stays on the conversion layer and leaves the fetch mechanics to #153.

## Status

| Phase | Task | State |
| --- | --- | --- |
| P0 — Prerequisites & plumbing | 46577 | Implemented (build + clippy clean, 332 tests pass) |
| P1 — Typed SQLGetData | 46578 | Implemented (int/float/guid/date-time C targets + char/wchar rendering; 491 tests pass). Chunked retrieval and incremental PLP streaming are owned by #153 (merged), on top of which the typed targets are dispatched; missing source-type conversions tracked as P1a; `SQL_C_BINARY` and binary→char hex are **not** implemented (see the P1 section); `sql_variant` underlying-type resolution deferred to P2. |
| P1a — Mandatory source-type conversions | 47107 | Implemented (decimal, money and character sources into the numeric and date/time C targets; `01S07` on lossy numeric conversion, `22018` on an invalid character literal). |
| P2 — SQLColAttributeW | 46579 | Implemented (common descriptor fields + `SQL_CA_SS_VARIANT_TYPE`, plus the `SQL_SS_VARIANT` type mapping and the zero-length `SQL_C_BINARY` probe the variant path depends on). Binary *delivery* remains unimplemented (Task 47239). |
| P3 — SQLBindCol + SQLFetchScroll | 46580, 47359 | Implemented ([#322](https://github.com/microsoft/mssql-rs/pull/322)). Column-wise binding and forward-only block fetch, sharing P1's conversion core. 47359 was briefly split out to be worked in parallel and folded back in, since the fill loop cannot be exercised end to end without `SQLBindCol`. Bound PLP columns remain unimplemented (Task 47361). |
| P4 — Exports & driver-load compat | 46581 | Partly done — `SQLColAttributeW` exported and advertised in P2; `SQLBindCol` and `SQLFetchScroll` in P3. Remaining: confirm the full symbol set loads under `mssql-python`. |
| P5 — Testing & end-to-end | 46582 | Not started |

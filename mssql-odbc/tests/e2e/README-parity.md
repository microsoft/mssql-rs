# mssql-python parity suite

`tests/mssql_python_parity_test.cpp` is a conformance backlog for the ODBC
surface that [mssql-python](https://github.com/microsoft/mssql-python) drives.

It exists because mssql-python does **not** use a Driver Manager. It calls
`LoadLibraryW` on `msodbcsql18.dll` and resolves the entrypoints itself, so this
driver alone owns the entire ODBC contract — there is no Driver Manager to
normalise arguments, sequence calls, or paper over a missing export. The suite
loads the driver the same way, which makes it a faithful stand-in for the Python
integration suite and far faster to run.

## Every case is disabled

The cases describe behaviour the driver is expected to have, ahead of the driver
having it. They all carry the gtest `DISABLED_` prefix, so CI builds and runs the
binary but reports them as skipped, and the build stays green.

**To enable a case, drop its `DISABLED_` prefix in the same pull request that
implements the behaviour.** Two rules:

- Do not enable a case without the corresponding driver work.
- Do not re-disable a case to make CI pass. A case that starts failing is a
  driver regression, not a test problem.

## Running it

```powershell
cmake --build build --config Release
$env:MSSQL_ODBC_DLL  = "<repo>\target\release\msodbcsql18.dll"
$env:MSSQL_ODBC_CONNSTR = "Server=localhost;Database=<db>;Uid=<u>;Pwd=<p>;Encrypt=no;TrustServerCertificate=yes;"

# Enabled cases only (what CI does):
.\build\mssql_python_parity_test.exe

# Everything, to see where the driver actually stands:
.\build\mssql_python_parity_test.exe --gtest_also_run_disabled_tests
```

Windows-only for now: the loader uses `LoadLibraryW`/`GetProcAddress`, and the
fixtures assume a 2-byte `wchar_t` for `SQLWCHAR`. Porting to `dlopen` plus an
explicit UTF-16 string type is future work; `CMakeLists.txt` guards the target
with `if(WIN32)`.

## Cases

| Case | ODBC surface | Notes |
| --- | --- | --- |
| `AutocommitRoundTrips` | `SQLSetConnectAttr`, `SQLGetConnectAttr` | `SQL_ATTR_AUTOCOMMIT` set and read back |
| `ManualCommitPersistsRows` | `SQLEndTran` | |
| `ManualRollbackDiscardsRows` | `SQLEndTran` | |
| `EndTranInAutocommitIsANoOp` | `SQLEndTran` | Must succeed, not error |
| `BlockFetchFillsColumnWiseArrays` | `SQLBindCol`, `SQLFetchScroll` | Row stride comes from `SQL_ATTR_ROW_ARRAY_SIZE`, **not** `BufferLength`, for fixed-width C types |
| `BlockFetchReportsNullIndicators` | `SQLBindCol` | Indicator array must receive `SQL_NULL_DATA` |
| `FetchScrollRejectsNonForwardOrientations` | `SQLFetchScroll` | Forward-only cursor must reject, not crash |
| `SecondCursorRunsWhileFirstIsOpen` | `SQLAllocHandle` | Two live statements on one connection |
| `GetDataConvertsCommonTypes` | `SQLGetData` | tinyint 128–255, money precision, 100ns time ticks, ANSI code page |
| `GetDataReportsNullAndTruncation` | `SQLGetData` | NULL with a null indicator must be `SQL_ERROR` + `22002`; truncation must be `22001` |
| `ColAttributeReportsNameTypeAndCount` | `SQLColAttribute` | `numeric` vs `decimal`, UDT sizes, ODBC 2.x date codes |
| `ColAttributeVariantTypeDoesNotCrash` | `SQLColAttribute` | Driver-specific field 1215 is queried on *every* column |
| `TablesReturnsOdbcShapedResultSet` | `SQLTables` | ODBC 3 column names |
| `ColumnsAndPrimaryKeysSucceed` | `SQLColumns`, `SQLPrimaryKeys` | |
| `ProceduresAndStatisticsSucceed` | `SQLProcedures`, `SQLStatistics` | |
| `BoundParametersRoundTrip` | `SQLBindParameter`, `SQLPrepare`, `SQLExecute` | |
| `DataAtExecutionStreamsParameterInChunks` | `SQLParamData`, `SQLPutData` | `SQL_NEED_DATA` loop; token must round-trip unchanged |
| `GetDataStreamsLobInChunks` | `SQLGetData` | Partial reads must report bytes *remaining*; final call returns `SQL_SUCCESS` |
| `SetDescFieldOnApdSucceedsForNumeric` | `SQLSetDescField` | Precision/scale on the APD for every decimal argument |
| `UnbindClearsPreviousBindings` | `SQLFreeStmt` | |
| `RowCountReportsAffectedRows` | `SQLRowCount` | |
| `MoreResultsWalksMultiStatementBatch` | `SQLMoreResults` | Also surfaces `PRINT` output and deferred errors |

## Suggested order

Roughly the order in which failures block each other:

1. Exports resolve at all — a missing symbol fails `LoadLibrary` binding and every case skips.
2. `BlockFetch*` — a wrong row stride corrupts memory and makes every later result unreliable.
3. `GetDataReportsNullAndTruncation` — silently returning success for NULL yields garbage values rather than errors.
4. `GetDataConvertsCommonTypes` — remaining silent-corruption conversions.
5. `DataAtExecution*`, `BoundParametersRoundTrip`, `SetDescFieldOnApd*` — parameter binding.
6. Catalog cases.
7. `MoreResultsWalksMultiStatementBatch` and diagnostics.

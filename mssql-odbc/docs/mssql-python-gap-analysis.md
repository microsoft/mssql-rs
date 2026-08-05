# mssql-python parity gap analysis

`mssql-odbc` builds `msodbcsql18.dll`, a drop-in replacement for the Microsoft
ODBC Driver 18 for SQL Server. [mssql-python][mssql-python] ships that driver
inside its wheel and loads it directly with `LoadLibraryW`, resolving 38 entry
points by name — there is no Driver Manager in the path, so the driver alone is
responsible for the whole ODBC contract.

That makes mssql-python's integration suite a good conformance test. This
document records what it found.

[mssql-python]: https://github.com/microsoft/mssql-python

## Result

| Driver | Failed | Passed |
| --- | --- | --- |
| `msodbcsql18.dll` (C++, shipped) | 2 | 1930 |
| `msodbcsql18.dll` (this crate), before | 300+, plus two hard crashes | — |
| `msodbcsql18.dll` (this crate), after | 2 | 1929 |

(The counts differ by one because the runs are collected differently: the
baseline was a single pytest invocation, and the parity runs are per-file so
that a crash in one file does not hide the rest.)

The two remaining failures are identical under both drivers:

- `test_015_utf8_path_handling.py::test_very_long_path_component` — exceeds
  Windows `MAX_PATH`; no ODBC call involved.
- `test_019_bulkcopy.py::test_bulkcopy_udt_geometry` — goes through
  mssql-python's own `mssql_py_core` PyO3 extension, which opens its own TDS
  connection and never calls the driver.

The largest single file, `test_004_cursor.py`, went from an access violation
part-way through the run to 518 passed, 0 failed.

## Method

1. Build `mssql-odbc` and copy `msodbcsql18.dll` over the copy in
   `mssql_python/libs/windows/x64/`, keeping the original as `.orig`.
2. Run the suite per file so one crash does not hide the rest of the results.
3. Record a baseline with the original driver, and treat only the delta as a
   gap.
4. Fix, rebuild, re-run the affected file, and confirm the count moved.

Both drivers ran against the same local SQL Server and the same database, over
a SQL login, with `Encrypt=no`.

## Gaps

Each entry below is a distinct defect found by the suite. "Blast radius" is the
number of tests that changed state when it was fixed, which is a better measure
of severity than the defect itself: a small mistake in a hot path fails
hundreds of tests, and a missing feature usually fails one.

### A. Data-at-execution parameters were unimplemented

`SQLParamData` and `SQLPutData` returned "not implemented", so any value the
application chose to stream — which mssql-python does for large text and
binary — could not be sent at all.

Fixed by implementing the full `SQL_NEED_DATA` loop: `SQLExecute` returns
`SQL_NEED_DATA` for each parameter whose indicator is `SQL_DATA_AT_EXEC`, the
application streams the value in chunks, and the statement is executed once the
last parameter is satisfied.

### B. Parameter arrays were ignored

`SQL_ATTR_PARAMSET_SIZE` was accepted and then ignored, so `executemany` sent
only the first row and silently discarded the rest — a data-loss bug, not just
a failure.

Fixed by executing each row of the array in turn and accumulating the row
counts.

### C. `sql_variant` collapsed to a string

Every `sql_variant` value came back as text regardless of the base type stored
in it, so an integer stored in a variant came back as `"1"`.

Fixed by decoding the variant header and returning the base type.

### D. UDT and spatial columns returned hex text

`geometry`, `geography`, and `hierarchyid` were rendered as a hex string rather
than returned as bytes, so they could not be round-tripped.

Fixed by treating UDTs as binary.

### E. Non-Unicode character data was transcoded to UTF-8

`char` and `varchar` data was handed to the application as UTF-8 while
`SQL_C_CHAR` is defined to be in the client's ANSI code page. Every non-ASCII
character in a CP1252 column was corrupted.

Fixed by encoding `SQL_C_CHAR` in the client code page.

### F. Long values reported a negative length

`SQLGetData` on a LOB returned `-1` for the length instead of the remaining
byte count, so chunked reads could not be driven.

Fixed by reporting the true remaining length and `SQL_SUCCESS_WITH_INFO` while
data remains.

### G. Errors used a generic SQLSTATE

Syntax errors and truncations both surfaced as `HY000`, so applications could
not distinguish them. mssql-python maps SQLSTATE to its exception hierarchy, so
every error arrived as the wrong Python exception type.

Fixed by mapping known error numbers to their SQLSTATE and, for unknown
numbers, deriving the class from the server's severity.

**Blast radius: large.** Exception-type assertions appear throughout the suite.

### H. The connection stayed busy after a result set

The connection was released only when the statement was freed, so a second
statement on the same connection failed with "connection is busy" — which is
the normal pattern for a cursor that has been fully read but not closed.

Fixed by releasing the connection once the last result set in a batch reaches
end-of-data.

### I. DDL rollback — not reproducible

The driver only sets `IMPLICIT_TRANSACTIONS ON` in manual-commit mode rather
than beginning an explicit transaction, which was expected to leave DDL
outside the transaction. It does not: SQL Server starts an implicit
transaction on `CREATE TABLE` as well, and a rollback discards it. Verified
directly, and the behaviour matches the reference driver.

### J. The application name was wrong

The driver reported its own name in the TDS login packet, so `APP_NAME()`
returned the driver rather than the application, and `APP=` in the connection
string was ignored.

Fixed by parsing `APP=` and using it as the application name.

### K. `SQL_C_TINYINT` was signed

ODBC's `SQL_C_TINYINT` is unsigned, matching SQL Server's `tinyint`. Reading it
as signed made every value from 128 to 255 fail to bind.

### L. Time values were scaled by 100

The TDS time fields count 100-nanosecond ticks; the code treated them as
nanoseconds. Every `time`, `datetime2`, and `datetimeoffset` value was wrong by
a factor of 100, which for most values overflowed into an invalid time.

**Blast radius: large.** All date/time tests, plus anything using a timestamp
column incidentally.

### M. Not a driver defect

`test_very_long_path_component` exceeds the Windows path limit and fails under
both drivers.

### N. Errors after column metadata were deferred

When a statement produced column metadata and then failed — a constraint
violation on an `INSERT ... OUTPUT`, for example — the error was not raised
until the rows were read, so `execute()` appeared to succeed.

Fixed by draining the token stream far enough to see the error before
returning.

### O. `SQL_ATTR_CURRENT_CATALOG` was unsupported

Reading or setting the current database through the connection attribute
failed, so `conn.setcatalog()` and the reverse did not work.

### P. `numeric` was reported as `decimal`

`SQLDescribeCol` reported `SQL_DECIMAL` for a `numeric` column. The two are
distinct ODBC types and mssql-python surfaces the distinction.

### Q. `SQLGetInfo` returned wrong or missing values

Several information types were unimplemented or returned placeholder values,
including the identifier quote character, the driver name, and the supported
conformance level.

### R. Bound columns were strided by the wrong amount

**This one caused the crashes.** ODBC ignores `BufferLength` for fixed-width C
types, and mssql-python exploits that: when binding a `SQL_C_SS_TIMESTAMPOFFSET`
column it passes the size of the *whole array* as the buffer length. The driver
used that value as the per-row stride and wrote far past the end of the buffer.

Symptoms were non-deterministic heap corruption and access violations, which is
why the first several runs of `test_004_cursor.py` died at a different test each
time.

Fixed by deriving the stride from the C type, and only falling back to
`BufferLength` for variable-length types.

**Blast radius: the entire file.** Everything after the crash point was
unreported.

### S. ODBC 2.x date and time type codes were rejected

`SQL_DATE`, `SQL_TIME`, and `SQL_TIMESTAMP` (9, 10, 11) are the ODBC 2.x
spellings of the 3.x codes 91, 92, and 93. Applications still use them, and the
driver rejected them.

### T. Parameter arrays left a cursor open

Each row of an `executemany` that produced a result set left its cursor open,
so the next row failed the invalid-cursor check. Intermediate cursors are now
drained — but the last row's cursor stays open, because a statement with an
`OUTPUT` clause is expected to leave its rows available.

### U. UDT columns reported a size of zero

`SQLDescribeCol` returned zero for a UDT's column size, so mssql-python treated
every UDT as a LOB and read it through the streaming path.

### V. NULL failed to bind to a binary buffer

`SQLGetData` with `SQL_C_BINARY` on a NULL value returned an error instead of
setting the indicator.

### W. Money lost precision

`money` and `smallmoney` were converted through `f64`, which cannot represent
every value of a 4-decimal fixed-point type exactly.

Fixed by keeping the scaled integer.

### X. `SQLDescribeParam` was a stub

It returned a fixed `SQL_VARCHAR(1)` guess for every parameter. mssql-python
calls it to decide how to bind, so the guess propagated into every binding
decision.

Fixed by calling `sp_describe_undeclared_parameters` and caching the result for
the prepared statement. Note that this procedure cannot see temporary tables or
table variables; mssql-python has its own fallback for that case.

### Y. Catalog functions did not match ODBC 3

Four distinct problems in `SQLTables`, `SQLColumns`, `SQLStatistics`,
`SQLSpecialColumns`, `SQLPrimaryKeys`, `SQLForeignKeys`, and `SQLProcedures`:

1. The underlying system procedures return ODBC 2.x column names
   (`TABLE_QUALIFIER`, `TABLE_OWNER`), and ODBC 3 renamed them
   (`TABLE_CAT`, `TABLE_SCHEM`). mssql-python builds row attributes from the
   driver's column names, so the attribute names were wrong.
2. `sp_statistics` filters on `index_name LIKE @index_name`, which matches
   nothing when the argument is NULL. The reference driver always passes `'%'`.
3. `sp_tables` needs each element of a table type list quoted individually, so
   `TABLE,VIEW` becomes `'TABLE','VIEW'` — the whole list was being quoted as
   one element.
4. A catalog argument naming a database that does not exist must produce an
   empty result set, not an error. The batch is now guarded on `DB_ID`.

**Blast radius: 44 tests.**

### Z. `PRINT` output never reached the application

A `SET @v = ...` assignment emits a row-count-flagged `DONEINPROC` with no
column metadata. Statement-wise navigation treated that as a statement boundary
and stopped, leaving the following `PRINT` message unread — so
`cursor.messages` was empty.

Fixed by recognising an assignment's `DONEINPROC` and continuing past it. DML
carries a different command code, so row counts are unaffected.

### AA. NULL without an indicator must fail

**The highest-value finding.** ODBC requires `SQLGetData` to return `SQL_ERROR`
with SQLSTATE `22002` when the value is NULL and the application passed a null
indicator pointer — there is nowhere to report the NULL, so it is an error.

The driver returned `SQL_SUCCESS` and left the buffer untouched. mssql-python's
`FetchOne_wrap` relies on the documented behaviour for every fixed-width type,
so `fetchone()` on any NULL fixed-width column returned whatever happened to be
on the stack: `0`, `447`, `621`, varying between runs.

`fetchall()` was unaffected because it uses `SQLBindCol` with real indicator
arrays. That split — same query, same column, correct through one API and
garbage through the other — is what identified it.

**Blast radius: 10 tests, and silent data corruption in any application using
`fetchone`.**

### AB. ODBC escape sequences were passed through

`{CALL proc(?)}`, `{fn ...}`, `{ts '...'}`, and the rest are ODBC syntax that
the driver is required to translate. They were sent to the server verbatim,
which answered `Incorrect syntax near '{'`.

Fixed with a quote- and comment-aware translator. Note that `EXEC proc(1)` is
also invalid T-SQL, so a call's parenthesised argument list has to become a
bare argument list: `EXEC proc 1`.

### AC. An error after the first row of a rowset was lost

`INSERT ... OUTPUT` with a duplicate key emits column metadata, one row, and
then the error. The driver returned the row it had with
`SQL_SUCCESS_WITH_INFO`, but had already closed the cursor, so the caller's
next fetch reported `24000 invalid cursor state` instead of the constraint
violation.

Discarding the partial rowset would have been simpler but loses genuine rows
when a `SELECT` fails part-way. The diagnostic is instead held and replayed on
the next fetch.

### AD. NULL decimals were declared too narrow

A NULL carries no precision, so a NULL `decimal` parameter was declared with
the TDS default of `decimal(18, 10)`. In a parameter array the first row's
declaration is baked into the prepared plan and reused for every later row, so
`executemany` with a NULL in the first row rejected any subsequent value wider
than 18 digits.

The bindings are byte-identical whether or not the array contains a NULL, which
is what made this hard to see: the failure depends only on the *order* of the
rows.

Fixed by declaring NULL decimal parameters with the precision and scale the
application bound, falling back to the widest declaration when it bound none.

## Notes for future work

- `sp_describe_undeclared_parameters` cannot see temporary tables or table
  variables. `SQLDescribeParam` therefore cannot describe a parameter of a
  statement that targets one, and applications need their own fallback.
- `SELECT ... INTO #t` reports the same command code as a plain `SELECT` along
  with a row count. The `DONEINPROC` navigation change in gap Z assumes an
  assignment; this case is untested.
- The suite exercises Windows only. The catalog, escape, and conversion fixes
  are platform-independent, but nothing here validates the Linux or macOS
  builds.

# Implementation Plan — Column-wise SQLFetch / SQLGetData (mssql-odbc)

## Background

Commit `c19d2dd` ("Add incremental PLP column streaming and sparse row-column
read APIs") added, in `mssql-tds`, the plumbing for incremental, column-at-a-time
row decoding:

- `RowWriter::pause_after_column(col)` — decoding pauses **after** column `col`.
- `RowWriter::read_active_plp_bytes` / `active_plp_reached_end` /
  `active_plp_collation` — incremental PLP (MAX-type) chunk streaming.
- `ResultSet::next_row_into(writer)` resumes a paused row on each call and only
  fires `end_row()` once the row is fully consumed.

The `mssql-odbc` layer, however, still **materializes the entire row** in
`SQLFetch` (`client.next_row()` → `Vec<ColumnValues>` stored in
`StmtState::current_row`). `SQLGetData` then indexes that vector. That is not the
msodbcsql model, where `SQLFetch` positions on a row without reading any column
and each `SQLGetData(n)` decodes column `n` off the wire on demand, draining the
columns in between.

## Goal

Move `mssql-odbc` to the msodbcsql column-wise model:

1. `SQLFetch` positions on the next row and reads **no** columns.
2. `SQLGetData(n)` decodes/returns column `n`, discarding any un-requested
   columns before it.
3. PLP (`*(MAX)`, `xml`) columns stream through repeated `SQLGetData` calls on
   the same column until the value is exhausted.
4. No full-row `Vec<ColumnValues>` is materialized on the fetch path.

## Design

### 1. mssql-tds: pause **before** the first column

`pause_after_column` cannot express "stop before column 0". Add a new opt-in
hook to `RowWriter`:

```rust
/// Returns `true` to pause row decoding *before* the first column is read.
/// Default `false`. ODBC-style writers return `true` so `SQLFetch` positions
/// on a row without consuming any column.
fn pause_before_first_column(&self) -> bool { false }
```

It is honored only on the **initial** row read (`receive_row_into_internal`),
right after the ROW/NBCROW token (and NBCROW null bitmap) is consumed, returning
`RowReadResult::RowPaused { next_column_index: 0, .. }` without decoding a
column. The resume path (`resume_row_into_internal`) never consults it, so
resuming from a col-0 pause proceeds normally (no infinite pause).

### 2. mssql-odbc: `OdbcRowWriter`

A `RowWriter` (like the test-only `SparseCaptureWriter`) that:

- `pause_before_first_column()` → `true` (always, for the ODBC model).
- `pause_after_column(col)` → `true` when `col == requested` (0-based).
- Captures only the requested column's `ColumnValues`; discards all others.
- Records whether `end_row()` fired (used by `SQLFetch` to tell "finished the
  previously-positioned row" from "positioned on a new row").

### 3. StmtState streaming fields

Replace `current_row: Option<Vec<ColumnValues>>` with:

- `row_active: bool` — a row is positioned for `SQLGetData`.
- `getdata_next_col: usize` — 0-based index of the next not-yet-consumed column.
- `plp_stream_col: Option<SqlUSmallInt>` — 1-based column currently mid-PLP-stream.

`reset_row_stream()` clears all three; called everywhere `current_row = None`
was.

### 4. SQLFetch

Take the client, then loop:

```text
loop:
  w = OdbcRowWriter (position mode, no requested column)
  has = client.next_row_into(w)
  if !has: row_active = false; return SQL_NO_DATA
  if w.end_row_fired(): continue        # finished the previous paused row
  else: break                           # paused before col 0 of a fresh row
row_active = true; getdata_next_col = 0; plp_stream_col = None; return SUCCESS
```

The loop drains any partially-read previous row (its trailing columns) before
positioning on the next one — matching msodbcsql discarding unread columns at
the next fetch. Connection stays busy (cursor open).

### 5. SQLGetData(col)

Let `col0 = col - 1`.

- **Continuing a PLP stream** (`plp_stream_col == Some(col)`): read the next
  chunk via `read_active_plp_bytes`. `SQL_SUCCESS_WITH_INFO` (01004) while more
  remains, `SQL_SUCCESS` on the final chunk; then advance `getdata_next_col` and
  clear `plp_stream_col`.
- **Already consumed** (`col0 < getdata_next_col`, not the active PLP col):
  `SQL_NO_DATA` (column already fully returned).
- **New column**: request `col0` on an `OdbcRowWriter`, `next_row_into` (resumes
  from the current pause point, discarding intervening columns):
  - captured a value → convert to the target C type and return; set
    `getdata_next_col = col0 + 1`.
  - PLP paused (no captured value, row not ended) → begin streaming: read the
    first chunk, set `plp_stream_col = Some(col)` unless the value fit/ended.
  - row ended (`end_row` fired, nothing captured) → `SQL_NO_DATA`.

Connection stays busy throughout; the pause state lives in the `TdsClient`
stored on the DBC.

## Testing

- **tds unit**: `receive_row_into_internal` with a `pause_before_first_column`
  writer returns `RowPaused { next_column_index: 0 }` for ROW and NBCROW.
- **tds e2e** (`test_client_read_apis.rs`): an ODBC-style writer that positions
  before col 0, then reads specific columns and streams a PLP column.
- **odbc unit**: `OdbcRowWriter` capture/discard/pause/end-row semantics; new
  `SQLFetch`/`SQLGetData` state-guard tests. Old full-row-materialization unit
  tests in `get_data.rs` are removed (they asserted `current_row` indexing).
- **e2e (C++)**: `get_data_test.cpp` — `SQLFetch` + column-wise `SQLGetData`,
  out-of-order-forward columns, and a MAX-type streamed via repeated
  `SQLGetData`.

## Non-goals / limitations

- Always Encrypted paused PLP streaming remains unimplemented (fails fast, as
  before).
- `SQLBindCol` is out of scope; only `SQLGetData` retrieval is covered.
- PLP `SQL_C_CHAR` from an `nvarchar(max)` (UTF-16 → UTF-8) transcodes per wire
  chunk; a UTF-16 surrogate pair split across chunks is a known edge limitation.

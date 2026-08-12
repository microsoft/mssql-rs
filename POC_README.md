# TDS row decoding proof of concept

This branch shows five changes to `mssql-tds` that cut the cost of reading rows.

It is a proof of concept. The code quality is not production ready. The intent
is to prove these changes work and to show what each one is worth.

We found these while making a PostgreSQL foreign data wrapper faster. The
wrapper reads large result sets from SQL Server. Past a few hundred thousand
rows the client became the bottleneck, not the server.

## What we measured

One query. 1,499,000 rows, 48 columns each, 39 integers and 9 short strings.

| Measure | Before | After |
| --- | ---: | ---: |
| Wall clock | 7.45 s | 3.92 s |
| CPU on the reading process | 7.35 s | 2.58 s |
| CPU inside `mssql-tds` | 4.69 s | 0.97 s |

The server stopped waiting on us too. Before the change it spent 4.54 seconds
blocked on `ASYNC_NETWORK_IO`. After the change it spent none.

The unit tests pass. Run them with `cargo test -p mssql-tds --lib`.

These numbers were taken before this branch was rebased onto current `main`.
The rebase changed which code runs on the async path, so treat the split
between the groups as indicative. The buffered path itself is unchanged.

## What changed

### 1. Work out each column once

The old path read the type, precision, scale and collation from the column
metadata on every row. None of that changes while a result set is open.

Now it is worked out once and stored as a small plan. See `DecodeOp` in
`mssql-tds/src/token/tokens.rs`.

**Why it is built on the first row.** The plan sits in a `OnceLock` on the
column metadata token and is filled the first time a row needs it. That was a
proof of concept choice. It kept the change to one field and one method, and it
needed no changes in the parser. The plan is built once per result set, so the
cost of building it disappears against the per row work it removes.

Where the cache lives is an open question, and the lazy shape is not the
recommendation. Building it in the COLMETADATA parser would also work and would
be cleaner. That parser already walks every column and already holds the type
info, so the facts could be plain fields with no lock and no laziness. It would
also put them within reach of the async path, which is the gap described below.

### 2. Decode a whole row from the buffer

The old path read every column with its own `await`. Each of those reads goes
through `#[async_trait]`, which allocates a boxed future on the heap. A 48
column row cost 96 of them, and the row was usually already in memory.

The new path takes the whole row under one borrow. It runs two passes. The
first measures and bounds checks. The second decodes with no checks, because
the first pass already proved the offsets are good.

If the row runs past the end of the buffer, the new path steps aside and the old
one handles it. See `try_receive_row_into_buffered` in
`mssql-tds/src/io/token_stream.rs`.

It also steps aside in two other cases. Cursor reads pause part way through a
row, so anything other than `ColumnPolicy::DecodeAll` goes to the old path. So
does any result set with an Always Encrypted decryptor, because the new path
would hand you ciphertext.

### 3. Take the tracing off the row path

Two callsites fired once per row. At 1,499,000 rows that added up.

We deleted them to measure the cost. That is not what we suggest you do. There
are better options, and they are listed in the notes we shared separately.

### 4. Hand values over by reference

The old path copied every string into a `Vec`, wrapped it in a `SqlString`, and
handed that to the consumer. The consumer often copied it again.

Now the bytes are offered as a borrow. A consumer that needs to keep them can
still copy. See `write_string_ref` in `mssql-tds/src/datatypes/row_writer.rs`.

### 5. Let the consumer own the buffer for large values

A `varbinary(max)` or `varchar(max)` value has its length declared up front. The
consumer can allocate its own storage first and let the decoder read into it.

That removes one full copy of the value. See `string_destination` and
`bytes_destination` in `mssql-tds/src/datatypes/row_writer.rs`.

## What this does not do

Read this part before you judge the numbers.

### NOT NULL columns are not covered

SQL Server sends a different type code for a column that cannot be null.

A nullable `int` arrives as `IntN`. It carries a length byte before the value. A
`NOT NULL int` arrives as `Int4`. It is four bytes with no length byte at all.

The decode plan only covers the nullable forms. `Int4`, `Flt8`, `Bit`, `Money`,
`DateTime` and the rest of the fixed length types all fall through to
`DecodeOp::Generic`.

That matters more than it sounds. One `Generic` column sends the whole row back
to the old path. A table of `NOT NULL` columns gets none of this speedup.

The good news is that these types are easier to handle than the ones we did.
There is no length byte to read and no length to validate. The same two pass
pattern works, with less code per type.

### Large values are not in the fast row path

A column holding `varchar(max)` or `varbinary(max)` is marked `Generic` as well.
Those values can span many packets, so they need the async path.

Change 5 above still helps them. It just happens on the old path, not the new
one.

### Some ordinary types are still missing

`uniqueidentifier`, `datetimeoffset`, `money`, `sql_variant`, `xml`, `json` and
`vector` are all `Generic` today. Each is a small addition.

### The async path does not use the plan yet

Change 1 gives every column a `DecodeOp` tag. Only the buffered path reads it.

The async path still works the type out from `ColumnMetadata` on every column,
inside `drive_row_columns`. That function also handles cursor policies, Always
Encrypted and paused rows, so wiring the plan into it is a real piece of work
rather than a small edit.

`decode_op_into` in `mssql-tds/src/datatypes/decoder.rs` shows the shape it
would take. Nothing calls it today.

### One workload, one platform

We measured on Linux against SQL Server 2022, with one query shape. Your results
will differ. Treat the numbers as a direction, not a promise.

## Where to look

| File | What is in it |
| --- | --- |
| `mssql-tds/src/io/token_stream.rs` | The two pass row decoder |
| `mssql-tds/src/token/tokens.rs` | The per column decode plan |
| `mssql-tds/src/io/packet_reader.rs` | Non blocking reads and their helpers |
| `mssql-tds/src/datatypes/decoder.rs` | Per column decoding and the PLP paths |
| `mssql-tds/src/datatypes/row_writer.rs` | The consumer side contract |

Start with `try_receive_row_into_buffered`. It has a long comment at the top
that explains the two passes and the rules they have to follow.

## Why it is not production ready

- The two passes have to agree on every field width. Nothing enforces that today
  except care and tests.
- The fallback path is exercised by our workload, but not by a test that forces a
  row to straddle a packet boundary.
- The guards that send cursor reads and encrypted rows to the old path have no
  test of their own.
- The tracing change deletes diagnostics that other callers may want.

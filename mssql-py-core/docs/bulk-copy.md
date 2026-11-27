# Native Python bulk-copy behavior

Conversion notes for SDK authors using the
[`mssql_py_core` extension](../README.md). For the Python database API, use
[`mssql-python`](https://github.com/microsoft/mssql-python).

## JSON snapshots and Arrow input

`BulkCopyJson(text: str)` is an internal, frozen bridge for SDK adapters, not a
replacement public `Json` constructor. Pass the public wrapper's existing
immutable serialized text snapshot. Carrier construction retains the Python
string without encoding or parsing it. Only mapped cells are converted: a
carrier requires a JSON destination and copies its strict UTF-8 text into
`SqlJson`, without `loads`, `dumps`, or a JSON value tree. Unmapped carriers are
ignored, including their payloads. Legacy bare strings retain their existing
JSON validation; dicts and lists retain their existing serialization.
JSON does not inspect later rows to choose a wire representation. Mapped mutable
dict/list cells are converted before the next source pull, so generators may reuse
those containers between yields; a later NULL cell does not introduce look-ahead.

Arrow JSON input accepts Utf8, LargeUtf8 and Utf8View serialized text, either
plain or carrying the canonical `arrow.json` extension (`pa.json_()`), with
existing plain null-array behavior. Mapped field names, physical types and relevant
logical annotations are checked for each batch, including empty batches.
The JSON extension must use one of those string storage types and absent/empty
or JSON-object extension metadata, as specified by
[Arrow's canonical JSON extension](https://arrow.apache.org/docs/format/CanonicalExtensions.html#json).
Unknown extensions, invalid storage and malformed extension metadata are rejected.
Plain and canonical JSON batches may share a plan when their mapped names and
physical types match. Unrelated field metadata and unmapped fields are
not interpreted. Struct, List and Dictionary JSON serialization is not supported.
Native PyArrow readers retain their per-batch schemas instead of being re-exported
as C streams. External C-stream producers must honor their advertised fixed
schema; annotations discarded upstream cannot be reconstructed by the importer.

Both input paths reuse JSON bulk's NVARCHAR(MAX), UTF-16LE PLP transport. Text
adapters do not validate explicit snapshots or Arrow JSON syntax/root shape;
SQL Server does. Server storage can normalize text. The writer still allocates
UTF-16 scratch for each JSON value and rejects lengths beyond its single-chunk
limit; this is not a zero-copy or bounded-byte-buffer guarantee.
In particular, Arrow accepts standalone JSON values such as `123`, `true` and
textual `null`, but SQL JSON requires an object or array. Those values fail at
the server through normal bulk error cleanup; they are not converted or wrapped.
An Arrow null cell remains SQL NULL, not the JSON text `null`. Arrow fetching is
unchanged; accepting `pa.json_()` on input does not require extension-typed output.

### Source failures and integration

Row iterator and Arrow producer/import errors stop the load through the bulk
failure path, before completing a truncated batch. Original Python exceptions
are retained where the producer/importer preserves them; C-stream producers may
translate them into Arrow exceptions. Errors at first read or between completed
batches are not successful EOF. There is no retry, replay or whole-input scan.
Garbage collection of a previous core connection during a producer callback
uses nonblocking runtime shutdown, avoiding a Tokio destructor panic while
retaining the original source exception and traceback.

With internal transactions, an active failing batch is rolled back when the
connection remains usable; earlier committed batches remain. A failed transport
is retired. Without internal transactions, do not assume rollback of accepted
rows. Bulk uses a separate connection in the public driver, so its parent's
rollback cannot undo these writes. `batch_size=0` means all rows in one batch,
not an automatically selected server batch size.

Python-side enablement requires a **published, qualified runtime containing both
this carrier and fail-fast row/Arrow source handling**, with an exact dependency
pin to that artifact. A carrier-symbol check or the existing package version alone
is not sufficient; this local change does not assign a release version.
For known older cores, leave public `Json` cells untouched and translate only
their exact in-writer mapped unsupported-type diagnostic. Do not raise a
generator-time fallback error that those cores can swallow, broaden translation
to arbitrary `ValueError`, or replay a load. Public-wrapper adaptation and C++
Arrow JSON output remain separate `mssql-python` integration work.

### Contributor checks

The Rust cursor importer tests exercise real PyArrow, not a replacement importer.
Install the `dev` extra in the Python environment used by `PYO3_PYTHON`; when
embedding a virtual environment on Windows, also set `PYTHONPATH` to its
`Lib\site-packages`. Run `cargo nextest run --manifest-path mssql-py-core/Cargo.toml
--frozen --lib` for native tests. Build/install the local wheel with maturin before running
the JSON and source-failure Python tests. Live tests require the normal
`tests/conftest.py` SQL configuration and a native JSON-capable test server.

## Decimal scale

Bulk copy rounds Python `Decimal`, decimal strings, and Arrow `decimal128`
values to the destination `DECIMAL`/`NUMERIC` scale, with ties rounded away
from zero (`123.455` becomes `123.46`; `-123.455` becomes `-123.46` at scale 2).
Conversion uses exact decimal arithmetic, independent of Python's decimal
context. Increasing the scale pads with zeros. Precision is checked after
rounding, so a carry that exceeds the destination precision still raises a
conversion error.

For `Decimal` and decimal-string inputs, source precision may exceed 38 if the
rounded value fits the destination. For example,
`123456789012345678901234567890123456.785` fits `DECIMAL(38,2)` after rounding to
`123456789012345678901234567890123456.79`. This differs from `mssql-python`
`execute()`, which rejects `Decimal` inputs with source precision above 38
before server conversion.

Python `float` inputs use binary floating-point formatting instead of this
decimal rounding rule. Exact binary ties round to even: `2.5` becomes `2` at
scale 0, and `0.125` becomes `0.12` at scale 2. Use `Decimal` or decimal strings
when decimal half-away-from-zero rounding is required.

## Datetime strings

Bulk copy parses ISO datetime strings targeting `DATETIME2` and `DATETIMEOFFSET`
in Rust, preserving all seven fractional-second digits (100 ns precision).
Both space and `T` separators and a trailing `Z` are supported on every supported
Python version. Fractions longer than seven digits are rejected rather than
silently truncated. The destination column's scale is applied by the TDS
serializer, using the existing truncation behavior for lower scales.
Python-compatible fractional forms such as `12000012` and `12:00:00:12`
are retained; both represent `12:00:00.12`.

`DATETIME2` retains the local date and time when an offset is present.
`DATETIMEOFFSET` preserves the offset and normalizes its wire timestamp to UTC;
offsets must be whole minutes within `-14:00` through `+14:00`, and both the local
and UTC dates must be within years 1 through 9999. Offset fields are normalized
before validation, matching Python (for example, `+05:60` becomes `+06:00`).
Strings without an offset
continue to use UTC for `DATETIMEOFFSET`. Python `datetime` objects retain their
existing microsecond resolution.

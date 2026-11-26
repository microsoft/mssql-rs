# Native Python bulk-copy behavior

Conversion notes for SDK authors using the
[`mssql_py_core` extension](../README.md). For the Python database API, use
[`mssql-python`](https://github.com/microsoft/mssql-python).

## Decimal scale

Bulk copy rounds Python `Decimal`, decimal strings, and Arrow `decimal128`
values to the destination `DECIMAL`/`NUMERIC` scale, with ties rounded away
from zero (`123.455` becomes `123.46`; `-123.455` becomes `-123.46` at scale 2).
Conversion uses exact decimal arithmetic, independent of Python's decimal
context. Increasing the scale pads with zeros. Precision is checked after
rounding, so a carry that exceeds the destination precision still raises a
conversion error.

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

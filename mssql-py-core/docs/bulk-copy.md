# Native Python bulk-copy behavior

Conversion notes for SDK authors using the
[`mssql_py_core` extension](../README.md). For the Python database API, use
[`mssql-python`](https://github.com/microsoft/mssql-python).

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

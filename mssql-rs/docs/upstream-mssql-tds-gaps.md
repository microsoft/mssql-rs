# Original upstream mssql-tds gaps

These metadata needs were tracked in [upstream issue #63](https://github.com/saurabh500/mssql-tiberius-bridge/issues/63).
The status below reflects the `mssql-tds` implementation in this workspace.

## `ColumnMetadata::get_precision() -> Option<u8>`

`mssql-tds` now exposes this accessor, and `mssql-rs::Column::precision()`
preserves its value.

## `MultiPartName` accessors

`mssql-tds` now exposes accessors, and `mssql-rs::Column` preserves the
source name when it is present in column metadata. The native parser currently
reads source names for `text`, `ntext`, and `image`; browse-mode table-name
tokens are skipped.

## Public `ColumnMetadata`/`TypeInfo` construction for tests

`TypeInfo` now provides public constructors. `ColumnMetadata` still includes
crate-private fields, but the `test-util` feature supplies scripted metadata
helpers for downstream tests.

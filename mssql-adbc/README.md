# SQL Server ADBC driver

Experimental [Arrow Database Connectivity](https://arrow.apache.org/adbc/)
driver implemented in Rust over
[mssql-tds](https://github.com/microsoft/mssql-rs/tree/main/mssql-tds).
No ODBC driver or Python runtime is required. This is a source-built prototype,
not a published package.

## Build and load

```sh
cargo fetch
cargo build -p mssql-adbc --release --frozen
```

Load `adbc_driver_mssql.dll`, `libadbc_driver_mssql.so`, or
`libadbc_driver_mssql.dylib` from `target/release`. The initialization symbol is
`AdbcDriverMssqlInit` (`AdbcDriverInit` is also exported). The current exporter
supports ADBC **1.1.0**. Release all connections, statements, and Arrow streams
before unloading the library.

For Python, install `adbc-driver-manager` and `pyarrow` separately, then use:

```python
import os
import adbc_driver_manager.dbapi

with adbc_driver_manager.dbapi.connect(
    driver=os.environ["ADBC_TEST_LIBRARY"],  # absolute shared-library path
    entrypoint="AdbcDriverMssqlInit",
    db_kwargs={
        "mssql.data_source": os.environ["ADBC_TEST_SERVER"],
        "mssql.database": os.environ["ADBC_TEST_DATABASE"],
        "username": os.environ["ADBC_TEST_USER"],
        "password": os.environ["ADBC_TEST_PASSWORD"],
    },
    autocommit=True,
) as connection:
    with connection.cursor() as cursor:
        cursor.execute("SELECT CAST(42 AS int) AS answer")
        print(cursor.fetch_arrow_table())
```

Rust callers use `MssqlDriver` with the re-exported `adbc_core` traits.
The [streaming example](https://github.com/microsoft/mssql-rs/blob/main/mssql-adbc/examples/stream.rs)
shows connection setup, bounded batch consumption, and timing.

## Capabilities

SQL queries, updates, prepared execution, Arrow batch/stream binding, schema
description without running the query, manual transactions and isolation levels,
`get_info`, `get_objects`, `get_table_schema`, and `get_table_types` are supported.
Discovery is scoped to the connected database and SQL Server metadata visibility.
Catalog/schema/table/column filters use SQL `LIKE` with backslash escaping.
Optional XDBC discovery fields may be null.

SQL parameters are native **`@p1`, `@p2`, ...**, in Arrow column order; `?`
placeholders are not rewritten. Bound batches can be executed repeatedly; bound
streams are consumed once and require rebinding. Each input row is one execution.
Query schemas must remain identical across parameter rows. Setting new SQL clears
bindings, ingestion targets, and prepared state. Preparation is server-materialized
on first execution.

Only one live result stream may own a connection. Conflicting operations fail
with invalid state instead of blocking. Re-executing/reconfiguring its statement
invalidates the old reader. A reader retains its connection/runtime even after
the originating handles are released. Consume or drop it before another operation.
Dropping a partial stream settles/discards the response.

Autocommit defaults to true. With autocommit disabled, transactions start lazily;
commit/rollback require autocommit disabled. Set isolation before starting a
transaction. Enabling autocommit commits pending work. Connection release rolls
back outstanding work.

## Options

| Scope | Key | Default / values |
|---|---|---|
| Database | `mssql.data_source` | Required; e.g. `tcp:localhost,1433` |
| Database | `mssql.database` | Existing database; never created by this driver |
| Database | `uri` | Alternative `mssql://host[:port]/database`; no credentials/query |
| Database | `username`, `password` | SQL authentication; password is write-only |
| Database | `mssql.authentication` | `sql` or `integrated` (existing TDS SSPI/GSSAPI) |
| Database | `mssql.access_token` | Caller-supplied access token; no token refresh |
| Database | `mssql.encrypt` | Existing TDS TLS default; `strict`, `true`, `false` |
| Database | `mssql.trust_server_certificate` | `false`; explicitly opt in only for development |
| Database | `mssql.connect_timeout_seconds` | Existing TDS connection timeout |
| Database/statement | `mssql.query_timeout_seconds` | `30`; `0` is unlimited |
| Database/statement | `mssql.batch_rows` | `4096`; positive, at most 1,000,000 |
| Database/statement | `mssql.batch_bytes` | `8388608`; approximate payload bytes per batch |
| Connection | ADBC autocommit/current catalog/isolation | SQL Server-supported values |
| Statement | ADBC target table/catalog/schema, ingest mode, temporary | See ingestion below |

Booleans are `"true"`/`"false"`; numeric options accept integer or decimal-string
values. Query timeout is applied to each parameter execution, not to total time
spent consuming a bound stream. Batch byte limits are checked at row boundaries:
one large row/LOB can exceed the limit. The driver owns decoded Arrow buffers;
this is not zero-copy from the network. Strings are decoded strictly.

## Types

| SQL Server | Arrow query output |
|---|---|
| bit / tinyint | Boolean / UInt8 |
| smallint / int / bigint | Int16 / Int32 / Int64 |
| real / float | Float32 / Float64 |
| decimal / numeric / money / smallmoney | Decimal128 with exact precision/scale |
| character / XML / JSON | Utf8 |
| binary | Binary |
| uniqueidentifier | FixedSizeBinary(16), canonical UUID bytes |
| date | Date32 |
| time | Time64(ns), exact 100-ns SQL ticks |
| datetime / smalldatetime | Timestamp(us), no timezone |
| datetime2 | Struct of non-null `date: Date32`, `time: Time64(ns)` |
| datetimeoffset | Same struct plus non-null `offset_minutes: Int16` |

Temporal structs preserve years 1-9999 and 100-ns precision; a nanosecond timestamp
cannot cover that range. Parameters also accept Arrow timestamps (timezone-bearing
timestamps become datetimeoffset at UTC), smaller/wider unsigned integers,
LargeUtf8/Binary and string/binary views. UInt64 maps to decimal(20,0).
Sub-100-ns temporal input, decimal overflow, malformed text, and unsupported types
fail rather than silently losing data. SQL variant, UDT/spatial, vector, dictionary
and arbitrary nested inputs are unsupported.

## Bulk ingestion

Bind a batch or stream, set the standard ADBC target-table option, then call
`execute_update` / driver-manager `adbc_ingest`. Modes are create (default), append,
replace, and create-append. Target table/catalog/schema are **identifier components**,
not SQL fragments; default schema is `dbo`. Local temporary targets require a
`#name`, `temporary=true`, and no catalog/schema. Global temporary tables are rejected.

Ingestion uses native TDS bulk copy, keeps nulls, checks constraints, and fires
triggers. Autocommit ingestion wraps DDL and all input batches in one transaction;
conversion/input/server failures roll it back, including replace DDL. In manual
mode it participates in the caller's transaction; the caller owns rollback.
Input fields match destination names exactly and are reordered to destination
order. Matching types are required; decimal precision/scale must match and modern
temporal destinations must use scale 7. Identity/encrypted columns and lossy
cross-type conversions are not supported. Variable-width cells are materialized
for the TDS writer's owned-value API; whole result/input streams are not collected.
The returned count is acknowledged bulk rows submitted; triggers can change the
number ultimately retained. Cancellation is checked between bulk rows; blocked
bulk I/O is bounded by the bulk timeout rather than immediate cancellation.

Partitioned execution, Substrait, statistics, read-only enforcement and multiple
tabular result sets from one SQL execution return not implemented. Driver-manager
stream failures retain an error message, but the upstream Rust exporter does not
currently export `ErrorFromArrayStream` for extended stream diagnostics.

## Development

```sh
cargo nextest run -p mssql-adbc --all-features --frozen
cargo clippy -p mssql-adbc --all-features --all-targets --frozen -- -D warnings
cargo check -p mssql-adbc --no-default-features --frozen
```

Unit, mock-server and static C ABI tests require no SQL Server. Ignored live tests
require `ADBC_TEST_SERVER`, `ADBC_TEST_DATABASE`, `ADBC_TEST_USER`, and
`ADBC_TEST_PASSWORD`; set these through your shell or a private `.env`, never source
control. The database must already exist. Tests create/drop local temporary tables
and one uniquely named `dbo.adbc_test_*` table for constraint discovery:

```sh
cargo nextest run -p mssql-adbc --test live --run-ignored ignored-only --frozen
ADBC_TEST_LIBRARY=/absolute/path/to/library \
  cargo nextest run -p mssql-adbc --test ffi --run-ignored ignored-only --frozen
cargo run -p mssql-adbc --example stream --release --frozen
python mssql-adbc/tests/driver_manager.py
```

# ODBC driver configuration and behavior

Configuration, application-visible behavior, and migration notes for
[`mssql-odbc`](../README.md). For build instructions and driver status, start
with the README. Deliberate departures from Microsoft ODBC Driver for SQL
Server are recorded in the [parity registry](parity-deviations.md).

## Connection defaults

Eligible TCP connections use MultiSubnetFailover (MSF) by default. Set
`MultiSubnetFailover=No` in a connection string or
`SQL_COPT_SS_MULTISUBNET_FAILOVER=SQL_IS_OFF` before connecting to opt out.
The attribute takes precedence over the keyword. Set
`MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=false` before application startup for a
deployment or application-wide default; explicit connection settings still
win. The default freezes on first use, including an inherited MSF attribute
read, so configure it before loading pools or opening connections.

Explicit non-TCP transports and named instances requiring SQL Browser retain
their existing paths when MSF is inherited. On Windows, local targets without a
protocol or port also retain the shared-memory-first path, including instances
with TCP/IP disabled. `MultiSubnetFailover=Yes`, `tcp:`, or a port explicitly
selects TCP instead. MSF-on connections reject hosts
with more than 64 addresses rather than ignoring part of the address list.
`TransparentNetworkIPResolution=Disabled` and `SQL_COPT_SS_TNIR=SQL_IS_OFF`
are compatibility no-ops; enabling TNIR is unsupported (`HYC00`).
Driver Managers can cache pre-connect attribute setters, deferring driver
validation until connect; a successful cached set is not driver acceptance.
**TNIR Disabled does not disable MSF.** See the
[shared connection policy and migration guide](../../mssql-tds/docs/connection-behavior.md#connection-policy-and-migration)
for the full precedence and applicability rules.

## Connection-operation timeouts

`SQL_ATTR_CONNECTION_TIMEOUT` bounds connection-scoped network requests,
including pooled reset, isolation and autocommit changes, transaction completion,
and cancellation settlement when those operations close pending cursors.
Cursor cleanup abandons unfinished server work rather than draining it.
Zero leaves request I/O unlimited and uses the default cancellation budget.
Cancellation settlement and a pool's replacement login can add time; this is not
an end-to-end pool-checkout deadline. A connection that cannot finish cancellation
is retired rather than returned with an unsynchronized response stream.

## Descriptors

Binding and descriptor APIs share the same records. Freeing an explicit
descriptor restores associated statements' implicit descriptors. Application
buffers must remain valid until outstanding operations finish.

IPD SQL-definition changes take effect on the next prepared execution, including
edits overlapping a synchronous execution. Equivalent definitions and
application-buffer-only changes retain cached plans. During Need Data, complete
or cancel the data-at-execution sequence before changing bindings.

## Data-at-execution parameters

PLP-capable values supplied through `SQLParamData`/`SQLPutData` are streamed to
SQL Server as chunks rather than retained as a complete value. This includes
`varchar(max)`, `nvarchar(max)`, `varbinary(max)`, XML, and CLR UDT parameters.
Character input is transcoded incrementally when its application and wire
encodings differ. XML uses UTF-16LE on the wire and is normalized to exactly one
leading byte-order mark; CLR UDT chunks remain their opaque serialized bytes.

Streaming keeps peak memory bounded by the active conversion and packet buffers
when every remaining DAE parameter is streamable. Bind order is observable in a
mixed sequence: a streamable parameter before a fixed-width or otherwise
buffered parameter is collected whole until that later parameter is available,
because `SQLParamData` tokens must remain in bind order. Once no buffered
parameters remain, subsequent PLP values stream normally. Validation that
depends on the completed value can occur during `SQLPutData` or when
`SQLParamData` closes the parameter rather than at bind time.

TVP DAE cells are buffered until the complete table can be sent. Row-control
staging grows with supplied batches rather than the bound row capacity; the
collected table still requires memory proportional to its actual contents.

See [Data-at-Execution Streaming](data-at-execution-streaming.md) for the call
sequence, examples, and supported conversions.

## Legacy LOB parameters

`SQL_LONGVARCHAR`, `SQL_WLONGVARCHAR`, and `SQL_LONGVARBINARY` parameters use
`text`, `ntext`, and `image` by default, including typed NULLs. Set
`LongAsMax=Yes` in the connection string, or set `SQL_COPT_SS_LONGASMAX` to
`SQL_XL_ON`, to use `varchar(max)`, `nvarchar(max)`, and `varbinary(max)`.
Explicit connection attributes take precedence over the keyword.

TVP columns always use the corresponding MAX types, including NULL and DAE
cells, regardless of `LongAsMax`.

**Migration note:** earlier alpha builds always substituted MAX types. Use
`LongAsMax=Yes` to retain that behavior. `ColumnSize` bounds still apply to
LONG bindings in either mode; use the ordinary variable-length SQL types with
`ColumnSize=0` for explicitly unbounded MAX parameters.

An already-executed prepared statement retains its server-side declaration
when the option changes; prepare it again to select a new declaration. DAE
parameters remain chunked in both modes.

## Data Classification

Every connection requests SQL Server Data Classification version 1, as
msodbcsql does. Set `SQL_COPT_SS_DATACLASSIFICATION_VERSION` (1400) to `2` to
also receive sensitivity ranks; values outside `1..=2` return `HY024`. The
attribute can be set while connected, and the new value is requested at the
next login. unixODBC frees the driver's connection at `SQLDisconnect` and
replays only attributes set before connecting, so there set it before
`SQLDriverConnect`.

A result set's classification is read from its IRD header (`RecNumber` 0):

- `SQL_CA_SS_DATA_CLASSIFICATION_VERSION` (1238) is the version the server
  acknowledged for the session, or `0` when it did not enable the feature.
- `SQL_CA_SS_DATA_CLASSIFICATION` (1237) is the DATACLASSIFICATION token
  payload in the [MS-TDS] layout, byte-identical to msodbcsql's for
  well-formed names; an unpaired UTF-16 surrogate is returned as U+FFFD (see
  [parity deviation 41](parity-deviations.md)). A short
  buffer receives a prefix with `01004` (msodbcsql reports `HY004`; see
  [parity deviation 40](parity-deviations.md)), and a result set
  without classification returns `HY091`.

`SQLColAttribute` does not answer either field. Each result set reports only
its own classification: as in msodbcsql, it is dropped when an execution
starts, when an execution or row read fails or is cancelled, at the end of the
batch, and when the cursor of a statement that is not prepared is closed.

## Integer connection-attribute getters

Supported integer getters accept NULL value and length pointers independently.
Successful getters report a four-byte value length when requested, except
`SQL_COPT_SS_WARN_ON_CP_ERROR`, which writes the length only when the value
pointer is non-NULL.

## Character encoding

Narrow character results (`SQL_C_CHAR`) use the platform client encoding:
the Windows ANSI code page (`GetACP()`), or the supported process-locale
encoding on Linux/macOS, with UTF-8 as the fallback for the C/POSIX or an
unrecognized locale. Locale selection is captured on first use; set the locale
before loading/using the driver. Changing a database collation does not select
the client's encoding.

This applies to `SQLGetData`, bound-column fetches, and procedure output
parameters, including streamed character values. Characters that cannot be
represented use the platform's best-fit/substitution behavior. Set
`SQL_COPT_SS_WARN_ON_CP_ERROR` to `SQL_WARN_YES` to include `01000` when
retrieval truncates and conversion reports loss. Like msodbcsql, a converted
result that fits returns success without a loss warning, even with this
option enabled; do not use the option as a guarantee of lossless retrieval.
Loss warnings describe only the bytes delivered by that call. On Unix,
successful iconv substitutions/transliterations do not produce loss warnings;
explicit conversion-error recovery remains distinct, matching the native
driver. Malformed source text is not a successful client substitution.

Use `SQL_C_WCHAR` for lossless Unicode retrieval; it remains UTF-16LE.
`SQL_C_BINARY` continues to return unconverted wire bytes. mssql-python's
default wide-character fetching is unchanged; an explicitly selected narrow
decoder must match the platform client encoding.

On Windows, a streamed `varchar(max)` value whose collation code page is 1252
is delivered to `SQL_C_CHAR` under ANSI code page 1252 without decoding and
re-encoding, as msodbcsql does when the code pages match. The bytes are the
same as converting them, and the copy may read ahead up to 64 KiB of the value.

**Migration note:** narrow results were previously always UTF-8. Applications
that assumed UTF-8 on a non-UTF-8 system should use wide retrieval or decode
with the client encoding. Input `SQL_C_CHAR` parameters still use UTF-8 pending
AB#47565; this change affects retrieval only.

## Temporal character literals

Character parameters and character-column fetches accept `YYYY/MM/DD` dates
and ODBC temporal literals: `{d 'YYYY-MM-DD'}`, `{t 'HH:MM:SS'}`, and
`{ts 'YYYY-MM-DD HH:MM:SS[.fraction]'}`. Escape keywords are case-insensitive;
numeric fields are fixed-width, and timestamps retain up to nine fractional
digits before target-specific conversion. Slash dates are date-only and do
not apply inside escapes. Malformed values report `22018`.

Existing plain-text forms remain supported, including ISO `T` separators,
unpadded month/day/time fields, and times without seconds. See
[the parameter conversion notes](parameters_plan.md#character-c-type-to-a-temporal-parametertype-ab47851)
for grammar and reference-driver measurements.

## Fetch buffering

Fetch reuses binding storage between calls while honoring descriptor changes on
the next fetch. The connection retains one heap-allocated protocol client to
avoid large client copies during result retrieval; cancellation and connection
ownership behavior are unchanged.

Maintainer-level data-path contracts are in the
[ODBC driver engineering guidelines](../../.github/instructions/mssql-odbc.instructions.md#11-performance-and-ffi-data-movement).
Measurements and trade-offs live beside the implementing code; subsystem design
notes are in this directory.

## Tracing

Tracing is disabled by default and is intended for diagnostics.

| Variable | Default | Purpose |
|---|---|---|
| `MSSQL_TDS_TRACE` | `false` | Set to `true` to enable tracing |
| `MSSQL_TDS_TRACE_LEVEL` | `warn` | Set a `tracing_subscriber::EnvFilter` expression |
| `MSSQL_TDS_TRACE_DIR` | unset | Write per-process trace files to this directory instead of stderr |
| `MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB` | `100` | Roll over to a new file once the live file reaches this size. Accepts 1-1024; any other value falls back to the default. A threshold, not a hard cap - see below |

For example, from the repository root:

```bash
MSSQL_TDS_TRACE=true \
MSSQL_TDS_TRACE_LEVEL="warn,mssqlodbc=debug" \
MSSQL_TDS_TRACE_DIR="./traces" \
cargo nextest run -p mssqlodbc --lib
```

Trace events can contain SQL text and parameter values. On Unix the driver
creates trace files with mode `0600`, and warns on stderr when
`MSSQL_TDS_TRACE_DIR` is writable by group or other users, or points inside the
system temporary directory. Configuration is captured on the first ODBC call: a
relative directory is resolved to an absolute path at that point, and the
settings cannot be changed while the driver stays loaded.

### Correlation

Connection-scoped events include `dbc` (process-local connection handle ID) and
`cid` (the full physical client connection GUID). Statement calls also include
`stmt`; command execution and subsequent fetch/result calls include `exec`.
For example:

```text
2026-10-09T17:24:01.108Z, 19, ERROR, mssqlodbc::api::exec_direct, dbc=17 cid=8A73124F-61D2-4B8E-9A37-2C095D8146AB stmt=42 exec=8 SQLExecDirectW: execution failed
```

Filter by `cid` to find the connection directly, or by `dbc`, `stmt`, and `exec`
to isolate an execution. Local IDs are scoped to one driver load/process; retain
the process-identifying filenames when combining captures. A reconnect changes
`cid` but retains `dbc`. Pool reset without reconnect retains `cid`. Startup,
environment-wide calls, invalid handles, and connection attempts before PRELOGIN
have `cid=-`. Authentication workers retain the identity of the attempt that
started them, even if a timeout lets a later attempt begin.
MultiSubnetFailover connection tasks carry context during each future poll;
TCP attempts, including retries, redirects, and recovery, have `cid=-` until
PRELOGIN supplies a GUID. The connection-ID getter retains the last established
GUID until recovery succeeds. `SQLDescribeParam`
metadata RPCs have their own execution sequence; cached descriptions do not
replace the active result's sequence.
Only result/stream continuation and cleanup APIs inherit the last accepted
execution's `exec`; this includes `SQLRowCount` after a completed no-row command.
Preparation, binding, and statement-configuration calls omit `exec` so their
errors cannot be attributed to an earlier command.
`SQLCancel`'s entry event omits `exec` until it selects the target under the
cancellation lock. Its target/return events then identify the interrupted
operation, including cancellation before the starter publishes its token.
Successful preparation also invalidates the prior execution identity: metadata
probes for the replacement statement have no `exec` until it executes.
Diagnostic retrieval logs identify the connection and statement but omit `exec`:
a rejected concurrent call may have replaced diagnostics without replacing the
active result. Use the generating call's error/return event for its execution ID.
Unassociated background-library events have no connection context.

Applications can retrieve the same uppercase GUID string using
`SQLGetConnectAttrW(SQL_COPT_SS_CLIENT_CONNECTION_ID)`: it needs 74 bytes including
the UTF-16 terminator, and the returned length is 72 bytes. This is a string, not
a binary GUID. The attribute is read-only and requires an established connection.
The GUID matches PRELOGIN's connection ID and SQL Server Extended Events that
capture `client_connection_id`. `exec` is a local execution sequence, not a TDS
ActivityID or a server request identifier.

At `warn`, failing events carry correlation but successful command history is
not collected. Use `warn,mssqlodbc=debug` for command entry/return events; add
`mssql_tds=debug` for more protocol detail. Correlation itself adds no SQL text
or parameter values, but existing events can contain them. Full GUIDs increase
trace size and make files rotate sooner. Disabled tracing skips correlation
lookup, formatting, and statement trace allocation; enabled API calls look up
handle identity once, and event formatting takes no handle locks.

### File rollover

Writing starts in `mssql_tds_trace_<timestamp>_<pid>.log`. Each rollover creates
the next numbered file (`.1.log`, `.2.log`, ...) and continues there, so the
highest-numbered file is the live one and earlier files are left untouched. A
process that never reaches the size limit leaves a single unsuffixed file. The
driver never deletes a trace file, so an enabled `MSSQL_TDS_TRACE_DIR` grows
without bound - reclaiming disk space is the operator's responsibility.

`MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB` is a rollover threshold rather than a hard
cap, and an individual file can exceed it in two ways. The size is checked
between events, so the event that crosses the threshold is written whole instead
of being split across two files. And if creating the successor fails - a full or
read-only volume, for example - the driver reports the error on stderr and keeps
writing to the current file, retrying only after another size interval has been
written; while rollover is failing, the per-file limit is not enforced.

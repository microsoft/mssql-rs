# Data-at-Execution Streaming

## Overview

The ODBC **data-at-execution** (DAE) protocol lets an application supply large
parameter values incrementally — one chunk at a time — instead of marshalling
the entire value into a buffer before calling `SQLExecute`. This is the
standard mechanism for inserting `VARCHAR(MAX)`, `NVARCHAR(MAX)`, and
`VARBINARY(MAX)` data that may be arbitrarily large.

The call sequence is:

```
SQLPrepare      → prepare the SQL text
SQLBindParameter(…, SQL_DATA_AT_EXEC, …)  → mark param as streamed
SQLExecute      → SQL_NEED_DATA
SQLParamData    → SQL_NEED_DATA  (writes ParameterValuePtr to *ValuePtrPtr)
SQLPutData(…)   → SQL_SUCCESS    (zero or more chunks)
SQLParamData    → SQL_NEED_DATA  (if more DAE params) or SQL_SUCCESS (done)
```

## Supported Types

Streamability is decided by the **C type** alone. The bound SQL type is not
consulted: every streamed parameter is declared `varchar(max)`,
`nvarchar(max)`, or `varbinary(max)` on the wire, and the server converts to the
column's own type on assignment.

| C Type         | Declared as        | Wire encoding    |
|----------------|--------------------|------------------|
| `SQL_C_CHAR`   | `varchar(max)`     | UTF-8 bytes      |
| `SQL_C_WCHAR`  | `nvarchar(max)`    | UTF-16LE bytes   |
| `SQL_C_BINARY` | `varbinary(max)`   | raw bytes        |

So the fixed-width SQL types (`SQL_CHAR`, `SQL_WCHAR`, `SQL_BINARY`) stream just
as the MAX ones do, as do `SQL_VARCHAR` / `SQL_WVARCHAR` / `SQL_VARBINARY` and
their `LONG` variants — any SQL type the conversion matrix pairs with one of the
three C types above.

Other C types (numeric, date/time) and Always Encrypted parameters are not yet
supported via DAE — use the normal bound-value path for those.

## ODBC API Reference

### `SQLBindParameter` — mark a parameter as data-at-execution

Set `StrLen_or_IndPtr` to point to a `SQLLEN` whose value is:

- `SQL_DATA_AT_EXEC` — the data will be supplied via `SQLPutData`, with no
  declared total length.
- `SQL_LEN_DATA_AT_EXEC(length)` — same, but declares the total byte count.
  This driver **enforces** the declaration: the bytes accumulated across the
  parameter's `SQLPutData` calls must equal `length` exactly. Over-sending
  fails the offending `SQLPutData` and under-sending fails the `SQLParamData`
  that closes the parameter, both with `22026` (string data, length mismatch),
  and both abandon the execution. Pass `SQL_DATA_AT_EXEC` when the total is not
  known up front.

The `ParameterValuePtr` field is used as an opaque application token: it is
written to `*ValuePtrPtr` by `SQLParamData` so the application can identify
which parameter needs data. It does not need to point to a valid buffer.

### `SQLExecute` — detects DAE parameters

When one or more bound parameters have a data-at-execution indicator,
`SQLExecute` starts the TDS streaming RPC and returns `SQL_NEED_DATA` instead
of completing the execution. The connection is held busy for the duration of
the DAE sequence.

### `SQLParamData(StatementHandle, *ValuePtrPtr)` → return code

The function serves two purposes:

1. **First call after `SQLExecute`**: writes the `ParameterValuePtr` of the
   first DAE parameter to `*ValuePtrPtr` and returns `SQL_NEED_DATA`.
2. **Subsequent calls**: closes the current parameter on the wire, then either:
   - advances to the next DAE parameter (`SQL_NEED_DATA`, `*ValuePtrPtr`
     updated) if more remain, or
   - completes the execution and returns the statement result (`SQL_SUCCESS`,
     `SQL_NO_DATA`, or `SQL_ERROR`).

### `SQLPutData(StatementHandle, DataPtr, StrLen_or_Ind)`

Supplies one chunk for the currently-open DAE parameter.

| `StrLen_or_Ind` value | Effect                                    |
|-----------------------|-------------------------------------------|
| `SQL_NULL_DATA` (-1)  | Mark the parameter as SQL `NULL`. No bytes are sent; `DataPtr` is ignored. `HY020` if any earlier `SQLPutData` already supplied a value for this parameter, including a zero-length one. |
| `0`, `DataPtr` non-null | A present, zero-length value contribution. No bytes go on the wire, but the parameter is now committed to a value and can no longer become `NULL`. |
| `0`, `DataPtr` null   | Equivalent to `SQL_NULL_DATA`, matching msodbcsql. |
| Positive integer _n_  | Send `DataPtr[0..n]` as the next chunk. `HY020` if the parameter was already marked `NULL`. |
| `SQL_NTS`             | Send `DataPtr` up to its NUL terminator (`u16` units for `SQL_C_WCHAR`). `HY009` if `DataPtr` is null. |
| Other negative        | `HY090` (invalid string or buffer length). |

`SQLPutData` must be called **at least once** for every parameter the DAE
sequence opens: the `SQLParamData` that closes a parameter returns `HY010` if
no chunk call was made. An intentionally empty (but non-`NULL`) value therefore
needs one `SQLPutData` call with a non-null `DataPtr` and length `0`.

## C Example

```c
SQLHSTMT hstmt;
SQLLEN   ind = SQL_DATA_AT_EXEC;
char    *token = (char *)1;   /* opaque app token, not a real pointer */

SQLPrepare(hstmt, "INSERT INTO docs(content) VALUES (?)", SQL_NTS);
SQLBindParameter(
    hstmt,
    1, SQL_PARAM_INPUT,
    SQL_C_CHAR, SQL_LONGVARCHAR,
    0, 0,
    token,          /* ParameterValuePtr — returned by SQLParamData */
    0,
    &ind);

SQLRETURN rc = SQLExecute(hstmt);
if (rc != SQL_NEED_DATA) { /* handle error */ }

char *p;
while ((rc = SQLParamData(hstmt, (SQLPOINTER *)&p)) == SQL_NEED_DATA) {
    /* p == token (the ParameterValuePtr we supplied at bind time) */
    const char *chunk = "Hello, ";
    SQLPutData(hstmt, (SQLPOINTER)chunk, strlen(chunk));
    const char *chunk2 = "world!";
    SQLPutData(hstmt, (SQLPOINTER)chunk2, strlen(chunk2));
    /* Calling SQLParamData again will close this param */
}
/* rc is now SQL_SUCCESS or SQL_ERROR */
```

## Mixed Parameters

A statement can mix DAE and regular parameters. Regular parameters are
converted from their bound buffers at `SQLExecute` time; DAE parameters are
streamed afterwards in their original `@P1..@Pn` order.

```c
SQLLEN  ind_name = SQL_NTS;
SQLLEN  ind_body = SQL_DATA_AT_EXEC;
char    name_buf[64] = "my-document";
char   *body_token = (char *)2;

SQLPrepare(hstmt, "INSERT INTO docs(name, body) VALUES (?, ?)", SQL_NTS);
/* @P1 = name — regular, bound-value parameter */
SQLBindParameter(hstmt, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR,
                 0, 0, name_buf, sizeof(name_buf), &ind_name);
/* @P2 = body — data-at-execution */
SQLBindParameter(hstmt, 2, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_LONGVARCHAR,
                 0, 0, body_token, 0, &ind_body);

rc = SQLExecute(hstmt);   /* → SQL_NEED_DATA */
/* SQLParamData will point to body_token (only @P2 is DAE) */
```

## Implementation Notes

### TDS Streaming

When DAE parameters are detected, the entry point calls into the TDS client
with the full parameter list (materialized params carry real values; DAE params
carry `data_at_exec()` placeholders). This writes the RPC header plus all
non-DAE parameters, then suspends the message in the `StreamedWriteState`
machine.

Which RPC that is depends on the entry point, and streaming does not change the
choice:

| Entry point | TDS call | RPC |
|---|---|---|
| `SQLExecute`, first execute | `begin_execute_prepared` | `sp_prepexec` |
| `SQLExecute`, handle already cached | `begin_execute_prepared` | `sp_execute` |
| `SQLExecDirect` | `begin_sp_executesql` | `sp_executesql` |

A data-at-execution parameter therefore does not cost a prepared statement its
plan: the streamed values go into the same procedure a materialized execute
would have used, so one prepare serves every later execute whether or not that
execute streams. This matches msodbcsql, which selects the procedure purely on
whether the statement is prepared and treats DAE as orthogonal, parking a
half-written `sp_prepexec` / `sp_execute` RPC the same way this driver does.
`SQLExecDirect` has no plan to preserve, so it stays on ad-hoc `sp_executesql`.

Because `sp_prepexec` returns its `@handle` as a RETURNVALUE that trails the
result set, the statement id is claimed when the message is parked rather than
when it completes. A sequence that is cancelled or fails never produces the
handle, so `cancel_streamed_write` and `abort_streamed_write` disarm the pending
capture; leaving it armed would divert an unrelated RPC's first return value
into the handle map.

`SQLPutData` calls `TdsClient::write_streamed_chunk` (or
`write_streamed_null`). `SQLParamData` calls `TdsClient::end_streamed_param`
which writes the PLP terminator and either opens the next parameter's header
or finalises and sends the packet.

### Parameter Order

Any parameter may be data-at-execution, in any position, freely interleaved
with ordinary bound parameters. `build_params_with_dae` decides per marker,
so a `SELECT ? + ? + ?` with only the middle parameter streamed is supported
(`DataAtExecutionInterleavesWithBoundParams` covers exactly that).

Both streamed entry points emit materialized parameters ahead of streamed ones,
on the wire and in the `@params` declaration where there is one. This is
invisible to the application, because `build_named_params` names every parameter
`@P{n}`: `@P2` in the SQL text resolves to the `@P2` that was sent regardless of
position, and that holds for `sp_execute` against a cached plan just as it does
for `sp_executesql`. The partition is order-stable, so the k-th entry in the
parked DAE list still identifies the k-th streamed parameter and the
`SQLParamData` token sequence follows ascending parameter number.

### State Lifecycle

| State | Flags |
|-------|-------|
| After `SQLExecute` (DAE detected) | `EXEC_STARTED \| NEED_DATA` |
| After first `SQLParamData` | `EXEC_STARTED \| NEED_DATA` |
| After `SQLPutData` | `EXEC_STARTED \| NEED_DATA` |
| After final `SQLParamData` completes | `EXEC_CONTEXT` (cursor) or idle |
| On error | all flags cleared, connection returns to idle |

The `TdsClient` is held in the statement's `StmtState::dae` alongside the
prepared plan and orphaned handle the sequence suspended. The DBC's `client`
field is `None` and `active_stmt` is set to prevent concurrent access from other
statements.

### Error Recovery

If `write_streamed_chunk`, `write_streamed_null`, or `end_streamed_param`
fails, `TdsClient::abort_streamed_write` is called internally (which closes
the transport). The ODBC layer:

1. Clears all DAE state (`take_dae`).
2. Writes the prepared plan back so `SQLExecute` can be retried.
3. Returns the client to idle and posts the TDS error.

The application receives `SQL_ERROR` with an appropriate diagnostic.

### Cancellation

`SQLCancel` during a DAE sequence discards the parked request via
`TdsClient::cancel_streamed_write`, restores the prepared plan, and returns the
client to idle, releasing the statement from the Need Data state. This is the
only way out of Need Data that does not involve completing or failing the
sequence.

The request is retracted at the protocol level, so the connection stays usable
and `SQL_ATTR_CONNECTION_DEAD` still reports `SQL_CD_FALSE`. Matching msodbcsql,
the retraction depends on how much has escaped: a request still buffered in the
client is dropped locally with no bytes sent, and one that is partially sent is
terminated with an `EOM | IGNORE` packet so the server discards it, after which
the `DONE` it answers with is drained. Only if that handshake itself fails is
the transport closed, since the request is then neither complete nor
retractable.

The same path unwinds a sequence the driver rejects (`22026`, `HY020`,
`HY010`), so a misused API call does not cost the application its connection
either.

## Limitations (Phase 1)

- Only `SQL_C_CHAR`, `SQL_C_WCHAR`, and `SQL_C_BINARY` C types are supported for
  DAE streaming.
- Always Encrypted columns cannot use the DAE path.
- Output parameters cannot be DAE.
- A DAE sequence that fails on the wire (rather than being cancelled or
  rejected) closes the transport, because a request interrupted by a send or
  receive failure cannot be retracted with `EOM | IGNORE`. msodbcsql can recover
  here (`GetBatchCtxOrRecover`).
- The `SQL_NULL_DATA`-after-chunks case is caught by the unixODBC 2.3.9 driver
  manager first, which returns `HY011` before the driver is reached. The driver
  guard still posts `HY020` per the spec for callers that bypass that driver
  manager. The opposite direction (a chunk after `SQL_NULL_DATA`) is not
  intercepted and surfaces the driver's `HY020`.

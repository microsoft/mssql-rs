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

Only MAX-length character/binary SQL types are streamable:

| C Type         | SQL Type(s)                          | Wire encoding    |
|----------------|--------------------------------------|------------------|
| `SQL_C_CHAR`   | `SQL_VARCHAR`, `SQL_LONGVARCHAR`     | UTF-8 bytes      |
| `SQL_C_WCHAR`  | `SQL_WVARCHAR`, `SQL_WLONGVARCHAR`   | UTF-16LE bytes   |

Other C types (numeric, date/time, binary) and Always Encrypted parameters
are not yet supported via DAE — use the normal bound-value path for those.

## ODBC API Reference

### `SQLBindParameter` — mark a parameter as data-at-execution

Set `StrLen_or_IndPtr` to point to a `SQLLEN` whose value is:

- `SQL_DATA_AT_EXEC` — the data will be supplied via `SQLPutData`.
- `SQL_LEN_DATA_AT_EXEC(length)` — same, but hints the total length (used by
  some drivers for pre-allocation; this driver treats it identically to
  `SQL_DATA_AT_EXEC`).

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
| `SQL_NULL_DATA` (-1)  | Mark the parameter as SQL `NULL`. No bytes are sent; `DataPtr` is ignored. `HY020` if any value bytes were already supplied for this parameter. |
| `0`                   | No-op: a present, zero-length value contribution. |
| Positive integer _n_  | Send `DataPtr[0..n]` as the next chunk. `HY020` if the parameter was already marked `NULL`. |
| Other negative        | `HY090` (invalid string or buffer length). |

`SQLPutData` may be called zero or more times between consecutive
`SQLParamData` calls.

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

When DAE parameters are detected, `SQLExecute` calls
`TdsClient::begin_sp_executesql` with the full parameter list (materialized
params carry real values; DAE params carry `data_at_exec()` placeholders).
This writes the `sp_executesql` RPC header plus all non-DAE parameters, then
suspends the message in the `StreamedWriteState` machine.

`SQLPutData` calls `TdsClient::write_streamed_chunk` (or
`write_streamed_null`). `SQLParamData` calls `TdsClient::end_streamed_param`
which writes the PLP terminator and either opens the next parameter's header
or finalises and sends the packet.

### Parameter Order

Any parameter may be data-at-execution, in any position, freely interleaved
with ordinary bound parameters. `build_params_with_dae` decides per marker,
so a `SELECT ? + ? + ?` with only the middle parameter streamed is supported
(`DataAtExecutionInterleavesWithBoundParams` covers exactly that).

`begin_sp_executesql` does emit materialized parameters ahead of streamed ones,
both in the `@params` declaration and on the wire. This is invisible to the
application: `sp_executesql` binds by name, so `@P2` in the SQL text resolves to
the `@P2` that was sent regardless of position. The partition is order-stable,
so `dae_param_indices[k]` still identifies the k-th streamed parameter and the
`SQLParamData` token sequence follows ascending parameter number.

### State Lifecycle

| State | Flags |
|-------|-------|
| After `SQLExecute` (DAE detected) | `EXEC_STARTED \| NEED_DATA` |
| After first `SQLParamData` | `EXEC_STARTED \| NEED_DATA` |
| After `SQLPutData` | `EXEC_STARTED \| NEED_DATA` |
| After final `SQLParamData` completes | `EXEC_CONTEXT` (cursor) or idle |
| On error | all flags cleared, connection returns to idle |

The `TdsClient` is held in `StmtState::dae_client` while the DAE sequence is
in progress. The DBC's `client` field is `None` and `active_stmt` is set to
prevent concurrent access from other statements.

### Error Recovery

If `write_streamed_chunk`, `write_streamed_null`, or `end_streamed_param`
fails, `TdsClient::abort_streamed_write` is called internally (which closes
the transport). The ODBC layer:

1. Clears all DAE state (`reset_dae`).
2. Writes the prepared plan back so `SQLExecute` can be retried.
3. Returns the client to idle and posts the TDS error.

The application receives `SQL_ERROR` with an appropriate diagnostic.

### Cancellation

`SQLCancel` during a DAE sequence discards the parked request via
`TdsClient::cancel_streamed_write`, restores the prepared plan, and returns the
client to idle, releasing the statement from the Need Data state. This is the
only way out of Need Data that does not involve completing or failing the
sequence.

The half-written RPC cannot be retracted, so `cancel_streamed_write` closes the
transport and the connection reports `SQL_CD_TRUE` for
`SQL_ATTR_CONNECTION_DEAD` afterwards; the application must reconnect unless
connection resiliency is negotiated, in which case the next command recovers the
session transparently. msodbcsql does better here: it discards a still-unsent
request locally, and after a partial send terminates the packet with
`EOM | IGNORE` and drains the `DONE`, keeping the connection alive. Matching
that is tracked by the TODO on `TdsClient::abort_streamed_write`.

## Limitations (Phase 1)

- Only `SQL_C_CHAR`, `SQL_C_WCHAR`, and `SQL_C_BINARY` C types are supported for
  DAE streaming.
- Always Encrypted columns cannot use the DAE path.
- `SQLExecDirect` + DAE is not yet implemented (returns `HYC00`). msodbcsql
  supports this by stringifying the statement (`ProcessDAECmd`).
- Output parameters cannot be DAE.
- A failed DAE sequence cannot be recovered by reconnecting, because the
  transport is suspended mid-write. msodbcsql can recover here
  (`GetBatchCtxOrRecover`).
- The `SQL_NULL_DATA`-after-chunks case is caught by the unixODBC 2.3.9 driver
  manager first, which returns `HY011` before the driver is reached. The driver
  guard still posts `HY020` per the spec for callers that bypass that driver
  manager. The opposite direction (a chunk after `SQL_NULL_DATA`) is not
  intercepted and surfaces the driver's `HY020`.

# sqlcmd JSON output

Output contract implemented by
[`mssql-sqlcmd`](https://github.com/microsoft/mssql-rs/blob/main/mssql-sqlcmd/README.md).

Native sqlcmd hands each piece of output to a JSON document instead of printing
it — the batches it sends, result sets and rows, row counts, server messages —
and renders the document once, at exit, with the connection details, timing,
status and exit code. The document follows the `format json` contract of the
sqlcmd specification, `contractVersion` 1.0. It is written with `serde_json`,
indented, with each row and column description on one line; shown here more
compactly:

```json
{
  "contractVersion": "1.0",
  "sqlcmd": { "version": "18.7.0001.1", "platform": "win-x64" },
  "connection": { "server": "tcp:localhost,1433", "database": "master", "authentication": "SqlPassword",
                  "encrypt": true, "serverVersion": "17.00.4085", "connectMs": 167 },
  "startTime": "2026-10-03T04:17:02.118Z",
  "durationMs": 178,
  "executionStatus": "completed",
  "operationOutcome": "succeeded",
  "exitCode": 0,
  "output": [
    { "type": "batch", "index": 1, "durationMs": 7, "text": "SELECT id, price FROM t;\nPRINT 'done';\n" },
    { "type": "resultSet",
      "columns": [ { "ordinal": 0, "name": "id", "driverType": "int", "precision": 10, "scale": 0, "nullable": false },
                   { "ordinal": 1, "name": "price", "driverType": "decimal", "precision": 10, "scale": 2, "nullable": true } ],
      "rows": [ ["1", "9.99"], ["2", null] ],
      "rowsAffected": 2 },
    { "type": "message", "number": 0, "severity": 0, "state": 1, "server": "db01", "line": 2, "sqlState": "01000", "text": "done" }
  ]
}
```

- **Header.** `contractVersion` is `major.minor`: the major version changes only
  for an incompatible change. `platform` is the runtime (`win-x64`,
  `linux-musl-arm64`, ...). `connectMs` appears after connection succeeds;
  `serverVersion` appears when supplied by the caller. `startTime` (UTC) and
  `durationMs` cover the whole run.
- **Status.** `executionStatus` says whether sqlcmd carried out the run:
  `completed`, `canceled` (Ctrl+C) or `invalidInvocation` (the command line was
  rejected). `operationOutcome` is `succeeded`, `failed`, `canceled` or
  `notExecuted`: with the exit code, which is unchanged by `--format json`, it
  says whether the work succeeded. No failure category is derived; the errors
  themselves say what went wrong. A statement error that does not stop the run
  is an `error` entry in a `succeeded` document, as its exit code is 0.
- **Batches.** One `batch` entry per batch sent (`GO` separates batches), with
  its duration. Its `text` is included only with `-e`, as text mode echoes it
  only with `-e`, so a password in, say, `CREATE LOGIN` is not written out by
  default.
- **Result sets.** Columns are described as the driver describes them:
  `ordinal` (from 0) and `name` (`null` when unnamed) always; `driverType`,
  `size`, `precision`, `scale` and `nullable` when the driver exposes them, and
  type names exactly as the driver reports them. Rows are positional arrays.
  Values are strings, as sqlcmd would print them: binary with a `0x` prefix,
  `-R` and `-k` applied, and cut to the same `-y`/`-Y` width as text output;
  SQL `NULL` is JSON `null`. `rowsAffected` is the result set's "(n rows
  affected)", absent under `SET NOCOUNT ON`; a statement without a result set
  gets its own entry instead, `{ "type": "rowsAffected", "count": 3 }`.
- **`FOR JSON` / `FOR XML`** output is ordinary result-set rows: one string
  per row the server returned, not parsed, combined or embedded.
- **Messages and errors** carry what the console shows for them:
  `number` (Msg), `severity` (Level), `state`, and when known `server`,
  `procedure`, `line` and `source` (the driver, e.g. `Microsoft ODBC Driver 18 for
  SQL Server`, or `Sqlcmd`, when the server did not send it). `sqlState` is the
  ODBC SQLSTATE (`28000` for a refused login, `08001` for a connection failure,
  `HYT00` for a timeout), the stable field to classify errors by. `text` is the
  message itself.

`output` entries follow the order they began. A result set contains all its
rows, so a message received between rows follows the entire result set rather
than being interleaved with its rows.

The whole document is held in memory until sqlcmd exits, because it begins
with details only known then (the exit code). Peak memory is a few times the
size of the output: the collected values, the rendered UTF-8 text, and the
UTF-16 copy handed back to sqlcmd. For very large results, text output still
streams.

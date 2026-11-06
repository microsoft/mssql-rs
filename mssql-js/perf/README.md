# Connection and query microbenchmark

Each sample opens a connection, queries `sys.columns`, and closes the
connection. It measures that full lifecycle, not query execution alone.

First follow the [local build instructions](../README.md#build-from-source).
From `mssql-js/perf`, run:

```sh
node query.mjs
```

Set `SQL_PASSWORD` for the SQL login. `DB_HOST` defaults to `localhost` and
`DB_USER` to `sa`; the helper uses port 1433, database `master`, and trusts the
server certificate. Use a disposable test environment.

Example output:

```text
SQL Server Query x 9.55 ops/sec ±1.53% (49 runs sampled)
Fastest is SQL Server Query
```

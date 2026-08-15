<!--
Copyright (c) Microsoft Corporation.
Licensed under the MIT License.
-->

# mssql-python ↔ mssql-odbc Connection-Pool E2E Verification Plan

Companion to [`odbc-connection-pooling-plan.md`](./odbc-connection-pooling-plan.md)
(ADO #47317). That plan delivers the ODBC/TDS reset, liveness, transaction, and
isolation semantics that let **mssql-python's** existing client-side pool safely
reuse a physical Rust ODBC connection. This document is the **Workstream C**
verification plan: the exact steps, environment, and expected results for
exercising that pool against this driver end to end.

## Why this is a documented plan, not an in-repo test

There is **no in-repo hook that loads mssql-python against the Rust driver**, and
adding one would mean pulling heavy new infrastructure (a Python toolchain plus
the `mssql-python` package and its own C++ pool) into `mssql-rs`, which does not
fit the current repo layout. `mssql-python` is a separate product with its own
pool implementation (`pybind/connection/connection_pool.cpp`); the non-goal is
explicitly "no second pool in Rust." The supported, already-existing hook is the
ODBC **driver registration** the e2e runners perform, so mssql-python can select
this driver by name in a connection string. This plan documents how to drive
that combination manually or in CI, per the plan's guidance to avoid fabricating
new infrastructure.

The in-repo coverage that backs these scenarios already exists and is exercised
by `cargo btest` / the ODBC e2e suite:

- **Unit (mock, no server):** `mssql-odbc/src/api/txn.rs`,
  `mssql-odbc/src/api/get_connect_attr.rs`,
  `mssql-odbc/src/api/set_connect_attr.rs` — `CONNECTION_DEAD` polarity,
  `RESET_CONNECTION` value validation / busy rejection / poison-on-failure,
  isolation re-apply after reset, checkout lifecycle, access-token-after-connect
  rejection.
- **TDS (mock):** `mssql-tds/src/connection/tds_client.rs` — reset ack clears
  session-bound caches and restores login defaults; fatal-error death marking.
- **Live C++ e2e:** `mssql-odbc/tests/e2e/tests/connection_pool_test.cpp` —
  same-physical-connection reuse, clean state for the next borrower, isolation
  reset each checkout, prepared-statement survival, `CONNECTION_DEAD` on a
  healthy connection.

This plan covers the remaining, cross-product parity: mssql-python's **own** pool
driving those same primitives.

## Prerequisites

| Requirement | Notes |
|---|---|
| A live SQL Server | e.g. `./dev/dev-launchsql.sh`, or any reachable instance. |
| The Rust ODBC driver, registered | Build with `cargo build` in `mssql-odbc/`, then register it (below). |
| `mssql-python` from GitHub `main` | Authoritative `reset()` / isolation behavior is on GitHub `microsoft/mssql-python` `main` (PR #343). A local ADO-mirror clone may predate #343 — do not rely on it. |
| Python 3.10+ | To run the verification snippets. |

### Registering the Rust driver for mssql-python

Reuse the exact registration the e2e runners already perform — do **not** invent
a new mechanism. See `mssql-odbc/tests/e2e/README.md` §"Driver Registration".

- **Windows** (`mssql-odbc/tests/e2e/run_e2e.ps1`, requires Administrator)
  registers the driver under `HKLM\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for
  SQL Server (Rust)`, alongside any installed `msodbcsql18`.
- **Linux / macOS** (`mssql-odbc/tests/e2e/run_e2e.sh`) writes an `odbcinst.ini`
  registering `[ODBC Driver 18 for SQL Server (Rust)]` and points `ODBCSYSINI`
  at it.

mssql-python selects the driver by name in its connection string, so target the
Rust registration explicitly:

```
Driver={ODBC Driver 18 for SQL Server (Rust)};Server=localhost;Database=tempdb;UID=sa;PWD=...;TrustServerCertificate=Yes;
```

Run each scenario twice — once against this string, once against
`Driver={ODBC Driver 18 for SQL Server}` (msodbcsql18) — and compare. Parity with
msodbcsql18 is the pass criterion, mirroring the `-CompareWithMsodbcsql` gate the
C++ e2e already uses.

## Scenarios

Each maps to the plan's Definition of Done. "Expected" is the behavior that must
match msodbcsql18.

### (a) Same-physical-connection reuse after reset

- **Steps:** From a pool of size 1, acquire a connection, record `@@SPID`, create
  a `#temp` table, `USE` another database, raise isolation to `SERIALIZABLE`, open
  a transaction, then return the connection to the pool. Acquire again from the
  same pool.
- **Expected:** Same `@@SPID` (no new physical login); `@@TRANCOUNT = 0`; the
  `#temp` table is gone; `transaction_isolation_level` is READ COMMITTED (2); the
  database is back to the login default.
- **Backing in-repo:** `connection_pool_test.cpp::ResetRestoresCleanStateForNextBorrower`,
  `checkout_cycle_reuses_one_physical_connection` (unit).

### (b) Dead idle-connection discard

- **Steps:** Acquire a connection and record its `@@SPID`. From a *separate*
  admin connection, `KILL` that SPID. Return the connection to the pool and
  acquire again.
- **Expected:** The pool's checkout liveness probe
  (`SQLGetConnectAttr(SQL_ATTR_CONNECTION_DEAD)`) reports the killed connection
  dead (or its reset fails with `08S01`), so the pool **discards** it and hands
  out a fresh physical connection (new `@@SPID`). No query is ever run on the dead
  session. A never-connected / disconnected handle reports `SQL_CD_TRUE`.
- **Backing in-repo:** `get_connect_attr.rs` unit tests
  (`connection_dead_reports_true_when_*`), `reset_connection_poisons_client_on_failure`;
  live death-marking in `session_recovery_test.cpp`.

### (c) Prepared statements invalidated across reset

- **Steps:** Acquire, prepare and execute a statement (server-side prepared
  handle created), return to the pool (reset), acquire again, and execute the
  same logical statement.
- **Expected:** The second execution returns correct results. The reset ack
  cleared the client's session-bound prepared-handle cache, so the driver
  re-prepares against the fresh session instead of aliasing a dropped or unrelated
  server handle. No `08S01` / invalid-handle error.
- **Backing in-repo:** `connection_pool_test.cpp::PreparedStatementSurvivesResetViaReprepare`;
  `reset_connection_ack_clears_caches_and_restores_login_defaults` (TDS unit).

### (d) Concurrent checkout

- **Steps:** With a pool of size N, run N worker threads that each acquire, run a
  short query (e.g. `SELECT @@SPID`), and release, in a loop.
- **Expected:** No panics, no cross-talk, no "connection busy" errors from the
  driver; each borrower sees a clean session; `@@SPID`s are stable per physical
  connection and never interleave results. A busy connection (open cursor /
  active statement) is never handed out as idle, and a reset is never run on a
  busy connection (rejected with `HY000`/`08S01`).
- **Backing in-repo:** `reset_connection_rejects_busy_connection` (unit); the
  "no DBC mutex across network I/O" invariant in `txn.rs`.

### (e) Token-identity separation + near-expiry reconnect

- **Steps:** Configure the pool for AAD access-token auth (mssql-python sets the
  token pre-connect). Acquire/reset/reuse within one token's validity. Then
  present a **new/rotated** token and acquire.
- **Expected:** Reuse-with-reset never re-authenticates — it is the same physical
  login, and the auth/recovery context is preserved (see B6). A rotated token
  cannot be applied to a live session: setting `SQL_COPT_SS_ACCESS_TOKEN` after
  connect is rejected (`HY011`), so a new token forces a **new physical login**
  (fresh `SQLDriverConnect`), not a re-auth of the pooled session. Two connections
  authenticated with different token identities never share a physical session.
- **Backing in-repo:** `access_token_after_connect_is_rejected` (unit); the B6
  auth-preservation note on `TdsClient::reset_connection`.

### (f) Isolation-reset scenario (mssql-python #343)

- **Steps:** Acquire, set isolation to `SERIALIZABLE` via the connection
  attribute, return to the pool, re-acquire.
- **Expected:** The pool's checkout re-applies `READ COMMITTED`
  (`SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION, SQL_TXN_READ_COMMITTED)`), which our
  handler emits as a real `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` batch
  carrying the armed reset bit. `transaction_isolation_level` is 2 for the next
  borrower. **Documented shared limitation:** isolation changed via *raw T-SQL*
  (`SET TRANSACTION ISOLATION LEVEL ...`, bypassing the attribute) can leak,
  because `sp_reset_connection` does not reset isolation (D9) and mssql-python's
  #343 workaround only tracks the attribute path — this matches msodbcsql18 and is
  a Non-goal to "fix."
- **Backing in-repo:** `reset_then_checkout_isolation_reapplies_read_committed`
  (unit); `connection_pool_test.cpp::IsolationReturnsToReadCommittedEachCheckout`.

## Reference verification snippet

A minimal driver for scenarios (a) and (f), assuming `mssql-python` from GitHub
`main` and the Rust driver registered as above:

```python
import mssql_python

CONN = (
    "Driver={ODBC Driver 18 for SQL Server (Rust)};"
    "Server=localhost;Database=tempdb;UID=sa;PWD=your-password;"
    "TrustServerCertificate=Yes;"
)

def isolation(cur):
    cur.execute(
        "SELECT CAST(transaction_isolation_level AS int) "
        "FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
    )
    return cur.fetchone()[0]

# Pool of size 1 so both acquisitions land on the same physical connection.
mssql_python.pooling(max_size=1, idle_timeout=30)

c1 = mssql_python.connect(CONN)
cur1 = c1.cursor()
cur1.execute("SELECT @@SPID"); spid1 = cur1.fetchone()[0]
cur1.execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")  # via T-SQL: see (f) caveat
c1.close()  # returns to pool

c2 = mssql_python.connect(CONN)
cur2 = c2.cursor()
cur2.execute("SELECT @@SPID"); spid2 = cur2.fetchone()[0]
assert spid1 == spid2, "same physical connection expected"
assert isolation(cur2) == 2, "checkout must restore READ COMMITTED"
c2.close()
```

Run the same script against `Driver={ODBC Driver 18 for SQL Server}` and confirm
identical results. Diverging behavior between the two drivers is the signal to
investigate (per Workstream C's "diagnose any behavior differences vs.
msodbcsql18").

## CI wiring (future)

If this cross-product check is later automated in CI, prefer extending the
existing containerized ODBC e2e job (`.pipeline/scripts/containerized-odbc-e2e.sh`,
which already owns a SQL Server and registers the driver) with a Python step that
`pip install`s `mssql-python` and runs the snippets above against both driver
names, rather than adding a standalone harness. Until then, this document is the
manual verification procedure.

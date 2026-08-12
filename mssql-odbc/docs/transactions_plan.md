# Transactions — `SQLEndTran` / `SQL_ATTR_AUTOCOMMIT` / `SQL_ATTR_TXN_ISOLATION`

Plan for connection-scoped transaction support in the ODBC Driver 18 (Rust),
covering spec §4.9.

Two reference implementations drive the design:

- **`msodbcsql`** (`C:\work\msodbcsql\Sql\Ntdbms\sqlncli`) — the C++ ODBC driver
  we must be wire- and behavior-compatible with. Every design decision below
  cites the file and line it is derived from.
- **`mssql-python`** (`microsoft/mssql-python`) — the primary consumer. Its DDBC
  binding (`connection.py` / `connection.cpp` / `helpers.py`) defines the exact
  ODBC call sequence the driver must satisfy.

---

## 1. Goals

### G1 — Manual-commit mode (`SQL_ATTR_AUTOCOMMIT`)

`SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT, SQL_AUTOCOMMIT_OFF)` puts the connection
in manual-commit mode; the driver keeps a transaction open until `SQLEndTran`.
`SQLGetConnectAttr(SQL_ATTR_AUTOCOMMIT)` reads it back. Settable both before and
after connect. Switching OFF→ON commits any open transaction and returns
`SQL_SUCCESS_WITH_INFO` with SQLSTATE `01000`.

> **Driver default is `SQL_AUTOCOMMIT_ON`, not OFF.** The spec bullet
> "connections open with autocommit = False" describes *mssql-python's*
> `Connection.__init__` default, which it achieves by issuing an explicit
> `SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT, SQL_AUTOCOMMIT_OFF)` immediately after
> `SQLDriverConnect` (`connection.py:589`). The ODBC specification and
> `msodbcsql` (`sqlcfunc.cpp:3462`, `SQL_AUTOCOMMIT_DEFAULT = SQL_AUTOCOMMIT_ON`)
> both default the *driver* to autocommit ON. Defaulting the driver to OFF would
> diverge from msodbcsql and break every non-mssql-python ODBC application. The
> driver's job is to **support** manual-commit; the manual-commit *default* is
> the language binding's policy.

### G2 — `SQLEndTran(SQL_HANDLE_DBC, hdbc, SQL_COMMIT | SQL_ROLLBACK)`

Commits or rolls back the connection's transaction. Also supports
`SQL_HANDLE_ENV`, fanning out over every connection owned by that environment and
promoting the worst return code (`sqlctran.cpp:29-41`).

### G3 — `SQL_ATTR_TXN_ISOLATION`

Accepts `READ UNCOMMITTED` / `READ COMMITTED` / `REPEATABLE READ` /
`SERIALIZABLE` / `SQL_TXN_SS_SNAPSHOT`. Default `SQL_TXN_READ_COMMITTED`.
Applied by emitting `SET TRANSACTION ISOLATION LEVEL <level>`
(`sqlcstr.cpp:56-60`). Readable back via `SQLGetConnectAttr`.

`SQL_TXN_SS_SNAPSHOT` is implemented for msodbcsql parity but is unreachable
through the Windows Driver Manager, which screens the attribute value itself —
see §7.1.

### G4 — Transaction-safe disconnect

`SQLDisconnect` with a user-started transaction open posts SQLSTATE `25000` and
returns `SQL_ERROR` (`sqlcconn.cpp:1234-1238`). A driver-started *piggyback*
transaction — one auto-begun because autocommit is OFF but carrying no
user-visible uncommitted work — is rolled back best-effort before teardown, so
no transaction is ever left dangling on a pooled server session.

### G5 — `SQLGetInfo` transaction capabilities

| Info type | Value | C type |
|---|---|---|
| `SQL_TXN_CAPABLE` (46) | `SQL_TC_ALL` (2) | `SQLUSMALLINT` |
| `SQL_DEFAULT_TXN_ISOLATION` (26) | `SQL_TXN_READ_COMMITTED` (2) | `SQLUINTEGER` |
| `SQL_TXN_ISOLATION_OPTION` (72) | `0x2F` | `SQLUINTEGER` |
| `SQL_MULTIPLE_ACTIVE_TXN` (37) | `"Y"` | nul-terminated string |
| `SQL_CURSOR_COMMIT_BEHAVIOR` (23) | `SQL_CB_CLOSE` (1) | `SQLUSMALLINT` |
| `SQL_CURSOR_ROLLBACK_BEHAVIOR` (24) | `SQL_CB_CLOSE` (1) | `SQLUSMALLINT` |

Source: `sqlcinfo.cpp:300-349`.

### G6 — Cursor-close semantics on commit/rollback

`SQLEndTran` closes every open cursor on the connection before ending the
transaction, matching the advertised `SQL_CB_CLOSE`
(`sqlctran.cpp:302-323`).

### G7 — Transaction entry points advertised through `SQLGetFunctions`

`SQL_API_SQLENDTRAN` (1005) reported supported in both the ODBC-2 array and the
ODBC-3 bitmap. `SQL_API_SQLGETCONNECTATTR` (1007) must be advertised alongside
it: the Windows Driver Manager short-circuits `SQLGetConnectAttrW` with `IM001`
otherwise, which makes both transaction attributes write-only from an
application — including for `mssql_python`, whose `autocommit` property issues a
live `SQLGetConnectAttr` on every read (`connection.py`).

---

## 2. Non-goals

Capabilities present in `msodbcsql` that this work deliberately does **not**
implement. Each is inert in `mssql-python` too, which is why none of them blocks
the binding.

| # | msodbcsql capability | Source | Why out of scope |
|---|---|---|---|
| N1 | **Distributed transactions (MSDTC)** — `SQL_ATTR_ENLIST_IN_DTC` / `SQL_COPT_SS_ENLIST_IN_DTC`, `SQLEnlistTransaction`, `TM_GET_DTC_ADDRESS`, `TM_PROPAGATE_XACT`, SQLSTATE `25S12` | `sqlcmisc.cpp:2065`, `sqlctran.cpp:109` | Windows-only MSDTC/OLE dependency. mssql-python defines the constant but omits it from `validate_attribute_value`'s allowlist (`helpers.py:162-169`), so `set_attr` raises `ProgrammingError`. |
| N2 | **XA transactions** — `SQL_ATTR_ENLIST_IN_XA` / `SQL_COPT_SS_ENLIST_IN_XA`, `StartXATransaction`, `pXaCaller` | `sqlcmisc.cpp:2079-2143` | Same as N1; unreachable from mssql-python. |
| N3 | **Transaction promotion / propagation** — `TM_PROMOTE_XACT`, `TM_PROPAGATE_XACT` | `viperrm.h:32-37` | Only meaningful with N1/N2. `mssql-tds` exposes `get_dtc_address()` but nothing consumes it. |
| N4 | **Savepoints** — `SAVE TRANSACTION` / `TM_SAVE_XACT` | Zero matches in msodbcsql | msodbcsql has **no** ODBC savepoint surface at all. `mssql-tds::save_transaction` exists but stays unused; ODBC has no savepoint API to expose it through. |
| N5 | **Explicit `BEGIN TRANSACTION` API** | — | Neither ODBC nor msodbcsql exposes one. Transactions begin implicitly from autocommit-OFF mode. |
| N6 | **Nested transactions** | — | SQL Server has no true nesting; `@@TRANCOUNT` nesting is a T-SQL concern the application drives itself. |
| N7 | **`SQL_COPT_SS_PRESERVE_CURSORS`** | `msodbcsql.h:181`, `sqlctran.cpp:302-323` | SQL Server-specific extension letting cursors survive commit. We hardcode the default `SQL_PC_OFF` behavior (`SQL_CB_CLOSE`) and reject the attribute with `HYC00`. |
| N8 | **`SQL_COPT_SS_AUTOBEGINTXN`** | `msodbcsql.h:275` | Round-trip optimization that folds "begin next transaction" into the commit TM request (`TM_BEGIN_NEW_XACT`). We instead auto-begin lazily before the next statement — identical observable behavior (see §4.4), one extra round trip in the worst case. |
| N9 | **Pre-Yukon (SQL Server ≤ 2000) fallbacks** — `SET IMPLICIT_TRANSACTIONS ON/OFF`, `IF @@TRANCOUNT > 0 COMMIT TRAN` | `sqlcstr.cpp:69-70`, `sqlcconn.cpp:3639-3688` | `mssql-tds` targets TDS 7.4+. The Yukon+ TDS transaction-manager path is the only one implemented. |
| N10 | **`SQL_ATTR_RESET_CONNECTION` / connection pooling reset** | `sqlcmisc.cpp:2373-2463` | Pooling is not implemented in this driver yet. mssql-python's `Connection::reset` (`connection.cpp:478-524`) needs it, but it is a separate work item; the isolation-reset half of it is already covered by G3. |
| N11 | **DM-faking `SQL_CB_PRESERVE` first-call trick** | `sqlcinfo.cpp:1139-1154` | Windows Driver Manager work-around that returns `SQL_CB_PRESERVE` on the *first* `SQL_CURSOR_COMMIT_BEHAVIOR` call to suppress statement re-preparation. We always return the truthful `SQL_CB_CLOSE`. Deliberate, documented divergence. |
| N12 | **`SQLTransact` (ODBC 2.x)** | Not defined in msodbcsql either | The Driver Manager maps `SQLTransact` onto `SQLEndTran`; no driver export is needed. |
| N13 | **MARS-specific transaction interaction** | `sqlccmd.cpp:10360` | MARS is not implemented in this driver. |
| N14 | **`SET XACT_ABORT` management** | — | Not emitted by msodbcsql either; purely an application concern. |

---

## 3. ODBC ↔ TDS isolation-level mapping

ODBC uses a **bitmask**; TDS uses a **dense byte**. They are not interchangeable
and require an explicit table.

| ODBC constant | ODBC value | TDS `TransactionIsolationLevel` | TDS byte | T-SQL emitted |
|---|---|---|---|---|
| `SQL_TXN_READ_UNCOMMITTED` | `0x01` | `ReadUncommitted` | `0x01` | `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED` |
| `SQL_TXN_READ_COMMITTED` | `0x02` | `ReadCommitted` | `0x02` | `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` |
| `SQL_TXN_REPEATABLE_READ` | `0x04` | `RepeatableRead` | `0x03` | `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ` |
| `SQL_TXN_SERIALIZABLE` | `0x08` | `Serializable` | `0x04` | `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` |
| `SQL_TXN_SS_SNAPSHOT` | `0x20` | `Snapshot` | `0x05` | `SET TRANSACTION ISOLATION LEVEL SNAPSHOT` |

Isolation is applied as a **T-SQL batch**, exactly as msodbcsql does
(`sqlcmisc.cpp:1760`), not through the TM request's `bIsoLevel` field. The
auto-begin TM request therefore carries `NoChange` (`0x00`) and inherits the
session-level isolation set by the `SET` statement.

---

## 4. Design

### 4.1 Two transaction flags

Mirrors msodbcsql's `CONN_ST_LOCALTRANS_STARTED` vs `FIsLocalTranActive`
(`sqlcprot.h:2298-2334`):

| Flag | Source of truth | Meaning |
|---|---|---|
| `local_tran_started: bool` on `DbcState` | Driver-side | The application executed a statement in manual-commit mode, so there may be uncommitted user work. Set before statement execution; cleared by `SQLEndTran` and by autocommit changes. |
| `TdsClient::has_active_transaction()` | TDS `ENVCHANGE` descriptor | A server-side transaction genuinely exists — including one begun by a T-SQL `BEGIN TRAN` inside a stored procedure. |

The distinction matters at disconnect (G4): only `local_tran_started` blocks it.

### 4.2 `SQLEndTran`

```
validate completion_type ∈ {SQL_COMMIT, SQL_ROLLBACK}   → else HY012
if handle_type == SQL_HANDLE_ENV: fan out over child DBCs, promote worst rc
if !local_tran_started:                                  → SQL_SUCCESS (silent no-op)
close all open cursors on the connection                  (SQL_CB_CLOSE)
if client.has_active_transaction():
    commit_transaction(None, None) | rollback_transaction(None, None)
local_tran_started = false
```

The `!local_tran_started` early return reproduces `sqlctran.cpp:293` exactly —
committing with nothing open is a silent success, never a warning or error.

### 4.3 Autocommit transitions

| Transition | Action |
|---|---|
| same value | no-op `SQL_SUCCESS` (`sqlcmisc.cpp:1720`) |
| not connected | store only; applied post-connect |
| ON → OFF | `begin_transaction(NoChange)` if no active txn (`sqlcconn.cpp:3692`); `local_tran_started = false` |
| OFF → ON, user txn open | **commit** it, post `01000`, return `SQL_SUCCESS_WITH_INFO` (`sqlcconn.cpp:3696-3700`) |
| OFF → ON, piggyback txn only | **roll back** silently, no warning (`sqlcconn.cpp:3712`) |

### 4.4 Auto-begin before statement execution

msodbcsql's `CheckOptions` (`sqlccmd.cpp:10572-10585`) re-issues `TM_BEGIN_XACT`
before every statement when autocommit is OFF and no server transaction is
active. This recovers from a transaction the server aborted (`XACT_ABORT`,
deadlock victim, timeout) or one the user rolled back with raw T-SQL. We
replicate it in the shared execute path, which also makes N8 unobservable:
after `SQLEndTran(SQL_COMMIT)`, the next statement re-begins the transaction, so
`SELECT @@TRANCOUNT` reports the same value it would under msodbcsql.

`local_tran_started` is set at the same point.

### 4.5 Isolation level

```
value ∉ {1, 2, 4, 8, 0x20}      → HYC00, SQL_ERROR   (sqlcmisc.cpp:347-355)
local_tran_started              → HY011, SQL_ERROR   (sqlcmisc.cpp:360-364)
not connected                   → store; applied at connect
connected                       → SET TRANSACTION ISOLATION LEVEL <level>
```

`SQLGetConnectAttr` returns the cached value — no server round trip, matching
`sqlcmisc.cpp:3426-3433`.

### 4.6 Disconnect

```
if local_tran_started            → post 25000, return SQL_ERROR   (sqlcconn.cpp:1234)
else if has_active_transaction() → best-effort rollback, ignore failure
free child statements; drop client; state = Disconnected
```

---

## 5. Error / SQLSTATE matrix

| Condition | SQLSTATE | Return |
|---|---|---|
| `SQLEndTran` bad completion type | `HY012` | `SQL_ERROR` |
| `SQLEndTran` bad handle type | `HY092` | `SQL_ERROR` |
| `SQLEndTran`, no transaction open | — | `SQL_SUCCESS` |
| `SQLEndTran` on disconnected DBC | `08003` | `SQL_ERROR` |
| `SQL_ATTR_TXN_ISOLATION` invalid value | `HYC00` | `SQL_ERROR` |
| `SQL_ATTR_TXN_ISOLATION` while txn open | `HY011` | `SQL_ERROR` |
| `SQL_ATTR_AUTOCOMMIT` invalid value | `HY024` | `SQL_ERROR` |
| Autocommit OFF→ON with open txn | `01000` | `SQL_SUCCESS_WITH_INFO` |
| `SQLDisconnect` with open txn | `25000` | `SQL_ERROR` |

---

## 6. Files touched

| File | Change |
|---|---|
| `api/odbc_types.rs` | Transaction constants, `SQL_API_SQLENDTRAN`, `SQL_API_SQLGETCONNECTATTR` |
| `api/txn.rs` | **new** — shared autocommit / isolation / begin / end logic |
| `api/end_tran.rs` | **new** — `SQLEndTran` |
| `api/mod.rs`, `api/exports.rs`, `api/get_functions.rs` | Register / export / advertise |
| `api/sqlstate.rs` | `25000`, `HY012` and their diagnostic messages |
| `handles/dbc.rs` | `autocommit`, `txn_isolation`, `local_tran_started` |
| `api/set_connect_attr.rs`, `api/get_connect_attr.rs` | Both attributes |
| `api/get_info.rs` | Four info types |
| `api/disconnect.rs` | Replace the `25000` TODO |
| `api/exec_common.rs` | Auto-begin + `local_tran_started` |
| `api/driver_connect.rs` | Apply pre-connect attributes |

---

## 7. Test matrix

Rust unit tests (`TestHandles`) cover argument validation, state transitions and
diagnostics without a server. C++ e2e tests
(`tests/e2e/tests/transaction_test.cpp`) run against a live SQL Server under
`run_e2e.ps1 -CompareWithMsodbcsql`, so every assertion must hold for **both**
drivers unless guarded by `SKIP_IF_COMPARING_MSODBCSQL()`.

| # | Scenario | Layer |
|---|---|---|
| 1 | Autocommit defaults to ON | unit + e2e |
| 2 | Set/get autocommit round-trip both ways | unit + e2e |
| 3 | Setting the same value twice is a no-op | unit |
| 4 | Invalid autocommit value → `HY024` | unit + e2e |
| 5 | Commit persists an insert | e2e |
| 6 | Rollback discards an insert | e2e |
| 7 | Autocommit ON → insert is immediately durable | e2e |
| 8 | `SQLEndTran` with no open txn → `SQL_SUCCESS` | unit + e2e |
| 9 | `SQLEndTran` invalid completion type → `HY012` | unit + e2e |
| 10 | `SQLEndTran` invalid handle type → `HY092` | unit |
| 11 | `SQLEndTran` on `SQL_HANDLE_ENV` fans out | e2e |
| 12 | `SQLEndTran` on disconnected DBC → `08003` | unit |
| 13 | `SQLEndTran` null handle → `SQL_INVALID_HANDLE` | unit |
| 14 | Isolation default is `READ COMMITTED` | unit + e2e |
| 15 | All four ODBC-standard isolation levels round-trip | unit + e2e |
| 16 | Isolation actually applied server-side (`sys.dm_exec_sessions`) | e2e |
| 17 | Invalid isolation → `HYC00` | unit |
| 17b | Invalid isolation through the Windows DM → `HY024` (DM screens the attribute; see below) | e2e |
| 17c | `SQL_TXN_SS_SNAPSHOT` accepted by the driver, rejected by the Windows DM | unit + e2e |
| 18 | Isolation set mid-transaction → `HY011` | e2e |
| 19 | Isolation set before connect is applied at connect | e2e |
| 20 | Autocommit OFF→ON commits open work + `01000` | e2e |
| 21 | Disconnect with open txn → `25000` | e2e |
| 22 | Cursors closed by commit/rollback | e2e |
| 23 | Transaction survives across multiple statements | e2e |
| 24 | Rollback after server-side `BEGIN TRAN` in a proc | e2e |
| 25 | All six `SQLGetInfo` values | unit + e2e |
| 26 | `SQLGetFunctions` reports `SQL_API_SQLENDTRAN` and `SQL_API_SQLGETCONNECTATTR` | unit + e2e |

### 7.1 What the Driver Manager decides for us

Two behaviours are owned by the Windows Driver Manager, not by either driver, and
the e2e tests assert the observable result rather than the driver's own answer:

- **`SQL_ATTR_TXN_ISOLATION` values are screened by the DM.** Anything outside
  `{SQL_TXN_READ_UNCOMMITTED, READ_COMMITTED, REPEATABLE_READ, SERIALIZABLE}` is
  rejected with `HY024` before the driver is called — including
  `SQL_TXN_SS_SNAPSHOT`, and including on msodbcsql. The driver's own `HYC00`
  (msodbcsql's answer in `SetTxnIsolation`, `sqlcmisc.cpp`) is therefore asserted
  by unit tests, which have no DM in the path. Applications reach SNAPSHOT
  through `SQL_COPT_SS_TXN_ISOLATION` or `SET TRANSACTION ISOLATION LEVEL`, both
  out of scope here.
- **`SQL_CURSOR_COMMIT_BEHAVIOR` is cached by the DM.** msodbcsql answers
  `SQL_CB_PRESERVE` to the DM's first query (`sqlcinfo.cpp`) so the DM will not
  close cursors on its behalf; the DM then rejects the next statement with
  `24000` until the application closes the cursor explicitly. mssql-odbc answers
  truthfully (`SQL_CB_CLOSE`, non-goal N11), so a statement is reusable straight
  after a commit. The shared part — the cursor is gone and the commit persisted —
  is asserted on both drivers; the divergence lives in its own
  `SKIP_IF_COMPARING_MSODBCSQL()` test.

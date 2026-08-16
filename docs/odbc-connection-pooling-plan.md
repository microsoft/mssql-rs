# Plan: Connection Pool Constructs in `mssql-odbc` (ADO #47317)

## Goal
Let `mssql-python`'s existing client-side pool safely reuse a physical Rust ODBC
connection. **No second pool in Rust.** We only supply the ODBC/TDS reset,
liveness, transaction, and isolation semantics that make reuse safe.

## mssql-python consumer analysis (verified against `C:\work\mssql-python`)
Confirmed the planned ODBC surface is **exactly** what mssql-python's pool calls. The pool is C++ (`pybind/connection/connection_pool.cpp`), one pool per connection string, keyed/pruned/sized entirely in `mssql-python` — **no second Rust pool** (aligned with the non-goal). Checkout (`ConnectionPool::acquire`):
1. `conn->isAlive()` → **`SQLGetConnectAttr(SQL_ATTR_CONNECTION_DEAD)`**, reuse only if `SQL_CD_FALSE` (`connection.cpp:304-313`). → **needs B2.**
2. `conn->reset()` → **`SQLSetConnectAttr(SQL_ATTR_RESET_CONNECTION, SQL_RESET_CONNECTION_YES)`**; on failure it `disconnect()`s and discards (`connection.cpp:315-332`). → **needs B1/B3.**
3. Then the Python `Connection.__init__` calls `setautocommit(autocommit)` (default `autocommit=False`) → **`SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT)`** (`connection.py:242`). → **needs B5.**
4. `close()`/check-in path rolls back when not autocommit → **`SQLEndTran` rollback** (`connection.py:1415-1421`). → **needs B5.**
5. `reset()` issues **`SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION, SQL_TXN_READ_COMMITTED)`** on **every** checkout, immediately after arming the reset bit (`connection.cpp`, added by **[mssql-python #343](https://github.com/microsoft/mssql-python/pull/343)** / AB#40573 / GH #337). This is a deliberate workaround: `sp_reset_connection` does **not** reset the isolation level (see D9), so the pool re-applies it explicitly. → **needs B4** (our `SQL_ATTR_TXN_ISOLATION` handler must emit a real `SET TRANSACTION ISOLATION LEVEL READ COMMITTED`, and that batch carries the armed reset bit).

**Answer: yes — the planned work is required for mssql-python, and it is also sufficient.** Every pool primitive maps to a plan item (B1/B2/B3/B4/B5), and Workstream A is what makes the reset actually clean session-bound caches. Nothing extra is needed; nothing planned is unused.

**Correction (2026-08-14): the original B4 framing was right; an earlier "reframe" was based on a pre-#343 checkout of `C:\work\mssql-python`.** The pool **does** issue `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` on checkout. So the reset bit is carried by that isolation request exactly as the work item assumed:
- **The isolation SET is the reset carrier.** After `reset()` arms the bit, the very next request is `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION, READ_COMMITTED)`; when the previous borrower had raised isolation via the attribute, our cache differs (`txn.rs:487`) and a real SET batch goes out carrying the reset bit — reset + isolation re-apply in one round trip, server order being reset-first then SET. ✅ Satisfies acceptance criterion #4.
- **Same-value short-circuit caveat (autocommit-default connections):** if the borrower never changed isolation, the checkout's `READ_COMMITTED` equals the cached value and `txn.rs:487` short-circuits with no I/O, so the reset bit instead rides `setautocommit(False)`'s begin-transaction (autocommit-off, the mssql-python default) or the borrower's first statement. Still leak-free; just not "acked inside the isolation call."
- **A2 remains recommended** so `SQL_ATTR_RESET_CONNECTION` itself is self-acking regardless of the short-circuit, guaranteeing ack-before-checkout uniformly and letting the pool discard a failed-reset connection at checkout rather than failing the borrower's first query.

## Current state (what already exists — do NOT rebuild)
- `mssql-tds`
  - `TdsClient::prepare_reset_connection(bool)` — one-shot RESETCONNECTION / RESETCONNECTIONSKIPTRAN bit on the next batch/RPC/txn-mgr request. ✅
  - `TdsClient::is_connection_dead()` — cached, socket-free liveness read. ✅
  - Reconnect path (`tds_client.rs:460-486`) already clears `prepared_handles`, `prepared_param_encryption`, `pending_capture`, `pending_prepared_param_encryption`, and resets `session_state_table`. ✅
  - Live reset tests: `tests/test_reset_connection.rs`, `tests/test_connection_pool_primitives.rs`. ✅
- `mssql-odbc`
  - `DbcState` owns `TdsClient` behind the DBC mutex on a shared Tokio runtime; take-client / release-lock / I/O / restore pattern in place. ✅
  - `claim_dbc_client` / `release_dbc_client` — DBC-level idle-client claim/restore helpers already exist (`api/txn.rs:76-104`). ✅ (work item's "add a DBC-level claim/restore helper" is already satisfied — reuse it.)
  - `SQL_ATTR_AUTOCOMMIT` set/get + `SQLEndTran` commit/rollback with real server semantics (`api/txn.rs`, `api/end_tran.rs`). ✅
  - `SQL_ATTR_TXN_ISOLATION` (+ `SQL_COPT_SS_TXN_ISOLATION`) applies a real `SET TRANSACTION ISOLATION LEVEL` batch and caches the value (`api/txn.rs:462`). ✅

## msodbcsql source review — discoveries (added after reading `C:\work\msodbcsql`)
Reviewed the real driver: `Sql/Ntdbms/sqlncli/odbc/{sqlcmisc.cpp, sqlcfunc.cpp, sqlcconn.cpp, sqlctokn.cpp, sqlcerr.cpp, dbcinfotoken.cpp}`. These change/confirm the plan:

- **D1 — `SQL_ATTR_CONNECTION_DEAD` default polarity is DEAD.** msodbcsql initializes `dwOptions[SQL_ATTR_CONNECTION_DEAD] = SQL_CD_TRUE` at alloc/reset (`sqlcfunc.cpp:3473`, `sqlcconn.cpp:5521/5957`) and flips it to `SQL_CD_FALSE` only after a successful token read (`sqlctokn.cpp:2099`). **Confirms B2**: disconnected/never-connected ⇒ report `SQL_CD_TRUE`.
- **D2 — the default get is cached, no probe.** The network-probe path is behind the opt-in legacy compat flag `fConnectionPeek` (default OFF); default just returns cached `dwOptions` (`sqlcmisc.cpp:3108-3116`). **Confirms B2's no-probe design is the msodbcsql default**, not a shortcut.
- **D3 — liveness-marking trigger gap (VERIFIED — real gap, needs a small fix).** msodbcsql also marks the connection dead on a **server fatal error token** (`bClass >= MINFATALERR`, `sqlctokn.cpp:3265`) and on `08S01`/`70100` errors (`sqlcerr.cpp:1636`), even when the socket is still readable. **Verified in mssql-tds:** the `Tokens::Error` handlers (`tds_client.rs:1221/2422/2626/3992/4417`) only push `SqlErrorInfo` (severity kept as `class`); **nothing sets `known_dead` from severity.** `known_dead` is set *only* on socket write/read failure, EOF, or explicit close (`network_transport.rs:575/740/890/895/961/1554`). So after a class ≥ 20 fatal error on a still-open socket, mssql-rs reports `SQL_CD_FALSE` where msodbcsql reports dead. **Action (new, small):** add `mark_known_dead()` on the transport and call it from the shared error-token path when `severity >= 20`; add a unit test. Closes "busy/dead cannot be confused."
- **D4 — RESET does far more than arm a bit (`sqlcmisc.cpp:2373-2461`).** On `SQL_COPT_SS_RESET_CONNECTION`/`SQL_ATTR_RESET_CONNECTION` msodbcsql: (a) if a local txn is active, **rolls it back first** via `TM_ROLLBACK_XACT`; (b) arms `ResetConnection(TRUE)` (non-skiptran); (c) resets client-side `cchTextsize`/`crow`; (d) **restores cached DATABASE/LANGUAGE/COLLATION to login `Init*` values** (`conninfo.InitDataBase/InitLanguage/InitCollation`, saved at login `sqlctokn.cpp:2112`); (e) `pConnectionRecoveryManager->ResetCurrentState()`; (f) clears `CONN_ST_NO_BROWSE_TABLE`; (g) clears local-tran status. **Refines A1 + B3** (see below).
- **D5 — SET-options re-sync (VERIFIED — NOT needed for mssql-rs).** `BuildServerSideConnectOptions` (`sqlcfunc.cpp:1976`) re-emits `IMPLICIT_TRANSACTIONS`/isolation/`ANSI_NPW`/`CONCAT_NULL`/`QUOTED_IDENTIFIER` etc. after reset. **Verified in mssql-tds:** `ClientContext` (`client_context.rs:216`) has **no** ANSI SET-option fields (no quoted_identifier/ansi_nulls/ansi_warnings/concat_null/arithabort), and the connect path (`tds_connection_provider.rs:92-120` → action chain → LOGIN7 handshake) runs **no** post-login SET batch — the only `SET` statements in the crate are `SET FMTONLY` (metadata) and test SQL. ANSI defaults ride the **`fODBC` LOGIN7 bit** (`login_options.rs:352 OptionOdbc::On`), which `sp_reset_connection` restores automatically. **Isolation is the exception (see D9):** `sp_reset_connection` does not restore it, so mssql-python re-applies READ COMMITTED on checkout and our `SQL_ATTR_TXN_ISOLATION` handler must honor that. **Conclusion: no `BuildServerSideConnectOptions` equivalent is required for ANSI options; isolation is handled by B4.** Re-check only if a future change starts applying connection-string SET options via a post-login batch.
- **D6 — "acked before checkout" is carried by the isolation SET.** The pool's checkout `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` (D9/#343) is the request that carries the armed reset bit, so msodbcsql/mssql-python effectively process+ack the reset within that checkout call (when it emits I/O). **A2 still recommended:** when the borrower never changed isolation the checkout SET equals the cached value and short-circuits with no I/O (`txn.rs:487`), so the reset would ride a later request. A small reset-completion round-trip inside `SQL_ATTR_RESET_CONNECTION` makes acceptance criterion #4 ("reset processed and acknowledged before pool checkout succeeds") hold unconditionally.
- **D7 — error mapping for RESET.** Value must be `SQL_RESET_YES(1)` else `HY024` (`sqlcmisc.cpp:2374`); reset on a disconnected DBC ⇒ `08003` (`dbcinfotoken.cpp:359-363`). Verify `claim_dbc_client`'s `ERR_CONNECTION_DOES_NOT_EXIST` maps to `08003`, and reject non-`YES` values with `HY024` in B3.
- **D8 — recovery lock (minor).** `SQL_ATTR_CONNECTION_DEAD` get takes the recovery critical section when session recovery is enabled (`sqlcmisc.cpp:2995-2998`). In mssql-rs, peeking liveness under the short DBC mutex is the equivalent — do not `take()` the client.
- **D9 — `sp_reset_connection` does NOT reset transaction isolation level (VERIFIED against `mssql-python` tip + msodbcsql).** This is a documented SQL Server limitation. **mssql-python's pool works around it explicitly:** `Connection::reset()` on GitHub `main` (SHA `3502aba`, from **[PR #343](https://github.com/microsoft/mssql-python/pull/343)** / AB#40573 / GH #337) issues **two** ODBC calls on every checkout — first `SQLSetConnectAttr(SQL_ATTR_RESET_CONNECTION, YES)` (lines 500-501), then `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION, SQL_TXN_READ_COMMITTED)` (lines 514-515) — with the comment *"SQL_ATTR_RESET_CONNECTION does NOT reset the transaction isolation level."* **msodbcsql matches this:** the driver re-emits `SET TRANSACTION ISOLATION LEVEL <x>` itself (`sqlcstr.cpp:54-60`, `sqlcfunc.cpp:2007` startup, `sqlcmisc.cpp:1760-1827` on set-attr); the login default it restores to is READ COMMITTED (`sqlcconn.cpp:5494`, `sqlcfunc.cpp:3463`). **Impact on mssql-rs:** our `SQL_ATTR_TXN_ISOLATION` handler already emits a real `SET TRANSACTION ISOLATION LEVEL` batch and caches the level (`api/txn.rs:462`), so it satisfies the workaround — **B4 must keep this path working and ensure the checkout SET carries the armed reset bit.** **Caveat:** if a borrower changed isolation via raw T-SQL (not the attribute), our cache is stale and the checkout READ COMMITTED short-circuits (`txn.rs:487`) → isolation leaks. mssql-python's fix/test only cover the attribute path, so this is a **shared, documented limitation** (see Non-goals), not a regression we introduce.
  - **Stale-local-clone note:** my local `C:\work\mssql-python` is the ADO mirror at tip `e645151 (RELEASE 1.0.0)`, which predates #343 — its `reset()` has only the single reset call. **Authoritative source is GitHub `main`.** Refresh the local clone before future reads.
- **D10 — a *Driver-Manager-mediated* application cannot set `SQL_ATTR_RESET_CONNECTION` on Windows (VERIFIED against both drivers on the Windows e2e legs).** Attribute 116 is reserved for **DM → driver** communication: the DM sets it itself just before returning a connection to *its own* pool, and the ODBC 3.8 driver guidance states an application cannot set it directly. The Windows DM enforces that, answering `HY092` ("Option type out of range") before the call reaches any driver — the installed msodbcsql18 fails identically, so this is not a gap in our driver. This is why msodbcsql defines the vendor attribute `SQL_COPT_SS_RESET_CONNECTION` (`SQL_COPT_SS_BASE_EX+6` = 1246, value `SQL_RESET_YES`, `odbcss.h`): vendor-range attributes are passed through untouched, so that is the spelling a DM-mediated app uses on Windows. unixODBC applies no such gate, so 116 reaches the driver on Linux/macOS.
  - **Scope — the gate is the DM's, so it does not apply to callers that bypass the DM.** `mssql-python` loads the driver library directly (`LoadDriverLibrary()` in `mssql_python/pybind/ddbc_bindings.cpp` — `LoadLibraryW`/`dlopen`), binds the exports via `GetProcAddress`/`dlsym` (`SQLSetConnectAttr_ptr` et al.), and invokes them itself. Its `SQL_ATTR_RESET_CONNECTION = 116` therefore reaches our exported `SQLSetConnectAttrW` unchanged on every platform, Windows included — nothing to change upstream.
  - **Impact:** B1/B3 must accept **both** identifiers on every platform: 116 for the direct-loading consumer that ships this driver, and 1246 for DM-mediated callers on Windows (including our own C++ e2e suite, which links the DM). e2e tests select the spelling their transport requires. Note the DM gate is about *who may set the attribute*, not about the driver's advertised ODBC version — registering `DriverODBCVer = "03.80"` does **not** lift it.

## The actual gaps
1. `mssql-tds`: ResetConnection ENVCHANGE **acknowledgement** is handled in ~5 token-draining spots (`tds_client.rs:1232, 2431, 2607, 3975, 4429`) and each only calls `session_state_table.reset()`. On a real server reset the session-bound client caches are now stale but are **not** cleared, and negotiated login defaults are not restored.
2. `mssql-odbc`: `SQL_ATTR_CONNECTION_DEAD` (get) not wired.
3. `mssql-odbc`: `SQL_ATTR_RESET_CONNECTION` (set) not wired.
4. `mssql-odbc`: the pool-checkout `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` (issued by mssql-python's `reset()`, D9) must *carry* the pending reset bit, and our `SQL_ATTR_TXN_ISOLATION` handler must apply it as a real server-side change so isolation does not leak (D9 is a SQL Server limitation `sp_reset_connection` does not cover).

---

## Workstream A — `mssql-tds` (small–medium, ~1–3 days)

### A1. Centralize reset-ack state transition
- Add one private method, e.g. `fn on_reset_connection_ack(&mut self)`, that performs the **full** post-reset transition:
  - `self.recovery_context.session_state_table.reset()`
  - `self.prepared_handles.clear()`
  - `self.prepared_param_encryption.clear()`
  - `self.pending_capture = None`
  - `self.pending_prepared_param_encryption = None`
  - restore negotiated login defaults into `negotiated_settings` — **specifically DATABASE, LANGUAGE, and COLLATION back to the login values** (msodbcsql D4: restores `InitDataBase/InitLanguage/InitCollation`; borrower A's `USE otherdb` / `SET LANGUAGE` must not persist). Factor the reconnect path's default-restore so both call one helper.
  - reset any other session-recovery state that `session_state_table.reset()` does not already cover (msodbcsql D4: `ResetCurrentState()`).
- Replace all `if sub_type == ResetConnection { session_state_table.reset() }` sites with a single call to this method (grep `EnvChangeTokenSubType::ResetConnection`). Keep `capture_change_property` after it.
- Audit output-parameter / return-value encryption state and any other session-bound caches for cross-borrower leakage; clear whatever the reconnect path clears.
- **D5 (verified — no action):** confirmed no connection-string SET options are applied via a post-login batch (only via the `fODBC` LOGIN7 bit), so `sp_reset_connection` restores them automatically. No `BuildServerSideConnectOptions` equivalent needed.
- **D3 (verified — action required):** a server fatal-error token (severity ≥ 20) does **not** currently mark the transport `known_dead`. Add `mark_known_dead()` to the transport and call it from the shared error-token path when `severity >= 20`, with a unit test.

### A2. Reset-completion API (RECOMMENDED — makes reset self-acking regardless of the isolation short-circuit, see D6/D9/B4)
- mssql-python's pool issues `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` right after `reset()` (D9), which normally carries the reset bit. **But** when the borrower never changed isolation, that SET equals the cached value and short-circuits with no I/O (`txn.rs:487`), deferring the reset to a later request. To guarantee acceptance criterion #4 ("reset processed and acknowledged before pool checkout succeeds") unconditionally, expose `TdsClient::reset_connection().await` (or similar) that arms the bit and drives a minimal round-trip forcing the ENVCHANGE ack. The ODBC `SQL_ATTR_RESET_CONNECTION` set-attr calls this so a failed reset is caught at checkout (pool discards) rather than surfacing as the borrower's first-query error.

### A3. Tests (`mssql-tds`)
- Unit: after simulating a ResetConnection ENVCHANGE, assert `prepared_handles`, `prepared_param_encryption`, `pending_capture`, `pending_prepared_param_encryption` are empty and negotiated defaults restored. (Model on the existing reconnect-clears-caches tests around `tds_client.rs:6179-6698`.)
- Keep/extend live tests in `tests/test_reset_connection.rs` proving SET state, temp tables, transactions, and cached database reset — now also proving a prepared handle is invalidated across the reset.

---

## Workstream B — `mssql-odbc` (medium–large, ~4–8 days)

### B1. Constants & bookkeeping (`api/odbc_types.rs`)
- Add `SQL_ATTR_CONNECTION_DEAD = 1209`, `SQL_CD_TRUE = 1`, `SQL_CD_FALSE = 0`.
- Add `SQL_ATTR_RESET_CONNECTION = 116`, `SQL_RESET_CONNECTION_YES = 1`, and the msodbcsql vendor spelling `SQL_COPT_SS_RESET_CONNECTION = 1246` (D10 — 116 is what the direct-loading `mssql-python` consumer sends; 1246 is the only one a DM-mediated Windows app can send).
- No new `DbcState` fields needed for liveness/reset (they read through to the client); autocommit/isolation bookkeeping already present.

### B2. `SQLGetConnectAttr(SQL_ATTR_CONNECTION_DEAD)` (`api/get_connect_attr.rs`)
- Return `SQL_CD_TRUE` iff the DBC is connected and `client.is_connection_dead()` is true; else `SQL_CD_FALSE`.
- **No network probe** (D2: msodbcsql default is a cached read; the probe is behind the opt-in legacy `fConnectionPeek`). `SQL_CD_FALSE` means "not known dead," never "proven healthy."
- **Disconnected/never-connected ⇒ `SQL_CD_TRUE`** (D1: msodbcsql defaults this attribute to DEAD until a successful token read). Pool must discard.
- Do not hold the mutex across I/O — cached read, so a short lock is fine; **peek the client without `take`** (D8: msodbcsql takes the recovery lock; the brief DBC mutex is the mssql-rs equivalent).

### B3. `SQLSetConnectAttr(<reset attribute>, YES)` (`api/set_connect_attr.rs` + `api/txn.rs`)
- **Accept both spellings (D10):** `SQL_ATTR_RESET_CONNECTION` (116, what `mssql-python` sends — it bypasses the DM and calls our exports directly) and `SQL_COPT_SS_RESET_CONNECTION` (1246, the only route the Windows DM lets a DM-mediated app use). Both dispatch to the same handler.
- **Value validation (D7):** accept only `SQL_RESET_CONNECTION_YES(1)`; any other value ⇒ `HY024`.
- Claim the idle client via `claim_dbc_client` (rejects busy/`active_stmt`/open-cursor and disconnected — reuse its diagnostics: `ERR_CONNECTION_BUSY`, `ERR_CONNECTION_DOES_NOT_EXIST`). **D7:** verify `ERR_CONNECTION_DOES_NOT_EXIST` surfaces `08003` for the disconnected case, matching msodbcsql.
- Call `client.prepare_reset_connection(false)` (full reset, no txn preservation — pool checkout does not preserve).
- **Clear DBC-side bookkeeping (D4):** set `state.local_tran_started = false` (msodbcsql `SetLocalTranStatus(FALSE)`); if the driver is tracking an active local txn, roll it back first like msodbcsql (`TM_ROLLBACK_XACT`) rather than leaving it for the server to discard. Note `mssql-python` already rolls back before check-in, so this is defense-in-depth.
- Restore the client. Do **not** drain or mutate another statement's stream. Never hold the mutex across I/O.
- The bit is one-shot; it fires on the next request. In mssql-python's flow that is the checkout `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` (D9, when it emits I/O), otherwise the `setautocommit(False)` begin-transaction or the borrower's first statement. If eager ack is required — recommended, see B4/A2 — call the A2 reset-completion API here so a failed reset is caught at checkout.

### B4. Ensure the reset is carried/acked + isolation applied (D9)
- **mssql-python re-applies isolation on every checkout (D9/[#343](https://github.com/microsoft/mssql-python/pull/343)).** After `reset()` arms the bit, the pool immediately calls `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION, READ_COMMITTED)`. Our handler (`api/txn.rs:462`) already maps this to a real `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` batch — **keep it working**; that batch is the natural carrier of the armed reset bit (reset-first, then SET, server-side).
- **Primary approach — make `SQL_ATTR_RESET_CONNECTION` self-sufficient (A2):** drive a minimal round-trip inside the reset set-attr so the reset is processed+acked before `reset()` returns. This covers the case where the checkout isolation SET short-circuits (`txn.rs:487`, borrower never changed isolation) and lets the pool discard a connection whose reset fails at checkout instead of failing the borrower's first query.
- **Piggyback path (defense):** verify no earlier no-op/short-circuit swallows a pending reset. The isolation same-value short-circuit (`txn.rs:487`) and the autocommit same-mode short-circuit (`txn.rs:339-341`) must not consume/hide an armed reset; `switch_to_manual_commit`'s `begin_transaction` must carry it.
- Consistent error handling: on failure, poison/disconnect or restore the client deterministically; surface `08S01`/appropriate SQLSTATE so mssql-python discards.
- **Raw-T-SQL isolation caveat (D9):** if a borrower issued `SET TRANSACTION ISOLATION LEVEL ...` directly (bypassing the attribute), our cache is stale and the checkout READ COMMITTED short-circuits → isolation leaks. This matches mssql-python's own coverage gap; document as a shared limitation (Non-goals), do not silently paper over it.

### B5. Autocommit / EndTran lifecycle confirmation
- Verify the exact `mssql-python` sequence works end to end: `acquire` (isAlive → reset) → `setautocommit(False)` (begin-txn carries reset) → borrower work → `rollback` on check-in → next `acquire` reuses the same physical connection → borrower B sees clean defaults. Most plumbing exists (`api/txn.rs`, `api/end_tran.rs`); confirm no regression, add coverage where thin.

### B6. Auth / recovery context across reset
- Reset must preserve authentication/recovery context (it is the *same* physical login). A new/rotated access token from `mssql-python` must trigger a **new physical login**, not re-auth of the existing session — confirm connect path already does this; do not add token refresh on a live session (explicit non-goal).

### B7. Tests (`mssql-odbc`)
- Unit (`#[cfg(test)]` in the api modules, mock/no-server): set/get autocommit and isolation; `CONNECTION_DEAD` for connected/disconnected/known-dead; `RESET_CONNECTION` value validation; busy-state rejection (open cursor / `active_stmt`); error recovery leaves DBC consistent.
- e2e (`tests/e2e`, live server, mirror `transaction_test.cpp`): borrower A changes db/isolation/SET options + temp table + open txn; check-in; borrower B on the **same physical connection** observes clean defaults, no temp table, no open txn.

---

## Workstream C — End-to-end `mssql-python` integration (~2–4 days)
- Run `mssql-python`'s pool against the Rust ODBC driver:
  - same-physical-connection reuse after reset;
  - dead idle-connection discard;
  - prepared statements invalidated across reset;
  - concurrent checkout;
  - token-identity separation + near-expiry reconnect (new login, not re-auth).
- Diagnose any behavior differences vs. `msodbcsql18`.
- **No in-repo `mssql-python` harness exists** (adding one would pull a Python +
  `mssql-python` toolchain into `mssql-rs`, which does not fit the layout and is
  the "no second pool" non-goal). The supported hook is the existing driver
  registration the e2e runners perform, so `mssql-python` selects this driver by
  name. The exact manual/CI steps, env, and expected results for each scenario
  above are documented in
  [`odbc-connection-pooling-python-e2e.md`](./odbc-connection-pooling-python-e2e.md),
  backed by the in-repo unit / TDS / live-C++ coverage listed there.

---

## Validation / Definition of Done
Maps to the work item acceptance criteria:
1. `mssql-python` reuses a physical Rust connection with no second Rust pool.
2. `SQL_ATTR_CONNECTION_DEAD` returns cached known-dead status, no probe.
3. The reset attribute arms a real TDS reset (under both the standard and vendor spellings, D10); fails cleanly when disconnected/busy.
4. Reset processed & acknowledged before checkout succeeds; borrower B sees no leaked temp table / txn / db / isolation / SET options.
5. `SQL_ATTR_TXN_ISOLATION` applies real server-side change incl. reset to READ COMMITTED.
6. Autocommit set/get + commit/rollback lifecycle functional.
7. Reset ack invalidates session-bound prepared handles + client metadata.
8. No DBC mutex held across network I/O; busy/open-cursor never mistaken for dead/idle.
9. Unit, live-server, and `mssql-python` e2e pooling tests pass (dead-connection discard + same-physical-connection reuse).
10. `cargo bfmt`, `cargo bclippy`, and focused/required suites pass. (Remember `mssql-odbc` uses `SQLAllocHandle`-style FFI; run `cargo btest` / nextest. `mssql-py-core` is separate but not touched here.)

## Non-goals
- Driver Manager pooling or any second pool in `mssql-odbc`/`mssql-tds`.
- Replacing TDS idle-connection resiliency with pooling.
- Refreshing an access token on an already-authenticated session.
- **Resetting isolation set via raw T-SQL** (not the `SQL_ATTR_TXN_ISOLATION` attribute). `sp_reset_connection` does not reset isolation (D9), and both mssql-python's #343 workaround and our driver track isolation only through the attribute — a borrower that runs `SET TRANSACTION ISOLATION LEVEL ...` directly can leak it across checkout. Shared, documented limitation; not addressed here.

## Rough sequencing
1. A1 + A3 (tds reset-ack centralization + tests) — unblocks safe reuse.
2. B1–B3 (constants, CONNECTION_DEAD, RESET_CONNECTION).
3. B4–B5 (reset-carrying checkout, lifecycle confirm).
4. B6–B7 + C (auth/recovery, odbc tests, python e2e).

## Estimate
~1–2 engineer-weeks happy path; up to 3 weeks if txn/autocommit work needs rework or integration exposes driver-behavior differences.

# Design: ODBC Connection-Pool Reuse (ADO #47317)

## Purpose

`mssql-python` owns the client-side connection pool. `mssql-odbc` does not add a
second pool; it supplies the reset, liveness, transaction, and isolation
semantics that let the existing pool safely reuse one physical TDS connection
for multiple borrowers.

This document describes the implemented design. The companion
[`odbc-connection-pooling-python-e2e.md`](./odbc-connection-pooling-python-e2e.md)
describes integration verification.

## Consumer flow

`mssql-python` performs these operations around a pooled connection:

1. On return, roll back borrower work when autocommit is disabled.
2. On acquire, read `SQL_ATTR_CONNECTION_DEAD`; discard the connection only
   when it is known dead.
3. Call `SQLSetConnectAttr(SQL_ATTR_RESET_CONNECTION,
   SQL_RESET_CONNECTION_YES)`.
4. Reapply `SQL_ATTR_TXN_ISOLATION = SQL_TXN_READ_COMMITTED`. SQL Server reset
   does not restore transaction isolation, so this is required on every
   checkout ([mssql-python #343](https://github.com/microsoft/mssql-python/pull/343)).
5. Apply the requested autocommit mode and serve the borrower.

The reset call in step 3 only arms the TDS reset bit. The isolation call in
step 4 carries that bit; `TdsClient` verifies the server's acknowledgement on
whichever request carries it, so there is no dedicated reset round trip.

## State model

The similar reset flags belong to different layers:

| State | Owner | Meaning | Cleared when |
|---|---|---|---|
| `pending_reset` | TDS transport | One-shot `RESETCONNECTION` or `RESETCONNECTIONSKIPTRAN` bit for the next eligible packet | The packet writer consumes it |
| `reset_dispatched` | TDS transport | A packet header carrying that bit reached the wire | `TdsClient` takes it, or a new reset is armed |
| `reset_state` | `TdsClient` | `Idle` / `Armed` (bit set, not yet sent) / `AwaitingAck` (bit sent, ENVCHANGE owed) | ENVCHANGE arrives, verification fails the request, or a successful reconnect creates a clean session |
| `server_isolation_unknown` | ODBC DBC | The cached isolation level is no longer evidence about the server | An isolation SET reaches the server, or a new login starts from a known state |
| `reset_generation` | ODBC DBC | Monotonic count of armed resets; an isolation SET only clears the invalidation it actually satisfied | Never (monotonic) |
| `local_tran_started` | ODBC DBC | The application executed work in a manual-commit transaction | Commit, rollback, disconnect, or pool reset |
| `transaction_descriptor` | TDS execution context | SQL Server has an active transaction, including an empty driver-begun transaction | Commit, rollback, full reset, or reconnect |
| `known_dead` | TDS transport | I/O, close, fatal server error, or unacknowledged reset proved the connection unusable | A new transport is created |

`local_tran_started` and `transaction_descriptor != 0` are deliberately not
equivalent. Manual-commit setup may begin an empty transaction before the
application executes any work.

## Reset and checkout sequence

```mermaid
sequenceDiagram
    participant Pool as mssql-python pool
    participant ODBC as mssql-odbc
    participant TDS as TdsClient
    participant Server as SQL Server

    Pool->>ODBC: SQL_ATTR_CONNECTION_DEAD
    ODBC-->>Pool: known-dead status (no network probe)
    Pool->>ODBC: SQL_ATTR_RESET_CONNECTION = YES
    ODBC->>ODBC: Mark cached isolation untrusted, read local_tran_started
    opt Known borrower work and live descriptor
        ODBC->>Server: Roll back transaction
    end
    ODBC->>TDS: prepare_reset_connection(false)
    TDS->>TDS: Arm bit and invalidate session-bound state
    ODBC-->>Pool: SQL_SUCCESS (no reset I/O)
    Pool->>ODBC: SQL_ATTR_TXN_ISOLATION = READ_COMMITTED
    ODBC->>ODBC: Skip same-value short circuit (cached level untrusted)
    ODBC->>Server: SET isolation + RESETCONNECTION packet bit
    Server->>Server: Reset session before executing SET
    alt Acknowledged
        Server-->>TDS: ResetConnection ENVCHANGE, then the response
        TDS->>TDS: Clear reset_state
        ODBC-->>Pool: SQL_SUCCESS, connection may be reused
    else Not acknowledged
        Server-->>TDS: Response with no ResetConnection ENVCHANGE
        TDS->>TDS: Mark connection known dead, fail the request
        ODBC-->>Pool: 08S01, discard connection
    end
```

The DBC mutex is never held across network I/O. Transaction state is read
before `claim_dbc_client`, and the mutex is not acquired again until the client
is released. Otherwise, a poisoned lock could drop the claimed client and
strand the DBC as `Connected` with no client to serve future requests.

## Arm-time and acknowledgement-time invalidation

`prepare_reset_connection` invalidates client state immediately:

- managed prepared-statement handles;
- cached Always Encrypted parameter metadata;
- pending `sp_prepexec` capture state;
- accumulated session-recovery state;
- current database, language, and collation, restored to login defaults;
- the transaction descriptor for a full reset.

Arm-time invalidation prevents the carrying request from using an object that
the request's own reset bit will invalidate on the server. For example, sending
a cached prepared handle with the reset bit would make SQL Server drop that
handle before executing it.

`on_reset_connection_ack` repeats the cache and settings transition
intentionally. Arm time closes the stale-state window; acknowledgement time is
the shared protocol transition used by every token-processing path and
reconciles the client with the confirmed server reset.

Server-owned state such as temp tables and SET options is reset by SQL Server
before it executes the carrying request.

## Acknowledgement verification

Verification lives in `TdsClient`, not in the ODBC layer, so it does not depend
on what a consumer happens to send after arming.

The packet writer — not the client — consumes the armed mode, so the client
learns the bit truly reached the wire by taking the transport's
`reset_dispatched` record. That is done only while the client still believes a
reset is `Armed`, so a record left behind by a response that was never read can
never be charged to an unrelated later request.

SQL Server resets the session *before* it parses the carrying request, so the
`ResetConnection` ENVCHANGE precedes everything that request produces. The
first response token that is evidence the request actually ran — DONE,
COLMETADATA, ROW, RETURNSTATUS — is therefore the last moment the
acknowledgement could still have arrived. Reaching it while `AwaitingAck` means
the session was never reset: the connection is marked known dead and the
request fails with `Error::ConnectionResetNotAcknowledged`, which `mssql-odbc`
reports as `08S01` from whatever entry point the request came through.

Three cases are deliberately not treated as failures:

- **ENVCHANGE / INFO / SESSIONSTATE tokens.** These can legitimately precede or
  accompany the acknowledgement, so they never trigger the verdict.
- **ERROR tokens.** The server's own diagnostic is more actionable and must
  reach the caller unmasked. A reset still unacknowledged after it is caught on
  the next token, so the connection is still condemned — just behind the
  server's error.
- **Transparent reconnect.** The armed bit dies with the old transport and no
  ENVCHANGE can arrive. The new transport carries no dispatch record, and
  adopting a recovered session clears `reset_state` outright, so a healthy new
  connection is never discarded for an acknowledgement that can no longer come.

A message the server is told to ignore (the attention/cancel path) is likewise
never recorded as having delivered the bit.

### Abandoned carriers

A verdict is only ever valid against the response of the request that carried
the bit. When that response is abandoned before any token is read, the
suspicion must not be carried forward.

Cancellation and timeout are the reachable case. `NetworkTransport::receive_token`
answers both by draining to the attention acknowledgement, and that drain
discards every other token — the `ResetConnection` ENVCHANGE included. The
carrying request therefore ends with the bit on the wire and nothing observed
about it.

`begin_command` settles this. It runs at the start of every request path,
including the transaction-manager ones, and always *before* the current request
has sent anything, so a dispatch record or outstanding acknowledgement seen
there necessarily belongs to an earlier request. Without this the next,
unrelated request would be condemned on its first token — marking a healthy
connection dead, which is a worse outcome than the gap the verification closes.

The settlement treats the session as reset rather than failed. That is what the
protocol supports: SQL Server resets before it processes the carrying request,
the bit demonstrably reached the wire, and `prepare_reset_connection` already
reconciled every client-side cache at arm time. No pool guarantee is weakened,
because an abandoned carrier fails its own request — the checkout that issued it
reports failure and discards the connection.

## Open cursors at check-in

The reset sweeps open cursors before claiming the connection, matching the five
other connection-scoped operations and `claim_dbc_client`'s stated precondition.
An application that closes its connection without closing a cursor is the
ordinary check-in case; rejecting it would make that connection non-poolable and
force the pool to discard a recyclable connection. msodbcsql reaches the same
outcome by a different route — its reset runs on an internal driver statement and
never contends with user cursors — but with a single `TdsClient` and no MARS,
sweeping is how this driver gets there. A connection that is busy for a reason the
sweep cannot clear is still rejected.

## Transaction behavior

Pool reuse always requests a full `RESETCONNECTION`; transactions must not cross
borrowers.

| Mode | TDS packet bit | Transaction descriptor at arm time | Intended caller |
|---|---|---|---|
| Full reset | `RESETCONNECTION` (`0x08`) | Cleared | ODBC connection pool |
| Preserve transaction | `RESETCONNECTIONSKIPTRAN` (`0x10`) | Preserved | Low-level TDS caller with a transaction that must survive |

Clearing the descriptor for a full reset is required before the carrying
request is constructed. Otherwise an empty driver-begun transaction creates
this failure:

1. `local_tran_started == false`, but `transaction_descriptor != 0`.
2. Reset skips the explicit rollback because there is no borrower work.
3. Checkout sees the stale descriptor and sends a rollback carrying the reset
   bit.
4. SQL Server processes the reset first and discards the transaction.
5. SQL Server then processes the rollback and returns error 3903 because no
   transaction remains.

`RESETCONNECTIONSKIPTRAN` must keep the descriptor because its purpose is to
preserve that transaction.

The explicit pre-reset rollback is defense in depth. It closes known borrower
work before reuse and makes rollback failure observable during pool reset; a
failure marks the connection dead instead of relying on a later reset to clean
an uncertain session.

## Why the reset is piggybacked

An earlier implementation sent `SELECT 1` solely to carry and acknowledge the
reset. On loopback, 300 release-build iterations measured:

| Design | Reset plus first query |
|---|---:|
| Dedicated reset round trip | 1.63-1.71 ms |
| Piggyback on checkout isolation SET | 0.79-0.83 ms |

The difference is a complete request/response and therefore grows with network
latency. Piggybacking preserves both properties the eager request provided:

- **Cache safety:** session-bound client state is invalidated when reset is
  armed.
- **Fail at checkout:** `TdsClient` verifies the acknowledgement on whichever
  request carries the bit, so the reset fails on that request rather than
  silently leaving a dirty session behind.

A consumer that does not issue the checkout isolation SET gets the same
guarantee: the bit rides its first eligible request, SQL Server resets before
executing it, and that request is where a missing acknowledgement surfaces.
Only the isolation restore below is specific to a consumer that issues the SET.

## Isolation semantics

`sp_reset_connection` does not restore transaction isolation. The checkout
`SET TRANSACTION ISOLATION LEVEL READ COMMITTED` is therefore both:

1. the reset carrier; and
2. the operation that restores isolation for the next borrower.

`DbcState::txn_isolation` tracks only changes made through the ODBC attribute.
Raw T-SQL can make `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` report a stale
cached value. That reporting limitation remains.

The cross-borrower leak does not remain. Arming a reset sets
`server_isolation_unknown`, which disables the same-value optimization, so the
checkout SET reaches SQL Server even when the previous borrower changed
isolation through raw T-SQL.

The invalidation is raised before the client is claimed *and* re-asserted under
the lock that records the completed arm, which also bumps `reset_generation`. An
isolation SET captures that generation before it sends and only clears the
invalidation if it is unchanged. Without the generation, a SET that reached the
server just before a concurrent reset armed could clear an invalidation it never
satisfied, and the next same-value checkout SET would short-circuit against a
session the newer reset had made unknown again.

The driver deliberately does **not** assign `txn_isolation = READ COMMITTED`
when it arms a reset. The reset does not restore the isolation level, so after
arming the server is still running at whatever the previous borrower left. The
last level set through the attribute is the closest thing the driver knows, and
claiming READ COMMITTED instead would be an assertion it cannot back — and,
worse, would make the checkout SET of READ COMMITTED short-circuit and leave the
server at the previous borrower's level. Marking the cache untrusted expresses
what is actually true and forces the SET that makes it true again.

## Liveness semantics

`SQL_ATTR_CONNECTION_DEAD` is a cached read and never probes the socket:

- `SQL_CD_TRUE` means the connection is known dead.
- `SQL_CD_FALSE` means it has not been observed dead; it is not proof of health.
- A disconnected or never-connected DBC reports `SQL_CD_TRUE`.
- A connected DBC whose client is temporarily absent because another operation
  claimed it reports `SQL_CD_FALSE`, not dead.

The transport becomes known dead after explicit close, observed I/O failure,
EOF, a fatal server error token (severity at least 20), or an unrecoverable
pool-reset failure.

## Reset attribute identifiers

The driver accepts both identifiers:

| Identifier | Value | Used by |
|---|---:|---|
| `SQL_ATTR_RESET_CONNECTION` | 116 | `mssql-python`, which loads and calls the driver directly; unixODBC DM callers |
| `SQL_COPT_SS_RESET_CONNECTION` | 1246 | Driver-Manager-mediated Windows callers |

Windows Driver Manager reserves attribute 116 for DM-to-driver communication
and rejects an application setting it directly with `HY092`. Advertising ODBC
3.8 does not change that rule. `mssql-python` bypasses the Driver Manager, so
116 reaches this driver on every platform.

The reset handler accepts only `SQL_RESET_CONNECTION_YES`; other values produce
`HY024`. Reset on a disconnected DBC produces `08003`, and a busy connection is
rejected rather than disturbing another statement's stream.

## Recovery and authentication

Reset reuses the existing physical login and does not repeat LOGIN7, federated
authentication, or access-token exchange. Rotated credentials require a new
physical connection.

If session recovery reconnects while a reset is pending, the new login
supersedes that reset. The old reset bit died with the old transport, but the new
session is already clean, so adopting the recovered session clears the pending
reset. Leaving it set would incorrectly discard a healthy new connection for
lacking an acknowledgement that can no longer arrive.

## Verified parity decisions

The msodbcsql source used for parity review was
`Sql/Ntdbms/sqlncli/odbc/{sqlcmisc.cpp, sqlcfunc.cpp, sqlcconn.cpp,
sqlctokn.cpp, sqlcerr.cpp, dbcinfotoken.cpp}`.

- Cached, socket-free liveness matches the default msodbcsql path.
- Disconnected connections default to known dead.
- Fatal server errors mark the connection dead even if the socket remains
  readable.
- Pool reset rolls back known local work, restores cached login defaults, and
  resets recovery state.
- ANSI defaults need no client-side replay because they come from the ODBC
  LOGIN7 option and SQL Server reset restores them. Isolation is the exception.
- Clearing prepared handles at arm time deliberately exceeds msodbcsql, which
  can surface native error 8179 after reset; this driver transparently
  re-prepares instead.
- Explicit acknowledgement verification also exceeds msodbcsql by rejecting a
  request that completed without the expected reset ENVCHANGE. It lives in
  `TdsClient` rather than the ODBC layer so it does not depend on the consumer's
  checkout sequence.
- **msodbcsql does not piggyback.** After arming, it immediately sends a re-sync
  batch on its own driver statement (`sqlcmisc.cpp:2410-2446`), and
  `BuildServerSideConnectOptions` (`sqlcfunc.cpp:2007+`) re-emits
  `SET TRANSACTION ISOLATION LEVEL` for any non-READ-COMMITTED cached level, plus
  ANSI_NPW / CONCAT_NULL and QUOTED_IDENTIFIER when non-default. So msodbcsql pays
  a round trip this driver does not, and re-applies settings this driver leaves to
  the consumer's checkout SET. Do not describe the piggyback design as "matching
  msodbcsql" — it is a deliberate divergence.
- **Follow-up as session settings grow.** `apply_post_connect_txn_settings`
  (`txn.rs`) is this driver's analogue of `BuildServerSideConnectOptions`, and the
  reset path does not call it. Today that is harmless: the isolation level is the
  only session setting emitted post-login, and the checkout SET re-applies it.
  When `QuotedId`, `AnsiNPW`, `CONCAT_NULL`, `SQL_ATTR_MAX_LENGTH`/`MAX_ROWS` are
  added, each will silently desync across a reset unless the reset routes through
  that function. Tracked as a follow-up rather than done here, because letting its
  batch carry the bit reintroduces I/O into the reset handler — the cost this
  design deliberately removed.

## Validation requirements

The implementation is covered at three levels:

- TDS unit/live tests for reset acknowledgement, prepared-handle invalidation,
  login-default restoration, full-reset versus SKIPTRAN transaction behavior,
  reconnect handling, and the verification path itself — the missing-ENVCHANGE
  verdict on both a no-row and a row-returning carrier, a server error not being
  masked by it, an armed bit that never reached the wire condemning nothing, and
  a recovered session clearing the pending reset.
- ODBC unit tests for validation, busy/disconnected handling, rollback ordering,
  no-I/O arming, the untrusted isolation cache forcing the checkout SET, the
  08S01 mapping of a missing acknowledgement, transaction descriptor clearing,
  and cached liveness.
- C++ live tests for same-SPID reuse, clean borrower state, isolation reset,
  liveness, and prepared-statement behavior against this driver and msodbcsql
  where behavior is shared.

## Non-goals

- Driver Manager pooling or another pool in `mssql-odbc` or `mssql-tds`.
- Replacing TDS idle-connection resiliency.
- Refreshing an access token on an authenticated physical session.
- Making `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` observe isolation changes
  issued through raw T-SQL.

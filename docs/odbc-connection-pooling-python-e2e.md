<!--
Copyright (c) Microsoft Corporation.
Licensed under the MIT License.
-->

# mssql-python / mssql-odbc pool verification

Companion to the [connection-pool design](odbc-connection-pooling-plan.md)
(ADO #47317). `mssql-python` owns the pool; the Rust driver supplies reset,
liveness, transaction, and isolation behavior for reuse of a physical connection.

## Existing cross-repository CI

The [cross-repo test job](../.pipeline/templates/test-mssql-python-odbc-template.yml)
already builds `mssql-odbc` and runs the upstream `mssql-python` suite against it.
It preserves the upstream Rust runtime and replaces only the bundled
`libmsodbcsql` binary. Driver registration by name is **not** how this job selects
the replacement.

For reproduction, use the same upstream revision, dependencies, and binary swap
as that job:

1. Select the upstream source with
   [clone-mssql-python.sh](../.pipeline/scripts/clone-mssql-python.sh).
   PR/local runs use the committed pin; non-PR CI follows upstream `main`.
2. Build the driver and upstream bindings using the template's container setup.
3. Use [swap-mssql-python-odbc-driver.sh](../.pipeline/scripts/swap-mssql-python-odbc-driver.sh)
   to replace the bundled binary.
4. Run the suite with
   [run-mssql-python-odbc-tests.sh](../.pipeline/scripts/run-mssql-python-odbc-tests.sh).
   The template supplies its environment and collects JUnit results.

Do not substitute an arbitrary installed upstream version when reproducing a PR.
See the [cross-repo setup notes](../.pipeline/scripts/README.md#clone-mssql-pythonsh)
for pin selection and failure policy. For a reference-driver comparison, repeat
the same workload and configuration with the upstream bundled driver unchanged.

## Pool scenarios

Exercise the upstream pool API from the selected revision. A one-connection pool
makes physical-session reuse observable through `@@SPID`; close each borrower
before acquiring the next.

| Scenario | Expected Rust-driver behavior | In-repo coverage |
|---|---|---|
| Reuse after reset | Same `@@SPID`; no borrower temp tables or open transaction; login database restored | `connection_pool_test.cpp`, TDS reset tests |
| Isolation after checkout | Checkout's `SQL_ATTR_TXN_ISOLATION` SET restores READ COMMITTED, including after raw T-SQL changed isolation | `reset_then_checkout_isolation_reapplies_read_committed`, `armed_reset_suppresses_the_isolation_short_circuit` |
| Prepared statement after reset | Invalidated server handles are re-prepared before reuse | `PreparedStatementUsableAcrossReset`, `PreparedStatementSurvivesResetViaReprepare` |
| Known-dead connection | The pool discards a connection reported dead or one whose next request/reset carrier fails | `get_connect_attr.rs`, `session_recovery_test.cpp` |
| Concurrent borrowers | Each physical connection has one borrower at a time; no result cross-talk or leaked session state | Upstream pool/stress tests and ODBC busy-connection tests |
| Rotated access token | Reuse does not re-authenticate; changing the token on a live connection is rejected, so rotation requires a new physical login | `access_token_after_connect_is_rejected` |

The C++ tests are under
[`mssql-odbc/tests/e2e/tests`](../mssql-odbc/tests/e2e/tests);
Rust ODBC tests are in
[`mssql-odbc/src/api`](../mssql-odbc/src/api), and TDS reset tests are in
[`tds_client.rs`](../mssql-tds/src/connection/tds_client.rs).
Use the [ODBC e2e guide](../mssql-odbc/tests/e2e/README.md) for running the C++
suite and its reference-driver comparison.

## Interpreting results

`SQL_ATTR_CONNECTION_DEAD` is a cached observation, not a network probe.
`SQL_CD_FALSE` does not prove a killed idle connection is healthy. A subsequent
request can be the first operation to detect its death; do not require the
attribute read alone to detect a server-side `KILL`.

Reset arms a TDS packet bit rather than sending a dedicated reset request.
The checkout isolation SET normally carries it. Reset marks the cached isolation
unknown, so that SET is not skipped merely because the cached value is already
READ COMMITTED. This prevents a raw-T-SQL isolation change from leaking across
reset. Without an intervening reset, the separate same-value attribute shortcut
can still leave a raw-T-SQL change unobserved.

The Rust driver verifies the carrying request's reset acknowledgement and
transparently re-prepares invalidated handles. These are deliberate differences,
not universal reference-driver expectations; see the design's
[parity decisions](odbc-connection-pooling-plan.md#verified-parity-decisions).
Compare shared observable outcomes separately from Rust-specific assertions.

## Reset attribute routing

`mssql-python` loads the driver directly, so `SQL_ATTR_RESET_CONNECTION` (116)
reaches it without a Driver Manager. Windows Driver-Manager-mediated
applications instead use `SQL_COPT_SS_RESET_CONNECTION` (1246); the Windows DM
reserves attribute 116 for its own calls. unixODBC passes 116 through.
The Rust driver accepts both identifiers. A DM-level rejection is not evidence
that the driver's reset implementation was called.

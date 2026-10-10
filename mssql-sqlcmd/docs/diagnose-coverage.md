# `sqlcmd diagnose` coverage matrix

Matrix version: **1.0** (reported in the JSON report as `coverageMatrixVersion`).

This matrix maps each connection phase of the sqlcmd specification (its
`PHASE-*` ids) to the check that evaluates it, the stable signals the check
relies on, the coverage states it can report, where it runs, and its release
scope. The conformance tests (the crate's unit tests, and the native
`TCSqlcmdDiagnose` gtests) exercise the rows marked as shipped.

Coverage states are those of the specification: `passed`, `diagnosed` (stable
evidence identifies the failure), `classified` (weaker evidence, such as error
text), `inconclusive`, `skipped` (with what blocked it), `notApplicable`.
`outOfScope` is not produced yet: no check observes anything outside its phase.

| Phase | Check (depth) | Signals used, in order of preference | States | Platforms | Scope |
|---|---|---|---|---|---|
| `PHASE-LOCAL-INPUT` | `connectionInput` (`connectionInput`) | the `-S` server grammar: protocol prefix, host, `\instance`, `,port`; no external call | passed, diagnosed | all | P1, shipped for `-S`. The ODBC connection-string grammar (`-D` DSN, keyword strings) and driver-load signals are not evaluated: the attempt is made with `mssql-tds`, not the ODBC driver. |
| `PHASE-DNS` | `nameResolution` (`endpointResolution`) | on Windows, the WinSock result code (11001 not found, 11002 temporary failure, 11004 no address); elsewhere Rust does not expose the resolver's code (`EAI_*`), so the result is classified from its message and marked `fromMessageText`; every IPv4/IPv6 address | passed, diagnosed, inconclusive | all | P1, shipped |
| `PHASE-NAMED-INSTANCE` | `instanceResolution` (`endpointResolution`) | the SQL Server Browser answer (port, no TCP port, protocol error) and, sharing one 2 s limit, `timedOut` when resolving the server uses it up or `noAnswer` when the Browser is silent for the rest; never a guessed port. `admin:` uses port 1434; `admin:` with a named instance is inconclusive (`dacLookupUnsupported`), because its DAC port needs the Browser's DAC request, which is not made. `np:host\instance` gets its pipe from the Browser or, without an answer, the instance's standard pipe (`pipeSource`). The connection attempt then uses the confirmed port or pipe, without a second Browser lookup | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-TCP` | `tcpConnect` (`networkReachability`) | the operating system's connect result per address (refused, timed out, host/network unreachable, reset) and its error code; a local refusal (no sockets, no local address, permission) makes the run `partial` | passed, diagnosed, inconclusive | all | P1, shipped |
| `PHASE-TLS` | `connectionAttempt` (`connectionAttempt`) | the TLS phase of `mssql-tds`'s connect-stage spans; the typed TLS error, else its text (marked) | passed, diagnosed, classified, inconclusive | all | P1, shipped. Certificate subject/SAN and validity details are not exposed by the client yet. |
| `PHASE-KERBEROS` | `connectionAttempt` (`connectionAttempt`) | the SPN the client requests; off Windows, whether a file ticket cache exists (`KRB5CCNAME`, `/tmp/krb5cc_<uid>`); the server/client clock offset from session validation | passed, diagnosed, classified, inconclusive | Windows (SSPI), Linux and macOS (GSSAPI) | P1, partial: encryption types, KCM/KEYRING caches and SPN registration are not inspected. Delegation and gMSA are P2. |
| `PHASE-SQL-AUTH` | `connectionAttempt` (`connectionAttempt`) | SQL Server error number, state and class (18456, 18452, 18470, 18486-18488) | passed, diagnosed | all | P1, shipped |
| `PHASE-ENTRA-AUTH` | — | — | — | — | P1, not yet: `-G` is refused by `sqlcmd diagnose`. |
| `PHASE-DATABASE` | `connectionAttempt` (`connectionAttempt`) | SQL Server errors 4060 and 4064 after a good login | diagnosed | all | P1, shipped |
| `PHASE-CONNECTION-DEADLINE` | every timed check | each check's `deadlineMs` and `durationMs`; the connection attempt's failed phase | inconclusive | all | P1, shipped |
| `PHASE-SESSION-TRANSPORT` | `sessionValidation` (`sessionValidation`) | the minimal query's result or SQL Server error, within the time limit | passed, diagnosed, inconclusive | all | P1, shipped. Transport-loss retry counts are not modelled. |
| `PHASE-COMMAND-DEADLINE` | — | — | — | — | Not part of `sqlcmd diagnose`: it covers user queries, which diagnose does not run. |

When the connection attempt or session validation ends `inconclusive` or
`classified`, the report gives `tracingGuide`, the stable link to the
driver-tracing documentation, since the client's own evidence goes no further.

Changes to this matrix bump its version: the minor version for added rows or
signals, the major version when a row's meaning or states change.

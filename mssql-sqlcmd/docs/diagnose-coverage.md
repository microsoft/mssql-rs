# `sqlcmd diagnose` coverage matrix

Matrix version: **1.0** (reported in the JSON report as `coverageMatrixVersion`).

This matrix maps each connection phase of the sqlcmd specification (its
`PHASE-*` ids) to the check that evaluates it, the stable signals the check
relies on, the coverage states it can report, where it runs, and its release
scope. The conformance tests (the crate's unit tests, and the native
`TCSqlcmdDiagnose` gtests) exercise the rows marked as shipped.

Every check observes the client's real connection: `mssql-tds` makes one
connection attempt, as a normal run does, and its connect-stage spans
(`mssql_tds::connection::connect_stage`) are the signals; sqlcmd makes no
network call of its own. The depth stops the attempt after resolution
(`endpointResolution`) or once the transport is open (`networkReachability`).
Over TCP the client also connects to the addresses after the one that
accepted (up to 64), each in a stage of its own. A stage that closes
without saying whether it succeeded was abandoned (a time limit or a cancel
cut it short), which is not a failure.

Coverage states are those of the specification: `passed`, `diagnosed` (stable
evidence identifies the failure), `classified` (weaker evidence, such as error
text), `inconclusive`, `skipped` (with what blocked it), `notApplicable`.
`outOfScope` is not produced yet: no check observes anything outside its phase.

| Phase | Check (depth) | Signals used, in order of preference | States | Platforms | Scope |
|---|---|---|---|---|---|
| `PHASE-LOCAL-INPUT` | `connectionInput` (`connectionInput`) | the resolved settings that native sqlcmd passes in (command-line options, environment variables and defaults, with source labels); the `-S` server grammar: protocol prefix, host, `\instance`, `,port`, and shared memory (`lpc:`) only to this computer and only on Windows, as the client allows it; local findings for a named instance with an explicit port, for `-F` with `-C`, and for `-C` with `-Ns` (strict encryption ignores `-C` and validates the certificate); no external call | passed, diagnosed | all | P1, shipped for command-line options, SQLCMD/OSQL environment variables and defaults. The ODBC connection-string grammar (`-D` DSN, keyword strings) and driver-load signals are not evaluated: the attempt is made with `mssql-tds`, not the ODBC driver. |
| `PHASE-DNS` | `nameResolution` (`endpointResolution`) | the client's `dns` stage (or, for a named instance, the addresses its SQL Server Browser lookup resolved): on Windows, the WinSock result code (11001 not found, 11002 temporary failure, 11004 no address); elsewhere Rust does not expose the resolver's code (`EAI_*`), so the result is classified from its message and marked `fromMessageText`; every IPv4/IPv6 address, with its IPv6 scope. `notApplicable` when the client used a pipe or shared memory | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-NAMED-INSTANCE` | `instanceResolution` (`endpointResolution`) | the client's `instance_lookup` stage: the SQL Server Browser answer (port, pipe, no usable endpoint: `noTcpPort`) or its failure code (`noAnswer`, `receiveFailed`, ...) within the 2 s Browser limit; never a guessed port. `admin:` uses port 1434; `admin:` with a named instance is inconclusive (`dacLookupUnsupported`), because its DAC port needs the Browser's DAC request, which the client does not make. `np:host\instance` reports the pipe the Browser listed or, when it listed none, the instance's standard pipe the client opened (`pipeSource`) | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-TCP` | `tcpConnect` (`networkReachability`) | the client's `tcp` stages: the operating system's connect result per address (refused, timed out, host/network unreachable, reset) and its error code, the addresses after the one used included; addresses the time limit left untried are `notAttempted`; a local refusal (no sockets, no local address, permission) makes the run `partial`. `notApplicable` when the client used a pipe or shared memory | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-TLS` | `connectionAttempt` (`connectionAttempt`) | the TLS phase of `mssql-tds`'s connect-stage spans; the typed TLS error, else its text (marked) | passed, diagnosed, classified, inconclusive | all | P1, shipped. Certificate subject/SAN and validity details are not exposed by the client yet. |
| `PHASE-KERBEROS` | `connectionAttempt` (`connectionAttempt`) | the SPN the client requested, as its `login` stage records it; off Windows, whether a file ticket cache exists (`KRB5CCNAME`, `/tmp/krb5cc_<uid>`); the server/client clock offset from session validation | passed, diagnosed, classified, inconclusive | Windows (SSPI), Linux and macOS (GSSAPI) | P1, partial: encryption types, KCM/KEYRING caches and SPN registration are not inspected. Delegation and gMSA are P2. |
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

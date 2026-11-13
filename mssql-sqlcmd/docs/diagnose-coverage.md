# `sqlcmd diagnose` coverage matrix

Matrix version: **1.4** (reported in the JSON report as `coverageMatrixVersion`).

This matrix maps each connection phase of the sqlcmd specification (its
`PHASE-*` ids) to the check that evaluates it, the stable signals the check
relies on, the coverage states it can report, where it runs, and its release
scope. The conformance tests (the crate's unit tests, and the native
`TCSqlcmdDiagnose` gtests) exercise the rows marked as shipped.

`mssql-tds` supplies the transport evidence through its connect-stage spans
(`mssql_tds::connection::connect_stage`). When native sqlcmd supplies its
ODBC callback, that connection decides the connection/session results; the
diagnostic connection adds evidence only. Without a callback, `mssql-tds`
also performs login and session validation. The depth stops transport after resolution
(`endpointResolution`) or once the transport is open (`networkReachability`).
Over TCP the client also connects to the addresses after the one that
accepted (up to 64), each in a stage of its own. A stage that closes
without saying whether it succeeded was abandoned (a time limit or a cancel
cut it short), which is not a failure.

Coverage states are those of the specification: `passed`, `diagnosed` (stable
evidence identifies the failure), `classified` (weaker evidence, such as error
text), `inconclusive`, `skipped` (with what blocked it), `notApplicable`.
`outOfScope` is reported separately in `outOfScopeObservations`, not as a
replacement for a required check: after an observed session transport loss,
recurrence/source analysis has its own evidence, specialist domain and tracing
handoff. It never participates in outcome aggregation.

| Phase | Check (depth) | Signals used, in order of preference | States | Platforms | Scope |
|---|---|---|---|---|---|
| `PHASE-LOCAL-INPUT` | `connectionInput` (`connectionInput`) | resolved settings and source labels; connection options `-K`, `-M`, `-a`, `-A` and `-H`; the client's `-S` grammar (protocol, host, instance and port), including platform restrictions for LocalDB, named pipes and shared memory; local findings for an instance with an explicit port, certificate settings that leave validation unused, and `-A` where it does not select a dedicated administrator connection; `-M` with a non-TCP transport is diagnosed (`multiSubnetFailoverNeedsTcp`); the library also accepts caller-resolved DSN settings and reports missing-DSN, missing-server and ignored-login findings; no external call. See [the request and option rules](diagnose.md). | passed, diagnosed | all | P1, shipped for command-line options, SQLCMD/OSQL environment variables and defaults. Library DSN request support exists, but native `diagnose -D` on Linux/macOS remains separate work; on Windows `-D` is obsolete. ODBC keyword strings are not accepted as connection input. At connection-attempt depth, the native callback also evaluates ODBC driver-load states IM002/IM003/IM004/IM005/IM014 and the driver's actual connection result. |
| `PHASE-DNS` | `nameResolution` (`endpointResolution`) | the client's `dns` stage (or, for a named instance, the addresses its SQL Server Browser lookup resolved): on Windows, the WinSock result code (11001 not found, 11002 temporary failure, 11004 no address); elsewhere Rust does not expose the resolver's code (`EAI_*`), so the result is classified from its message and marked `fromMessageText`; every IPv4/IPv6 address, with its IPv6 scope. When the resolver failed and the client went on with addresses its DNS cache kept from an earlier resolution (`source: fallbackCache`), the resolver's failure, classified as above, with the cached addresses, which `tcpConnect` still checks. `notApplicable` when the client used a pipe or shared memory, or when TCP failed and the pipe the protocol order then tried opened (its errors kept, `superseded`) | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-NAMED-INSTANCE` | `instanceResolution` (`endpointResolution`) | the client's `instance_lookup` stage: the SQL Server Browser answer (port, pipe, no usable endpoint: `noTcpPort`, which `tcp:host\instance` also reports for an answer with only a pipe) or its failure code (`noAnswer`, `receiveFailed`, ...) within the 2 s Browser limit; never a guessed port. When it gives no usable TCP port (or this computer would not send the request), the check carries `requires: explicitPort` and asks for `-S <host>,<port>` at every depth, and the checks that need the port are skipped with `explicitPortRequired`, `blockedBy: instanceResolution`. `admin:` uses port 1434; `admin:` with a named instance is inconclusive (`dacLookupUnsupported`), because its DAC port needs the Browser's DAC request, which the client does not make. `np:host\instance` reports the pipe the Browser listed or, when it listed none, the instance's standard pipe the client opens, whether or not it opened and at `endpointResolution` too (`pipeSource`); the text report names that pipe, not a TCP port the Browser also listed. On Windows, a local named instance without a protocol prefix at `endpointResolution` is inconclusive (`notLookedUp`): the client tries shared memory first and asks the Browser only if that fails, and this depth opens no connection | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-TCP` | `tcpConnect` (`networkReachability`) | the client's `tcp` stages: the operating system's connect result per address (refused, timed out, host/network unreachable, reset) and its error code, the addresses after the one used included; addresses the attempt left untried are `notAttempted` (their error names the time limit only when an earlier address timed out); a local refusal (no sockets, no local address, permission) on every address tried makes the run `partial` (addresses it left untried do not count); port 0, which the client takes as a port and every connect to fails, is `diagnosed` with `client.inputConfiguration`, not a local refusal; when the time limit was spent before the port SQL Server Browser gave was tried, `inconclusive` with `result: notAttempted` and the time-limit error; likewise for the pipe the Browser answer pointed to (`np:`, or without a prefix an answer with only a pipe) that the time limit left unreached (`transport: namedPipe`, `pipe`, `result: notAttempted`). A pipe given directly (`np:`, `lpc:`, LocalDB) that does not open is reported here at `networkReachability`, on its own transport (`transport`, `pipe`, `result`): diagnosed when refused, inconclusive (`timedOut`) without an answer in the time limit or when the pipe's own wait for a free instance timed out; a LocalDB instance that fails to resolve before any pipe is reported with its `transport` and `result: notAttempted`, and a TCP named instance without a port that fails before any stage with its `host` and `instance` and no port (SQL Server Browser, not reached, would have given it); deeper, the connection attempt reports it. `notApplicable` when the client used a pipe or shared memory, or when TCP failed and the pipe the protocol order then tried opened (its errors kept, `superseded`) | passed, diagnosed, inconclusive, notApplicable | all | P1, shipped |
| `PHASE-TLS` | `connectionAttempt` (`connectionAttempt`) | with the native callback, ODBC's actual connection result and diagnostic records decide the check; `mssql-tds`'s TLS spans and typed TLS error add evidence only when needed. Without the callback, the spans and typed error decide it; text-derived evidence is marked | passed, diagnosed, classified, inconclusive | all | P1, shipped. `driver` names the ODBC driver; `diagnosticClient` reports the supplemental evidence login's result when one was needed. Certificate subject/SAN and validity details are not exposed by the client yet. |
| `PHASE-KERBEROS` | `connectionAttempt` (`connectionAttempt`) | the SPN the client requested, as its `login` stage records it; off Windows, whether a file ticket cache exists (`KRB5CCNAME`, `/tmp/krb5cc_<uid>`); the server/client clock offset from session validation | passed, diagnosed, classified, inconclusive | Windows (SSPI), Linux and macOS (GSSAPI) | P1, partial: encryption types, KCM/KEYRING caches and SPN registration are not inspected. Delegation and gMSA are P2. |
| `PHASE-SQL-AUTH` | `connectionAttempt` (`connectionAttempt`) | SQL Server error number, state and class (18456, 18452, 18470, 18486-18488) | passed, diagnosed | all | P1, shipped |
| `PHASE-ENTRA-AUTH` | — | — | — | — | P1, not yet: `-G` is refused by `sqlcmd diagnose`. |
| `PHASE-DATABASE` | `connectionAttempt` (`connectionAttempt`) | SQL Server errors 4060 and 4064 after a good login | diagnosed | all | P1, shipped |
| `PHASE-CONNECTION-DEADLINE` | every timed check | each check's `deadlineMs` and `durationMs`; the connection attempt's failed phase | inconclusive | all | P1, shipped |
| `PHASE-SESSION-TRANSPORT` | `sessionValidation` (`sessionValidation`) | the minimal query's result or SQL Server error, within the time limit; `loginCompleted` and `lastSuccessfulOperation` (`login`, or `query` once it passed); `retries`: 0, since the diagnostic client's `connect_retry_count` is 0, which also turns off its session recovery (idle connection resiliency), so it never reconnects during the query and its first error is the one reported (not reported through sqlcmd's own driver connection, whose retries are the driver's); a connection lost during the query, classified from the failed operation's I/O error (read beneath any wrapper, keeping its operating-system code as `osError`) as `transportLoss`: `closed` (a read found the connection's end), `reset` (WinSock 10054, `ECONNRESET`), `aborted` (10053, `ECONNABORTED`), `brokenPipe` (`EPIPE`) or `communicationFailure` (anything else); through sqlcmd's own driver connection only SQLSTATE 08S01 shows a lost connection, as `communicationFailure` with no operating-system code. A lost connection is `inconclusive`, `undetermined` between `connectivity.network` and `target.sqlServer`, its finding names no source (SQL Server, a device between them or the client), and a limitation says a loss can be intermittent; only SQL Server's own error is `diagnosed` with `target.sqlServer`; a query with no answer in time is `inconclusive` (`timedOut`) | passed, diagnosed, inconclusive | all | P1, shipped. One attempt: reproducing an intermittent loss or analyzing how often it occurs is out of scope. |
| `PHASE-COMMAND-DEADLINE` | — | — | — | — | Not part of `sqlcmd diagnose`: it covers user queries, which diagnose does not run. |

When the connection attempt or session validation ends `inconclusive` or
`classified`, the report gives `tracingGuide`, the stable link to the
driver-tracing documentation, since the client's own evidence goes no further.

Changes to this matrix bump its version: the minor version for added rows or
signals, the major version when a row's meaning or states change.

## Redaction and sharing

Diagnose output is for review before sharing; it is never share-safe, with or
without `--local-detail`. The text report begins with the warning
"Diagnostic output may contain unobfuscated server names, host names, and IP
addresses. Review the output before sharing it."; the JSON report carries it
in the top-level `sharingWarning` and says in `localDetail` whether database
and user names were revealed.

| Data | Default | `--local-detail` |
|---|---|---|
| Password and other secrets | never collected or shown | never shown |
| Server, instance and host names (`-F` included), IP addresses, named pipe paths, Kerberos SPNs | shown as they are | shown as they are |
| Database and user names | labels (`database-1`, `user-1`), equal within one run, not stable across runs; replaced in error text as whole words | shown as they are |
| Certificate subject or SAN names, file paths | not reported by any check; would be labelled | — |
| Error and finding text quoted from the client or the server | flagged in `reviewBeforeSharing` | flagged in `reviewBeforeSharing` |
| Codes, phases, durations, versions, OS family, endpoint type | shown | shown |

# sqlcmd diagnose

`sqlcmd diagnose [-S <server> | -D -S <dsn>] [-U <login> -P <password> | -E] [-d <database>] [-N[s|m|o]] [-C] [-F <certificate-name>] [-l <seconds>] [-K ReadOnly] [-M] [-a <packet-size>] [-A] [-H <workstation>] [--depth <depth>] [--local-detail] [--format json]`
checks, from the bottom up, that a connection can be made, and when it cannot,
where and why. It follows the diagnostic depths of the sqlcmd specification;
each includes the ones before it, and the deepest is the default:

| Depth (`--depth`) | Checks |
|---|---|
| `connectionInput` | the settings sqlcmd resolved and where each came from (a command-line option, an environment variable, an ODBC data source (DSN) given with `-D`, or the default); the server given to `-S`, or the one the DSN names, checked with the client's rules; local findings for an instance name with a port, for `-F` with `-C`, for a DSN that names no server, for a login that a DSN's `Trusted_Connection=yes` leaves unused, and for `-A` where it makes no dedicated administrator connection (with `-D`, or with a protocol prefix, a pipe path or a port in `-S`); `-M` with named pipes, shared memory or the dedicated administrator connection, which the client refuses, is `diagnosed`; no external call |
| `endpointResolution` | the client resolves the host name and asks SQL Server Browser for a named instance's port (never guessing 1433), then stops |
| `networkReachability` | the client also opens its transport: a TCP connection to the first resolved address that accepts, and to every address after it, or a pipe; then it stops |
| `connectionAttempt` | the client also logs in, with its pre-login, TLS and login phases |
| `sessionValidation` | a minimal query on the new session: the engine edition and the server's UTC clock |

Checks run best effort: a failed check skips only what needs its result, and
each skipped check names what blocked it. In the JSON report a skipped check
gives its `reason` (`blockedByPrerequisite`, `explicitPortRequired`,
`notApplicable`, `canceled`), the check that blocked it as `blockedBy`, and a
`detail` in words (none for `blockedByPrerequisite`).

The library request can describe settings resolved from an ODBC data source
(DSN), including `dsn` source labels and local findings for a missing DSN,
a DSN without a server, or a login ignored by `Trusted_Connection=yes`.
The caller must supply those resolved settings; the library does not read
`odbc.ini` itself. **Native sqlcmd rejects
`diagnose -D` on Linux and macOS**; native DSN resolution is separate work.
DSN connections outside `diagnose` are unaffected. On Windows, native
sqlcmd treats `-D` as obsolete. Native diagnosis also refuses `-G`
and `-J`. Command-line `-K` and `-M` are supported through the request fields
below.

Native diagnosis rejects password-change switches `-z` and `-Z` before
connecting. Password changes remain available outside `diagnose`.

The connection options `-K` (application intent), `-M` (MultiSubnetFailover),
`-a` (packet size), `-A` (dedicated administrator connection) and `-H`
(workstation name) are part of the request (`application_intent`,
`multi_subnet_failover`, `packet_size`, `dedicated_admin`,
`workstation_name`, each with its `*_source`), appended to the C request
struct: a caller that leaves them zeroed gets the defaults and today's report.
The diagnostic client applies them as sqlcmd's driver does: `-K ReadOnly` and
`-H` go in its login, `-a` sets its packet size (512 to 32767 bytes, as `-a`
takes it; another value is an invalid argument), and `-A` puts `admin:` before
the server, as sqlcmd does, so the connection is checked as `admin:` is (port
1434); with `-D`, whose data source names the server, sqlcmd does not add it.
`-M` connects over TCP only, so a server without a prefix is checked over TCP
(connection input reports `tcp`, not `auto`); the diagnostic client still
connects to the addresses one at a time rather than all at once, which a
limitation states once it connects (when sqlcmd's own driver made the
connection attempt, the limitation is about the diagnostic client's evidence
only, since the driver used `-M`). The text report's depth line names each
option given (`; read-only application intent`, `; MultiSubnetFailover`,
`; packet size <n> bytes`, `; dedicated administrator connection`,
`; workstation <name>`), with where it came from under connection input; the
JSON report's `target` carries `applicationIntent`, `multiSubnetFailover`,
`packetSize`, `dedicatedAdminConnection` and `workstationName`, each left out
at its default, and connection input's `sources` their origins. The server is
reported as given to `-S`.

An explicitly empty `-H` remains empty, distinct from an omitted workstation
name; callers preserve that distinction with `workstation_name_source`.
ODBC error-chain entries retain nonzero signed native codes as `nativeError`;
`number` remains the unsigned SQL Server number when available. The clock
offset uses the minimal query's midpoint derived from callback entry and its
connection/query durations, excluding disconnect and result-cleanup time.

Connection input reports, without any network call, the option combinations
native sqlcmd accepts but its driver refuses or that do not do what they ask:

- `-M` (or a DSN's `MultiSubnetFailover=Yes`) with named pipes (`np:` or a
  pipe path), shared memory (`lpc:`) or the dedicated administrator connection
  (`admin:` or `-A`): MultiSubnetFailover connects only over TCP, and the ODBC
  driver, as the client, refuses the others. Connection input is `diagnosed`
  (`error`: `multiSubnetFailoverNeedsTcp`, `client.inputConfiguration`), with
  a confirmed finding, and the checks after it, sqlcmd's own driver connection
  included, are skipped.
- `-A` with `-D`: sqlcmd gives its driver the data source, so `-A` is ignored
  (informational finding).
- `-A` with a protocol prefix in `-S` (`tcp:`, `np:`, `lpc:`, `admin:`): sqlcmd
  makes it `admin:<prefix>:...`, and the client takes the prefix as part of
  the server name; with a pipe path the client opens the pipe instead; with a
  port the client refuses it, since the dedicated administrator connection
  takes none (informational findings; the checks then show the outcome).

Combinations sqlcmd itself refuses (`-E` with `-U` or `-P`, `-a` outside 512 to
32767, `-l` outside 0 to 65534) never reach diagnose. Others are not reported
because no local rule decides them: `-M` with a named instance (the driver
supports it), `-K ReadOnly` with `-A` (read-only routing is the server's), an
empty `-H` (the driver sends it as given), and `-C` or `-F` with `-No` (they
take effect only when the server forces encryption).

A named instance whose port SQL Server Browser does not give (no usable port
in its answer, for `tcp:host\instance` an answer that lists only a pipe
included; no answer; or a request this computer would not send) is not
tried on port 1433 or any other port. At `endpointResolution` and every deeper
depth, the `instanceResolution` check reports the Browser's `result`, carries
`"requires": "explicitPort"`, and its finding asks for the instance's port as
`-S <host>,<port>`. The checks that need the port (`tcpConnect` and the ones
after it) are skipped with `"reason": "explicitPortRequired"` and
`"blockedBy": "instanceResolution"`. A pipe instance (`np:`) has no port to
give: the checks after it are `blockedByPrerequisite`. When the Browser gave
the port but the time limit was spent before the client tried it, `tcpConnect`
is `inconclusive`, with `"result": "notAttempted"` and the time-limit error;
likewise for the pipe the Browser's answer pointed to (`np:`, or without a
prefix an answer with only a pipe), named by `transport` and `pipe`.

A pipe given directly (`np:`, `lpc:`, LocalDB) that does not open is
reported at `networkReachability` by `tcpConnect`, the reachability check,
which names the transport (`transport`, `pipe`) instead of a port:
`diagnosed` when the operating system refused it, `inconclusive`
(`result: timedOut`) when it did not answer in the time limit or the pipe's own
wait for a free instance timed out. Deeper, the connection attempt reports it.

Each check reports a coverage state
(`passed`, `diagnosed`, `classified`, `inconclusive`, `skipped`,
`notApplicable`), its duration on a monotonic clock, its time limit (`-l`, or sqlcmd's default login
timeout of 8 s; 2 s for SQL Server Browser), and what it found: the addresses
resolved, the operating system's result for every TCP connect (`refused`,
`timedOut`, `hostUnreachable`, ...), the connection's phases (each `passed`,
`failed`, or `abandoned` when the attempt ended elsewhere before it answered;
a phase that ran out of time is the `failedPhase`, and failed), and with `-E` the
SPN the client requests and, off Windows, whether a Kerberos ticket cache
exists. Session validation reports how far the server's clock is from the
client's, that the login completed (`loginCompleted`), the last operation that
succeeded (`lastSuccessfulOperation`: `login`, or `query` once the query
passed) and, for the diagnostic client, `retries: 0`: its
`connect_retry_count` is 0, which also turns off its session recovery, so it
never reconnects during the query and the first error is the one reported.
When the session's connection is lost during the query, `transportLoss` says
how, from the I/O error beneath any wrapper: `closed` (a read found the
connection's end), `reset` (WinSock 10054, `ECONNRESET`), `aborted` (10053,
`ECONNABORTED`), `brokenPipe` (`EPIPE`) or `communicationFailure`, with the
operating system's code as `osError`. Nothing observed shows whether SQL
Server, a load balancer, firewall, proxy, NAT, VPN or the client ended it, so
the check is `inconclusive`, the area `undetermined` between
`connectivity.network` and `target.sqlServer`, its finding names no source,
and a limitation says a loss can be intermittent, which one attempt neither
reproduces nor analyzes; only SQL Server's own error is `diagnosed` with
`target.sqlServer`. Through sqlcmd's own driver connection only the driver's
SQLSTATE 08S01 shows a lost connection: it is `communicationFailure`, with no
operating-system code, and no `retries` (the driver's own are not seen); a
driver timeout (`HYT00`, `HYT01`) is `inconclusive` between the network and the
server, and another driver failure without a SQL Server error between the
driver and the server. In text, these show in the details when the query
failed (`login completed; last successful operation: login; retries: 0;
transport loss: reset (OS error 10054)`). When the resolver fails and the client goes on with addresses its
DNS cache kept from an earlier resolution, `nameResolution` reports the
resolver's failure, with `"source": "fallbackCache"` and the cached
addresses, and `tcpConnect` is still checked with those addresses. The report adds:

- `executionStatus` (`completed`, `partial`, `canceled`, `failed`,
  `invalidInvocation`) and `diagnosticOutcome` (`passed`, `issueDetected`,
  `inconclusive`, `notEvaluated`), and an exit code per category: 0 passed,
  1 issue detected, 2 inconclusive, 3 partial, 4 canceled, 5 internal failure,
  6 invalid invocation. A run is `partial` when this computer would not let a
  check run (for example, the operating system refused to open the socket), and
  `canceled` when the host cancels it (`mssql_sqlcmd_diagnostics_cancel`; sqlcmd
  does so on Ctrl+C): the checks that finished are kept and the others are
  skipped as `canceled`;
- findings marked `confirmed`, `suspected` or `informational`, and no
  remediation advice beyond the request for a named instance's port above;
- the ordered error chain, with SQL Server's errors in the order the ODBC
  driver lists them: an error with its own SQLSTATE (18456 is 28000) before
  one given its class's (42000 for classes 11 to 18, then HY000, then 01000),
  otherwise as received, not by class. In text, an error the driver has a
  wording for is printed as sqlcmd prints it
  (`Sqlcmd: Error: mssql-tds : ...`), once when several addresses give the
  same line; any other error is printed with its
  identifiers (SQL Server error, state and class, operating-system error,
  symbolic code). The JSON report keeps every error with its identifiers, the
  driver's wording as `errors[].text`, and `fromMessageText` where a
  classification relied on message text. After a failed connection the chain
  ends with the driver's summaries, as `client` errors:
  `networkRelatedError` when the transport did not open,
  `preloginFailure` or `communicationLinkFailure` when a pre-login or login
  read failed after it opened, or `clientUnableToEstablishConnection` after a
  failed TLS handshake (with the driver's link to its encryption guide only
  for `-N` mandatory without `-C`, as the driver decides it, whatever the TLS
  error), and `loginTimeoutExpired` only when a time limit ran out
  (outside Windows it comes before the errors it sums up, where the driver
  prints it). When the client's own time limit runs out, its error has the
  code `timedOut`; when the server closed the connection, `connectionClosed`
  (a read that failed, summarized as such in pre-login and login);
- the specialist domain for a support handoff (`connectivity.network`,
  `security.tlsCertificate`, `security.authentication.sql`, ..., or
  `undetermined` with candidates), never presented as a root cause; unless a
  diagnosed check named it, the text and `specialistDomain.note` say it is the
  area for a support handoff, not a confirmed cause;
- limitations, such as the connection client used;
- the client's operating system and version;
- `tracingGuide`, a stable link to the driver-tracing documentation, when the
  connection attempt or session validation ends where the client's evidence
  does not reach;
- `coverageMatrixVersion`, the version of
  [the diagnostic coverage matrix](diagnose-coverage.md) the checks follow.

Both reports follow sqlcmd's locale (see [localization](localization.md)). In
JSON only sqlcmd's own prose is translated: `sharingWarning`,
`exitCategory.meaning`, `checks[].detail`, `findings[].text`, `limitations`,
`specialistDomain.note`, `client.osVersion`'s wording and sqlcmd's own
`errors[].message`. Keys, ids, names, codes, statuses, exit codes and
`contractVersion` are the same in every locale, as are texts sqlcmd does not
own: SQL Server's, the operating system's and `mssql-tds`'s messages as
received, and the driver's wording in `errors[].text`.

FR-097 observations are separate from required checks. After an observed
session transport loss, `outOfScopeObservations` identifies recurrence/source
analysis as `outOfScope`, retains the check's evidence, names network and SQL
Server specialists, and gives a tracing handoff. The text report has the same
separate section. One loss does not prove intermittency or its cause.
The required `sessionValidation` check remains in scope and keeps its result;
observations never change aggregation, execution status or exit category.
No observations property/section is emitted when nothing was observed.

Diagnose output is for review before sharing; it is never share-safe. Every
report begins with this warning, in the report's locale
(`diagnose.sharing.warning`; in JSON, the top-level `sharingWarning`); in
English:
"Diagnostic output may contain unobfuscated server names, host names, and IP
addresses. Review the output before sharing it." Server, instance and host
names (the `-F` certificate name included), IP addresses, named pipe paths and
Kerberos SPNs identify the endpoint and are shown as they are. Database and
user names are replaced with labels (`database-1`, `user-1`) that are stable
within one run only, in the text and JSON report alike, and in error text
where they appear as whole words. `--local-detail` shows them as they are
(JSON `localDetail: true`). Text sqlcmd cannot classify, such as error
messages, is flagged for review before sharing (`errors[].message` and
`errors[].text` in `reviewBeforeSharing`), with or without local detail. No
check reports certificate subject or SAN names or file paths; they would be
labelled the same way. The password is never part of the report.

For transport evidence, `mssql-tds` makes one diagnostic connection attempt,
through its diagnostic entry point (`TdsConnectionProvider::diagnose_client`):
it resolves the name, asks SQL Server Browser (past its instance cache)
whenever the attempt reaches it, follows its protocol order, connects, and
logs in. The depth sets
where it stops: after resolution (`endpointResolution`), or once the
transport is open, before pre-login (`networkReachability`). Over TCP, once an
address accepts, the client also connects to the addresses after it (up to
64), at the same time and each within the time limit, closing each
connection at once, so every address is reported while the login goes on
over the first. The
attempt's connect-stage spans (target `mssql_tds::connect`, see
`mssql_tds::connection::connect_stage`), recorded by a `tracing` layer that
enables nothing else, are the evidence: each stage (`dns`, `instance_lookup`,
`tcp`, `pipe`, `prelogin`, `tls`, `login`) records what it found, and whether
it succeeded or failed with what; a stage the time limit cut short was
abandoned, which is not a failure.

Strict (TDS 8.0) transport diagnostics include TLS negotiation. On macOS,
Network.framework combines TCP connection and TLS negotiation in one attempt.
If that attempt times out, `tcpConnect` is inconclusive, with
`"tcpAcceptance": "unknown"`: the report does not claim that no server accepted
TCP, because the peer may have stalled during TLS, and its errors end with
`loginTimeoutExpired` but not the network-related summary for a transport that
did not open.
Successful attempts and explicit TCP refusals retain their usual results.

A server given without a protocol prefix or port on Windows follows the
client's protocol order: shared memory first for this computer (when it
connects, the TCP checks do not apply), then TCP, then named pipes. The
errors of a transport the order moved past stay in the JSON report, marked
`superseded`, as a normal run would not print them. When TCP does not
connect and the named pipe the order then tries opens, the TCP checks that
failed do not apply either (`notApplicable`: sqlcmd's connection did not use
TCP), and neither the outcome, the execution status nor the specialist area
counts them. With a port, or
elsewhere, it is TCP alone, and `lpc:` (on Windows, to this computer only)
or `np:` diagnoses a transport directly. At `endpointResolution`, which opens
no connection, a local named instance given this way is not looked up, as
the client asks SQL Server Browser only once shared memory has failed:
`instanceResolution` is `inconclusive`, with `"result": "notLookedUp"`, and
the outcome is `inconclusive`. Run the default depth, or give
`tcp:host\instance` to check the Browser lookup.

When the caller passes its own driver connection (`driver_connect`: native
sqlcmd connects with its ODBC driver and every connection option, as for a
query), that connection makes the connection attempt and session validation
and decides the result, even if a preliminary name or TCP check failed.
`mssql-tds` still makes its diagnostic connection for
the name, instance and transport checks, stopping once the transport is open,
without logging in; it logs in, as a normal run does, only when the driver
failed and its errors leave the cause open (not for a refused login or
database, so a wrong password is never tried twice), and its findings are then
evidence. The driver's records are the chain's `driver` errors: in text, those
sqlcmd prints appear as sqlcmd printed them (`Sqlcmd: Error: <ODBC driver
name> : ...`); the JSON report keeps every one, with its `sqlState`. They
include the driver's own summaries, so diagnose adds none.
Connection records are kept separate from query records: a login information
message such as 5701 cannot replace a failed query's actual error. The deciding
session query is the same edition/clock probe for ODBC and `mssql-tds`.
Native sqlcmd runs only that minimal query; it does not issue another query
for version, transport, server name or database metadata. Those details are
unavailable unless the caller already knows and supplies them. A successful
native connection supersedes preliminary diagnostic-client failures even when
its transport is unavailable; the report does not infer a transport from that
success.
JSON `client.connectionClient` is `odbc` when this callback is supplied,
otherwise `mssql-tds`. The callback applies its supplied login time limit and
sets the minimal query's ODBC statement timeout. Minimal-probe result-shape
and column-read failures fail session validation rather than becoming SQL
NULL values. The synchronous native callback must return before cancellation
can be reported, so an ODBC operation already in progress may wait for its
configured timeout. Checks that have finished are retained; checks not started
are marked canceled.
`connectionAttempt.driver` names the ODBC driver. When a diagnostic evidence
login was necessary, `connectionAttempt.diagnosticClient` says whether it
connected or failed; it never replaces the authoritative ODBC result.

`mssql-tds` is 64-bit only, so 32-bit builds (`win-x86`) leave diagnose out:
there `mssql_sqlcmd_diagnostics_run` returns `MSSQL_SQLCMD_UNSUPPORTED`.
`mssql-tds` is compiled into the archive and needs the TLS libraries it uses.
On Windows these are Schannel and other Win32 libraries, listed in
`native-static-libs.txt`. On Linux the archive does not link OpenSSL: it loads
`libssl.so.4`, else `libssl.so.3`, else `libssl.so.1.1`, when TLS is first used,
so `native-static-libs.txt` lists no `libssl` or `libcrypto`; without OpenSSL
installed, only TLS connections are unavailable.

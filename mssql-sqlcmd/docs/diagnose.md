# sqlcmd diagnose

`sqlcmd diagnose [-S <server>] [-U <login> -P <password> | -E] [-d <database>] [-N[s|m|o]] [-C] [-F <certificate-name>] [-l <seconds>] [--depth <depth>] [--local-detail] [--format json]`
checks, from the bottom up, that a connection can be made, and when it cannot,
where and why. It follows the diagnostic depths of the sqlcmd specification;
each includes the ones before it, and the deepest is the default:

| Depth (`--depth`) | Checks |
|---|---|
| `connectionInput` | the settings sqlcmd resolved and where each came from (a command-line option, an environment variable, or the default); the server given to `-S`, checked with the client's rules; local findings for an instance name with a port and for `-F` with `-C`; no external call |
| `endpointResolution` | the client resolves the host name and asks SQL Server Browser for a named instance's port (never guessing 1433), then stops |
| `networkReachability` | the client also opens its transport: a TCP connection to the first resolved address that accepts, and to every address after it, or a pipe; then it stops |
| `connectionAttempt` | the client also logs in, with its pre-login, TLS and login phases |
| `sessionValidation` | a minimal query on the new session: the engine edition and the server's UTC clock |

Checks run best effort: a failed check skips only what needs its result, and
each skipped check names what blocked it. In the JSON report a skipped check
gives its `reason` (`blockedByPrerequisite`, `explicitPortRequired`,
`notApplicable`, `canceled`), the check that blocked it as `blockedBy`, and a
`detail` in words (none for `blockedByPrerequisite`).

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
is `inconclusive`, with `"result": "notAttempted"` and the time-limit error.

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
client's. When the resolver fails and the client goes on with addresses its
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
  `networkRelatedError`, or `clientUnableToEstablishConnection` after a failed
  TLS handshake (with the driver's link to its encryption guide only for `-N`
  mandatory without `-C`, as the driver decides it, whatever the TLS error), and `loginTimeoutExpired` only when a time limit ran out
  (outside Windows it comes before the errors it sums up, where the driver
  prints it). When the client's own time limit runs out, its error has the
  code `timedOut`;
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

Diagnose output is for review before sharing; it is never share-safe. Every
report begins with this warning (localized in the text report,
`diagnose.sharing.warning`; in JSON, the top-level `sharingWarning`, always in
English):
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

The checks observe the client's real connection: sqlcmd makes no network call
of its own. `mssql-tds` makes one connection attempt, as a normal run does,
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

`mssql-tds` is 64-bit only, so 32-bit builds (`win-x86`) leave diagnose out:
there `mssql_sqlcmd_diagnostics_run` returns `MSSQL_SQLCMD_UNSUPPORTED`.
`mssql-tds` is compiled into the archive and needs the TLS libraries it uses:
Schannel and other Win32 libraries on Windows, OpenSSL (`libssl`,
`libcrypto`) on Linux, listed in `native-static-libs.txt`.

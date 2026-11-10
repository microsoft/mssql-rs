# sqlcmd diagnose

`sqlcmd diagnose [-S <server> | -D -S <dsn>] [-U <login> -P <password> | -E | -G {-U <login> -P <password> | -P <token-file>}] [-d <database>] [-N[s|m|o]] [-C] [-F <certificate-name>] [-l <seconds>] [--depth <depth>] [--local-detail] [--format json]`
checks, from the bottom up, that a connection can be made, and when it cannot,
where and why. It follows the diagnostic depths of the sqlcmd specification;
each includes the ones before it, and the deepest is the default:

| Depth (`--depth`) | Checks |
|---|---|
| `connectionInput` | the settings sqlcmd resolved and where each came from (a command-line option, an environment variable, an ODBC data source (DSN) given with `-D`, or the default); the server given to `-S`, or the one the DSN names, checked with the client's rules; with `-G -P <token-file>`, the access token file, read as sqlcmd reads it; local findings for an instance name with a port, for `-F` with `-C`, for a DSN that names no server, and for a login that a DSN's `Trusted_Connection=yes` leaves unused; no external call |
| `endpointResolution` | the client resolves the host name and asks SQL Server Browser for a named instance's port (never guessing 1433), then stops |
| `networkReachability` | the client also opens its transport: a TCP connection to the first resolved address that accepts, and to every address after it, or a pipe; then it stops |
| `connectionAttempt` | the client also logs in, with its pre-login, TLS and login phases |
| `sessionValidation` | a minimal query on the new session: the engine edition and the server's UTC clock |

Checks run best effort: a failed check skips only what needs its result, and
each skipped check names what blocked it. In the JSON report a skipped check
gives its `reason` (`blockedByPrerequisite`, `explicitPortRequired`,
`notApplicable`, `canceled`, `notSupported`), the check that blocked it as
`blockedBy`, and a `detail` in words (none for `blockedByPrerequisite`). A
check that is `notSupported` could not be completed because diagnose lacks
what it needs; it keeps the phases it went through, and the run is
`partial`.

With `-G` (Microsoft Entra authentication), diagnose takes the method sqlcmd
selects, and names it as sqlcmd's `--format json` does: `-G` with `-U` is
`ActiveDirectoryPassword`, and `-G -P <file>` without `-U`
`ActiveDirectoryAccessToken`, with the authentication's source `-G`. `-G`
without `-U` or `-P` (`ActiveDirectoryIntegrated`) is not supported yet: the
driver sqlcmd uses then logs in with Microsoft Entra, or with Windows
authentication (Kerberos elsewhere) when the server's pre-login answer does
not require Microsoft Entra or only the login is encrypted, and diagnose does
not model that choice. Connection input reads the access token file as sqlcmd
does, its bytes as they are (up to sqlcmd's 1 MiB), and the client sends them
as the token: a file that is missing (`accessTokenFileNotFound`), cannot be
read (`accessTokenFileUnreadable`), is too large (`accessTokenFileTooLarge`)
or is not UTF-16LE text (`accessTokenFileNotUtf16`), the form in which sqlcmd
passes the token to the driver, is `diagnosed` with
`client.inputConfiguration`, and the file is not named. The server's rejection
of an Entra login is `diagnosed` with `security.authentication.entra` and
`"entraStage": "serverRejection"`. diagnose does not acquire Entra tokens yet:
with `ActiveDirectoryPassword` the client goes as far as the server's request
for a token, after pre-login and TLS; the connection attempt is then `skipped`
(`notSupported`), with an informational finding that the server asked for a
token, the authentication outcome is `notEvaluated`, and the run is
`partial`. No token, password or authority the server names is reported.

With `-D` (Linux and macOS; sqlcmd on Windows treats `-D` as obsolete), `-S`
names an ODBC data source (DSN). Native sqlcmd reads the DSN from the user and
system `odbc.ini` with the same calls and rules as the driver, and passes the
settings a normal run takes from it: its server, and its database,
authentication (`Trusted_Connection`) and certificate settings
(`TrustServerCertificate`, `HostNameInCertificate`) unless the command line or
an environment variable sets them. Without `-U` or `-E`, sqlcmd gives the
driver no login: unless the DSN sets `Trusted_Connection=yes`, the driver logs
in as a SQL login with no name (it does not prompt, so it takes no `LastUser`
from the DSN). sqlcmd always sets encryption itself, so a DSN's `Encrypt` is
not used. Each setting the DSN supplied has the source `dsn` (in JSON
`"source": "dsn", "name": "<dsn>"`; in text, `server from DSN <dsn>`, and in
the depth line `server certificate trusted (DSN <dsn>)` rather than `(-C)`),
and `connectionInput` names the DSN (`dsn`). `-C` and `-F` override the DSN's
`TrustServerCertificate` and `HostNameInCertificate` each on its own, so the
finding about a certificate name that trust leaves unvalidated names where each
of the two came from, the DSN or the option. A
DSN that was not found, or names no driver (`dsnNotFound`), is `diagnosed` at
`connectionInput`, with `client.inputConfiguration`, and the checks after it,
which need a server, are skipped. A DSN that names no server is its own server,
as the driver makes it: connection input says so in an informational finding,
and the checks go on with it. When a DSN sets `Trusted_Connection=yes`, the
driver logs in with integrated authentication and does not use a login given
with `-U` or `SQLCMDUSER`; connection input reports that as an informational
finding. Native sqlcmd refuses, as an invalid invocation, a DSN with settings
diagnose does not model yet (`Authentication`, `ServerCertificate`, a
read-only `ApplicationIntent`, `MultiSubnetFailover=Yes`, or `RegPrecedence`),
as it refuses `-G`, `-J`, `-K` and `-M`.

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
client's. When the resolver fails and the client goes on with addresses its
DNS cache kept from an earlier resolution, `nameResolution` reports the
resolver's failure, with `"source": "fallbackCache"` and the cached
addresses, and `tcpConnect` is still checked with those addresses. The report adds:

- `executionStatus` (`completed`, `partial`, `canceled`, `failed`,
  `invalidInvocation`) and `diagnosticOutcome` (`passed`, `issueDetected`,
  `inconclusive`, `notEvaluated`), and an exit code per category: 0 passed,
  1 issue detected, 2 inconclusive, 3 partial, 4 canceled, 5 internal failure,
  6 invalid invocation. A run is `partial` when this computer would not let a
  check run (for example, the operating system refused to open the socket), or
  when diagnose could not complete a check (`notSupported`, such as a Microsoft
  Entra login whose token diagnose does not acquire), and
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

`mssql-tds` is 64-bit only, so 32-bit builds (`win-x86`) leave diagnose out:
there `mssql_sqlcmd_diagnostics_run` returns `MSSQL_SQLCMD_UNSUPPORTED`.
`mssql-tds` is compiled into the archive and needs the TLS libraries it uses.
On Windows these are Schannel and other Win32 libraries, listed in
`native-static-libs.txt`. On Linux the archive does not link OpenSSL: it loads
`libssl.so.4`, else `libssl.so.3`, else `libssl.so.1.1`, when TLS is first used,
so `native-static-libs.txt` lists no `libssl` or `libcrypto`; without OpenSSL
installed, only TLS connections are unavailable.

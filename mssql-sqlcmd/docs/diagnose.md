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

This build runs the `connectionInput` checks only: at a deeper depth the
other checks are reported as skipped, `notCheckedByThisBuild`.

Checks run best effort: a failed check skips only what needs its result, and
each skipped check names what blocked it. Each reports a coverage state
(`passed`, `diagnosed`, `classified`, `inconclusive`, `skipped`,
`notApplicable`), its duration on a monotonic clock, its time limit when it
has one, and what it found. The report adds:

- `executionStatus` (`completed`, `partial`, `canceled`, `failed`,
  `invalidInvocation`) and `diagnosticOutcome` (`passed`, `issueDetected`,
  `inconclusive`, `notEvaluated`), and an exit code per category: 0 passed,
  1 issue detected, 2 inconclusive, 3 partial, 4 canceled, 5 internal failure,
  6 invalid invocation. A run is `canceled` when the host cancels it
  (`mssql_sqlcmd_diagnostics_cancel`; sqlcmd does so on Ctrl+C): the checks
  that finished are kept and the others are skipped as `canceled`;
- findings marked `confirmed`, `suspected` or `informational`, and no
  remediation advice;
- the ordered error chain with its identifiers (a symbolic code, such as
  `invalidPort`);
- the specialist domain for a support handoff (`client.inputConfiguration`,
  ..., or `undetermined` with candidates), never presented as a root cause;
  unless a diagnosed check named it, the text and `specialistDomain.note` say
  it is the area for a support handoff, not a confirmed cause;
- limitations;
- the client's operating system and version;
- `coverageMatrixVersion`, the version of
  [the diagnostic coverage matrix](diagnose-coverage.md) the checks follow.

Diagnose output is for review before sharing; it is never share-safe. Every
report begins with this warning (in JSON, the top-level `sharingWarning`):
"Diagnostic output may contain unobfuscated server names, host names, and IP
addresses. Review the output before sharing it." Server, instance and host
names (the `-F` certificate name included), IP addresses, named pipe paths and
Kerberos SPNs identify the endpoint and are shown as they are. Database and
user names are replaced with labels (`database-1`, `user-1`) that are stable
within one run only, in the text and JSON report alike, and in error text
where they appear as whole words. `--local-detail` shows them as they are
(JSON `localDetail: true`). Text sqlcmd cannot classify, such as error
messages, is flagged for review before sharing (JSON `reviewBeforeSharing`),
with or without local detail. No check reports certificate subject or SAN
names or file paths; they would be labelled the same way. The password is
never part of the report.

The checks that connect use `mssql-tds`, which is 64-bit only, so 32-bit
builds (`win-x86`) leave diagnose out: there `mssql_sqlcmd_diagnostics_run`
returns `MSSQL_SQLCMD_UNSUPPORTED`.

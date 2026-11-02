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
| `PHASE-LOCAL-INPUT` | `connectionInput` (`connectionInput`) | the resolved settings that native sqlcmd passes in (command-line options, environment variables and defaults, with source labels); the `-S` server grammar: protocol prefix (only `tcp:`, `np:`, `lpc:` and `admin:`; anything else before a colon is part of the server name, as for the client), host, `\instance`, `,port` (0 to 65535, as the client takes it; an empty port is 1433, or for a named instance the port SQL Server Browser gives), LocalDB and named pipes only on Windows, and shared memory (`lpc:`) only to this computer and only on Windows, as the client allows them; local findings for a named instance with an explicit port, for `-F` with `-C`, and for `-C` with `-Ns` (strict encryption ignores `-C` and validates the certificate); no external call | passed, diagnosed | all | P1, shipped for command-line options, SQLCMD/OSQL environment variables and defaults. The ODBC connection-string grammar (`-D` DSN, keyword strings) and driver-load signals are not evaluated: the attempt is made with `mssql-tds`, not the ODBC driver. |
| `PHASE-ENTRA-AUTH` | — | — | — | — | P1, not yet: `-G` is refused by `sqlcmd diagnose`. |
| `PHASE-COMMAND-DEADLINE` | — | — | — | — | Not part of `sqlcmd diagnose`: it covers user queries, which diagnose does not run. |

The network and connection phases (name resolution, the named instance, TCP,
TLS, authentication, the database, the deadlines and the session) are not
checked by this build: at a deeper depth diagnose reports their checks as
skipped, `notCheckedByThisBuild`.

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

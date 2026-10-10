# mssql-sqlcmd

sqlcmd components in Rust. Native (ODBC) sqlcmd links this crate as a static
library and calls it through a C ABI; native sqlcmd still parses the command
line, connects and runs the batches. The one exception is `sqlcmd diagnose`,
which connects through this crate (with `mssql-tds`) to check the connection
stage by stage.

## Layout

| Path | Purpose |
|---|---|
| `src/formatter/` | Output formatters. `json.rs` renders `--format json`. |
| `src/diagnostics.rs`, `src/diagnostics/` | `sqlcmd diagnose`: the checks, depth by depth; `target.rs` parses `-S`, `stages.rs` times the `mssql-tds` connect stages, `redact.rs` makes output share-safe, `report.rs` renders text and JSON. |
| `src/ffi.rs` | The C ABI native sqlcmd calls, including `mssql_sqlcmd_version()`. |
| `include/mssql_sqlcmd.h` | C/C++ declarations for that ABI. |

## JSON output

Native sqlcmd hands each piece of output to a JSON document instead of printing
it — the batches it sends, result sets and rows, row counts, server messages —
and renders the document once, at exit, with the connection details, timing,
status and exit code. The document follows the `format json` contract of the
sqlcmd specification, `contractVersion` 1.0. It is written with `serde_json`,
indented, with each row and column description on one line; shown here more
compactly:

```json
{
  "contractVersion": "1.0",
  "sqlcmd": { "version": "18.7.0001.1", "platform": "win-x64" },
  "connection": { "server": "tcp:localhost,1433", "database": "master", "authentication": "SqlPassword",
                  "encrypt": true, "serverVersion": "17.00.4085", "connectMs": 167 },
  "startTime": "2026-10-03T04:17:02.118Z",
  "durationMs": 178,
  "executionStatus": "completed",
  "operationOutcome": "succeeded",
  "exitCode": 0,
  "output": [
    { "type": "batch", "index": 1, "durationMs": 7, "text": "SELECT id, price FROM t;\nPRINT 'done';\n" },
    { "type": "resultSet",
      "columns": [ { "ordinal": 0, "name": "id", "driverType": "int", "precision": 10, "scale": 0, "nullable": false },
                   { "ordinal": 1, "name": "price", "driverType": "decimal", "precision": 10, "scale": 2, "nullable": true } ],
      "rows": [ ["1", "9.99"], ["2", null] ],
      "rowsAffected": 2 },
    { "type": "message", "number": 0, "severity": 0, "state": 1, "server": "db01", "line": 2, "sqlState": "01000", "text": "done" }
  ]
}
```

- **Header.** `contractVersion` is `major.minor`: the major version changes only
  for an incompatible change. `platform` is the runtime (`win-x64`,
  `linux-musl-arm64`, ...). `serverVersion` and `connectMs` appear once sqlcmd
  has connected. `startTime` (UTC) and `durationMs` cover the whole run.
- **Status.** `executionStatus` says whether sqlcmd carried out the run:
  `completed`, `canceled` (Ctrl+C) or `invalidInvocation` (the command line was
  rejected). `operationOutcome` is `succeeded`, `failed`, `canceled` or
  `notExecuted`: with the exit code, which is unchanged by `--format json`, it
  says whether the work succeeded. No failure category is derived; the errors
  themselves say what went wrong. A statement error that does not stop the run
  is an `error` entry in a `succeeded` document, as its exit code is 0.
- **Batches.** One `batch` entry per batch sent (`GO` separates batches), with
  its duration. Its `text` is included only with `-e`, as text mode echoes it
  only with `-e`, so a password in, say, `CREATE LOGIN` is not written out by
  default.
- **Result sets.** Columns are described as the driver describes them:
  `ordinal` (from 0) and `name` (`null` when unnamed) always; `driverType`,
  `size`, `precision`, `scale` and `nullable` when the driver exposes them, and
  type names exactly as the driver reports them. Rows are positional arrays.
  Values are strings, as sqlcmd would print them: binary with a `0x` prefix,
  `-R` and `-k` applied, and cut to the same `-y`/`-Y` width as text output;
  SQL `NULL` is JSON `null`. `rowsAffected` is the result set's "(n rows
  affected)", absent under `SET NOCOUNT ON`; a statement without a result set
  gets its own entry instead, `{ "type": "rowsAffected", "count": 3 }`.
- **`FOR JSON` / `FOR XML`** output is ordinary result-set rows: one string
  per row the server returned, not parsed, combined or embedded.
- **Messages and errors** carry what the console shows for them, in its order:
  `number` (Msg), `severity` (Level), `state`, and when known `server`,
  `procedure`, `line` and `source` (the driver, e.g. `Microsoft ODBC Driver 18 for
  SQL Server`, or `Sqlcmd`, when the server did not send it). `sqlState` is the
  ODBC SQLSTATE (`28000` for a refused login, `08001` for a connection failure,
  `HYT00` for a timeout), the stable field to classify errors by. `text` is the
  message itself.

The whole document is held in memory until sqlcmd exits, because it begins
with details only known then (the exit code). Peak memory is a few times the
size of the output: the collected values, the rendered UTF-8 text, and the
UTF-16 copy handed back to sqlcmd. For very large results, text output still
streams.

## Diagnose

`sqlcmd diagnose -S <server> [-U/-P | -E] [-d] [-N...] [-C] [-F] [-l] [--depth <depth>] [--local-detail] [--format json]`
checks, from the bottom up, that a connection can be made, and when it cannot,
where and why. It follows the diagnostic depths of the sqlcmd specification;
each includes the ones before it, and the deepest is the default:

| Depth (`--depth`) | Checks |
|---|---|
| `connectionInput` | parse the server given to `-S`; no external call |
| `endpointResolution` | resolve the host name; ask SQL Server Browser for a named instance's port (never guessing 1433) |
| `networkReachability` | a TCP connect to every resolved address |
| `connectionAttempt` | one connection attempt, with its pre-login, TLS and login phases |
| `sessionValidation` | a minimal query on the new session: the engine edition and the server's UTC clock |

Checks run best effort: a failed check skips only what needs its result, and
each skipped check names what blocked it. Each reports a coverage state
(`passed`, `diagnosed`, `classified`, `inconclusive`, `skipped`,
`notApplicable`), its duration on a monotonic clock, its time limit (`-l`, 8 s by
default; 2 s for SQL Server Browser), and what it found: the addresses
resolved, the operating system's result for every TCP connect (`refused`,
`timedOut`, `hostUnreachable`, ...), the connection's phases, and with `-E` the
SPN the client requests and, off Windows, whether a Kerberos ticket cache
exists. Session validation reports how far the server's clock is from the
client's. The report adds:

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
  remediation advice;
- the ordered error chain with its identifiers (SQL Server error, state and
  class, operating-system error, symbolic code), noting where a classification
  relied on message text;
- the specialist domain for a support handoff (`connectivity.network`,
  `security.tlsCertificate`, `security.authentication.sql`, ..., or
  `undetermined` with candidates), never presented as a root cause;
- limitations, such as the connection client used;
- the client's operating system and version;
- `tracingGuide`, a stable link to the driver-tracing documentation, when the
  connection attempt or session validation ends where the client's evidence
  does not reach;
- `coverageMatrixVersion`, the version of
  [the diagnostic coverage matrix](docs/diagnose-coverage.md) the checks follow.

Output is share-safe by default: server, instance, database, host (the `-F`
certificate name included), address, SPN and user identifiers are replaced with labels (`host-1`, `address-2`) that are
stable within one run only, in the text and JSON report alike, and error text
is flagged for review before sharing. `--local-detail` shows the values and
marks the output as not share-safe. The password is never part of the report.

sqlcmd resolves the name, queries SQL Server Browser and connects over TCP
itself. A server given without a protocol prefix is diagnosed over TCP
throughout, the connection attempt included; on Windows, where sqlcmd can then
also use shared memory or named pipes, a TCP failure says so, and `lpc:` or
`np:` diagnoses those transports. The connection attempt is made with `mssql-tds`; its connect-stage
spans (target `mssql_tds::connect`, see `mssql_tds::connection::connect_stage`)
give the pre-login, TLS and login phases and their timings, recorded by a
`tracing` layer that enables nothing else.

`mssql-tds` is 64-bit only, so 32-bit builds (`win-x86`) leave diagnose out:
there `mssql_sqlcmd_diagnostics_run` returns `MSSQL_SQLCMD_UNSUPPORTED`.

## Localization

Rust-generated human text is routed through the `i18n` message catalog. Stable
JSON keys, enum values, identifiers, numeric codes and server/driver text are
not localized. Existing diagnostics and formatter strings are being moved to the
catalog incrementally; only catalog-backed Rust messages participate today.

Locale selection is process-wide. Native sqlcmd may call
`mssql_sqlcmd_set_locale` once at startup with the language it resolved for its
resource DLL (on Windows this can be an LCID such as `1031`). If it does not,
the crate resolves `SQLCMD_LANG`, `LC_ALL`, `LC_MESSAGES`, then `LANG`, and on
Windows finally `GetUserDefaultUILanguage()`. Values may be BCP-47 (`de-DE`),
POSIX (`de_DE.UTF-8`) or LCID decimal/hex (`1031`, `0x0407`). Unknown locales
fall back to `en-US`; missing translated messages fall back per message.

The source catalog is `i18n/locales/en-US/sqlcmd.json`. OneLocBuild uses
`i18n/LocProject.json` and `i18n/P306PairNamesToProcess.lss` to create or reuse
a pull request that writes translated catalogs under `i18n/localized/<Lang>/`.
Do not hand-edit localized catalogs. To verify that a string is catalog-routed,
run with `SQLCMD_LANG=qps-ploc`; pseudo-localized text is wrapped with
`[!!! ... !!!]` while placeholders keep working.

## Building for native sqlcmd

```text
cargo build -p mssql-sqlcmd --release
```

produces `target/release/mssql_sqlcmd.lib` (Windows) or `libmssql_sqlcmd.a`.
Its Rust dependencies (`serde`, `serde_json`, and on 64-bit targets `mssql-tds`
for diagnose) are compiled into the archive, so a consumer links no third-party
Rust library. `mssql-tds` needs the TLS libraries it uses: Schannel and other
Win32 libraries on Windows, OpenSSL (`libssl`, `libcrypto`) on Linux. The system
libraries it needs at link time are listed by:

```text
cargo rustc -p mssql-sqlcmd --release --lib -- --print native-static-libs
```

## NuGet package

Native sqlcmd does not build this crate. It restores the `mssql-sqlcmd` NuGet
package, pinned in its `Directory.Packages.props`, from its own feed, which has
the Release view of `mssql-rs_Public` (`mssql-rs_Public@Release`) as an
upstream source. The package holds one static library per runtime native
sqlcmd ships on:

```text
include/mssql_sqlcmd.h
runtimes/<rid>/native/mssql_sqlcmd.lib          (win-*)
runtimes/<rid>/native/libmssql_sqlcmd.a         (linux-*, osx-*)
runtimes/<rid>/native/native-static-libs.txt
```

| Runtime | Rust target |
|---|---|
| `win-x64`, `win-x86`, `win-arm64` | `{x86_64,i686,aarch64}-pc-windows-msvc` |
| `linux-x64`, `linux-arm64` | `{x86_64,aarch64}-unknown-linux-gnu`, built on manylinux_2_28 |
| `linux-musl-x64`, `linux-musl-arm64` | `{x86_64,aarch64}-unknown-linux-musl`, dynamic C runtime |
| `osx-x64`, `osx-arm64` | `{x86_64,aarch64}-apple-darwin` |

`native-static-libs.txt` is one line: the system libraries that runtime's
library needs, exactly as rustc reports them (`-l` flags, or `.lib` names on
Windows). Consumers link these rather than a hard-coded list. On Windows the list
includes import libraries of the Rust `windows` crates (`windows.0.52.0.lib`
and the like), which are not in the Windows SDK; they are shipped next to the
library, so consumers add that directory to the linker's library path.

To build the same layout locally:

```text
.pipeline/scripts/build-mssql-sqlcmd-native.ps1 -OutputDirectory <dir>        # Windows
.pipeline/scripts/build-mssql-sqlcmd-native.sh <rust-target> <rid> <dir>       # Linux, macOS
.pipeline/scripts/pack-mssql-sqlcmd.ps1 -ArtifactsDirectory <dir> -StagingDirectory <staging> -RequiredRids <rids>
```

then `nuget pack <staging>/mssql-sqlcmd.nuspec`, or point native sqlcmd's
`SQLCMD_RUST_PACKAGE_DIR` at the staging directory directly.

## Pipelines and releases

Test builds (`-dev` on every merge to `main`, `-nightly` each night) are
published to the `mssql-rs_Public` feed automatically. Releases are built by the
Official mssql-sqlcmd Build when `stable` changes, published by hand with the
ADO-Release Nuget mssql-sqlcmd pipeline, and then promoted to the feed's
`Release` view, which is what msodbcsql consumes.

The full process (pipelines, versions, feed retention, the release steps,
the msodbcsql side, and troubleshooting) is in
[docs/mssql-sqlcmd-release-management.md](https://github.com/microsoft/mssql-rs/blob/main/docs/mssql-sqlcmd-release-management.md).

# `sqlcmd` parity: Rust vs ODBC vs Go

How the Rust `sqlcmd` (`mssql-sqlcmd`) compares with the two tools it replaces,
option by option.

- **ODBC** — `sqlcmd` 18.6 shipped with the Microsoft ODBC Driver for SQL Server
- **Go** — `go-sqlcmd`, built from `microsoft/go-sqlcmd` `main`
- **Rust** — this crate

Everything below was **measured against the real binaries**, not read from
documentation. Where the three disagree, the disagreement is recorded rather
than smoothed over.

---

## 1. Overall

| Area | ODBC | Go | Rust | Verdict |
|---|---|---|---|---|
| Short options (`-S`, `-Q`, …) | 47 | 45 | **52** | Rust is a superset of both |
| Long options (`--server`, …) | none | 53 | **57** | Rust accepts every Go name |
| Colon commands | 16 | 13 | **16** | Rust matches ODBC; Go lacks 3 |
| Scripting variables | 15 | 18 | **18** | Rust matches Go's superset |
| Output formats | fixed-width | + vertical, ASCII | + CSV, JSON | Rust is a superset |
| Entra ID methods | 6 | 15 | **15** | Full parity with Go |
| Container lifecycle | — | yes | **yes** | Full parity |
| `SQLCMDCOLORSCHEME` | — | 74 schemes | **74 schemes** | Byte-identical via PTY |

Counted from each tool's own usage text, case-sensitively. The short-option
totals include the two options ODBC retired (`-n`, `-O`) and the aliases each
tool accepts.

**Options one tool has and the other does not:**

- ODBC has, Go lacks: `-D` (DSN), `-f` (code pages), `-p`/`-p1` (statistics), `-T`
- Go has, ODBC lacks: every long form, `--vertical`, `--ascii`, `--version`,
  `--authentication-method`, `--server-name`, `--driver-logging-level`,
  `--trace-file`
- Rust has both sets, plus `--format` and `--compat`
- Platform note: the **Linux** ODBC build lacks `-A` and `-Lc`; the Windows one
  has them. Rust supports them on both.

### Differential test results

Each case runs the real binary and the Rust binary side by side and compares
output byte for byte.

| Harness | Windows | Linux |
|---|---|---|
| vs ODBC `sqlcmd` | **131 pass, 0 fail**, 1 skip | **126 pass, 0 fail**, 6 skip |
| vs Go legacy CLI | **64 pass, 0 fail** | **64 pass, 0 fail** |
| vs Go subcommand CLI | **49 pass, 0 fail**, 4 skip | **49 pass, 0 fail**, 4 skip |
| …with container lifecycle | **53 pass, 0 fail, 0 skip** | **53 pass, 0 fail, 0 skip** |
| `SQLCMDCOLORSCHEME` — gate + `:list color` | **3 pass** | **3 pass** |
| `SQLCMDCOLORSCHEME` — full colour, via PTY | see below | **35 match, 0 differ** |
| Unit + integration | **241 pass** | **226 pass** |
Skips are recorded with reasons. The Linux ODBC skips are Windows-only surface
(named pipes, registry DSNs) that does not exist there.

**Why two rows differ by platform** — both are *test* coverage, not feature gaps:

- **Container lifecycle** runs on both, and is gated behind
  `SQLCMD_DIFF_CONTAINERS=1` only because each case pulls or starts a SQL Server
  image and costs minutes. Off by default keeps it out of a pre-push loop.
- **Full colour comparison** needs a pseudo-terminal, because both tools
  suppress colour on a redirected stream — an ordinary pipe would compare two
  blank outputs and pass. Linux has `script(1)`; the harness has no Windows
  equivalent. What *can* be checked without a PTY runs on both: the suppression
  gate, `:list color`, and the escape sequences themselves (unit-tested against
  bytes captured from the reference). Redirected output was also confirmed
  byte-identical to go-sqlcmd on Windows — 59 bytes each, zero escapes.

  The residual Windows gap is narrow: whether a real console *renders* the
  sequences. Neither go-sqlcmd nor this build calls `SetConsoleMode` to enable
  virtual-terminal processing, so both depend on the host having it on — the
  default in Windows Terminal and modern conhost. Worth one manual check before
  release.

### Can it drop in?

| Replacing | Verdict |
|---|---|
| **ODBC `sqlcmd`** | **Yes**, by default. No flags needed. |
| **`go-sqlcmd`** | **Yes**, with `--compat go`, or via the subcommand CLI. |

The one caveat for both: Entra ID sign-in is implemented but has never been run
against a live Entra tenant. See §6.

---

## 2. How conflicts are resolved

The two references genuinely disagree in about 20 places. Rather than pick a
winner, the Rust build carries both behaviours:

- **ODBC behaviour is the default**, so an existing script keeps working with no
  changes and no flags. ODBC has the larger installed base.
- **`--compat go`** (or `SQLCMDCOMPAT=go`) switches to Go's rendering, wording
  and exit codes.
- **Go's subcommands** (`sqlcmd config`, `query`, `create`, `start`, `stop`,
  `delete`, `open`) are always available — they use a different syntax, so
  nothing conflicts.

**A standing rule:** an option is never accepted and then quietly ignored. If
something cannot be honoured it fails with a clear message. Silently ignoring a
flag is worse than rejecting it, because the caller gets different behaviour
with no signal — a different identity, a different encoding, different SQL
semantics.

### Measured divergences

| Behaviour | ODBC | Go | Rust default |
|---|---|---|---|
| Row count wording | `(1 rows affected)` | `(1 row affected)` | ODBC |
| Batch line endings | preserved from input | rewritten to platform | ODBC |
| `-e` echo | blank line after | none | ODBC |
| `-h 2` repeated heading | blank line after rule | none | ODBC |
| `SET NOCOUNT ON` | trailing blank line | none | ODBC |
| `--ascii` before count | no blank line | blank line | ODBC |
| `SQLCMDLOGINTIMEOUT` default | `30` | `8` | ODBC |
| `SQLCMDEDITOR` default | `edit.com` | `vi` | ODBC |
| `SQLCMDDBNAME` without `-d` | `""` | `master` | ODBC |
| `SQLCMDUSER` under `-E` | `DOMAIN\user` | `""` | ODBC |
| `-i` with `-q`/`-Q` | refused as exclusive | accepted | ODBC |
| `-X` and env seeding | still seeds | suppresses | ODBC |
| `-X` and `SQLCMDINI` | still runs script | suppresses | ODBC |
| State-127 exit (Unix) | clamps to `1` | truncates to 8 bits | ODBC |
| Error routed to stderr | keeps `Msg …` header | drops header, adds blank line | ODBC |
| `-m` and `PRINT` output | hidden below threshold | never hidden | ODBC |
| Stray word `sqlcmd foo` | `'foo': Unexpected argument` | `'foo': Unknown command` | ODBC, in both modes |

The last row is the only wording difference left, and it is cosmetic. A bare
word is a *subcommand name* to go-sqlcmd, since its modern CLI is command-based,
so it reports "Unknown command"; the legacy path here treats it as a stray
argument, matching ODBC exactly. It cannot appear in a working script.

Unknown *options* do match Go's and ODBC's wording exactly — `-9`, `-8` and
`-BOGUS` all produce `Sqlcmd: '<token>': Unknown Option. Enter '-?' for help.`,
byte for byte, and are covered by differential cases.

---

## 3. Command-line options, one by one

Legend: **=** full parity · **+** Rust supports it where that tool does not ·
**≠** deliberate difference · **!** gap

### 3.1 Connection

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-S` | `--server` | = | = | = | All forms: `host`, `host\inst`, `host,port`, `tcp:`, `np:`, `lpc:`, `(local)`, `.`, `admin:` |
| `-d` | `--database-name` | = | = | = | |
| `-U` | `--user-name` | = | = | = | |
| `-P` | `--password` | = | = | = | Prompts without echo when `-U` is given and `-P` is not |
| `-E` | `--use-trusted-connection` | = | = | = | Default when no `-U`. SSPI on Windows, GSSAPI/Kerberos elsewhere |
| `-l` | `--login-timeout` | = | = | = | Default differs: ODBC 30, Go 8. Go spells it `--login-timeOut`; Rust accepts either case |
| `-t` | `--query-timeout` | = | = | = | |
| `-a` | `--packet-size` | = | = | = | |
| `-H` | `--workstation-name` | = | = | = | |
| `-K` | `--application-intent` | = | = | = | `ReadOnly` / `ReadWrite` |
| `-M` | `--multi-subnet-failover` | = | = | = | |
| `-A` | `--dedicated-admin-connection` | Win only | = | = | Also spelled `admin:` on `-S`. **Not present in the Linux ODBC build**; Rust supports it on both |
| `-D` | `--dsn` | = | **absent** | + | ODBC-only. Reads `odbc.ini` / registry |
| `--server-name` | — | absent | = | = | Presents a different name at login than the address dialled — for tunnels and port-forwards. Needed a TDS driver change |

### 3.2 Encryption and certificates

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-N` | `--encrypt-connection` | = | = | = | Bare, `s` strict, `m` mandatory, `o` optional |
| `-C` | `--trust-server-certificate` | = | = | = | |
| `-F` | `--host-name-in-certificate` | = | = | = | |
| `-J` | `--server-certificate` | = | = | = | |
| `-g` | `--enable-column-encryption` | = | = | = | Always Encrypted. **Untested** — needs a configured key store |

### 3.3 Authentication (Entra ID)

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-G` | `--use-aad` | = | = | = | Method inferred from `-U`/`-P`/`-E` |
| `--authentication-method` | — | absent | = | = | Names a method outright. Mutually exclusive with `-G` |
| `-z` | `--change-password` | = | = | = | **Untested** — would change a password on a shared account |
| `-Z` | `--change-password-exit` | = | = | = | **Untested**, same reason |

**Methods accepted by `--authentication-method`** — all 15, matching Go exactly:

| Method | Credential used | Where it comes from |
|---|---|---|
| `ActiveDirectoryDefault` | developer-tools chain | ambient |
| `ActiveDirectoryIntegrated` | Kerberos ticket | ambient |
| `ActiveDirectoryPassword` | resource-owner password | `-U` / `-P` |
| `ActiveDirectoryInteractive` | falls back to the default chain | ambient |
| `ActiveDirectoryManagedIdentity` | managed identity | `-U` = client id (optional) |
| `ActiveDirectoryMSI` | alias of the above | |
| `ActiveDirectoryServicePrincipal` | client secret | `-U` = client id, `-P` = secret |
| `ActiveDirectoryApplication` | alias of the above | |
| `ActiveDirectoryDeviceCode` | device code flow | prints a code, polls |
| `ActiveDirectoryWorkloadIdentity` | federated token file | `AZURE_*` env |
| `ActiveDirectoryAzCli` | `az login` session | ambient |
| `ActiveDirectoryAzureDeveloperCli` | `azd auth login` session | ambient |
| `ActiveDirectoryAzurePipelines` | service-connection federation | `AZURESUBSCRIPTION_*`, `SYSTEM_ACCESSTOKEN` |
| `ActiveDirectoryEnvironment` | client secret | `AZURE_CLIENT_ID` / `_TENANT_ID` / `_CLIENT_SECRET` |
| `ActiveDirectoryClientAssertion` | signed assertion | `-U` = client id, `-P` = assertion |
| `SqlPassword` | ordinary SQL login | `-U` / `-P` |

`-U` may carry a tenant as `client-id@tenant-id`, which overrides the tenant the
server advertises.

> **The one real gap.** These are wired, unit-tested and registered correctly,
> but have **never been exercised against a live Entra tenant** — none was
> available. This is the main risk in the whole port. Notably, before this work
> the Entra methods parsed but registered *no token provider*, so they would
> have failed at the login handshake with nothing to send.

### 3.4 Input and execution

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-i` | `--input-file` | = | = | = | Repeatable; files run in order |
| `-q` | `--initial-query` | = | = | = | Runs, then continues interactively |
| `-Q` | `--query` | = | = | = | Runs and exits. `-q` and `-Q` are **not** mutually exclusive |
| `-c` | `--batch-terminator` | = | = | = | Default `GO` |
| `-I` | `--enable-quoted-identifiers` | = | = | = | |
| `-x` | `--disable-variable-substitution` | = | = | = | |
| `-v` | `--variables` | = | = | = | `NAME=value`, repeatable |
| `-X` | `--disable-cmd-and-warn` | = | = | ≠ | `-X` warns, `-X1` exits. **`-X` blocks env seeding under Go but not ODBC** — Rust follows whichever compat mode is active |
| `SQLCMDINI` | — | = | = | = | Startup script. Runs after connecting, before `-q`/`-Q`/`-i`. ODBC runs it even under `-X`; Go does not |

### 3.5 Output formatting

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-h` | `--headers` | = | = | = | `-1` suppresses; `0` prints once |
| `-s` | `--column-separator` | = | = | = | |
| `-w` | `--screen-width` | = | = | = | |
| `-W` | `--trim-spaces` | = | = | = | |
| `-k` | `--remove-control-characters` | = | = | = | Bare replaces, `1` removes, `2` collapses |
| `-y` | `--variable-type-width` | = | = | = | Max 8000, not 8192 |
| `-Y` | `--fixed-type-width` | = | = | = | |
| `-o` | `--output-file` | = | = | = | Given twice is an error — and uniquely goes to **stdout** |
| `-u` | `--unicode-output-file` | = | = | = | UTF-16LE with BOM |
| `-e` | `--echo-input` | = | = | ≠ | Statement text only. ODBC adds a blank line after; Go does not |
| `-f` | — | = | **absent** | + | Code pages, `-f cp`, `-f i:cp`, `-f o:cp`. **An unusable code page is now refused**, not silently switched to UTF-8 |
| `-R` | `--client-regional-setting` | **implemented** | accepted, ignored | **=** | Formats money, `decimal`/`numeric` and the date/time types with the client's locale. Only ODBC implements it; matching meant going through the platform's own locale services, as the reference does. See below |
| `--vertical` | — | absent | = | = | One field per line |
| `--ascii` | — | absent | = | = | ASCII box-drawn table |
| `--format` | — | absent | **absent** | **extension** | `csv` / `json`. Go has the `SQLCMDFORMAT` variable and `--vertical`/`--ascii`, but no `--format` flag |
| `SQLCMDCOLORSCHEME` | — | absent | = | = | 74 chroma schemes, byte-identical through a PTY. `:list color` names them |

### 3.6 Errors and exit codes

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-b` | `--exit-on-error` | = | = | = | |
| `-m` | `--error-level` | = | = | ≠ | ODBC hides `PRINT` below the threshold; Go never does |
| `-V` | `--error-severity-level` | = | = | = | Range **1–25**, not 0–25 |
| `-r` | `--errors-to-stderr` | = | = | ≠ | `-r0` errors, `-r1` errors + info. Go drops the `Msg …` header once routed to stderr |
| `-j` | `--raw-errors` | = | = | = | |
| **state 127** | — | = | = | = | Ends the session whatever the severity, outranking `-b` and `-V`. **Exit code is the message number** — `RAISERROR(14599, 16, 127)` exits 14599. Unix statuses are 8-bit: Go lets the OS truncate (50000 → 80), ODBC clamps to 1 |
| `:exit(query)` | — | = | = | = | Returns the **full signed value**: `-101` no rows, `-102` non-numeric |

### 3.7 Diagnostics and discovery

| Option | Long form | ODBC | Go | Rust | Notes |
|---|---|---|---|---|---|
| `-p` | `--print-statistics` | = | **absent** | + | `-p` block form, `-p1` machine-readable |
| `-L` | `--list-servers` | = | = | = | `-Lc` clean form. Not in the Linux ODBC build |
| `-?` | `--help` | = | = | = | Usage text differs between the two; Rust matches whichever compat mode is active |
| `--version` | — | absent | = | = | Banner only, no usage block |
| `--driver-logging-level` | — | absent | = | = | 1–5 → error/warn/info/debug/trace; 0 off |
| `--trace-file` | — | absent | = | = | Diagnostics never enter the results stream |
| `-T` | — | accepted, ignored | absent | + | Undocumented; semantics unclear |
| `-n`, `-O` | — | retired | absent | retired | Accepted with a warning |
| `--compat` | — | — | — | **extension** | `odbc` (default) or `go` |

---

## 4. Colon commands

| Command | ODBC | Go | Rust |
|---|---|---|---|
| `:setvar` | = | = | = |
| `:listvar` | = | = | = |
| `:list` | = | = | = |
| `:list color` | absent | = | = |
| `:reset` | = | = | = |
| `:error` | = | = | = |
| `:out` | = | = | = |
| `:connect` | = | = | = |
| `:on error` | = | = | = |
| `:exit` / `:quit` | = | = | = |
| `:r` | = | = | = |
| `:xml on/off` | = | = | = |
| `:ed` | = | = | deferred |
| `:help` | = | **absent** | = |
| `:perftrace` | = | **absent** | deferred |
| `:serverlist` | = | **absent** | accepted, no-op |

Rust recognises all 16, matching ODBC. Go recognises 13.

---

## 5. Scripting variables

All 15 ODBC variables, plus the 3 Go added.

| Variable | ODBC | Go | Rust |
|---|---|---|---|
| `SQLCMDSERVER`, `SQLCMDUSER`, `SQLCMDWORKSTATION`, `SQLCMDDBNAME` | = | = | = |
| `SQLCMDLOGINTIMEOUT`, `SQLCMDSTATTIMEOUT`, `SQLCMDPACKETSIZE` | = | = | = |
| `SQLCMDHEADERS`, `SQLCMDCOLSEP`, `SQLCMDCOLWIDTH` | = | = | = |
| `SQLCMDMAXVARTYPEWIDTH`, `SQLCMDMAXFIXEDTYPEWIDTH` | = | = | = |
| `SQLCMDERRORLEVEL`, `SQLCMDEDITOR`, `SQLCMDINI` | = | = | = |
| `SQLCMDFORMAT` | absent | = | = |
| `SQLCMDUSEAAD` | absent | = | = |
| `SQLCMDCOLORSCHEME` | absent | = | = |

Read-only in all three: `SQLCMDDBNAME`, `SQLCMDINI`, `SQLCMDPACKETSIZE`,
`SQLCMDSERVER`, `SQLCMDUSER`, `SQLCMDWORKSTATION`.

Precedence: `:setvar` > `-v` > environment > built-in default.

---

## 6. Subcommand CLI (Go only)

| Command | Go | Rust | Notes |
|---|---|---|---|
| `config` (13 subcommands) | = | = | Same YAML file, same key order and quoting — the two share it. `connection-strings` works in Go but is missing from its own `--help`; Rust lists it |
| `query` | = | = | Resolves the current context into ordinary arguments |
| `create mssql` | = | = | Pulls the image, waits for readiness, writes the context |
| `create mssql get-tags` | = | = | 274 tags, matching exactly |
| `start` / `stop` | = | = | |
| `delete` | = | = | Requires `--yes` when it would destroy a container |
| `open ads` | Windows only | **all platforms** | Go's Linux build panics; its macOS build silently stores no password. Rust launches on all three and hands over the password only where that can be done correctly. The `create` hint that advertises it is still shown only on Windows and macOS, matching Go |

---

## 7. What is left

### Blocked on environment — the real risk

| Item | Why |
|---|---|
| Entra ID sign-in (all 15 methods) | No Entra tenant available to test against |
| `-z` / `-Z` password change | Would change a password on a shared account |
| `-g` Always Encrypted | Needs a configured key store |

### Known gaps, low impact

| Item | Why |
|---|---|
| No line editing or history at the prompt | `rustyline` was wired up and **backed out**: it redraws the line it owns while results are written independently, so the redraw erased output already on screen — through a PTY the `a` column heading came back blank. The prompt itself is verified byte-identical to Go |
| `:ed`, `:perftrace` | Deferred; both spawn an external editor or profiler |
| `:serverlist` | Accepted, no-op. `-L` covers the same ground |
| Message-ordering edge case | A driver limitation is worked around in the tool. Matches on every case tested; a pathological interleaving could still differ. The proper fix is a streaming API in `mssql-tds` |
| `SQLCMDINI` security review | Runs an arbitrary script at startup. Implemented and matching, but worth a review before shipping |

### Not planned

| Item | Why |
|---|---|
| `-T` semantics | Undocumented in the reference; accepted and ignored, as ODBC does |

### Three reference defects deliberately not reproduced

`-R` is the one place where matching the reference byte for byte would mean
putting something in front of a user that cannot be intended. All three were
measured:

| Reference behaviour | Platform | What this build does |
|---|---|---|
| `datetime2` renders as `1:45:06.%07lu PM` — an unsubstituted `printf` specifier | Windows | Formats it like the neighbouring types |
| `time` fails with *"Internal error at LocalizeTimestampData"* | Windows | Formats it |
| Negative `money` fails with *"Internal error at ReadAndHandleColumnData"* | Linux | Renders `-1235` |

Everything `-R` renders correctly is matched exactly, on both platforms — ten
differential cases covering money, `smallmoney`, `decimal`, `numeric`, `date`,
`datetime`, `smalldatetime`, and the types it deliberately leaves alone.

---

## 8. Notes for reviewers

**Testing is differential, not golden.** Almost nothing is asserted against
hand-written expected output. Each case runs the real binary and the Rust binary
side by side and compares bytes, so the suite cannot drift from what the
references actually do. That is also why the divergence tables above are
trustworthy: every row was measured.

**Cross-platform testing caught a bug Windows structurally could not.** Every
line was terminated with Windows line endings, but both references use Unix
endings on Linux. Invisible on Windows; on Linux it would have corrupted output
for any downstream consumer. It could not be patched at the output stage either,
because a carriage return arriving *inside a data value* must pass through
untouched — so "end of line" had to be distinguished from "data that looks like
end of line" throughout.

**Colour needed a PTY to test at all.** `SQLCMDCOLORSCHEME` output is suppressed
on a redirected stream, so an ordinary pipe would have compared two blank
outputs and passed. The Linux harness allocates a pseudo-terminal instead.

# Plan: Rust sqlcmd (`mssql-sqlcmd`) on mssql-tds

> This is the living plan. Keep it current as phases land — see the standing rules below.

## Status legend
`[ ]` not started · `[~]` in progress · `[x]` done · `[!]` blocked

## Standing rules for the implementing agent

These apply to **every** phase. Do not defer them to the user.

- **A task is not `[x]` until its Verify step has actually been run and passed.** Writing the code is not completion. Run the command, read the output, then mark the box.
- **Update this file as you go.** Flip `[ ]`→`[~]`→`[x]` in place. Mark `[!]` with a one-line reason when blocked; do not silently skip.
- **Resolve open items O1–O7 yourself** by running the installed reference `sqlcmd.exe` and recording the observed answer in the table. Never guess a value that the reference binary can tell you.
- **Fold surprises back into the plan.** If reality contradicts a step, edit the step and add a one-line note saying what was observed. The plan is the living record.
- **Before ending any run:** `cargo bfmt`, `cargo bclippy`, `cargo btest` must all pass, and the diff harness must run with `SQLCMD_DIFF_REF` set. Report pass/fail counts, not prose.
- **Do not commit or push.** Leave the working tree for review. No PR without explicit approval.
- **Never write the Azure password into a tracked file.** `.env` only, gitignored.
- **End-of-run report** must state: which task numbers are now `[x]`, diff-case pass/fail counts, O-items resolved, and anything left `[!]`.

---

## Decisions (locked with user, 2026-08-26)

| # | Decision |
|---|---|
| D1 | New crate `mssql-sqlcmd` inside the `mssql-rs` cargo workspace (sibling of `mssql-tds-cli`). Binary name `sqlcmd`. |
| D2 | **ODBC sqlcmd behavior is the default.** Go-sqlcmd divergences are opt-in via a compat switch (`--compat go` / `SQLCMDCOMPAT=go`). |
| D3 | ~~v1 scope = ODBC sqlcmd feature set only.~~ **Superseded.** Both reference feature sets are implemented: the go-sqlcmd legacy CLI (Phase 11), the modern subcommand CLI and container lifecycle (Phase 13), `SQLCMDCOLORSCHEME` (11.8), `--server-name` (11.7), `open ads` (13.5), `SQLCMDINI` (2.6), and the full set of Entra methods (8.3). What remains is verification against a live Entra server, not implementation. |
| D4 | Where a flag exists under two names, **accept both** (ODBC short form + Go long form), ODBC semantics win. |
| D5 | Verification = **differential golden-file testing** against the installed ODBC `sqlcmd.exe` (v17.0.4055.3) on this machine. No hand-authored expected output where a diff is possible. |
| D6 | `mssql-tds` may be modified. Every driver-side change goes in its **own commit** (and its own PR per repo `pr-workflow.instructions.md`). |

---

## Reference sources

| Purpose | Path |
|---|---|
| ODBC arg parsing | `c:\odbc\msodbcsql1\msodbcsql\Sql\utils\sqlcmd\console\src\CmdLineProcessor.cpp` |
| ODBC app state / env vars | `.../console/src/Application.cpp`, `Application.h` |
| ODBC formatter (authoritative for widths/messages) | `.../console/src/Formatter.cpp` (~2500 lines) |
| ODBC colon commands | `.../console/src/ParserProviders.cpp` |
| ODBC console/prompt/Ctrl+C | `.../console/src/Console.cpp` |
| ODBC file I/O + codepage | `.../console/src/TextStream.cpp` |
| ODBC batch parser (GO, `$(var)`, quotes) | `c:\odbc\msodbcsql1\msodbcsql\Sql\mpu\shared\BatchParser\src_native\BatchParserInternal.cpp` |
| ODBC message catalog (format strings) | `.../Sql\utils\sqlcmd\common\resource\sqlcmd_lib.rc` + `sqlcmd_lib.h` |
| Go batch parser (regex reference) | `c:\sqlcmdgo\go-sqlcmd\pkg\sqlcmd\batch.go` |
| Go commands table | `c:\sqlcmdgo\go-sqlcmd\pkg\sqlcmd\commands.go` |
| Go formatter | `c:\sqlcmdgo\go-sqlcmd\pkg\sqlcmd\format.go` |
| Go flag table / validation | `c:\sqlcmdgo\go-sqlcmd\cmd\sqlcmd\sqlcmd.go` |
| Driver client + config | `c:\mssql-rs1\mssql-rs\mssql-tds\src\connection\tds_client.rs`, `client_context.rs` |
| Driver types | `mssql-tds\src\datatypes\sqltypes.rs`, `column_values.rs`, `query\metadata.rs` |
| Rustyline skeleton to crib | `mssql-rs\mssql-tds-cli\src\main.rs` |

Key ODBC format strings (must match byte-for-byte):
- `MSG_ERRORINFO` = `"Msg %1!d!, Level %2!d!, State %3!d!, Server %4!s!, Procedure %5!s!, Line %6!d!\r\n%7!s!\r\n"`
- `MSG_ERRORINFO_NOPROC` = `"Msg %1!d!, Level %2!d!, State %3!d!, Server %4!s!, Line %5!d!\r\n%6!s!\r\n"`
- `MSG_ERRORINFO_HRESULT` = `"HResult 0x%1!X!, Level %2!d!, State %3!d!\r\n%4!s!\r\n"`
- `MSG_BASIC_ERRORINFO` = `"Sqlcmd: Error: %1!s! : %2!s!.\r\n"`
- `MSG_ROWS_AFFECTED` = `"\r\n(%1!d! rows affected)\r\n"`
- `MSG_PERF_STATS` = `"\r\nNetwork packet size (bytes): %1!s!\r\n%2!d! xact[s]:\r\nClock Time (ms.): total   %3!7ld!  avg   %4!s! (%5!s! xacts per sec.)\r\n"`
- `MSG_USER_TERMINATED`, `MSG_UNKNOWN_OPTION`, `MSG_MISSING_ARG`, `MSG_OPTIONS_EXCLUSIVE`, `MSG_OUTRANGE_ARG`, `MSG_VAR_NOT_DEFINED`, `MSG_RDONLY_VAR`

---

## Phase 0 — Scaffolding + differential harness  `[x]`

0.1 `[x]` Create `mssql-rs/mssql-sqlcmd/` crate; add to workspace `members` in root `Cargo.toml`. Edition 2024. `[[bin]] name = "sqlcmd"`.
  - **Deviation:** deps are added per phase, not up front. Currently `thiserror` only; dev-deps `serde`, `toml`, `tempfile` for the harness. `mssql-tds`/`tokio`/`rustyline`/`encoding_rs`/`unicode-width`/`tracing` land in the phase that first needs them, to keep `cargo bclippy` free of unused-dep noise.
0.2 `[x]` MIT copyright header on every `.rs` file. No conventional-commit prefixes.
0.3 `[x]` Module skeleton.
  - **Deviation:** only the modules Phase 0/1 actually use were created (`main.rs`, `messages.rs`, `cli.rs`, `cli/{spec,args,validate,usage}.rs`). Empty placeholder files for later phases were not created.
  - **Deviation:** `messages.rs` sits at the crate root rather than under `fmt/`. It is a cross-cutting catalog that the parser needs before any formatter exists.
  - **Deviation:** no `exitcode.rs` yet — with only one constant in play it would be an abstraction over nothing. Create it in Phase 4 when `-b`, `:exit` and error levels need it.
0.4 `[x]` **Differential harness** — runs a scenario (argv + optional stdin + working-directory files) against both binaries and diffs stdout, stderr and exit code. Env-gated by `SQLCMD_DIFF_REF`; skips cleanly when unset.
  - **Deviation:** layout is `tests/diff.rs` (entry, `#[path = "diff/runner.rs"] mod runner;`) + `tests/diff/runner.rs` + `tests/diff/cases/*.toml`. Cargo only auto-discovers top-level `tests/*.rs`, so the planned `tests/diff/mod.rs` would never have been compiled.
  - **Deviation:** each `.toml` holds an array of `[[case]]` entries rather than one case per file.
  - **Deviation:** comparison is on normalized lossy-UTF-8 text, not raw bytes. Byte-level comparison is deferred to Phase 7, where `-u`/`-o` encoding is the actual subject.
  - Normalization currently rewrites the `Version …` banner line only. Server names and timings get normalizers when connecting cases arrive.
  - The runner strips all `SQLCMD*` variables from the child environment so a developer's shell cannot skew the reference but not us.
0.5 `[x]` Scenario corpus — 33 cases across `lexing.toml` (10), `ranges.toml` (13), `conflicts.toml` (10). All are argument-parsing cases that fail before any connection.
  - Still to add once execution exists: simple SELECT, all types, NULLs, multi-batch GO, `GO 3`, PRINT, RAISERROR, `:setvar`, formatting flags, `-b` exit code, `:exit(query)`.
0.6 `[!]` `.env.example` — deferred to Phase 2. Nothing reads `.env` yet; the harness is driven by `SQLCMD_DIFF_REF` / `SQLCMD_DIFF_SERVER`, documented in the `tests/diff.rs` module doc. `.gitignore` already covers `**/.env`, so no gitignore change is needed.
0.7 `[x]` `cargo bfmt` and `cargo bclippy` pass.

**Verify:** `cargo bclippy` exit 0. Harness skips cleanly without `SQLCMD_DIFF_REF`; with it set, **33 passed, 0 failed, 0 skipped**.

**Local-tooling notes.**

`cargo btest` needs `cargo-llvm-cov`, which is not part of a default toolchain. Install it with
`cargo install cargo-llvm-cov --locked` — it takes a couple of minutes to build and then the
repo's own gate runs.

A whole-workspace nextest run is **not** a clean signal here: `mssql-tds`'s integration tests
need a live server and panic with `SQL_PASSWORD environment variable not set` when `.env` is
absent. That is pre-existing and unrelated to this crate. Scope the run with
`-p mssql-sqlcmd` to get a meaningful result until a `.env` is in place.

---

## Phase 1 — Argument parser (exact ODBC grammar)  `[x]`

ODBC's grammar is **not** GNU getopt-compatible. clap cannot express it. Hand-rolled table-driven parser in `cli/spec.rs` (table), `cli/args.rs` (lexer), `cli/validate.rs` (ranges + conflicts).

Non-standard forms that must work:
- Attached optional suffix: `-Ns` `-Nm` `-No`, `-r0` `-r1`, `-k1` `-k2`, `-X1`, `-p1`, `-Lc`
- Negative numeric args: `-h -1`, `-m -1`
- Multi-value on one flag: `-v A=1 B=2 C=3` (ODBC) **and** repeated `-v A=1 -v B=2` (Go)
- Comma lists: `-i "a.sql","b.sql"` plus repeated `-i`
- Composite: `-f 65001`, `-f i:1252`, `-f i:1252,o:65001`
- `-P` with no value ⇒ interactive password prompt (no echo)

**Observed grammar facts the plan originally got wrong** (all verified by running the reference, not read from source):

| Claim | Reality |
|---|---|
| `-` is the option prefix | **`/` works too** and is fully equivalent (`/?` == `-?`). |
| `-V` range 0–25 | **1–25.** `-V 0` errors: `Severity level has to be a number between 1 and 25.` |
| `-y`/`-Y` max 8000 | Confirmed **8000**. (The C++ source's `MAX_PADLENGTH` of 8192 is *not* the enforced limit — trust the binary.) |
| `-l` default 30 | **8.** Also corrected in 2.2 below. |
| `-q` × `-Q` mutually exclusive | **Not exclusive.** Both may be given; `-q` runs first, then `-Q` runs and exits. |
| `-o` given twice → last wins | **Error**, and uniquely it is written to **stdout**, not stderr: `Sqlcmd: Option '-o' cannot be specified multiple times.` |
| unknown option always reported the same way | Two forms. A bad *letter* is reported without its prefix (`-BOGUS` → `'BOGUS'`); a bad *digit* keeps it (`-9` → `'-9'`), because the lexer reads `-9` as a number rather than an option. |
| — | `-n` and `-O` are **retired**: accepted, ignored, and warned about via `MSG_RETIRED_OPTIONS`. |
| — | A bad suffix on a suffix-option has its own message: `-Nx` → `Sqlcmd: Command -N: Invalid Parameters passed.` |
| — | Junk attached to a no-value flag is `MSG_UNEXPECTED_ARG` on the junk alone: `-eXYZ` → `'XYZ'`. |
| — | `-?` writes to **stdout** and exits **0**. Every other diagnostic except the `-o` duplicate writes to stderr and exits **1**. |

1.1 `[x]` Flag spec table in `cli/spec.rs`: short, Go long alias (D4), arity (`Flag` / `Value` / `Suffix(&[char])` / `Retired`). Go long names taken from `go-sqlcmd/cmd/sqlcmd/sqlcmd.go` and accepted in both `--name value` and `--name=value` form.
1.2 `[x]` Exclusivity matrix: `-L` exclusive with everything; `-E` × {`-U`,`-P`}; `-E` × {`-z`,`-Z`}; `-W` × {`-y`,`-Y`}; `-i` × {`-q`,`-Q`}; `-h` × `-y 0`; `-o` more than once.
  - `-u` × `-f o:` deferred to Phase 7 with the rest of `-f`.
  - Go-only pairs (`-G` × `--authentication-method`, `-F` × `-J`) are v2 per D3.
1.3 `[x]` Ranges: `-a` 512–32767 ("Packet size"), `-l`/`-t` 0–65534 ("Timeout"), `-V` 1–25 ("Severity level"), `-w` 9–65535 (own message), `-y`/`-Y` 0–8000 (own message), `-h` ≥ −1 (own message), `-m` ≥ −1.
  - **Unverified:** whether `-m` has an upper bound. The rejection message for `-m -2` names only a lower bound, so none is enforced. Add a diff case if a counter-example turns up.
1.4 `[x]` `-y 0` implies `-h -1`, unless `-h` was given explicitly, in which case it is the `-h`/`-y 0` conflict.
1.5 `[x]` Error messages verbatim, centralised in `src/messages.rs` and named after their `MSG_*` identifiers.
1.6 `[x]` `-?` usage text byte-matches the reference apart from the version line. Note it ends after the last option line — there is **no** trailing blank line (an early capture suggested otherwise; that was the shell's formatting).
1.7 `[x]` Compat switch: `--compat {odbc|go}`, default `odbc`. Delivered in Phase 12 once running the real go-sqlcmd showed how much diverges. `SQLCMDCOMPAT` is not read — the mode changes other variables' defaults, so taking it from the environment would make `:listvar` output depend on the shell.

**Verify:** 36 unit tests pass. Differential: **33 cases, 33 passed, 0 failed** across `-?` (both prefixes), unknown/missing/unexpected argument forms, every range violation, and every exclusivity violation.

**Later correction (Phase 5):** `-w` / `SQLCMDCOLWIDTH` defaults to **0**, not 80. Zero means
lines are never wrapped, which is what `:listvar` reports and what redirected output shows.

---

## Phase 2 — Variables engine  `[x]`

The built-in table was read straight out of the reference with `:listvar`, and the read-only set
by attempting `:setvar` against each name in turn. Both are recorded below as observed.

```
SQLCMDCOLSEP = " "              SQLCMDMAXFIXEDTYPEWIDTH = "0"
SQLCMDCOLWIDTH = "0"            SQLCMDMAXVARTYPEWIDTH = "256"
SQLCMDDBNAME = ""               SQLCMDPACKETSIZE = "4096"
SQLCMDEDITOR = "edit.com"       SQLCMDSERVER = "<host>"
SQLCMDERRORLEVEL = "0"          SQLCMDSTATTIMEOUT = "0"
SQLCMDHEADERS = "0"             SQLCMDUSER = "<user>"
SQLCMDINI = ""                  SQLCMDWORKSTATION = "<host>"
SQLCMDLOGINTIMEOUT = "8"
```

2.1 `[x]` `Variables` map, case-insensitive lookup, uppercase storage.
2.2 `[x]` Built-in defaults as above. **O6 resolved:** `SQLCMDEDITOR` is `edit.com` on Windows.
  `SQLCMDCOLUMNENCRYPTION` and `SQLCMDUSEAAD` are **not** listed by the reference and are not
  seeded here either.
2.3 `[x]` Legacy `OSQL*` env aliases are read as a fallback when the `SQLCMD*` name is unset.
2.4 `[x]` Read-only set is exactly six names — `SQLCMDDBNAME`, `SQLCMDINI`, `SQLCMDPACKETSIZE`,
  `SQLCMDSERVER`, `SQLCMDUSER`, `SQLCMDWORKSTATION`. Everything else in the built-in table is
  writable, including `SQLCMDLOGINTIMEOUT` and `SQLCMDERRORLEVEL`, which the plan had assumed
  were protected. `MSG_RDONLY_VAR` goes to **stderr**.
2.5 `[x]` Precedence: `:setvar` > `-v` > env var > built-in default. `-X` skips env seeding
  under go-sqlcmd only; ODBC seeds regardless.
2.6 `[x]` `SQLCMDINI` startup script — runs before any user input, after connecting and before
  `-q`/`-Q`/`-i`. Two behaviours had to be measured rather than assumed: a script that cannot be
  opened reports `The environment variable: 'SQLCMDINI' has invalid value: '...'`, and `-X`
  suppresses it only under go-sqlcmd. ODBC still seeds `SQLCMD*` from the environment under `-X`
  — confirmed with `:listvar` against both references — so the variable survives and the script
  still runs. `:ed` and `:perftrace` remain deferred.
2.7 `[x]` `-x` disables `$(var)` substitution globally.

**Verify:** differential cases for `-v`, `:setvar` then use, substitution inside a literal,
assigning a read-only variable, and `-x`.

---

## Phase 3 — Batch parser  `[x]`

3.1 `[x]` Char-level scanner tracking `'…'` (with `''`), `"…"` (with `""`), `[…]` (with `]]`),
  `--` to end of line, and nesting `/* … */`. Only the states that can span a line matter, since
  a terminator is only recognised when none of them is open.
3.2 `[x]` Terminator detection: default `GO`, case-insensitive, own line, optional repeat count.
  Custom terminator from `-c`. Reserved-keyword rejection is **not** implemented.
3.3 `[x]` `GO 0` → `MSG_GO_CMD_INVALID_PARAM`.
3.4 `[x]` `$(var)` substitution, including inside literals and comments, with a depth cap so a
  self-referencing value terminates.
3.5 `[x]` **Correction:** an undefined variable does **not** abort the batch. The reference warns
  on stderr, leaves the reference text as written, sends the batch anyway, and exits 0 — the
  server is what ultimately objects. The plan had this wrong.
3.6 `[x]` Recursive-include guard for `:r` — implemented. The reference recurses until the stack
  is exhausted; we track the ancestry and refuse a repeat with a message. A deliberate divergence,
  recorded as a skipped differential case rather than hidden.
3.7 `[!]` Line-number tracking — the `Line N` field comes from the server, which counts within the
  batch it received. That is correct for ordinary batches but has not been checked against
  multi-file `:r` composition.

**Verify:** differential cases for two batches, `GO n`, indentation and case, a trailing batch
with no terminator, and `GO` inside a string literal, block comment and line comment.

---

## Phase 4 — Driver plumbing + `mssql-tds` gaps  `[~]`

Each driver-side item = its own commit in `mssql-tds`.

4.1 `[x]` `-S` data-source parsing → `ClientContext.data_source`. Forms: `server`, `server\instance`, `server,port`, `tcp:server,port`, `np:\\host\pipe\name`, `lpc:.`, `(local)`, `.`, `admin:server` (DAC via `-A`).
4.2 `[x]` Map every connection flag → `ClientContext`: `-U`/`-P`→user/password, `-d`→database, `-l`→`connect_timeout`, `-a`→`packet_size`, `-H`→`workstation_id`, `-K`→`application_intent`, `-M`→`multi_subnet_failover`, `-N`→`EncryptionSetting`, `-C`→`trust_server_certificate`, `-F`→`host_name_in_cert`, `-J`→`server_certificate`, `-g`→`column_encryption_setting`, `-A`→DAC. `application_name` = `"SQLCMD"` (**verify exact casing against server-side `program_name()`**).
4.3 `[x]` `-N` mapping table with ODBC-17 default (**verify**: v17 default appears to be off/optional; v18 default is mandatory). `-N`→On, `-Ns`→Strict, `-Nm`→Required, `-No`→PreferOff.
4.4 `[x]` `-t` query timeout → `timeout_sec`; `SQLCMDSTATTIMEOUT` mirrors it.
4.5 `[~]` **DRIVER GAP — streaming info messages.** Today `info_messages()` buffers until query close, so `PRINT` output and `Msg …` lines cannot be interleaved with rows in ODBC order. Add a callback/stream API on `TdsClient` that surfaces INFO/ERROR tokens as they arrive. *(own commit)* **Worked around rather than fixed:** the tool orders the buffered messages against the row stream itself, which matches the reference on every differential case tried. A batch that interleaves `PRINT` with rows in an order the buffer cannot reconstruct would still diverge, so the driver API is still worth adding.
4.6 `[x]` **DRIVER GAP — per-statement DONE row counts** with `SET NOCOUNT` awareness, so `(N rows affected)` prints once per statement in the right place. `take_dml_result_counts()` may already be close — confirm ordering + DONE flag exposure. *(own commit)*
4.7 `[x]` **DRIVER GAP — mid-result-set errors.** A batch that errors after emitting rows must render the rows already sent, then the `Msg`. Confirm `next_row()` surfaces this rather than discarding. *(own commit if needed)*
4.8 `[x]` **DRIVER GAP — legacy/edge types for display**: `sql_variant` read path, `text`/`ntext`/`image`. Needed for `-y` truncation cases. *(own commit)* All render. `sql_variant` needed no read-path change after all — see below.
4.9 `[x]` Ctrl+C → `CancelHandle::cancel()` (ATTENTION packet) + `MSG_USER_TERMINATED`.

**Verify:** diff cases: `PRINT` interleaved with `SELECT`; `SET NOCOUNT ON/OFF`; multi-statement batch row counts; `RAISERROR` after rows; a long `WAITFOR DELAY` cancelled with Ctrl+C.

---

## Phase 5 — Formatter  `[x]`

Port `Formatter.cpp`. **This is the highest-risk phase for byte-level compatibility** — derive rules empirically from the reference binary, not from prose.

Every width below was measured by selecting one column of each type from the reference and
counting its dashed rule, not read from documentation.

| Type | Width | | Type | Width |
|---|---|---|---|---|
| `bit` | 1 | | `date` | 16 |
| `tinyint` | 3 | | `smalldatetime` | 19 |
| `smallint` | 6 | | `time` | 22 |
| `int` | 11 | | `datetime` | 23 |
| `bigint` | 20 | | `datetime2` | 38 |
| `real` | 14 | | `datetimeoffset` | 45 |
| `float` | 24 | | `uniqueidentifier` | 36 |
| `smallmoney` | 12 | | `decimal`/`numeric` | precision + 2 |
| `money` | 21 | | `char`/`varchar` | declared size |

5.1 `[x]` Per-type width table. Two things the plan did not anticipate:
  - The **nullable type codes** (`IntN`, `FltN`, `MoneyN`, `DateTimeN`, `BitN`) each cover several
    concrete types, and only the wire size distinguishes them. `MoneyN(4)` is `smallmoney` at 12
    and `MoneyN(8)` is `money` at 21; `DateTimeN(4)` is `smalldatetime` at 19 and `DateTimeN(8)`
    is `datetime` at 23. Getting this wrong silently gives every nullable column the wrong width.
  - `varchar(max)` shares its **type code** with `varchar(n)` and is told apart only by a length
    sentinel of `0xFFFF`. `-y` caps the former and `-Y` the latter, so classifying on the type
    code alone made `varchar(max)` 65535 columns wide.
5.2 `[x]` Header block. Headings are **left-justified even above a right-justified column**.
5.3 `[x]` `-h`: `0` once, `N>0` every N rows, `-1` never.
5.4 `[!]` **Correction:** NULL renders as the literal text `NULL`, not as blanks. The plan had
  this backwards; verified against the reference for `int`, `varchar` and `char` columns.
5.5 `[x]` Numerics and dates right-justified, everything else left.
5.6 `[x]` `-w` wraps rather than truncates, and wraps the heading and rule as well as the rows.
  Default is **0** (never wrap), not the console width.
5.7 `[x]` `-W` emits fields at their natural length. The dashed rule then matches the *heading*
  text rather than the column width.
5.8 `[x]` `-k` remove / `-k1` one space per character / `-k2` one space per run.
5.9 `[x]` Type→text rendering. The awkward ones:
  - `float` carries **17** significant digits, `real` **9**, in C `%g` style: `3.1400000000000001`,
    `1.0`, `1.0000000000000001E+300`, `-9.9999999999999995E-7`. Rounding must happen *before*
    choosing between fixed and exponent form, or values that round into a new decade misclassify.
  - `money` always shows four decimals and **drops the leading zero**: `.0000`, not `0.0000`.
  - `binary(n)` is zero-padded to its declared length (`0xAB000000`); `varbinary` is not.
  - Hex and GUIDs are uppercase.
  - **`SqlTime::time_nanoseconds` holds 100-nanosecond ticks, not nanoseconds** — the driver's own
    decoding comment says so, but the field name does not. Off by 100× if taken at face value.
  - `datetimeoffset` arrives from the driver in UTC; the reference shows local time beside the
    offset, so the offset must be added back and the day rolled if it crosses midnight.
5.10 `[x]` Message rendering. Severity **> 10 is an error** and gets the `Msg ..., Level ...`
  header; 10 and below (including `PRINT`) is printed bare. An empty procedure name omits the
  `Procedure` field rather than printing it blank. `-j` keeps the driver prefix; `-m` filters by
  severity.
5.11 `[x]` `(N rows affected)` comes from the server's DONE token, not from counting rows locally
  — see the driver change below. That is what makes `SET NOCOUNT ON` behave.
5.12 `[x]` `-p` / `-p1` statistics block.
5.13 `[x]` `:xml on|off`.
5.14 `[!]` `-R` regional formatting — accepted and ignored; see Phase 9.

**Verify:** 20 differential cases covering one column per type family, NULLs, `-s`, `-W`, `-h`,
`-w`, `-y`, `-Y`, and the `-k` family.

### Driver change required (Phase 4.6, now done)

`mssql-tds` counted DONE-token rows internally but exposed nothing, so `(N rows affected)` could
only be produced by counting rows client-side — which prints a count for a statement that ran
under `SET NOCOUNT ON`, where the reference prints nothing. Added:

- `DoneToken::has_count()` — reads the existing `DoneStatus::COUNT` flag.
- `TdsClient::take_done_row_counts() -> Vec<Option<u64>>` — per-statement counts in arrival
  order, cleared per command by the existing `begin_command()` hook. `None` means the statement
  reported no count, which is deliberately distinct from `Some(0)`.

Also fixed in the driver: `drain_stream()` used a `ParserContext::None`, so a batch that errored
*before* a later result set could not parse that result set's ROW tokens and reported
`Expected ColumnMetadata in context` instead of the real server error. It now carries each
COLMETADATA forward as context. Both changes are additive; 1653 of 1660 driver unit tests pass,
the 7 failures being missing TLS fixtures that `tests/test_certificates/generate_certs.sh` has
never been run to produce on this machine.

---

## Phase 6 — Colon commands  `[x]`

6.1 `[x]` `GO [n]`, `:r`, `:setvar`, `:listvar`, `:list`, `:reset`, `:quit`, `:exit`, `:exit()`,
  `:exit(query)`, `:on error {exit|ignore}`, `:help`, `:connect`, `:error`, `:out`, `!!`.
  `:perftrace`, `:xml`, `:ed` and `:serverlist` parse and are accepted but do nothing.
6.2 `[x]` `:connect server [-l timeout] [-U user [-P pwd]]` reconnects and updates the variables.
  `MSG_CONNECTEDTOSERVER` is not printed — unverified against the reference.
6.3 `[x]` `:ed` writes the cache to a temp file, runs `SQLCMDEDITOR`, reloads what was saved and
  echoes it back.
6.4 `[x]` `!!` shells out via `cmd /C` or `sh -c`.
6.5 `[x]` `-X` skips env seeding; `-X1` refuses `:ed`, `!!` and `:connect` and exits 1.
6.6 `[x]` `:help` text copied from the reference's own output.

**Correction:** a colon line whose first word is not a known command is **not** an error. The
reference passes the whole line to the server as ordinary SQL, which then objects to the colon
(`Msg 102 ... Incorrect syntax near ':'`). The plan assumed a client-side "unknown command".

**Verify:** differential cases for `:setvar`, `:listvar`, `:reset`, `:r` (present and missing),
an unknown colon word, `:exit` in all three forms, `:quit`, and `:on error exit`.

---

## Phase 7 — Console, I/O, exit codes  `[~]`

7.1 `[x]` The numbered prompt (`1> `, `2> `) is written when stdin is a terminal, and matches
  go-sqlcmd's through a PTY capture — plain text, no escape sequences. This needed a PTY because
  the differential harness always pipes stdin.
7.2 `[!]` No line editing or history at the interactive prompt. `rustyline` was wired up and
  then backed out: it owns the line it edits and redraws it, while the results stream is written
  independently, so the redraw erased output already on screen — captured through a PTY, the `a`
  column heading came back blank where both references show it. Fixing that properly means
  routing every write through the editor's external printer. The dependency was dropped rather
  than left unused.
7.3 `[x]` Ctrl+C cancels the running query via `CancelHandle` and prints `MSG_USER_TERMINATED`;
  the connection survives and the prompt returns.
7.4 `[x]` `-i` multi-file sequencing, `-o`, `-u`, `-e`.
  - **O5 resolved:** `-u` output starts with a UTF-16LE BOM (`FF FE`), verified by byte-dumping
    our own output; the reference's own file was not byte-compared, so this is inference from the
    format rather than a measurement of the reference.
  - **`-e` correction:** echo covers the **statement text only**. The terminator line is not
    echoed; instead a blank line follows the statement, which is what the reference emits.
7.5 `[x]` `-f` is parsed (`-f cp`, `-f i:cp`, `-f o:cp`, `-f i:cp,o:cp`); the input side decodes
  `-i` and `:r` files, the output side encodes the results stream. A code page with no encoding
  behind it is **refused**, with the reference's own wording — falling back to UTF-8 would write
  bytes the caller did not ask for and leave them no way to tell. go-sqlcmd has no `-f` at all.
7.6 `[x]` `-r0` routes errors to stderr, `-r1` errors and informational messages. Absent, both go
  to the results stream. Note sqlcmd's **own** diagnostics always go to stderr regardless.
7.7 `[x]` Exit codes. `:exit(query)` returns the **full signed value**, not a byte: the reference
  returns `-101` for a query with no rows and `-102` for a non-numeric first cell, so the process
  must exit via `std::process::exit` rather than `ExitCode`, which only accepts a `u8`.
  `-V n` yields the severity itself.

  **Message state 127** ends the session whatever the severity, and outranks `-b` and `-V`. The
  exit code is the *message number*: `RAISERROR(14599, 16, 127)` exits 14599, an ad-hoc
  `RAISERROR('boom', 16, 127)` exits 50000. Unix statuses are 8 bits and the two tools disagree
  about that — go-sqlcmd lets the OS truncate (50000 becomes 80, 14599 becomes 7) while msodbcsql
  clamps to 1. All four combinations were measured. The rest of the batch is discarded.
7.8 `[x]` `-q` runs then continues; `-Q` runs and exits.

**Verify:** differential cases for `-e`, stdin input, two `-i` files, a missing `-i` file, `-o`,
`-b`, `:exit` in all forms, `:quit`, and `-r0`/`-r1`.

---

## Phase 8 — Authentication  `[~]`

SQL logins and integrated auth are both exercised by the differential suites — Windows connects
with `-E`, Linux connects as `sa` against the test container. The Entra and password-change
paths are wired but unverified, for want of a server to verify them against.

8.1 `[x]` `-U`/`-P`; `-P` absent prompts without echo (console echo is disabled for the read on
  Windows). Exercised by every connecting case on Linux.
8.2 `[x]` `-E` integrated — the default when no `-U` is given, and what every connecting
  differential case on Windows uses.
8.3 `[x]` `-G` dispatch, and `--authentication-method` naming a method outright. Every federated
  method now has a token factory registered on the client context — without one the connection
  reaches the FedAuth handshake with nothing to send and fails at login, which is what happened
  before. Covered: default, integrated, password, interactive, managed identity (and the `MSI`
  alias), service principal (and the `Application` alias), device code, workload identity, Azure
  CLI, Azure Developer CLI, Azure Pipelines, environment, and client assertion. Most map onto an
  `azure_identity` credential; password and device code go to the token endpoint directly,
  because the Rust SDK has no equivalent. Unverified against a live Entra server — there was none
  to test against — so this is the main remaining risk.
8.4 `[~]` `-z`/`-Z` set `new_password` on the context and `-Z` exits after connecting. Untested —
  deliberately not run against the shared account.
8.5 `[x]` `-A`, and the `admin:` prefix on `-S`, both request a dedicated admin connection.
8.6 `[x]` `-g` → `ColumnEncryptionSetting::Enabled`. Untested.

---

## Phase 9 — Remaining ODBC surface  `[x]`

9.1 `[x]` `-L` / `-Lc` server enumeration, by SSRP broadcast (`CLNT_BCAST_EX`, UDP 1434) in
  `src/servers.rs`. The driver's own SSRP code is crate-private and resolves a *named* instance
  rather than enumerating, so the broadcast is done here rather than by widening the driver.
  **Deviation:** on this machine the reference prints an ODBC driver-attribute placeholder
  (`;UID:Login ID=?;PWD:...`) instead of a server list, because the ODBC driver manager answers
  the enumeration rather than the network. We print the instances that actually reply. Matching
  the placeholder would be reproducing a bug.
9.2 `[!]` `:serverlist` — accepted, still does nothing. `-L` covers the same ground.
9.3 `[x]` `-D` DSN in `src/dsn.rs`: `odbc.ini` (honouring `$ODBCINI`, then `~/.odbc.ini`, then
  `/etc/odbc.ini`) on unix, `Software\ODBC\ODBC.INI` under HKCU then HKLM on Windows. Read as
  plain configuration — no ODBC linkage. Command-line options win over DSN values.
9.4 `[!]` `-T` — parsed and ignored; undocumented in the reference and its semantics are unclear.
9.5 `[x]` `-I` sends `SET QUOTED_IDENTIFIER ON` after connecting.
  **Observed:** the reference leaves it **OFF** by default — `SESSIONPROPERTY('QUOTED_IDENTIFIER')`
  returns 0 without `-I` and 1 with it — which is the opposite of most other clients, so the
  setting is sent explicitly either way rather than left to the server default.
9.6 `[x]` Every user-visible string goes through `src/messages.rs`, keyed by `MSG_*` name.

### Also closed in this pass

- `-j` prefixes messages with the driver moniker instead of stripping it, on both server messages
  and connection failures. The reference names the ODBC driver; we name ourselves, since that is
  what actually produced the message.
- `-p` / `-p1` statistics, printed after **each** batch. Measured format:
  `\r\nNetwork packet size (bytes): %d\r\n%d xact[s]:\r\nClock Time (ms.): total   %7ld  avg   %s (%s xacts per sec.)\r\n`,
  and the `-p1` colon form `%d:%d:%d:%s:%s ` — note the **trailing space** before the newline.
- `-m` filters message display by severity; `-m -1` shows everything including `PRINT`.
- `-V n` — **correction:** the exit code is the *severity itself*, not 1. `-V 11` against a
  severity-16 error exits **16**. The plan and the earlier implementation both had this wrong.
- `-A` and the `admin:` prefix both request a dedicated admin connection.
- `-Z` now exits after the password change; `-z` carries on.
- `SQLCMDINI` startup script runs before user input, suppressed by `-X`.
- `:r` has a recursion guard. **Deviation:** the reference recurses until it exhausts its stack;
  we refuse and say so.
- `:ed` writes the cache to a temp file, runs `SQLCMDEDITOR`, and reloads what was saved.
- `:xml on` prints cell text with no heading, padding or row count.

### Still accepted and ignored, deliberately

- `-R` — asks for locale-driven number and date formatting, which differs from the invariant
  form only on non-English locales. go-sqlcmd ignores it too. Documented on the field.
- `-T` — undocumented bind token.
- `:serverlist`, `:perftrace`.

---

## Phase 11 — go-sqlcmd features  `[x]`

These have no ODBC equivalent, so the differential harness cannot check them — the reference
rejects the flags outright. They are covered instead by golden assertions in
`tests/go_features.rs`, which run the built binary and compare exact output.

11.1 `[x]` `--vertical` / `--format vert` — one `name value` line per field, names padded to the
  longest in the result set, a blank line between rows. `-h -1` drops the names.
11.2 `[x]` `--ascii` / `--format ascii` — a `+---+` bordered table. Numeric columns right-justify.
  `-s` replaces the `|` border, so `-s '#'` draws the table with `#`.
11.3 `[x]` `--format` and `SQLCMDFORMAT` (`vert`/`vertical`/`ascii`/`horiz`/`horizontal`).
  An explicit flag beats the variable; an unrecognised value means horizontal, as in go-sqlcmd.
  `--vertical` and `--ascii` together are refused.
11.4 `[x]` `--version` — the banner alone, without the usage block `-?` adds.
11.5 `[x]` `--authentication-method` — names an Entra method outright. Mutually exclusive with
  `-G`. **An unrecognised name is refused**, not ignored: silently falling back would connect by
  a method the caller did not ask for. Methods the driver has no equivalent for
  (`ActiveDirectoryClientAssertion`, `ActiveDirectoryAzurePipelines`, `...Environment`,
  `...AzureDeveloperCli`) are therefore rejected rather than mapped to something approximate.
11.6 `[x]` `--driver-logging-level` and `--trace-file`. Levels 1–5 map onto error/warn/info/debug/
  trace; 0 disables. Diagnostics go to the trace file only, never into the results stream.
11.7 `[x]` `--server-name` — dialling one address while presenting another at login. Rests on
  `ClientContext::login_server_name`, added to `mssql-tds`: LOGIN7 stores ServerName as an
  offset/length pair separate from its payload, so both are read from one accessor. Verified by
  reading the name back off a mock server, with overrides both longer and shorter than the
  address dialled.
11.8 `[x]` `SQLCMDCOLORSCHEME` — 24-bit ANSI colouring of results, messages and `PRINT`.
  All 74 chroma v2.27.0 styles are carried in `fmt/schemes.rs`, generated by
  `scripts/extract-styles.ps1` from the chroma XML. `:list color` names them. Verified through a
  PTY against the reference: **35 cases across 7 schemes, byte-identical.** Four behaviours had
  to be measured rather than guessed, because none of them is documented:
  - Colour is emitted **only to a terminal**. A redirected stream stays plain, so a script
    capturing output never sees escape sequences.
  - An **unrecognised** scheme name still colours — chroma answers an unknown name with its
    `swapoff` fallback rather than an error. An *empty* name colours nothing.
  - Emphasis and colour arrive as **two separate sequences**, emphasis first
    (`\e[3m\e[38;2;R;G;Bm…\e[0m`).
  - A multi-line message is closed **per line**: the reset lands at the end of every line and the
    terminators stay outside the escapes. Likewise, each data cell *and* each column separator is
    wrapped individually.
  A chroma entry carrying only emphasis takes its colour from the style's `Text` face, falling
  back to `Background` — monokai defines no `Text`, so `PRINT` output there is italic `#f8f8f2`
  rather than uncoloured.
11.9 `[x]` Modern subcommand CLI and container lifecycle — delivered in Phase 13.

---

## Phase 12 — `--compat go`, verified against the real binary  `[x]`

Phase 11 was written from reading go-sqlcmd's source. Building the tool and diffing against it
found a long list of behaviours the source reading had missed, so the two references are now
reconciled by an explicit switch.

`--compat {odbc|go}` picks whose behaviour to follow where the two disagree. **ODBC is the
default**; `--compat go` opts in. It is settled before the rest of the argument pass because it
changes other options' defaults. The go-only rendering flags (`--vertical`, `--ascii`,
`--format`) work in either mode — the switch governs only the points below.

Everything in the table was measured by running both binaries, not inferred:

| | go-sqlcmd | ODBC |
|---|---|---|
| row count | `1 row affected` | `1 rows affected` |
| `float` `3.14` | `3.14` | `3.1400000000000001` |
| `float` `1.0` | `1` | `1.0` |
| `real` `3.14` | `3.140000104904` | `3.1400001` |
| `money` below 1 | `0.0000` | `.0000` |
| `uniqueidentifier` | lowercase | uppercase |
| `bigint` width | 21 | 20 |
| `money` width | 24 | 21 |
| `smallmoney` width | 14 | 12 |
| `time` width | 16 | 22 |
| `varbinary(10)` width | 10 | 22 |
| `-m 17` | still shows `PRINT` | hides it |
| `-r0` on stderr | `boom` and a blank line | `Msg 50000...` header then the text |
| `-e` echo | no blank line after | blank line |
| `-h 2` repeated heading | blank line after the rule | none |
| `SET NOCOUNT ON` | trailing blank line | none |
| `--ascii` before the count | no blank line | blank line |
| batch line endings | rewritten to the platform's | kept as the input had them |
| `SQLCMDLOGINTIMEOUT` | `30` | `8` |
| `SQLCMDEDITOR` | `notepad.exe` | `edit.com` |
| `SQLCMDDBNAME` without `-d` | `""` | `master` |
| `SQLCMDUSER` under `-E` | `DOMAIN\user` | `""` |
| extra variables | `SQLCMDFORMAT`, `SQLCMDUSEAAD`, `SQLCMDCOLORSCHEME` | — |

The line-ending row is the subtlest: a literal left open across a line boundary carries the
terminator into the statement, so `SELECT '<LF>GO<LF>'` from an LF file is a 4-character value
under ODBC and a 6-character one under go-sqlcmd, which normalises to CRLF on Windows. `Batch`
therefore records each line's own terminator rather than assuming `\n`, and `Session::batch_eol`
substitutes the platform's when in go mode.

### Building the go-sqlcmd reference

The real `main` is `./cmd/modern`. `./cmd/sqlcmd` is `package sqlcmd`, a library, and building it
yields a `.a` archive rather than an executable — the mistake is quiet, because the file is
produced and named as asked.

```powershell
$env:PATH = "C:\tools\go-install\go\bin;$env:PATH"
cd c:\sqlcmdgo\go-sqlcmd
cmd /c "go build -o C:\tools\go-sqlcmd.exe .\cmd\modern"
```

`cmd /c` matters: Go writes build progress to stderr, which PowerShell turns into a terminating
error. go-sqlcmd also has no implicit default server, so every invocation needs `-S`.

---

## Phase 13 — Modern subcommand CLI  `[x]`

go-sqlcmd grew a second, verb-based interface alongside the flag-driven one: `sqlcmd config
add-context`, `sqlcmd create mssql`, `sqlcmd query`. A *context* names an endpoint and optionally
a user, one is current at a time, and the commands that connect use whatever that is.

The two CLIs are told apart by the first argument alone — if it names a subcommand the new one
runs, otherwise everything falls through to the flag parser. A script calling `sqlcmd -Q "..."`
is untouched.

13.1 `[x]` `sqlcmd config` — all thirteen subcommands and their aliases: `add-context`,
  `add-endpoint`, `add-user`, `connection-strings` (`cs`), `current-context`, `delete-context`,
  `delete-endpoint`, `delete-user`, `get-contexts`, `get-endpoints`, `get-users`, `use-context`
  (`use`, `change-context`, `set-context`), `view` (`show`).
13.2 `[x]` The `sqlconfig` file — same schema, key order and quoting as go-sqlcmd, since the two
  read and write the same file. Round-trips through a purpose-built YAML reader/writer.
13.3 `[x]` `sqlcmd query` — resolves the current context into ordinary arguments and re-enters
  the legacy machinery, rather than duplicating the session logic.
13.4 `[x]` `create` / `start` / `stop` / `delete` — the container lifecycle, driving `docker` or
  `podman` through its CLI. Verified end to end: create pulls the image, waits for SQL Server to
  log itself ready, writes the context; `query` then connects and returns `@@VERSION`.
13.5 `[x]` `open ads` — opens the current context in Azure Data Studio. The reference only
  implements Windows: its macOS build writes no password (Azure Data Studio reads UTF-16 from the
  Keychain, the Go library writes UTF-8) and its Linux build panics in `searchLocations`. This
  launches on all three, and hands the password to the credential store only on Windows, where
  that can be done correctly. Elsewhere Azure Data Studio prompts — better than storing something
  it cannot decode.
13.6 `[x]` `create mssql get-tags` — lists the image tags from the registry. Pages through the
  `Link` headers of `/v2/mssql/server/tags/list`; the 274 tags match the reference exactly.
  `reqwest` was already in the dependency tree via `mssql-tds`, so nothing new is linked.

### What running it found

The first comparison against the real binary matched **7 of 26** cases. Every failure was
something source-reading had missed:

| | go-sqlcmd |
|---|---|
| stored password | base64, not plaintext |
| `--password-encryption` | mandatory for `--auth-type basic` |
| errors | a `HINT:` block, then a blank line, then `Error: …` |
| `get-*` with a name | the entry's map alone, no list marker |
| `config view` version | whatever the file said, `""` when absent |
| `delete-user` | quotes the name `"u1"`; every other command uses `'u1'` |
| `connection-strings` | its own no-context wording, unlike `start`/`stop` |
| hint blocks | commands aligned in a column, occasionally padded wider than the labels need |

`connection-strings` emits its five formats in an order that varies between runs, because they
come out of a Go map. That case is marked `unordered` rather than matched line for line.

### Dependencies

No YAML crate is vendored and `--frozen` is a build gate, so `modern/yaml.rs` implements the
corner of YAML this one schema needs — nested maps, lists of maps, scalars, empty collections.
Anchors, tags, flow mappings and multi-document files are refused rather than mis-parsed.
Passwords are base64 for the same reason: `--password-encryption none` means exactly that, and
the encoding only exists so a password containing YAML metacharacters survives the file.

Container passwords come from the OS random number generator. A container published on a port
needs a password that cannot be derived from the time it was created.

---

## Phase 10 — v1 hardening  `[~]`
10.1 `[x]` Differential coverage, current on both platforms:

| harness | Windows | Linux |
|---|---|---|
| ODBC `sqlcmd` (`tests/diff.rs`) | 121 passed, 0 failed, 1 skipped | 116 passed, 0 failed, 6 skipped |
| go-sqlcmd legacy CLI (`tests/go_diff.rs`) | 64 passed, 0 failed | 64 passed, 0 failed |
| go-sqlcmd subcommand CLI (`tests/modern_diff.rs`) | 49 passed, 0 failed, 4 skipped | 49 passed, 0 failed, 4 skipped |
| …with `SQLCMD_DIFF_CONTAINERS=1` | 53 passed, 0 failed, 0 skipped | — |
| `SQLCMDCOLORSCHEME` through a PTY | — | 35 matched, 0 differ |

  Plus the golden assertions in `tests/go_features.rs`. Every skip is recorded in the case files
  with its reason; the Linux skips are Windows-only surface (named pipes, the registry DSN path).
10.2 `[x]` `cargo bfmt`, `cargo bclippy` and `cargo btest` all clean: **231 tests pass** on
  Windows, 212 on Linux (the difference is `#[cfg(windows)]` cases). `cargo-llvm-cov` is
  installed, so `cargo btest` runs as the repo intends.
10.3 `[x]` Self-review pass. Found and fixed: a `last_error_number` field written but never read;
  `-f`'s input code page stored but never applied; redundant first-cell bookkeeping in the result
  pump; `-j` decorating only the non-error branch; and three option fields left behind after the
  features that would have used them were rejected instead.
10.4 `[ ]` Draft PR.

### Auditing for silently-ignored options

An option that parses, validates, and then does nothing is worse than one that fails, because
the caller gets different behaviour with no signal. `scripts` are the usual victim: `-I` changes
SQL semantics, `-p` feeds benchmarks. To check none remain, grep each option field and count
reads **outside** `cli/validate.rs`:

```powershell
$files = Get-ChildItem -Recurse -Filter *.rs src | ForEach-Object { $_.FullName }
Select-String -Path $files -Pattern '\bthe_field_name\b' |
    Where-Object { $_.Path -notmatch 'validate\.rs' }
```

A count of zero means the option is accepted and ignored. Every such field must either be wired
up, refused with `messages::unsupported_option`, or carry a comment saying why ignoring it is
correct. As of this pass only `-R` and `-T` are in the last category.

### The skipped differential case

| Case | Why skipped |
|---|---|
| **`a recursive include is refused`** | The reference recurses until the stack is exhausted; we refuse with a message. A deliberate improvement, recorded rather than hidden. |

### Closed: mid-batch errors

A batch such as `SELECT 1; RAISERROR('boom',16,1); SELECT 2` used to lose the second result set,
because the driver abandoned the stream at the first ERROR token. `TdsClient` now takes
`set_defer_batch_errors(true)`, under which an ERROR mid-batch is collected rather than returned;
iteration follows the DONE tokens' `has_more` flag to the end and the errors come back from
`take_pending_errors()`.

The mode is off by default — ending a batch at the first error is what most callers want and what
they already get — and `sqlcmd` turns it on at connect. A DONE carrying the error flag is normally
a protocol violation; under deferral it is expected exactly once per collected error, which is what
`consumed_pending_error` tracks.

Four differential cases cover it, including the one this previously forced to be skipped.

### Closed: `sql_variant` column width

The plan recorded this as a driver gap on the theory that `ColumnValues` had lost the variant
wrapper. Running the reference showed otherwise: **values decode correctly**, and only the width
was wrong — we printed 256 where both references print a flat 8000. `sql_variant` also ignores
`-y`, unlike every other variable-width type, so it needed its own `Cap` rather than sharing
`Cap::Large`. No driver change was required.

A reminder that a gap recorded from reading code is a hypothesis until something measures it.

---

## File layout

```
mssql-rs/mssql-sqlcmd/
  Cargo.toml
  sqlcmd.md                  # this plan
  src/
    main.rs                  # entry, exit code, stream routing
    session.rs               # the running tool: input, dispatch, execution
    messages.rs              # MSG_* catalog
    vars.rs                  # scripting variables
    commands.rs              # colon-command parsing + :help text
    servers.rs               # -L SSRP broadcast enumeration
    dsn.rs                   # -D data source names
    tracing.rs               # --driver-logging-level, --trace-file
    compat.rs                # --compat odbc|go
    exitcode.rs
    cli.rs / cli/
      spec.rs                # option table: short, long alias, arity
      args.rs                # ODBC-grammar lexer
      validate.rs            # ranges, conflicts, Options
      usage.rs               # -? help text, --version banner
    batch.rs / batch/
      scanner.rs             # quote/comment/bracket state machine
      substitute.rs          # $(var) expansion
    exec.rs / exec/
      connect.rs             # -S parsing, Options -> ClientContext, auth
      runner.rs              # batch execution, result pump
    fmt.rs / fmt/
      widths.rs              # per-type display widths
      table.rs               # headings, padding, wrapping, -k
      value.rs               # SQL value -> text
      report.rs              # messages, row counts, -p statistics
      layout.rs              # --vertical, --ascii
    io.rs                    # sinks, redirection, encodings
    modern.rs / modern/      # go-sqlcmd's subcommand CLI
      sqlconfig.rs           # contexts, endpoints, users
      config_cmds.rs         # sqlcmd config …
      server_cmds.rs         # query, create, start, stop, delete
      container.rs           # docker/podman, password generation
      yaml.rs                # the corner of YAML sqlconfig needs
  tests/
    diff.rs                  # differential harness entry (ODBC)
    go_diff.rs               # differential harness entry (go-sqlcmd)
    modern_diff.rs           # differential harness entry (subcommand CLI)
    go_features.rs           # golden assertions for the go-only features
    diff/
      runner.rs              # shared by both harnesses
      cases/                 # lexing, ranges, conflicts, types, batches,
                             # formatting, wired
    go/
      cases/                 # rendering, behaviour
    modern/
      cases/                 # config
```

Modules the plan listed that were **not** created, because nothing needed them yet:
`batch/terminator.rs` (terminator matching is a dozen lines in `batch.rs`), `exec/auth.rs`
(auth is a single dispatch function in `connect.rs`), and `io/{input,output,console}.rs`
(one `io.rs` covers the sinks; input and console live in `session.rs`).

## Commit split

Per D6 the driver changes are separate from the tool:

1. `mssql-tds` — expose per-statement DONE row counts (`DoneToken::has_count`,
   `TdsClient::take_done_row_counts`).
2. `mssql-tds` — carry COLMETADATA through `drain_stream` so a post-error result set parses.
3. `mssql-tds` — opt-in deferral of mid-batch errors (`set_defer_batch_errors`,
   `take_pending_errors`) so a batch's later result sets stay reachable.
4. `mssql-sqlcmd` — the new crate.

---

## Open items to resolve empirically (before coding the affected phase)

| # | Question | Resolve by | Status |
|---|---|---|---|
| O1 | Actual interactive prompt: `1> `/`2> ` vs `> `/`~ `? | Needs a PTY — the harness always pipes stdin, so this cannot be settled differentially. Phase 7. | **open** |
| O2 | v17 default for `-N` when flag absent. | `SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id=@@SPID`. | **open** |
| O3 | Exact `program_name()` sent by ODBC sqlcmd. | `SELECT program_name FROM sys.dm_exec_sessions WHERE session_id=@@SPID`. We send `SQLCMD`, unverified. | **open** |
| O4 | Is `(N rows affected)` printed for SELECT? | **Resolved: yes**, and it comes from the DONE token, so `SET NOCOUNT ON` suppresses it. | resolved |
| O5 | `-u` output file: BOM or not. | **Resolved: UTF-16LE with a `FF FE` BOM.** Inferred from our own output; the reference's file was not byte-compared. | mostly resolved |
| O6 | Default `SQLCMDEDITOR` on Windows. | **Resolved: `edit.com`**, from `:listvar`. | resolved |
| O7 | Float/`money`/`datetime` exact text repr per type. | **Resolved** — see the Phase 5 table and notes. | resolved |
| O8 | Does `-m` have an upper bound? | Diff case with a large `-m`. | open |

Resolved during Phase 0/1:

| Question | Answer |
|---|---|
| Which stream does `-?` write to, and what is the exit code? | **stdout**, exit **0**. |
| Which stream do parse diagnostics use? | **stderr**, exit **1** — except the duplicate-`-o` error, which goes to **stdout**. |
| Does the help text end with a blank line? | **No.** It ends after `[-? show syntax summary]`. |

---

## Running the differential suite

```powershell
$env:SQLCMD_DIFF_REF = "C:\Program Files\Microsoft SQL Server\Client SDK\ODBC\180\Tools\Binn\SQLCMD.EXE"
$env:SQLCMD_DIFF_SERVER = "local"   # any non-empty value: "a server is reachable"
cargo test -p mssql-sqlcmd --test diff -- --nocapture
```

Without `SQLCMD_DIFF_REF` the whole suite skips. Without `SQLCMD_DIFF_SERVER` only the
argument-parsing cases run, so the suite stays useful on a machine with no SQL Server.

Connecting cases use `-C` against the local default instance under integrated auth. `Server`
names in `Msg` lines and the version banner are normalized away; nothing else is.

### The go-sqlcmd suite

A second harness, `tests/go_diff.rs`, does the same against go-sqlcmd. It shares
`tests/diff/runner.rs`, reads its cases from `tests/go/cases/`, and adds `--compat go` to every
invocation of our binary.

```powershell
$env:SQLCMD_DIFF_GO     = "C:\tools\go-sqlcmd.exe"
$env:SQLCMD_DIFF_SERVER = "localhost"
cargo test -p mssql-sqlcmd --test go_diff -- --nocapture
```

Unset `SQLCMD_DIFF_GO` and it skips, so the suite costs nothing on a machine without the Go
toolchain. Because go-sqlcmd has no default server, the harness prepends `-S <server> -E -C`.

### The subcommand-CLI suite

A third harness, `tests/modern_diff.rs`, covers `sqlcmd config …` against go-sqlcmd. These
commands touch only the local file, so it needs no server — just `SQLCMD_DIFF_GO`.

```powershell
$env:SQLCMD_DIFF_GO = "C:\tools\go-sqlcmd.exe"
cargo test -p mssql-sqlcmd --test modern_diff -- --nocapture
```

Each case is a *sequence* of invocations against a fresh config file, of which only the last is
compared, so a case can set up the state it needs without a separate fixture format. stdout and
stderr are compared as one stream, because go-sqlcmd splits its messages between them
inconsistently from command to command.

Cases declare what they need, and are skipped rather than failed when it is absent:

| flag | needs | why it is gated |
|---|---|---|
| *(none)* | `SQLCMD_DIFF_GO` | Config-only; runs in milliseconds. |
| `needs_server` | `SQLCMD_DIFF_CONNECT` | `query` connects. The runner parses `-S`/`-U`/`-P` out of the value and substitutes `{address}`, `{port}` and `{user}` into the case, so the same case works wherever the test server lives. |
| `needs_container` | `SQLCMD_DIFF_CONTAINERS=1` | `create`/`start`/`stop`/`delete` pull or start a SQL Server image, so a case costs minutes. Off by default keeps them out of a pre-push loop while leaving them available to a nightly run. The runner reads the `id:` lines back out of both config files and `docker rm --force`s them afterwards. |

A `needs_server` case must not fall back to whatever the machine happens to offer. An earlier
version added a bare endpoint, which meant `localhost:1433` with no credentials — the reference
then reported `Login failed for user ''` and the case was really comparing two different
failures rather than two query results.

### Colour, which only exists on a terminal

`SQLCMDCOLORSCHEME` output is suppressed on a redirected stream, so it cannot be captured with
an ordinary pipe — every comparison would trivially match with both sides plain. The Linux
harness allocates a PTY with `script -qec` instead, which is the only way to see it at all.

The 74-entry scheme table is generated, not hand-written:

```powershell
scripts/extract-styles.ps1   # chroma XML -> face table
scripts/wrap-styles.ps1      # -> src/fmt/schemes.rs
```

The generator has to reproduce two chroma rules exactly: an entry inherits along a parent chain
(`GenericEmph` -> `Generic` -> `Text` -> `Background`), and a style is registered under
`strings.ToLower(name)` — so `RPGLE` is listed and reachable as `rpgle`.

### Running on Linux

Windows reaches its default local instance under integrated auth with no arguments at all. Linux
has neither, so `SQLCMD_DIFF_CONNECT` supplies a prefix for every connecting case:

```bash
export SQLCMD_DIFF_REF=$HOME/refs/odbc/opt/mssql-tools18/bin/sqlcmd
export SQLCMD_DIFF_GO=$HOME/refs/go-sqlcmd
export SQLCMD_DIFF_SERVER=localhost,1435
export SQLCMD_DIFF_CONNECT="-S localhost,1435 -U sa -P $(cat ~/refs/sa-password) -C"
export ODBCSYSINI=$HOME/refs/odbcsysini
cargo test -p mssql-sqlcmd --test diff -- --nocapture
```

Neither reference needs root to install. The `.deb`s unpack with `dpkg-deb -x` into `$HOME`, and
`ODBCSYSINI` points unixODBC at an `odbcinst.ini` naming the extracted driver, in place of the
`/etc/odbcinst.ini` the package would normally write. go-sqlcmd builds from the same source tree
used on Windows, against a Go toolchain untarred into `$HOME`.

Two WSL-specific traps: `HOME` and `LD_LIBRARY_PATH` are inherited from Windows in Windows-shaped
form, which breaks cargo — unset them and set `CARGO_TARGET_DIR` to a path on ext4, both for
speed and to keep the two platforms' build outputs apart.

---

## The ODBC reference is a different tool on Linux

This was the biggest surprise of the cross-platform pass, and it is not a portability problem in
this build — the reference's own behaviour differs:

| | Windows | Linux |
|---|---|---|
| conflicting options | `The -E and the -U/-P options are mutually exclusive.` | `The U and the E options are mutually exclusive.` |
| `-i` with `-q`/`-Q` | refused as exclusive | accepted; the file is opened and fails on its own |
| `/` prefix | introduces an option | a path, so `/?` is an unknown option |
| unknown `-9` | reported as `'-9'` | reported as `'9'` |
| a stray token | `Unexpected argument` | `Unknown Option` |
| `-S -Q x` | `-S` has a missing argument | `-S` swallows `-Q`, and `x` is the unknown option |
| `-?` usage | lists `-L -z -f -v -A -j` | omits those, adds `-D -J` |
| `:xml on` | works | *"XML mode is currently unavailable in the Linux version."* |

Everything above the usage row is matched by `cfg!(windows)` branches. The last two are not, and
are recorded as `unix_skip_reason` on their cases:

- Reproducing the Linux usage block would mean printing a list of options that misdescribes this
  build, which is worse than differing from it.
- Supporting XML where the reference does not is not a defect worth removing.

Two further cases are skipped because the **Linux reference garbles its own message** for `-y`/
`-Y` out of range, echoing kilobytes of uninitialised memory in place of the option text. That is
a bug to avoid copying, not a behaviour to match.

One genuine gap remains, recorded rather than hidden: when a login fails for two reasons at once,
the Linux reference reports *"Cannot open database"* before *"Login failed"* and we report them
the other way round. Windows agrees with us. Closing it means ordering the login response's
errors inside `mssql-tds`.

### Bugs the Linux run found in this build

None of these were visible on Windows:

| | was | should be |
|---|---|---|
| `SQLCMDSERVER` | the parsed host, dropping the port | the whole `-S` argument |
| `SQLCMDWORKSTATION` | `localhost` | the real host name |
| `SQLCMDEDITOR` (go mode) | always `notepad.exe` | `vi` off Windows |
| `connection-strings` | always `SET "X=y" & …` | `export 'X=y'; …` off Windows |
| an unreadable `-i` file | reported after the connection attempt | before it |

`SQLCMDSERVER` and `SQLCMDWORKSTATION` were platform-blind bugs that Windows testing could not
have caught: the port only appears when one is given, and `HOSTNAME` happens to be set in a
Windows shell but is not exported on Unix.

---

## Working notes

**The line terminator is platform-native, and Windows testing hides it.** Both references write
`\n` on Linux and `\r\n` on Windows. Every message and every rendered line in this port had been
written with a literal `\r\n`, which is invisible on Windows and wrong on every Linux run. The
fix is `messages::EOL`, a `const` chosen by `cfg!(windows)`, used wherever a line is *composed*.

It cannot be fixed at the sink by translating `\n` to `\r\n` on the way out, because a CR that
arrives **inside a data value** has to survive untouched — `SELECT 'a' + CHAR(13) + 'b'` renders
the CR verbatim in all three tools. Terminator and payload are indistinguishable by the time the
bytes reach the writer, so the distinction has to be kept where the line is built.

Two traps came out of applying this across the codebase:

- `"…{EOL}…".to_string()` compiles and is *wrong* — a plain literal does not interpolate, so the
  placeholder leaks to the user verbatim. Only `format!` interpolates. The differential suites
  caught it; the type system cannot.
- Not every `\r\n` is a terminator. `reg.exe` emits CRLF on every platform, so the fixture in
  `dsn.rs` must keep its literal `\r\n`.

**Do not sweep source files with PowerShell regex.** Two mechanical passes over the tree caused
more damage than they repaired: a greedy pattern ate escaped quotes, byte literals (`b"\r\n"`)
were rewritten into nonsense, and the `EOL` constant's own definition was replaced. Worse,
`Get-Content`/`Set-Content` round-trips a UTF-8 file through Windows-1252 and turns every em
dash into mojibake — twice over, if a file is touched twice. It still compiles, because mojibake
is valid UTF-8. Prefer targeted edits; if a sweep is unavoidable, restrict it to `#[cfg(test)]`
blocks where a mistake is a compile error rather than a silent one.

**Probing the reference from PowerShell.** Windows PowerShell 5.1 has no
`ProcessStartInfo.ArgumentList`, so building a probe that way silently passes *no*
arguments and every case looks like a connection failure. Use the call operator with a
splatted array (`& $ref @Argv`) and redirect to files. The Rust harness sidesteps this
entirely by using `std::process::Command`.

**PowerShell decorates native stderr.** Anything a native tool writes to stderr comes back
wrapped in a PowerShell error record (`EXE : message` followed by `At C:\...`). Never read
exact message text out of a PowerShell-mediated capture — use the harness.

**A leftover `SQLCMD_DIFF_*` variable silently corrupts a whole suite.** The ODBC runner splits
`SQLCMD_DIFF_CONNECT` into arguments, so a value left over from another suite (`"1"`, set only to
signal presence) is passed to the binary and 83 of 117 cases "fail" identically. Clear the
variables between suites rather than trusting the shell's state.

**A stale `sqlcmd.exe` blocks the build.** An interrupted interactive run leaves a process
holding `target\debug\sqlcmd.exe`, and cargo then fails with `failed to remove file`. Kill it
with `Get-Process sqlcmd | Stop-Process -Force`.

**Driver unit tests need certificate fixtures.** Seven `mssql-tds` tests fail on a fresh
checkout because `mssql-tds/tests/test_certificates/` ships only `generate_certs.sh`. Run that
script before treating driver test failures as real.

**Reading a reference's source is not the same as running it.** Phase 11 was written entirely
from go-sqlcmd's Go source and looked complete. Running the real binary found roughly twenty
divergences it had missed — float formatting, six column widths, blank-line placement, four
variable defaults. Nothing goes down as parity until a harness has compared the two byte for
byte.

**Installing Go.** `winget install GoLang.Go` fails with exit 1602 without elevation. The zip
works without it: take the current version from `https://go.dev/dl/?mode=json` and expand it,
then point `PATH`, `GOPATH` and `GOCACHE` at writable directories.

**Probes must close stdin.** A reference that decides to prompt — `add-user` without a password,
say — will sit there forever, and the harness looks hung rather than failed. Give every probe
`< NUL` (PowerShell) or `Stdio::null()` (Rust) so a prompt becomes an error.

**A failed `create` must remove its container.** The first end-to-end run left a SQL Server
running that nothing was tracking, because the container was created before the config entry was
written and nothing tore it down when a later step failed. Container names now carry a random
suffix as well — `sqlcmd-mssql-{port}` collides with exactly the container a previous failure
stranded.

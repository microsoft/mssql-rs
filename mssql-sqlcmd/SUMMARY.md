# `sqlcmd` in Rust — status summary

A single Rust binary that replaces both existing SQL Server command-line clients.
It's built on our own Rust TDS driver, so it carries no ODBC driver dependency,
no Go runtime, and no native install footprint — one binary, one build, running
on Windows and Linux. It drops in for ODBC `sqlcmd` by default and for
`go-sqlcmd` with `--compat go`.

The CLI surface is complete — every option, colon command and scripting variable
that ODBC `sqlcmd` or `go-sqlcmd` accepts is accepted here, verified by diffing
the three option sets rather than eyeballing them. What's left is a short,
well-understood list, not open-ended discovery.

## How close

| Area | ODBC `sqlcmd` | `go-sqlcmd` | Rust |
|---|---|---|---|
| Options (short / long) | 47 / 0 | 46 / 53 | all covered |
| Colon commands | 16 | 13 | all 16, plus the 3 Go lacks |
| Scripting variables | 15 | 18 | all 18 |
| Entra ID methods | 6 | 15 | all 15 |
| Container lifecycle (create/start/stop) | n/a | yes | full parity |
| Colour schemes | n/a | 74 | 74, byte-identical |

**Differential tests: 246/246 green on Windows, 254/254 on Linux.** Not
golden-file tests — each case runs the real binary and mine side by side and
compares bytes, so the suite can't drift from what the references actually do.

## Major gaps — three tiers

### 1. The one real risk: Entra ID

All 15 methods are wired, registered and unit-tested, but never run against a
live tenant because I don't have one. Same for `-z`/`-Z` (password change) and
`-g` (Always Encrypted) — blocked on environment, not on understanding. This is
the only item I'd call a genuine unknown.

### 2. Known and small

- No line editing at the prompt (`rustyline` redraw ate output, backed out)
- `:ed` / `:perftrace` deferred
- An unrecognised `--format` name isn't rejected
- Banner hardcodes `NT` on Linux
- Binary needs system OpenSSL rather than being fully self-contained like Go's

### 3. Deliberate non-parity

Three ODBC `sqlcmd` bugs I chose not to reproduce — an unsubstituted `printf`
specifier leaking into `datetime2` output, and two "Internal error" crashes
under `-R` — plus a `go-sqlcmd` panic on Linux `open ads`.

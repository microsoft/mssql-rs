# Parameterized execution - `SQLBindParameter` / `SQLExecute` / `SQLExecDirect`

Status, behavior, and known gaps for parameterized prepared-statement execution
in the ODBC Driver 18 (Rust). Updated 2026-09-01.

---

## Implemented

`SQLPrepare(W)`, `SQLBindParameter`, `SQLExecute`, and parameterized
`SQLExecDirect(W)`, including managed prepared-handle invalidation across
transparent reconnects.

- **Managed prepared statement** - `StmtState` stores a
  `mssql_tds::PreparedStatement` containing the rewritten SQL and, once
  materialized, an opaque client-issued `StatementId`. The live server handle
  lives in the `TdsClient`, keyed by that id. `SQLExecute` moves the statement
  out while executing and writes it back afterward. The first execute runs
  `sp_prepexec`; subsequent executes reuse the live handle via `sp_execute`.
- **`SQLExecDirect`** - parameterized text runs `sp_executesql` (direct, no
  cached handle); unparameterized text runs as a plain language batch.
- **Prepared-handle capture** - for a result-returning `sp_prepexec`, the
  `@handle` arrives after the result set. It is written straight into the
  client's `StatementId -> handle` map by `push_return_value` when the token
  lands, in the same token funnel that pins Always Encrypted metadata — no
  caller-side capture step, so no drain path can drop it.
- **`sp_unprepare` (handle release)** - a handle superseded by a re-prepare or
  rebind is deferred in `pending_unprepare` and released at the next
  `SQLExecute` by piggybacking onto `sp_prepexec`: the superseded handle is sent
  as that call's in/out `@handle`, so the server drops the old plan and prepares
  the new one in one round trip. `SQLExecDirect` supersede and
  `SQLFreeHandle(STMT)` use standalone `sp_unprepare` because they have no
  `sp_prepexec` on which to piggyback. A data-at-execution execute also declines
  to piggyback even though it does run `sp_prepexec`: the request stays open for
  the whole `SQLPutData` sequence and may be cancelled before it reaches the
  server, so evicting the superseded handle at build time could leak the plan
  until disconnect. It rides along with the parked state instead.
- **`sp_prepexec` failure ownership** - the pending handle remains in ODBC
  through reconnect, validation, parameter construction, and Always Encrypted
  setup. `mssql-tds` consumes it only when the prepexec RPC is ready to
  serialize, so definite pre-send failures restore it for a later cleanup.
  Serialization, send, and response failures remain ambiguous: the server may
  already have consumed the handle, so retrying cleanup could target an invalid
  or reused id. This matches msodbcsql after its `ExecRPCImmediate` boundary.
- **Stale-handle invalidation after transparent reconnect** - the client's
  `StatementId -> handle` map is cleared on every reconnect, alongside the
  Always Encrypted describe cache. `TdsClient::execute_prepared` performs
  recovery first, then resolves the statement's id against the (possibly
  cleared) map: a hit reuses the handle, a miss re-prepares the SQL. "Stale"
  therefore collapses to "absent from the map", and a superseded pending drop
  that the reconnect discarded is likewise absent and skipped. `unprepare`
  applies the same lookup. If ODBC cannot claim the connection before execution,
  it restores both the moved prepared statement and pending orphan to
  `StmtState`.
- **Lifecycle** - `SQL_RESET_PARAMS` clears bindings; `SQLCloseCursor` and
  `SQLFreeStmt(SQL_CLOSE)` preserve the handle; re-`SQLPrepare` and rebind
  orphan it for release.
- **Placeholder rewrite** - `SQLPrepare` rewrites `?` to `@P1...@Pn` once,
  skipping string literals, quoted identifiers, and comments. It stores the
  rewritten SQL and marker count, so repeated `SQLExecute` calls do not re-scan
  the text.
- **Bind-time type validation** - `api::type_rules` canonicalizes the C type
  (folding the deprecated `SQL_C_DATE` / `SQL_C_TIME` / `SQL_C_TIMESTAMP`
  spellings onto the `SQL_C_TYPE_*` forms), then applies the `HY003` gate to that
  canonical form, and classifies SQL data types three ways, like msodbcsql's
  `IsValidSqlType`: supported, real but with no SQL Server counterpart (`HYC00` -
  the interval types), or unknown (`HY004`). `params::conversion_matrix` owns the
  C -> SQL conversion table, shaped like msodbcsql's `fValidConversion` (one row
  per C type). The semantics differ: `fValidConversion` is a legality table,
  this one is an implementation-progress list, so a pairing it does not carry
  returns `HYC00` (unbuilt), not `07006` (illegal) or `HY003` (unknown type).
  Tracked by AB#47500; the state becomes `07006` once the table is complete.
- **`SQL_C_DEFAULT` resolution** - resolved at bind time to the C type implied
  by `ParameterType` and stored resolved in `BoundParam`, so the execute path
  never sees the placeholder. Version-aware, like msodbcsql's `Sql2CDefault`,
  which reads `rgbTRANSTYPE` for a 3.51-or-earlier application and
  `rgbTRANSTYPE380` otherwise: `SQL_SS_TIME2` and `SQL_SS_TIMESTAMPOFFSET`
  default to `SQL_C_BINARY` below ODBC 3.8. The resolved type is then run through
  the conversion matrix like an explicit one - see the design rule below - so a
  defaulted binding gets the same answer as naming the C type. `BoundParam`
  stores only the resolved type: it needs no defaulted flag, because a typed NULL
  is built from `ParameterType` for every binding, not just defaulted ones.
  `SQL_SS_UDT` and `SQL_SS_TABLE` are still rejected at bind
  time, since they need a server type name no describe call reports.
- **Value conversion** - the wire type follows `ParameterType`, not the C type:
  an integer, character or binary buffer is declared as the SQL type the
  application named and converted to it. Integer and character buffers reach
  each other's SQL types (P5); `SQL_C_BINARY` reaches only the binary SQL types,
  so binary is binary-to-binary only.
  Character indicators support `SQL_NULL_DATA`, `SQL_NTS`, and
  explicit byte length; binary values use explicit byte length or
  `BufferLength` when no indicator pointer is supplied.
- **Data-at-execution streaming** - `SQLParamData` / `SQLPutData` stream
  `SQL_C_CHAR`, `SQL_C_WCHAR`, and `SQL_C_BINARY` as PLP
  `(n)varchar(max)` / `varbinary(max)`, matching msodbcsql sequencing.
  Same-family pairings always stream. A C-type/SQL-type wideness mismatch
  within the character family (e.g. `SQL_C_WCHAR` against a narrow SQL type)
  is buffered and transcoded once at `SQLParamData` close via the
  connection's collation rather than rejected - msodbcsql accepts the same
  pairing but transcodes incrementally instead of buffering the whole value,
  a documented deviation, not a gap. The same-wideness narrow path
  (`SQL_C_CHAR` against a narrow SQL type) still assumes UTF-8 on the wire
  instead of reading the connection's collation (AB#47590); only the
  wideness-mismatch half of that gap has closed.
  Cross-*family* pairings (character/binary against an integer SQL type) are
  still **not** streamable: there is no transcode from arbitrary bytes to an
  integer wire value. Since P5 made those pairings bindable, the refusal
  moved from `SQLBindParameter` to execute - the DAE indicator is only read
  while building the parameter list - so an application gets `HYC00` from
  `SQLExecute` after setting up its `SQLParamData` loop rather than at bind.
  msodbcsql returns `SQL_NEED_DATA` for this pairing instead of refusing it.
  Pinned by `CrossFamilyDataAtExecutionIsRejectedAtExecute` and, for the
  wideness-mismatch fix, `NarrowCTypeAgainstWideSqlTypeDataAtExecutionTranscodes`
  / `WideCTypeAgainstNarrowSqlTypeDataAtExecutionTranscodes` in
  `execute_test.cpp`.

## `mssql-tds` prepared API

- `PreparedStatement` stores SQL plus an optional opaque `StatementId`; the
  server handle lives in the client's `StatementId -> handle` map.
- `execute_prepared` owns recovery, timeout deduction, live-handle reuse,
  stale-handle invalidation, reprepare, and live-orphan piggyback planning.
  `unprepare` sends `sp_unprepare` only when the client still holds a handle for
  the statement in the live session.
- `sp_prepexec` captures its `@handle` RETURNVALUE separately from user output
  parameters. Always Encrypted describe metadata is retained until capture and
  pinned under the returned handle, allowing the next managed `sp_execute` to
  encrypt parameters without another describe. When `sp_prepexec` replaces a
  prior handle, successful replacement capture also removes the superseded
  handle's metadata; failed or incomplete capture leaves it untouched.
- Focused coverage includes handle-map reuse/re-prepare planning, unprepare
  behavior, in-funnel handle capture, wire-byte assertions for piggybacked
  drops, claim-failure restoration, and Always Encrypted metadata pinning.

### Tracked follow-ups

- **Cross-client ownership:** closed structurally by the opaque `StatementId`.
  Ids are unique to the issuing client, so a `PreparedStatement` carried to a
  different client resolves to "not materialized here" and is re-prepared rather
  than aliasing an unrelated server handle. (Formerly tracked as ADO 47098.)
- **Enabled reconnect e2e:** session-recovery baseline state is being fixed in
  [ADO 46631](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46631).
  Enable `StaleHandleAfterReconnectIsInvalidatedAndReprepared` afterward under
  [ADO 47099](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47099).

## Conversion milestone: integers and strings

Goal: support parameters of narrow and wide integer C types and character C
types, with SQL <-> C conversion among them, on conversion infrastructure shared
with the fetch path
([`conversion/fetch_convert.rs`](../src/conversion/fetch_convert.rs)).

### Scope

| Axis | In scope |
| --- | --- |
| Narrow integer C | `SQL_C_STINYINT`, `SQL_C_TINYINT`, `SQL_C_UTINYINT`, `SQL_C_SSHORT`, `SQL_C_SHORT`, `SQL_C_USHORT` |
| Wide integer C | `SQL_C_SLONG`, `SQL_C_LONG`, `SQL_C_ULONG`, `SQL_C_SBIGINT`, `SQL_C_UBIGINT` |
| Character C | `SQL_C_CHAR`, `SQL_C_WCHAR` |
| Special | `SQL_C_DEFAULT` (resolved to a concrete C type) |
| Integer SQL | `SQL_TINYINT`, `SQL_SMALLINT`, `SQL_INTEGER`, `SQL_BIGINT` |
| Character SQL | `SQL_CHAR`, `SQL_VARCHAR`, `SQL_LONGVARCHAR`, `SQL_WCHAR`, `SQL_WVARCHAR`, `SQL_WLONGVARCHAR` |
| Stretch | `SQL_C_BIT` <-> `SQL_BIT` |

Four conversion quadrants:

| | to integer SQL | to character SQL |
| --- | --- | --- |
| **integer C** | A: narrow and range-check (`22003`) | C: format as text (`22001`) |
| **character C** | D: parse (`22018`, `22003`, `22001`) | B: transcode and length (`22001`) |

Out of scope for this milestone: decimal/numeric, money, temporal, GUID, binary,
output parameters, data-at-exec, parameter arrays, and TVPs. Binary (AB#47688)
and data-at-execution landed separately, outside this milestone; the rest still
emit their P0-era shapes.

### Design rules

- **The matrix lists only implemented pairs.** A pairing accepted at bind time is
  always one the execute path can convert, so there is no bind-succeeds /
  execute-fails window. Rows and entries are added as each phase lands.
- **A defaulted binding is checked like an explicit one.** `SQL_C_DEFAULT` is
  resolved to its concrete C type and then run through the same matrix, so an
  application gets the same answer whether it named the C type or let the driver
  pick it. The alternative - exempting defaulted bindings, on the grounds that
  `resolve_default_c_type` returns a pairing that is legal by construction -
  makes the two spellings of the same intent disagree:
  `SQL_C_TYPE_TIMESTAMP` + `SQL_TYPE_TIMESTAMP` is rejected at bind while
  `SQL_C_DEFAULT` + `SQL_TYPE_TIMESTAMP` is accepted.

- **Legality is decided per direction, at different moments.** msodbcsql consults
  its shared `fValidConversion` table only where both types are known up front:
  `SQLBindParameter` (`sqlcdesc.cpp`), output-parameter retrieval
  (`sqlcdata.cpp`), and BCP. `SQLBindCol` / `SQLGetData` cannot, since a column's
  SQL type may be unknown until after execute, so the fetch direction returns the
  same `07006` from inside `Convert()` (`CVT_ILLEGAL`, which is literally
  `IDS_07_006`). This driver mirrors that split: a bind-time matrix for
  parameters, `ConvError::Restricted` inside the fetch converters.
- **Direction changes severity.** Character truncation is benign outbound
  (`01004`, chunked `SQLGetData`) but an error inbound (`22001`). msodbcsql
  encodes this with an explicit `XLATDIR` argument
  (`sqlccnvt.cpp`: `CVT_CHAR_TRUNC && fConversionDirection == TODRIVER`).
- **Share the value model, not the pointer I/O.** Fetch and parameters share the
  canonical numeric value, literal parsers, and SQLSTATE vocabulary; each keeps
  its own audited unsafe edge.
- **`SqlType` metadata is sufficient for this milestone.** `Int(None)`,
  `Varchar(None, len)`, and `NVarchar(None, len)` carry type and length
  independent of the value. Only decimal and temporal typed NULLs need the
  `mssql-tds` metadata rework, and those are out of scope.

### Phases

Tracked under User Story
[46373](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46373),
one task per phase.

| Phase | Task | Status | Deliverable |
| --- | --- | --- | --- |
| P0 | [47364](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47364) | Code complete | Extract shared conversion core from `fetch_convert.rs` |
| P1 | [47365](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47365) | Code complete | Parameter type model, conversion matrix, `SQL_C_DEFAULT` |
| P2 | [47366](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47366) | Code complete | Safe C-buffer reader |
| P3 | [47367](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47367) | Code complete | Quadrant A: integer C -> integer SQL |
| P4 | [47368](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47368) | Code complete | Quadrant B: character C -> character SQL |
| P5 | [47369](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47369) | Code complete | Quadrants C and D: cross conversions |
| P6 | [47370](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47370) | Not started | Parity and e2e hardening |
| P7 | [47371](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47371) | Not started | Cleanup and follow-up hooks |

P1 is independent of P0; P3 onward depend on both.

#### P0 - Extract shared conversion core (code complete)

Pure refactor, no behavior change. `src/conversion/` now holds the value-level
conversion for both directions:

- `error.rs` - the outcome vocabulary (`ConvOk`, `ConvError`) lifted out of
  `fetch_convert.rs`. `NotHandledHere` stays a dispatch signal and must never
  reach an application.
- `numeric.rs` - `NumericSource` (exact `Int` / `Scaled` / `Float` model),
  `parse_decimal_literal`, `to_i128_truncating`, and `narrow_i128`, extracted
  from the `narrow!` macro that was local to `convert_integer_c`. This carries
  the 128-bit shift-overflow guard and the `22003` unrepresentable-value fix
  into the parameter path.
- `fetch_convert.rs` - `api/fetch_convert.rs` moved wholesale. It has no handle
  or diagnostic coupling, so it never belonged beside the `SQLxxx` entry points
  in `api/`.
- `param_convert.rs` - `params/convert.rs` moved, so both direction converters
  sit together on the shared core. `params/` keeps what is genuinely about
  bindings: the `BoundParam` record and the bind-time `conversion_matrix`.

Deferred to the phase that first constructs them, because `sqlstate.rs` carries
no `allow(dead_code)` and `cargo bclippy` runs `-D warnings` - an unused
SQLSTATE constant or a never-constructed enum variant fails the lint gate:

- `SQLSTATE_22001` and its `DiagMsg` - landed with P4. `WARN_STRING_TRUNCATION`
  (`01004`) already existed for the outbound path.
- `ConvDirection`, and a split of `Truncated` into `FractionalTruncation` /
  `StringTruncation`, were the anticipated shape for inbound severity. Neither
  was needed: the parameter path carries its own
  `ParamBuildError::StringTruncation`, and fetch keeps `ConvOk::Truncated`, so
  direction is expressed by which converter you are in rather than by a flag.

#### P1 - Parameter type model and conversion matrix (code complete)

- [`api/type_rules.rs`](../src/api/type_rules.rs) - C-type canonicalization, the
  `HY003` / `HY004` identifier gates, and version-aware `SQL_C_DEFAULT`
  resolution. Direction-neutral, so it sits in `api` rather than `params`.
- [`params/conversion_matrix.rs`](../src/params/conversion_matrix.rs) - one row
  per C type listing the SQL types it converts to. Rows as P1 landed them:
  `SQL_C_CHAR` -> `CHAR` / `VARCHAR` / `LONGVARCHAR`, `SQL_C_WCHAR` -> `WCHAR` /
  `WVARCHAR` / `WLONGVARCHAR`, and `SQL_C_BINARY` -> `BINARY` / `VARBINARY` /
  `LONGVARBINARY`. P3-P5 added the integer rows and the cross-family targets.
- [`api/bind_param.rs`](../src/api/bind_param.rs) - runs both checks and stores
  the resolved C type on the binding.

No value conversion changed here: `SQL_C_SLONG` + `SQL_INTEGER` still fails at
bind until P3 adds its row.

Deviations from msodbcsql, verified against source:

- ODBC 3.x reuses the 2.x date/time SQL values: `9` is both `SQL_DATE` (2.x
  concise) and `SQL_DATETIME` (3.x verbose), and `10` is both `SQL_TIME` and
  `SQL_INTERVAL`. A `ParameterType` of `9` is therefore ambiguous, so it is
  rejected (`HY004`) rather than folded - 3.x applications use `SQL_TYPE_*`
  (91-93), and the DM remaps a 2.x application's spelling first. The C side has
  no such collision (`SQL_C_DATE` is only ever 9), so `canonical_c_type` folds
  9-11 onto the `SQL_C_TYPE_*` forms instead of rejecting them. msodbcsql
  accepts both SQL spellings because it also serves 2.x applications and can
  disambiguate on the declared version, and it canonicalizes the C pair in the
  opposite direction, toward its 2.x internal representation.
- `SQL_C_DEFAULT` resolves the wide character types to `SQL_C_WCHAR` and
  `SQL_GUID` to `SQL_C_GUID`, following the ODBC 3.x default-C-type table.
  msodbcsql's `rgbTRANSTYPE380` resolves both to `SQL_C_CHAR`, an ANSI-transfer
  artifact with no equivalent here; resolving UTF-16 input to this driver's
  UTF-8 `SQL_C_CHAR` would silently corrupt data. Accepted deviation, also
  registered in
  [`mssql-odbc.instructions.md`](../../.github/instructions/mssql-odbc.instructions.md)
  and on AB#47365.
- msodbcsql normalizes types to its internal representation before validating:
  ODBC 3.x date/time identifiers down to their 2.x values, the SS types to their
  `*_MAPPED` ids, and `SQL_DOUBLE` to `SQL_FLOAT`. Those exist because its
  validators and default-type tables are dense arrays indexed by the internal id;
  this driver matches on the ODBC 3.x values directly and needs no equivalent.
  `SQL_DOUBLE` and `SQL_FLOAT` are therefore two distinct `Supported` types here
  that happen to resolve alike - revisit at P3/P4 if numeric matrix rows start
  duplicating them.

#### P2 - Buffer reader (code complete)

Landed with P3, the phase that gives its integer path a producer and a consumer.

[`conversion/param_buffer.rs`](../src/conversion/param_buffer.rs) is the single
audited read of application buffers, private to `conversion`. It reads in two
steps: `read_indicator` settles NULL and the special indicators before any value
buffer is touched, then `read_param_value` returns an owned `AppValue`
(`Integer`, `NarrowText`, `WideText`).

Rules it fixes:

- Fixed-width C buffers are read with `read_unaligned`; ODBC promises no
  alignment.
- `StrLen_or_Ind` is a length only for the variable-length C types (msodbcsql's
  `IsFixedCType`); a fixed-width type takes its size from the C type.
- A null indicator pointer means `SQL_NTS`, not NULL.
- A non-NULL parameter with a null `ParameterValuePtr` is `HY009` - wider than
  msodbcsql, which leaves the reachable case unguarded.
- `AppValue::Integer` is `i128`, so `SQL_C_UBIGINT` above `i64::MAX` reaches the
  narrowing check intact rather than as a negative.

#### P3 - Integer C to integer SQL (code complete)

- `conversion_matrix.rs` gains a row per integer C type reaching all four integer
  SQL types. Width is not legality: a value that does not fit is a runtime
  `22003`, not a rejected binding.
- `param_convert::integer_value` picks the `SqlType` from `ParameterType`, not
  the C type, typed NULL included - the first phase where `@P1` is declared `int`
  rather than `nvarchar(max)`.
- Narrowing goes through `numeric::narrow_i128`, so parameters and fetch share
  one range check. Overflow is `22003`, including `SQL_C_UBIGINT` above
  `i64::MAX`, which no SQL Server type can hold.
- `SQL_C_TINYINT` is sign-unknown: `sqlext.h` gives it neither
  `SQL_SIGNED_OFFSET` nor `SQL_UNSIGNED_OFFSET`, unlike `SQL_C_STINYINT` and
  `SQL_C_UTINYINT`. The rule is per pairing, not per type - a `tinyint`-to-
  `tinyint` transfer moves the byte unchanged, every other pairing reads it
  signed. `ConvertToFixed` range-checks against `SCHAR_MAX`/`SCHAR_MIN` and
  stores through `(SCHAR *)`, but skips the check when the source is any tinyint
  flavour (`sqlccnvt.cpp`). So `SQL_C_TINYINT` is not a synonym for
  `SQL_C_STINYINT`: only the same-width case differs, and `SQL_C_STINYINT` never
  gets it.
- msodbcsql spells that one rule two ways, and so does this driver, because the
  two directions have different representations available:
  - Fetch copies the byte outright, so no sign is ever chosen. msodbcsql usually
    does not even reach the converter - `sqlcdata.cpp` maps a `SQLINT1` column to
    `SQL_C_UTINYINT` and clears `fConvNeeded` - and where it does convert,
    `ConvertToFixed` forces a bit copy ("input or output is sign unknown and
    opposite parameter is same size").
  - Parameters cannot. `read_param_value` widens every integer C type to `i128`,
    and widening forces an interpretation; signed would turn an application byte
    of `0xC8` into `-56`, which a `tinyint` column cannot hold. Unsigned is the
    reading that keeps the widening lossless, so `effective_param_c_type`
    rewrites the C type instead - exactly as `ParamToSQLType` does ("If both are
    tinyint, change C type to unsigned", `sqlcfunc.cpp`), and for the same
    reason: that path also loads a widened `Temp` before converting.

  Net: a `tinyint` above 127 round-trips in both directions instead of failing
  `22003`, while `SQL_C_STINYINT` and every wider target keep the signed range
  check.
- Value failures travel as `ParamBuildError::Value(ConvError)`, so both
  directions map `OutOfRange` to the same `22003`.

Deviation: no identity fast path. msodbcsql has `IsParamConversionNeeded` to skip
a buffer-to-buffer copy when the types already agree; this driver always decodes
to a canonical `i128`, so there is no copy to skip.

Signedness, the `22003` state and its message text, and the unaligned reads are
all verified against msodbcsql source.

#### P4 - Character C to character SQL (code complete)

Twelve pairings: `SQL_C_CHAR` / `SQL_C_WCHAR` against each of `SQL_CHAR`,
`SQL_VARCHAR`, `SQL_LONGVARCHAR`, `SQL_WCHAR`, `SQL_WVARCHAR`,
`SQL_WLONGVARCHAR`. The six same-family ones were already in the matrix; P4 adds
the six cross-family ones, which transcode UTF-8 <-> UTF-16. `varchar(max)` and
`nvarchar(max)` are `SQL_VARCHAR` / `SQL_WVARCHAR` with
`ColumnSize == SQL_PREC_UNLIMITED`, which `variable_length` folds into the `Max`
variants. The declared type now comes from `ParameterType` + `ColumnSize` for
values, not just typed NULLs, reusing the `SQL_PREC_*` constants; the duplicate
`MAX_NARROW_LENGTH` / `MAX_WIDE_LENGTH` are gone.

**Truncation** (`sqlcfunc.cpp:2854`, the `fCType == SQL_C_WCHAR || SQL_C_CHAR`
arm of `ParamToSQLType`) raises `22001` only when the overflow holds a non-pad
character; an overflow of trailing blanks is trimmed silently
(`CheckTrailingChars` / `CheckTrailingWChars`, `:2957`). The checks at `:2630` /
`:2653` are the `SQL_C_BINARY` arm, which pads with `'0'`.

**The length is measured in msodbcsql's units, which approximate** (AB#47584).
The exact count is unknowable at conversion time - `varchar(n)` bounds
*collation* bytes, applied downstream by `serialize_string`. Every source is
therefore measured in the UTF-16 units it holds or would produce (`cchDest =
cbData/sizeof(WCHAR)`, *"Assumption: 1 WCHAR converts to 1 byte"*, `:2946`),
whichever family it lands in. Counting transcoded UTF-8 would falsely reject
values that fit: under a single-byte collation "cafe" with an acute accent is
four collation bytes, not five.

The unit is deliberately the same for both character C types, and this is where
P4 parts company with msodbcsql. Three of its four arms already count UTF-16
units - both wide-source arms, and the narrow-to-wide walk, which even counts an
astral character as two (`:2935`). Only narrow-to-narrow counts source bytes
(`cchDest = cbData`, `:2952`).

That byte count is the wire length only while no client-side transcode happens,
which is the ordinary case: TDS carries a collation with char data, so the bytes
ship under a declared collation and the *server* converts.
`DoCharToCharConversion` (`sqlcprot.h:4113`) enables the client-side conversion
only for an encoding TDS cannot name - a UTF-8 client against a non-UTF-8 server,
or the ISO-8859-x range - and translation is on by default (`SQL_XL_DEFAULT`).

A UTF-8 `SQL_C_CHAR` is this driver's permanent state, so that predicate would
always hold here. In that configuration msodbcsql transcodes but still measures
the *pre-transcode* UTF-8 bytes, rejecting "cafe" with an acute accent from a
`varchar(4)` that the four bytes it actually sends would fit. Copying the byte
rule reproduced that defect and left the two C types disagreeing on one value -
the same string was accepted as `SQL_C_WCHAR` and `22001` as `SQL_C_CHAR`. So it
is not replicated, on the same footing as the narrow-to-wide off-by-one below.
Holding both C types to UTF-16 units is what makes them agree; `char` counts
would not, since an astral character is one `char` but two units.

The residual error now runs one way only: the count errs low, never high. On a
collation whose bytes outnumber the units counted - reachable on any `_UTF8`
collation, not just a DBCS one - an over-long value passes here and
`serialize_char_varchar_direct` rejects it as a `UsageError`, surfacing `HY000`
rather than the `22001` the application should see. The `max` types and
`text`/`ntext` carry no such check at all and send the over-long value, which is
the worse outcome of the two. Routing either to `ERR_PARAM_STRING_TRUNCATION`
needs a typed error out of `mssql-tds` - matching on the message text would be
guesswork - so it belongs with AB#47584 rather than as a local patch.

**This is a behavioural regression for a subset of inputs, not a pure
improvement.** `SQL_C_CHAR` `"[three U+2615]"` into `varchar(3)` was a correct
`22001` under the byte count; it is now accepted as three units and fails
downstream as an opaque `HY000` - or, under a single-byte collation, each
character is unmappable and becomes a seven-byte numeric character reference
(`&#9749;`, AB#47598), so 21 bytes are offered to a `varchar(3)`. CJK and astral
input bound with an exact character count is the shape that regresses. The trade
was taken because over-rejection has no application workaround - the byte count
is encoding-dependent and the application cannot know it - while under-rejection
still errors, and because byte-counting *both* C types would have broken the
wide arm, the one msodbcsql gets right. No option preserved both parity and
self-consistency.

Verified against msodbcsql source:

- `text` / `ntext` carry no declared length but are still bounded by
  `ColumnSize`: msodbcsql applies the bound whenever `!fIsVarMax` (`:2898`), and
  `fIsVarMax` needs `cbColDef == 0` *and* a varmax type or `longToMax` (`:2577`),
  which no `SQL_LONGVARCHAR` binding satisfies.
- `SQL_LONGVARCHAR` / `SQL_WLONGVARCHAR` are sent as `varchar(max)` /
  `nvarchar(max)`, still bounded by `ColumnSize`. msodbcsql declares `text` /
  `ntext` by default - it sends `max` only under `SQL_COPT_SS_LONGASMAX`
  (`sqlcprot.h:1838`), which defaults off (`sqlcconn.cpp:84`, `:4686`) - so this
  matches its `LongAsMax` mode rather than its default. `text` / `ntext` cannot
  be declared until `mssql-tds` stops serializing them in bulk-copy ROW format:
  an RPC parameter currently carries a textptr/timestamp header and a stray
  table-name byte, and the server answers 4002 (AB#47591). Restoring the true
  declaration, with `SQL_COPT_SS_LONGASMAX` as the opt-in fallback, is AB#47592.
  The attribute itself is unimplemented, so the long types currently get its
  *result* unconditionally with no way to ask for either behaviour.
- **Blank padding is left to the server.** msodbcsql pads client-side when
  `SQL_COPT_SS_ANSI_NPW` is on and the value is shorter than the declared length
  (`sqlcmisc.cpp:7472`, gated on `SQLBIGCHAR` / `SQLBIGBINARY` / `SQLNCHAR`).
  This driver sends the actual length and lets the server pad, which
  `DeclaredLengthReachesTheServer` shows is observationally equivalent -
  `DATALENGTH` is 8 for a `char(8)` carrying three characters on both drivers.
  The attribute is unimplemented.
- **`ColumnSize` is a character count and msodbcsql agrees** - no divergence.
  `SQLBindParameter` converts it to an internal byte count for the wide types
  (`sqlcdesc.cpp:3311`, `cbColDef *= sizeof(WCHAR)`); every `/ sizeof(WCHAR)`
  elsewhere undoes that. An earlier revision of this plan called it a deliberate
  deviation - a misreading.
- **`SQL_C_CHAR` is UTF-8 deliberately**, because the only supported consumer,
  mssql-python, is UTF-8 native. msodbcsql reads the client code page
  (`sqlcprot.h:2830`, `Localization.hpp:742`, `LocalizationImpl.hpp:386`,
  consulted at `sqlcfunc.cpp:2913`), so the two agree on a UTF-8 locale and
  differ on a default Windows one. The spec fixes no encoding for `SQL_C_CHAR`.
  Code page support is AB#47565 (parameters) / AB#47564 (fetch); the
  server-collation axis is already handled by `serialize_string`.
- Malformed UTF-8 stays lossy - there is no msodbcsql behaviour to copy, since
  its conversion goes through `SystemLocale::FromUtf16` (`sqlccmd.cpp:10952`),
  which is not in this source tree. `22018` is tracked with AB#47565.
- Still `HYC00`: `SQL_SS_XML`. `DescribesMaxLengthParameters` is re-enabled with
  the binary types; the decimal case parked beside it still waits on AB#47500.

Deferred:

- **The 2GB ceiling on `max` types is enforced nowhere, here or in msodbcsql.**
  Conversion skips length checks for varmax (`:2862`), bind bounds the declared
  `ColumnSize` rather than the data, and `CRPCPolicy::WriteChunk`
  (`tds/tdsrpc.cpp:275`) reinterprets the low four bytes of a `SIZE_T` as the
  chunk length then writes the full count. `serialize_string` has the same wrap
  as an `as u32`, so a value at or past 4GB emits a header disagreeing with its
  payload. Reaching it needs one bound parameter that large.
- Narrow-text copies (AB#47576). `SQL_C_CHAR` -> `varchar` costs five: buffer
  read, `String::from_utf8`, `SqlString::from_utf8_string` transcoding to UTF-16
  at double size, `ColumnValues::String(v.clone())`, then `serialize_string`
  decoding back to UTF-8 to encode to the collation code page. Tagging the bytes
  `EncodingType::Utf8` removes two, but `sql_string.rs` carries an unresolved
  TODO claiming UTF-8 decode "is weirdly encoded", so this needs e2e evidence.
- P6: msodbcsql's narrow -> wide arm checks the running count *before*
  incrementing (`:2928`), so exactly one character of overflow escapes: retail
  18.05.0002 silently truncates `"abcd"` to `"abc"` in a `wvarchar(3)` and
  returns `SQL_SUCCESS` with no diagnostic. Two characters over is rejected, and
  the narrow -> narrow control rejects at one. The declaration is *not* widened -
  `MaxLength` reports the declared `nvarchar(3)`. Debug 18.06.0002 aborts on the
  assert at `sqlcmisc.cpp:7458` instead. `NarrowToWideOverlongParamIs22001` is
  skipped under `--compare-with-msodbcsql` for that reason;
  `NarrowToWideOverflowingBlanksAreTrimmed` is *not* a divergence on retail
  18.05.0002 - msodbcsql matches us - and its skip is retained only until the
  case is measured against the pinned retail 18.6.2.1 that CI compares against.
- P6: bound the 32-bit wire length fields in `mssql-tds`. Every
  `write_u32_async(len as u32)` in `tds_value_serializer.rs` narrows silently -
  the PLP chunk headers and the legacy `TEXT` / `NTEXT` / `IMAGE` totals. The
  guard belongs at the top of each `serialize_*`, before any header byte reaches
  the writer: that layer alone knows the encoded byte count, and failing after
  the header is emitted leaves a half-written RPC that can cost the connection.

Highest-risk phase - everything was `(n)varchar(max)` before, so declared lengths
had never applied. The e2e suite now passes under `--compare-with-msodbcsql`,
which confirms the server-side declaration assertions (`DATALENGTH`,
`SQL_VARIANT_PROPERTY`) hold on both drivers.

#### P5 - Cross conversions (code complete)

Both quadrants are adapters onto an existing converter rather than new
conversions, so each target keeps its own rules.

- Integer C to character SQL: format base 10, then hand the digits to the P4
  character converter, which applies the declared length. An over-long value is
  `22001` - digits are never blanks, so the trailing-blank exemption cannot
  absorb them. **Applying a length here at all is a divergence:** msodbcsql
  length-checks no integer C type (`sqlcfunc.cpp:2586`, `:2854`, `:3165`,
  `:3177`), and what it does instead is undefined per build - retail 18.05.0002
  silently truncates to `ColumnSize` with no diagnostic, debug 18.06.0002 aborts
  on `assert(*pstMaxLen >= stLen)` (`sqlcmisc.cpp:7458`), and retail 18.6.2.1
  hangs in `SQLExecute`. The fallthrough at `:7459` reads as widening the
  declaration; no measured build does that - measure, do not derive.
  `IntegerParamTooWideForColumnSizeIs22001` and
  `NegativeSignCountsAgainstColumnSize` carry `SKIP_IF_COMPARING_MSODBCSQL()`
  for it.
- Character C to integer SQL: parse, then hand the value to the P3 integer
  converter, which narrows. `"12"` is exact and an overflow is `22003`.
  Parsing is `numeric::parse_numeric_text`, **shared with the fetch direction**,
  because msodbcsql shares it too: `Convert` routes a character source to
  `ConvertToFixed`'s `case SQL_C_CHAR` arm whichever way the data moves, and
  `SQL_C_CHAR` and `SQL_CHAR` are both `1`, so a character *column* and a
  character *application buffer* are indistinguishable there. Extracting it
  tightened fetch to the same blanks-only padding rule (`CharToBigint`,
  `sqlccnvt.cpp:7777`); fetch previously inherited `str::trim` from
  `parse_decimal_literal` and accepted `"\t12"`, which msodbcsql rejects.
  Severity stays per-direction, as it does in msodbcsql: a dropped fraction is
  `01S07` outbound and `22001` inbound.

Two diagnostics differ from the plan this section originally carried, both
settled from the msodbcsql source rather than from the ODBC spec:

- **A dropped fraction is `22001`, not `01S07`.** Inbound severity is set by the
  parameter direction: `ParamToSQLType` rewrites `IDS_01_S07` to `IDS_22_001`
  for any non-2.x application (`sqlcfunc.cpp:3348`), so no warning channel is
  needed for this quadrant after all. Only a *non-zero* dropped digit counts -
  `if (c != '0') Error = CVT_FRACT_TRUNC` (`sqlccnvt.cpp:7823`) - so `"12.0"`
  converts cleanly.
- **`22003` outranks `22001`** when a value both overflows the target and
  carries a fraction, because the narrowing runs before that rewrite can fire.

`"abc"` is `22018` here, and msodbcsql agrees - this is parity, not a deviation.
`CharToBigint` returns `CVT_ERROR` = `IDS_22_005`, which reaches the `std_error`
branch of `SQL_DIAG_SQLSTATE` (`sqlcerr.cpp:990`) and resolves through the
driver-generated-error map (`cli_common/src/clntcomn.cpp:1015`,
`IDS_22_005 -> L"2200522018"`); a 3.x application takes the `22018` half. The
server-keyed table at `sqlcstr.cpp:136` is a different map and never applies
here, so the `lNative` gate at `sqlcerr.cpp:1377` is not on this path.

One input escapes that: a `SQL_C_WCHAR` buffer of nothing but blanks, where
retail 18.05.0002 answers `HY000`. The same blanks as `SQL_C_CHAR` and a
zero-length wide buffer both give `22018`, so the divergence is that one pairing
and nothing wider. Registered as a deviation and pinned by
`BlankOnlyWideLiteralIs22018`; the rest of `CharParamInvalidLiteralIs22018` runs
on the compare leg.

#### P6 - Parity and e2e hardening (not started)

- Parameter-numbered diagnostics.
- A serialization failure after a packet has already flushed leaves the request
  half-sent, and the server answers 4002 on the *next* command. Declaring real
  lengths is what made it reachable, so P4 exposed it rather than caused it.
  The retraction and its two e2e cases are tracked by AB#47687.
- Run the e2e suite under `--compare-with-msodbcsql`; mark driver-specific
  assertions with `SKIP_IF_COMPARING_MSODBCSQL()`.
- Add `Benefits-from-mock-tds:` notes where only the round-tripped value is
  observable and the declared RPC type is not.
- Settle one open `HY009` question: msodbcsql treats a null data pointer with a
  zero count as `SQL_NULL_DATA` on the `SQLPutData` path (`sqlccmd.cpp:4494`).
  If that convention also holds for a bound buffer, `SQL_C_CHAR` with a null
  pointer and `*ind == 0` is NULL there and `HY009` here. Not settleable from
  the source; needs a `--compare-with-msodbcsql` run on that exact binding.
- **`DecimalDigits` is validated at execute, not at bind.** `CheckSqlPrecScale`
  lives up to its name: `SQLBindParameter` runs it (`sqlcdesc.cpp:3038`) and it
  rejects a bad scale there, not later. `SQL_NUMERIC` / `SQL_DECIMAL` reject
  scale > precision (`sqlcdesc.cpp:11529`), and the temporal types run
  `CheckSqlScale` against `SCALE_DATETIME2` / `SCALE_TIME` /
  `SCALE_DATETIMEOFFSET`, all 7 (`tds.h:273-278`). We apply the same rules with
  the same `HY104` in `decimal_metadata` / `datetime_metadata`, only later -
  the same divergence already accepted for `ColumnSize` in `fixed_length`.
  Closing it means `parameter_column_size_is_valid` grows a scale argument, and
  `MAX_DATETIME_SCALE` moves to `type_rules.rs` with it.
- **`SQL_TYPE_DATE` accepts any `DecimalDigits`.** msodbcsql requires
  `SCALE_DATE == 0` (`sqlcdesc.cpp:11641`); `typed_null` maps the type with no
  metadata and never looks at the scale.
- **No `SQL_TIMESTAMP` precision/scale correlation check.** msodbcsql requires
  `ColumnSize == 19` for scale 0 and `20 + scale` otherwise - but it *repairs*
  rather than rejects, tracing "invalid precision or scale value has been
  corrected" and calling `FixupColumnSizeDecimalDigits` (`sqlcdesc.cpp:11571`);
  its own comment says full validation would break back-compat. We neither
  check nor repair, so a mismatched pair reaches the wire with a different
  declaration than msodbcsql would send.

#### P7 - Cleanup and hooks (not started)

- Remove remaining "Phase 1" language from `conversion/param_convert.rs` and
  `params/bound_param.rs`.
- Record the deferred blockers: `SqlType` metadata/value separation for decimal
  and temporal typed NULLs, and the hard-coded decimal precision/scale in
  `mssql-tds/src/datatypes/sqltypes.rs`.

### Shared with fetch

| Shared | Not shared |
| --- | --- |
| Outcome and error vocabulary | Pointer I/O (`write_fixed` vs the buffer reader) |
| Canonical numeric value, narrowing, overflow guards | Source model (`ColumnValues` vs `AppValue`) |
| Numeric and temporal literal parsers | Chunking, PLP streaming, cursor state |
| SQLSTATE mapping helpers | Direction-specific truncation severity |

Fetch is not retrofitted onto the conversion matrix, and should not be:
msodbcsql does not route its fetch path through `fValidConversion` either,
because `SQLBindCol` cannot know a column's SQL type at bind time. The
`is_*_c_target` helpers in `fetch_convert.rs` are converter routing - the same
role `Convert()`'s dispatch switch plays - not a legality table.

## Remaining work

- **Stream marker rewriting without an intermediate SQL string.** `SQLPrepare`
  already scans and rewrites once. A future allocation optimization could store
  the original SQL plus `Vec<usize>` marker offsets, then stream SQL chunks and
  `@P{n}` names directly to the TDS writer. Execute-time binding state would
  also allow `OUTPUT` and `?=` handling. This is no longer a repeated-parsing
  correctness issue.
- **Type matrix and TDS type selection:** tracked by the conversion milestone
  above. P3-P5 drive the wire type from `ParameterType` for the integer and
  character types, in both directions across the two families, and AB#47688 does
  the same for the binary types. Beyond this milestone the same work is needed
  for `uniqueidentifier`, money, decimal, and date/time values, which still emit
  their P0-era shapes. `ColumnSize` still does not bound a data-at-execution
  value in either family (AB#47590).
- **Deferred features:** output parameters (`SQL_PARAM_OUTPUT`, `SQL_PARAM_INPUT_OUTPUT`),
  parameter arrays (`SQL_ATTR_PARAMSET_SIZE`), and TVPs.
- **Data-at-exec follow-ups:** `SQLParamData` / `SQLPutData` are implemented for
  both `SQLPrepare` + `SQLExecute` and `SQLExecDirect` (see the
  delivered-features list above and `data-at-execution-streaming.md`), and a
  streamed execute keeps the statement prepared rather than falling back to
  ad-hoc `sp_executesql`. A sequence that fails on the wire loses the request
  and the socket, since a request interrupted mid-send cannot be retracted with
  `EOM | IGNORE` the way a cancelled or driver-rejected one is; the session is
  recovered lazily by the next execute via `check_and_reconnect`, matching
  msodbcsql's `GetBatchCtxOrRecover`.
- **Canonical procedure calls / `sp_prepexecrpc`:** support ODBC canonical
  calls (`{call proc(?)}`) with the appropriate parameter-count and single-row
  parameter-set guards. Ad-hoc T-SQL currently uses `sp_prepexec`.
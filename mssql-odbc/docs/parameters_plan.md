# Parameterized execution - `SQLBindParameter` / `SQLExecute` / `SQLExecDirect`

Status, behavior, and known gaps for parameterized prepared-statement execution
in the ODBC Driver 18 (Rust). Updated 2026-08-18.

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
  `sp_prepexec` on which to piggyback.
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
- **Bind-time type validation** - `api::type_rules` owns the `HY003` gate (every
  real ODBC C type, including the SQL Server extensions) and classifies SQL data
  types three ways, like msodbcsql's `IsValidSqlType`: supported, real but with
  no SQL Server counterpart (`HYC00` - the interval types), or unknown
  (`HY004`). `params::conversion_matrix` owns the C -> SQL conversion table,
  shaped like msodbcsql's `fValidConversion` (one row per C type). A C type that
  is real but not yet convertible returns `07006`, not `HY003`.
- **`SQL_C_DEFAULT` resolution** - resolved at bind time to the C type implied
  by `ParameterType` (msodbcsql `Sql2CDefault`, which reads `rgbTRANSTYPE380`
  for ODBC 3.8+ applications) and stored resolved in `BoundParam`, so the
  execute path never sees the placeholder.
- **Value conversion** - `SQL_C_CHAR` maps to varchar and `SQL_C_WCHAR` to
  nvarchar. Indicators support `SQL_NULL_DATA`, `SQL_NTS`, and explicit byte
  length.

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
with the fetch path ([`api/fetch_convert.rs`](../src/api/fetch_convert.rs)).

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
| **character C** | D: parse (`22018`, `22003`, `01S07`) | B: transcode and length (`22001`) |

Out of scope for this milestone: decimal/numeric, money, temporal, GUID, binary,
output parameters, data-at-exec, parameter arrays, and TVPs.

### Design rules

- **The matrix lists only implemented pairs.** A pairing accepted at bind time is
  always one the execute path can convert, so there is no bind-succeeds /
  execute-fails window. Rows and entries are added as each phase lands.
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
| P0 | [47364](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47364) | Not started | Extract shared conversion core from `fetch_convert.rs` |
| P1 | [47365](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47365) | Code complete, unmerged | Parameter type model, conversion matrix, `SQL_C_DEFAULT` |
| P2 | [47366](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47366) | Not started | Safe C-buffer reader and conversion-outcome channel |
| P3 | [47367](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47367) | Not started | Quadrant A: integer C -> integer SQL |
| P4 | [47368](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47368) | Not started | Quadrant B: character C -> character SQL |
| P5 | [47369](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47369) | Not started | Quadrants C and D: cross conversions |
| P6 | [47370](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47370) | Not started | Parity and e2e hardening |
| P7 | [47371](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47371) | Not started | Cleanup and follow-up hooks |

P1 is independent of P0; P3 onward depend on both.

#### P0 - Extract shared conversion core (not started)

Pure refactor, no behavior change. Create `src/conversion/`:

- `error.rs` - direction-aware outcome vocabulary lifted from `fetch_convert.rs`:
  a `ConvDirection`, an outcome (`Exact`, `FractionalTruncation`,
  `StringTruncation`), and an error kind (`OutOfRange`, `InvalidCharacterValue`,
  `Restricted`, `StringTruncation`). `NotHandledHere` stays private to fetch
  dispatch and must never reach an application.
- `numeric.rs` - move `NumericSource` (exact `Int` / `Scaled` / `Float` model),
  `parse_decimal_literal`, `to_i128_truncating`, and the checked-narrowing
  helper. This carries the hardening from the 128-bit shift-overflow guard and
  the `22003` unrepresentable-value fix into the parameter path.
- Add `SQLSTATE_22001` and an inbound string-truncation `DiagMsg`;
  `ERR_STRING_RIGHT_TRUNCATION` (`01004`) stays for the outbound path.

#### P1 - Parameter type model and conversion matrix (code complete, unmerged)

- [`api/type_rules.rs`](../src/api/type_rules.rs) - the `HY003` / `HY004`
  identifier gates and `SQL_C_DEFAULT` resolution. Direction-neutral, so it sits
  in `api` rather than `params`.
- [`params/conversion_matrix.rs`](../src/params/conversion_matrix.rs) - one row
  per C type listing the SQL types it converts to. Rows today: `SQL_C_CHAR` ->
  `CHAR` / `VARCHAR` / `LONGVARCHAR`, `SQL_C_WCHAR` -> `WCHAR` / `WVARCHAR` /
  `WLONGVARCHAR`.
- [`api/bind_param.rs`](../src/api/bind_param.rs) - runs both checks and stores
  the resolved C type on the binding.

No value conversion changed here: `SQL_C_SLONG` + `SQL_INTEGER` still fails at
bind until P3 adds its row.

Deviations from msodbcsql, verified against source:

- The driver is ODBC 3.x only, so the 2.x date/time SQL spellings (`SQL_DATE` /
  `SQL_TIME` / `SQL_TIMESTAMP`, 9-11) are not accepted; the DM maps them to the
  `SQL_TYPE_*` forms for a 3.x driver. msodbcsql accepts both because its
  internal representation is the 2.x form.
- `SQL_C_DEFAULT` resolves the wide character types to `SQL_C_WCHAR` and
  `SQL_GUID` to `SQL_C_GUID`, following the ODBC 3.x default-C-type table.
  msodbcsql's `rgbTRANSTYPE380` resolves both to `SQL_C_CHAR`, an ANSI-transfer
  artifact with no equivalent here; resolving UTF-16 input to this driver's
  UTF-8 `SQL_C_CHAR` would silently corrupt data. Accepted deviation, also
  registered in
  [`mssql-odbc.instructions.md`](../../.github/instructions/mssql-odbc.instructions.md)
  and on AB#47365.
- msodbcsql normalizes ODBC 3.x date/time identifiers to their 2.x values and
  `SQL_DOUBLE` to `SQL_FLOAT` before validating; this driver accepts both
  spellings directly. Equivalent today, but the normalization will be needed
  when descriptors land.

#### P2 - Buffer reader and outcome channel (not started)

- `params/buffer.rs` as the single audited unsafe input site: `read_unaligned`
  for fixed-width C integers, `SQL_NTS` / explicit length / `buffer_length` for
  character buffers.
- Fix indicator handling: for fixed-width C types `StrLen_or_Ind` is not a
  length. Only `SQL_NULL_DATA`, `SQL_DEFAULT_PARAM`, and data-at-exec values are
  meaningful; size comes from the C type (msodbcsql `IsFixedCType` /
  `GetLengthForFixedLengthCType`). Current code treats the indicator as a length
  for every type.
- Produce an `AppValue` (`Null`, `Integer`, `Text`) for the milestone subset.
- Thread a warning channel through `build_named_params` so `SQLExecute` and
  `SQLExecDirect` can return `SQL_SUCCESS_WITH_INFO`; it returns only
  `Result<Vec<RpcParameter>, SqlReturn>` today. Landing it here avoids a late
  signature change in P5.

#### P3 - Integer C to integer SQL (not started)

- Identity fast path for exact pairs (msodbcsql `IsParamConversionNeeded`).
- Cross-width narrowing through the shared numeric model, `22003` on overflow.
- Emit `SqlType::TinyInt` / `SmallInt` / `Int` / `BigInt` driven by
  `ParameterType`, not the C type, including typed NULL. This is the first phase
  where `@P1` is declared `int` instead of `nvarchar(max)`.
- `SQL_C_TINYINT` is unsigned, matching the documented fetch decision.
- `SQL_C_UBIGINT` above `i64::MAX` has no SQL Server target: `22003`.

#### P4 - Character C to character SQL (not started)

- Same-family and cross-family (`SQL_C_CHAR` -> `SQL_WVARCHAR` and the reverse),
  transcoding UTF-8 <-> UTF-16.
- Use `ColumnSize` to select `Varchar(_, n)` / `NVarchar(_, n)` / `Char` /
  `NChar` versus the `Max` variants, applying the existing 8000 / 4000
  thresholds.
- Inbound truncation beyond the declared length is `22001`, not the benign
  outbound `01004`.
- Highest-risk phase: everything is `(n)varchar(max)` today, so declared lengths
  have never applied. Verify against msodbcsql: `ColumnSize == 0`, and data
  longer than `ColumnSize`.
- Keep the documented UTF-8 `SQL_C_CHAR` divergence; do not add ANSI codepage
  translation.

#### P5 - Cross conversions (not started)

- Integer C to character SQL: format, then apply P4 length rules.
- Character C to integer SQL: reuse `parse_decimal_literal`. `"12"` is exact,
  `"12.7"` yields 12 with `01S07`, `"abc"` is `22018`, and an overflow is
  `22003`.

#### P6 - Parity and e2e hardening (not started)

- Parameter-numbered diagnostics.
- Run the e2e suite under `--compare-with-msodbcsql`; mark driver-specific
  assertions with `SKIP_IF_COMPARING_MSODBCSQL()`.
- Add `Benefits-from-mock-tds:` notes where only the round-tripped value is
  observable and the declared RPC type is not.

#### P7 - Cleanup and hooks (not started)

- Remove remaining "Phase 1" language from `params/convert.rs` and
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

One divergence to watch: this driver's matrix answers "implemented?" while
msodbcsql's answers "legal?". Both directions currently hold legality knowledge
in different shapes, so when P5 adds character/numeric cross conversions, check
that the two agree on what is legal versus merely unimplemented rather than
accepting a pairing inbound that is rejected outbound for no principled reason.

## Remaining work

- **Stream marker rewriting without an intermediate SQL string.** `SQLPrepare`
  already scans and rewrites once. A future allocation optimization could store
  the original SQL plus `Vec<usize>` marker offsets, then stream SQL chunks and
  `@P{n}` names directly to the TDS writer. Execute-time binding state would
  also allow `OUTPUT` and `?=` handling. This is no longer a repeated-parsing
  correctness issue.
- **Type matrix and TDS type selection:** tracked by the conversion milestone
  above. `params::convert` still ignores `sql_type` and emits `(n)varchar(max)`,
  relying on SQL Server implicit conversion; P3 and P4 drive the wire type from
  `ParameterType` instead. Beyond this milestone the same work is needed for
  binary, `uniqueidentifier`, money, decimal, and date/time values.
- **Deferred features:** output parameters (`SQL_PARAM_OUTPUT`, `SQL_PARAM_INPUT_OUTPUT`), data-at-exec
  (`SQLParamData` / `SQLPutData`), parameter arrays
  (`SQL_ATTR_PARAMSET_SIZE`), and TVPs. Data-at-exec requires an
  `sp_prepare` + `sp_execute` branch because `sp_prepexec` cannot carry streamed
  values.
- **Canonical procedure calls / `sp_prepexecrpc`:** support ODBC canonical
  calls (`{call proc(?)}`) with the appropriate parameter-count and single-row
  parameter-set guards. Ad-hoc T-SQL currently uses `sp_prepexec`.
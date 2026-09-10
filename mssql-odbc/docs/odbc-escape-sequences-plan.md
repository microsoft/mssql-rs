# mssql-odbc — ODBC escape sequences (AB#46384) — implementation plan

Status: **plan agreed, implementation in progress.**

Written from the work items, the msodbcsql reference source, `mssql-rs` `main` at `0d431b71`, and
live probes against SQL Server 2025 and msodbcsql 18.

Confidence markers: **[measured]** = observed against a live server or a live msodbcsql,
**[source]** = read in msodbcsql or mssql-rs source, **[inferred]** = reasoning, not verified.

### Measurement baseline

Every **[measured]** value below was taken against:

- **msodbcsql 18.6.2.1** (`SQL_DRIVER_VER` `18.06.0002`), which is the version CI pins for the
  parity comparison — `msodbcsqlVersion: '18.6.2.1'` in `.pipeline/validation-pipeline.yml:44`,
  consumed by `.pipeline/scripts/install-msodbcsql.sh`.
- **SQL Server 2025** (`SQL_DBMS_VER` `17.00.4006`), `mcr.microsoft.com/mssql/server:2025-latest`.

The escape tables in §2.2 and §3.5 were first taken on retail 18.6.1.1 (`18.06.0001`) and then
re-run byte-for-byte identically on the pinned 18.6.2.1, so the golden values do not depend on the
point build. Anything added later must be re-measured on the pinned version, not on whatever the
developer box happens to have.

---

## 1. Work items

| Item | Type | Notes |
| --- | --- | --- |
| AB#42845 | Feature | mssql-rs \| Build mssql-odbc the ODBC driver on Rust — for mssql-python |
| **AB#46384** | User Story | **mssql-odbc \| ODBC escape sequences** |
| AB#48049 | Task (child) | Output parameters, return values, `SQLNumParams` — **folded into the same PR** |
| AB#47086 | Related | `SQLGetInfo` subset — **merged** as #535 (`65a535bf`). This branch is rebased on it and finishes the escape-related info types it deliberately left at zero (§7) |
| AB#41343 | Related (mssql-tds) | Evaluate OUTPUT parameters in the `sp_executesql` API — §5.7 does part of this |

The story text says SQL Server "does not parse [escape sequences] natively" and that an untranslated
one "will generally cause a syntax error". That premise is wrong for SQL Server, and correcting it
is what makes this tractable as one PR. See §2.

---

## 2. Key finding: SQL Server parses almost every ODBC escape natively

### 2.1 Measured against the server

Each escape was wrapped in `EXEC('<literal>')` so the text reached the server's parser **without**
msodbcsql's scanner touching it (the driver skips string literals) **[measured]**:

| Escape | Parsed by the server itself |
| --- | --- |
| `{fn UCASE}`, `{fn CONCAT}`, `{fn CURDATE}`, `{fn TIMESTAMPADD}`, `{fn TIMESTAMPDIFF}`, `{fn CONVERT}` | yes |
| `{d '…'}`, `{t '…'}`, `{ts '…'}` | yes |
| `{oj a LEFT OUTER JOIN b ON …}` | yes |
| `… LIKE 'a\_b' {escape '\'}` | yes |
| `{guid '…'}` | yes |
| `{interval '1' DAY}` | **no** — rejected |

### 2.2 Measured against msodbcsql

`SQLNativeSqlW` through the unixODBC driver manager **[measured, 18.6.2.1]** — this is the parity
contract the new code must reproduce:

| Input | msodbcsql output |
| --- | --- |
| `SELECT {fn UCASE('abc')}` | `SELECT {fn UCASE('abc')}` |
| `SELECT {d '2020-01-02'}` | `SELECT {d '2020-01-02'}` |
| `SELECT {ts '2020-01-02 13:14:15'}` | `SELECT {ts '2020-01-02 13:14:15'}` |
| `SELECT {guid '6F96…'}` | `SELECT {guid '6F96…'}` |
| `SELECT {fn CONVERT(123, SQL_VARCHAR)}` | unchanged |
| `SELECT 1 /* {fn UCASE('x')} */ , '{d ''2020-01-02''}'` | unchanged — comments and literals are not scanned |
| `… LIKE 'a\_b' {escape '\'}` | `… LIKE 'a\_b'  ESCAPE '\' ` |
| `SELECT {encrypt N'abc'}` | `SELECT  0xB3A583A593A5 ` |
| `SELECT {interval '1' DAY}` | `SELECT  'INTERVAL +''1'' DAY(2)' ` |
| `{call sp_who}` | `  EXEC sp_who  ` |
| `{? = call sp_who(?)}` | `  EXEC ?=sp_who ?  ` |
| `{call dbo.myproc(?, DEFAULT, {ts '2020-01-02 13:14:15'})}` | `  EXEC dbo.myproc ?,DEFAULT,{ts '2020-01-02 13:14:15'}  ` |
| `SELECT {bogus 1}` | error, SQLSTATE `42000`, "Syntax error, permission violation, or other nonspecific error" |

Two things to note, because they constrain the design:

1. A translated escape is replaced by `" " + text + " "`, so an extra space appears on each side.
2. **`?` markers are preserved verbatim.** `SQLNativeSql` translates escapes and nothing else — it
   does not rewrite markers to `@Pn`. That is why translation and marker rewriting have to be
   separate phases (§5.1).

`SQLNativeSql` always uses the **textual** `EXEC` form for `{call}` because it has no statement
handle; on a real statement the RPC path is taken instead.

### 2.3 Confirmed in the reference source

msodbcsql passes those escapes straight through **[source]**:

- `SubstituteECodes` — `sqlcmisc.cpp:4581`, the scan/translate loop.
- `ProcessDTI` — `sqlcmisc.cpp:7892`: `if (wType != ECODE_INTERVAL) { *pfPassthru = TRUE; return 0; }`,
  commented "Sphinx has canonical datetime support". `{d}` / `{t}` / `{ts}` are **not** translated.
- `ECODE_FUNCTION` (`:4715`), `ECODE_OUTERJOIN` (`:4757`), `ECODE_GUID` (`:4780`) → `fPassthru = TRUE`.
- Pass-through works by temporarily rewriting `{` / `}` to `STX` / `ETX` and restoring them before
  send (`:4820-4905`), i.e. the braces genuinely go on the wire.

Only four constructs are actually transformed:

| Escape | Transformation | Reference |
| --- | --- | --- |
| `{escape 'c'}` | strip braces → `ESCAPE 'c'` | `ProcessEscape`, `sqlcmisc.cpp:8654` |
| `{interval …}` | emit a T-SQL string literal (§5.3) | `ProcessDTI`, `sqlcmisc.cpp:7892` |
| `{encrypt N'…'}` | obfuscate the literal to `0x…` (§5.4) | `ProcessEncrypt`, `sqlcmisc.cpp:8673` |
| `{[?=]call proc(…)}` | TDS RPC, or textual `EXEC proc …` (§5.5) | `ProcessCanonicalCall`, `sqlcmisc.cpp:7952` |

### 2.4 Consequence for scope

There is no `{fn}` scalar-function map to build and no `{d}` / `{ts}` literal rewriting. The work is:
a correct lexer; four real translations; `{call}` → RPC with output parameters and return status;
and the APIs that expose all of it (`SQLNativeSql`, `SQLNumParams`, `SQL_ATTR_NOSCAN`,
`SQLDescribeParam`).

---

## 3. Reference implementation walkthrough (msodbcsql)

All paths under `Sql/Ntdbms/sqlncli/odbc/` in the msodbcsql source.

### 3.1 Entry and gating

- `DoSubstitutions` (`sqlcmisc.cpp:4542`) reads `SQL_NOSCAN` from the statement and calls
  `SubstituteECodes` only when it is `SQL_NOSCAN_OFF`. With no statement handle it **forces scanning
  on** — so `SQLNativeSql` and `AutoFillIPD` always translate.
- `SQLNativeSqlW` (`sqlccmd.cpp:6943`) is a thin wrapper around `DoSubstitutions(NULL, …)`.
- Metadata paths temporarily force `SQL_NOSCAN_OFF` so canonical calls still parse
  (`sqlccmd.cpp:2042-2057`).

### 3.2 Lexing

- `GetLexToken` (`sqlctokn.cpp:88`): skips whitespace **and comments**, then recognises numbers,
  `0x` binary literals, dotted/qualified identifiers, `'…'` / `"…"` strings, and single characters.
- `FindECode` (`sqlcmisc.cpp:4962`): finds the next escape, resolves brace nesting to the innermost
  complete escape, flags `pNestedInCall`, and recognises the long vendor form
  `--(* Vendor(Microsoft), Product(ODBC) … *)--`.
- `ParseECodeType` (`sqlcmisc.cpp:7784`): classifies by tag length — 1 → `d` / `t` / `?`,
  2 → `fn` / `oj` / `ts`, 4 → `call` / `guid`, 6 → `escape`, 7 → `encrypt`, 8 → `interval`;
  anything else → `ECODE_ERROR` → `42000`.

### 3.3 `{call}`

`ProcessCanonicalCall` (`sqlcmisc.cpp:7952`) converts `[?=]call proc [(arg[,arg…])]` and picks:

- **RPC** (`bCanExecAsRPC`) — requires no cursor attributes, no T-SQL before the escape, and nothing
  but further `{call}` escapes or `;` after it.
- **Text** — `EXEC [?=]proc arg,arg,…`, with `DEFAULT` for omitted arguments and `@name=` prefixes
  for named parameters.

Behaviours worth copying:

- Procedure name may carry a group number, `proc;2` (`sqlcmisc.cpp:8215`).
- `?=` forces the first bound parameter to OUTPUT; `HY105` if bound `SQL_PARAM_INPUT`
  (`sqlcmisc.cpp:8310`), `07001` if unbound.
- A bound `SQL_PARAM_OUTPUT` inside a call is promoted to INPUT_OUTPUT on the wire (`:8455`).
- Trailing junk after the argument list is a syntax error (`:8570`).

### 3.4 Output parameter writeback

`GetReturnValue` (`sqlctokn.cpp:~250`) consumes `RETURNVALUE` tokens, matches them to bound
parameters **by name when available, otherwise by ordinal**, writes into the app buffer, and flushes
unmatched values off the wire rather than dropping them mid-stream.

### 3.5 `SQLDescribeParam` for `{call}`

`AutoFillIPD` (`sqlcdesc.cpp:9355`) calls `DoSubstitutions(NULL, lpdbc, &lpextbuffer)` **first**, then
hands the translated text to the server metadata path (`CImpODBCIObtainParameterMetadata`, i.e.
`sp_describe_undeclared_parameters` on SQL 2012+). So describe operates on `EXEC proc @P1,…`, not on
the original `{call …}` — and it does so regardless of `SQL_ATTR_NOSCAN`, because the statement
handle is not passed in.

Confirmed independently **[measured]**: `sp_describe_undeclared_parameters` accepts
`EXEC dbo.proc @P1, @P2, @P3` and returns full metadata; it rejects `{call …}` and `EXEC ?=proc ?`
with `42000`.

msodbcsql's observable behaviour, which the new code must match **[measured, 18.6.2.1]**, for
procedure `dp_probe(@a int, @b varchar(20), @c int OUTPUT)`:

| Statement | `SQLNumParams` | `SQLDescribeParam` |
| --- | --- | --- |
| `{call dbo.dp_probe(?,?,?)}` | 3 | `SQL_INTEGER(10)`, `SQL_VARCHAR(20)`, `SQL_INTEGER(10)` |
| `{? = call dbo.dp_probe(?,?,?)}` | 4 | param 1 = `SQL_INTEGER(10)` (return status), then the three |
| `{call dbo.dp_probe(?, 'lit', ?)}` | 2 | `SQL_INTEGER(10)`, `SQL_INTEGER(10)` — literals are skipped |
| `{call dbo.dp_probe}` | 0 | — |
| `{call …}; {call …}` | 6 | markers numbered across the whole batch |
| `SELECT {fn UCASE(?)}` | 1 | `SQL_WVARCHAR(4000)` |
| `SELECT ?, ?` | 2 | `42000` — the server cannot deduce the type (existing behaviour, unchanged) |

---

## 4. Current state of mssql-odbc (`main`, `0d431b71`)

| Piece | State | Reference |
| --- | --- | --- |
| SQL text lexer | `rewrite_param_markers` already skips `'…'`, `"…"`, `[…]`, `--`, `/* */`, and the `--(*…*)--` vendor form; returns `(String, usize)` | `mssql-odbc/src/api/util.rs:237` |
| Exec routing | `marker_count > 0` → `execute_sp_executesql(rewritten, params)`, else `execute(sql)` batch | `mssql-odbc/src/api/exec_direct.rs:309` |
| Retained SQL | `SQLPrepareW` stores only the **marker-rewritten** text; the original is dropped | `mssql-odbc/src/handles/stmt.rs:1015` (`PreparedPlan`) |
| RPC by name | `client.execute_stored_procedure(name, positional, named, opts)` proven against `sp_describe_undeclared_parameters` | `mssql-odbc/src/api/describe_param.rs:202` |
| Param building | `build_named_params` → `Vec<RpcParameter>` + DAE list | `mssql-odbc/src/api/exec_common.rs:668` |
| Output params | **hard-rejected** — `HYC00` "Output parameters not yet implemented" | `mssql-odbc/src/api/bind_param.rs:258` |
| `BoundParam` | already carries `input_output_type`, never read | `mssql-odbc/src/params/bound_param.rs:33` |
| `SQL_ATTR_NOSCAN` | accepted but **inert**, default `0` (`SQL_NOSCAN_OFF`) | `mssql-odbc/src/handles/stmt.rs:588`, `set_stmt_attr.rs:483` |
| `SQLNativeSqlW` | **not implemented, not exported** | `mssql-odbc/src/api/exports.rs` |
| `SQLNumParams` | **not implemented, not exported**; the count exists as `PreparedPlan::marker_count` | `mssql-odbc/src/handles/stmt.rs:1015` |
| `SQL_PROCEDURES` | ships `"Y"` since #535 — a forward commitment this branch has to make true | `mssql-odbc/src/api/get_info.rs:314` |
| Scalar-function masks | `SQL_NUMERIC_FUNCTIONS` / `SQL_STRING_FUNCTIONS` / `SQL_SYSTEM_FUNCTIONS` / `SQL_TIMEDATE_FUNCTIONS` all report `SQL_FN_NONE_SUPPORTED` (0), commented "Tracked by AB#46384" | `mssql-odbc/src/api/get_info.rs:329-332`, `odbc_types.rs:286-291` |
| Escape handling | **none** — no code reads `{` / `}` or `call` from user SQL | — |
| Output-param TODO | `// TODO: surface output-param availability here once output params land.` | `mssql-odbc/src/api/more_results.rs:306` |

Net effect today: the driver behaves as if `SQL_ATTR_NOSCAN` were permanently **on**. Everything the
server accepts natively already works by accident; nothing else does.

### 4.1 What mssql-tds already provides

| Capability | State | Reference |
| --- | --- | --- |
| Output parameter on the wire | `RpcParameter` + `StatusFlags::BY_REF_VALUE`; `RpcParameter::is_output()` | `mssql-tds/src/message/parameters/rpc_parameters.rs:34,151,529` |
| `RETURNVALUE` token (0xAC) | parsed; `TdsClient::get_return_values() -> Vec<ReturnValue>` with `ReturnValueStatus::{OutputParam, Udf}` | `mssql-tds/src/query/result.rs:17`, `mssql-tds/src/token/tokens.rs:1013` |
| `RETURNSTATUS` token (0x79) | parsed into `last_return_status`, **`pub(in crate::connection)` — no public getter** | `mssql-tds/src/connection/tds_client.rs:157,465` |
| `sp_executesql` | present, but see the gap below | `mssql-tds/src/connection/tds_client.rs:1536` |
| Tests | `mssql-tds/tests/test_rpc_results.rs::test_stored_proc{,_stream_results}` (RPC path only) | — |

Three mssql-tds gaps this PR closes:

1. **No public accessor for the procedure return status** — `{? = call proc}` needs it.
2. **`sp_executesql` cannot actually return output values.** `build_parameter_list_string_impl`
   emits `"{param_name} {param_type_name} "` and never appends `OUTPUT`, even when the parameter
   carries `StatusFlags::BY_REF_VALUE` **[source]**:

   ```rust
   // mssql-tds/src/message/parameters/rpc_parameters.rs:678
   params_list.push_str(&format!("{param_name} {param_type_name} "));
   ```

   T-SQL needs `OUTPUT` in *two* places — in the `@params` declaration and after the argument at the
   call site — so the `{call}` text fallback (§5.5) cannot propagate outputs until this is fixed.
   The RPC path is unaffected; `BY_REF_VALUE` on the wire is sufficient there.
3. **Procedure-name interpolation in the Always Encrypted describe path**:
   `let mut tsql = format!("EXEC {stored_procedure_name}");` at
   `mssql-tds/src/connection/tds_client.rs:5851` **[source]**. Today the only caller passes a
   constant. `{call}` makes that name **user-controlled text lifted out of the app's SQL**, turning
   this into a T-SQL injection vector on AE-enabled connections. Fixed in this PR — see §5.7.

---

## 5. Design

### 5.1 Two phases over one lexer

Escape translation and parameter-marker rewriting are **separate phases**. §2.2 shows why:
`SQLNativeSql` must return `EXEC ?=sp_who ?` with the `?` intact, so it cannot run the marker
rewrite. New module `mssql-odbc/src/sql/escape.rs` (or `api/escape.rs` to keep the flat `api`
layout):

```rust
/// Phase 1 — escape translation only. `?` markers are left exactly as written.
pub(crate) struct Translated {
    pub sql: String,
    /// Some(..) only when the whole statement is a single canonical call.
    pub call: Option<CallSite>,
}

pub(crate) enum EscapeKind { Fn, Date, Time, Timestamp, OuterJoin, Guid, Escape, Encrypt, Interval, Call }

pub(crate) fn translate_escapes(sql: &str) -> Result<Translated, EscapeError>;

/// Phase 2 — execution only: `?` -> `@P1..@Pn`. This is today's function, unchanged.
pub(crate) fn rewrite_param_markers(sql: &str) -> (String, usize);
```

Both phases walk the **same lexer** — a span iterator that classifies the text into string literal /
quoted identifier / bracketed identifier / line comment / block comment / vendor
`--(*…*)--` / code — so the two can never disagree about what a comment is.
`rewrite_param_markers` keeps its current signature and its existing tests unchanged; only its
internals move onto the shared lexer.

Callers:

| Caller | Phase 1 | Phase 2 |
| --- | --- | --- |
| `SQLNativeSql` | always | never |
| `SQLExecDirectW` / `SQLPrepareW` | unless `SQL_ATTR_NOSCAN` is on | always |
| `SQLDescribeParam` | always, from the retained original text (§5.2) | always |

Translation policy:

| Escape | Behaviour |
| --- | --- |
| `{fn …}`, `{d …}`, `{t …}`, `{ts …}`, `{oj …}`, `{guid …}` | validated, then **passed through verbatim** |
| `{escape 'c'}` | → `ESCAPE 'c'` |
| `{interval …}` | → T-SQL string literal, §5.3 |
| `{encrypt N'…'}` | → `0x…`, §5.4 |
| `{[?=]call proc(…)}` | RPC, or textual `EXEC`, §5.5 |
| anything else | `42000`, statement not sent |

Whitespace: match msodbcsql — a translated escape is emitted as `" " + text + " "`.

### 5.2 `SQL_ATTR_NOSCAN`, and retaining the original SQL

`SQL_ATTR_NOSCAN` moves out of `INERT_STMT_ATTRS` (`mssql-odbc/src/handles/stmt.rs:588`) into a real
statement field. Default stays `SQL_NOSCAN_OFF` (scanning on).

The statement must also **retain the original SQL text**. Today `PreparedPlan`
(`mssql-odbc/src/handles/stmt.rs:1015`) keeps only the marker-rewritten text, which is lossy in a way
that breaks metadata: with `SQL_ATTR_NOSCAN` on, `{? = call proc(?)}` is stored as
`{@P1 = call proc(@P2)}`, and a later forced scan can no longer recognise the canonical
return-status marker. Since `SQLDescribeParam` must translate regardless of `NOSCAN` (§3.5), it has
to work from the original. So `PreparedPlan` gains the original text alongside the rewritten text
and the marker count.

### 5.3 `{interval …}` — exact reproduction

**Grammar** (`ParseInterval`, `sqlccnvt.cpp:6489`):
`{ INTERVAL [+|-] '<value>' <leading-field> [ ( p [, s] ) ] [ TO <trailing-field> [ (s) ] ] }`,
fields `YEAR MONTH DAY HOUR MINUTE SECOND`. Defaults before parsing are **precision 2, scale 6**
(`ProcessDTI`, `sqlcmisc.cpp:7920`).

**Output** is a T-SQL *string literal* — the reason the server accepts it is simply that it is a
string, not an interval expression. Format strings, verbatim from `sqlcstr.cpp:383-395`
(`%c` = `-` when a `-` preceded the value, else `+`):

```
YEAR              'INTERVAL %c''%lu'' YEAR(%u)'
MONTH             'INTERVAL %c''%lu'' MONTH(%u)'
DAY               'INTERVAL %c''%lu'' DAY(%u)'
HOUR              'INTERVAL %c''%lu'' HOUR(%u)'
MINUTE            'INTERVAL %c''%lu'' MINUTE(%u)'
SECOND            'INTERVAL %c''%lu%s'' SECOND(%u,%u)'
YEAR TO MONTH     'INTERVAL %c''%lu-%02lu'' YEAR(%u) TO MONTH'
DAY TO HOUR       'INTERVAL %c''%lu %02lu'' DAY(%u) TO HOUR'
DAY TO MINUTE     'INTERVAL %c''%lu %02lu:%02lu'' DAY(%u) TO MINUTE'
DAY TO SECOND     'INTERVAL %c''%lu %02lu:%02lu:%02lu%s'' DAY(%u) TO SECOND(%u)'
HOUR TO MINUTE    'INTERVAL %c''%lu:%02lu'' HOUR(%u) TO MINUTE'
HOUR TO SECOND    'INTERVAL %c''%lu:%02lu:%02lu%s'' HOUR(%u) TO SECOND(%u)'
MINUTE TO SECOND  'INTERVAL %c''%lu:%02lu%s'' MINUTE(%u) TO SECOND(%u)'
```

`%s` is the fraction, formatted `.%09lu` from a nanosecond value and then truncated to `scale`
digits (`sqlccnvt.cpp:2818`), or empty when `scale == 0`.

Golden cases **[measured, 18.6.2.1]** — use these verbatim as unit-test expectations (the escape is
replaced by the text plus one space on each side):

| Input | Output |
| --- | --- |
| `{interval '1' DAY}` | `'INTERVAL +''1'' DAY(2)'` |
| `{interval -'1' DAY}` | `'INTERVAL -''1'' DAY(2)'` |
| `{interval '1' DAY(3)}` | `'INTERVAL +''1'' DAY(3)'` |
| `{interval '10' YEAR}` | `'INTERVAL +''10'' YEAR(2)'` |
| `{interval '1-2' YEAR TO MONTH}` | `'INTERVAL +''1-02'' YEAR(2) TO MONTH'` |
| `{interval '1 12' DAY TO HOUR}` | `'INTERVAL +''1 12'' DAY(2) TO HOUR'` |
| `{interval '1 12:30' DAY TO MINUTE}` | `'INTERVAL +''1 12:30'' DAY(2) TO MINUTE'` |
| `{interval '1 12:30:45.123' DAY TO SECOND(3)}` | `'INTERVAL +''1 12:30:45.123'' DAY(2) TO SECOND(3)'` |
| `{interval '30' SECOND}` | `'INTERVAL +''30.000000'' SECOND(2,6)'` |
| `{interval '30.5' SECOND(2,1)}` | `'INTERVAL +''30.5'' SECOND(2,1)'` |

Validation, all → `42000` (`ParseInterval`, `sqlccnvt.cpp:6720-6780`): precision > 9; scale > 9;
leading value ≥ 10^precision; `month > 11`; `hour > 23`; `minute > 59`; `second > 59`; trailing junk.
Fraction digits beyond `scale` are truncated and reported as a warning (`01S07`), not an error.

### 5.4 `{encrypt N'…'}` — exact reproduction

`ProcessEncrypt` (`sqlcmisc.cpp:8673`) requires the exact shape `{encrypt N'literal'}`
(`N` mandatory, `''` is an embedded quote, nothing after the closing quote), applies the classic TDS
password obfuscation to the **UTF-16LE bytes**, and emits `0x` + uppercase hex.

`EncryptPWD` (`TdsParser.h:5031`) **[source]**:

```c
*pbPWD = (((*pbPWD & 0x0f) << 4) | (*pbPWD >> 4)) ^ 0xa5;   // nibble swap, then XOR 0xA5
```

Verified: `{encrypt N'abc'}` → `0xB3A583A593A5` **[measured]**; `'a'` = `0x61` → swap `0x16` →
`^ 0xA5` = `0xB3`, and each UTF-16 high byte `0x00` → `0xA5`. Malformed input → `42000`.

This is **obfuscation, not encryption** — it is the TDS LOGIN7 password scrambler and provides no
confidentiality. Implement it for msodbcsql compatibility, name it accordingly
(`scramble_login_literal`, not `encrypt_*`), and comment it as such so nobody mistakes it for a
security primitive.

### 5.5 `{call}` routing

Two paths, mirroring msodbcsql:

- **RPC path** — the trimmed statement is exactly one `{[?=]call name[(args…)]}` and every argument
  is `?`, `DEFAULT`, or empty. Dispatch through `execute_stored_procedure(name, …)` with parameters
  from an extended `build_named_params`. Output parameters need only `StatusFlags::BY_REF_VALUE`.
- **Text path** — everything else (a call inside a batch, literal or nested-escape arguments,
  multiple calls): rewrite to `EXEC name @P1, @P2 OUTPUT, DEFAULT, …` and run through
  `sp_executesql`.

The text path has to carry binding direction into **both** halves of the generated statement:

```sql
-- @params declaration, produced by mssql-tds (§5.7 item 2)
N'@P1 int, @P2 int OUTPUT'
-- call site, produced by mssql-odbc
N'EXEC dbo.proc @P1, @P2 OUTPUT'
```

Omitting either one silently drops the output value, which is why §5.7 item 2 is a prerequisite for
this path rather than a nice-to-have.

**Procedure-name validation is mandatory before either path.** Accept only
`[db.][schema.]name` where each part is a regular identifier or a `[bracketed]` / `"quoted"`
identifier, plus an optional `;N` group number; reject everything else with `42000`. Never
concatenate the name into T-SQL without bracket-quoting it. This is the injection-relevant surface
of the change and must be reviewed as such.

### 5.6 Output parameters, return values, availability ordering

- `SQLBindParameter` accepts `SQL_PARAM_OUTPUT`, `SQL_PARAM_INPUT_OUTPUT`, `SQL_PARAM_RETURN_VALUE`
  with the validation ODBC requires; the `HYC00` rejection at `bind_param.rs:258` goes away.
- `build_named_params` marks them `StatusFlags::BY_REF_VALUE`.
- ODBC requires output values to be invisible until every result set the procedure produced has been
  consumed. `get_return_values()` fills as tokens arrive, so writeback is gated on batch exhaustion
  at `more_results.rs:306` — where the TODO already sits — not written back eagerly at execute time.
- Matching follows msodbcsql: **by name first, then by ordinal**. Honour `StrLen_or_IndPtr`
  including `SQL_NULL_DATA`; report truncation as `01004`.
- `{? = call …}` binds the return status to parameter 1: `HY105` when bound `SQL_PARAM_INPUT`,
  `07001` when unbound.

### 5.7 mssql-tds changes

1. Public accessor for the RPC return status (`last_return_status`), with tests — `RETURNSTATUS` has
   no coverage today.
2. Append ` OUTPUT` in `build_parameter_list_string_impl`
   (`rpc_parameters.rs:678`) when `RpcParameter::is_output()` (`:529`), so the `sp_executesql`
   `@params` declaration matches the call site. Partly satisfies AB#41343.
3. Fix `format!("EXEC {stored_procedure_name}")` at `tds_client.rs:5851`. Bracket-quote the
   identifier (doubling any `]`) rather than interpolating raw text, and keep the driver-side
   validation from §5.5 as defence in depth.

### 5.8 `SQLDescribeParam` / `SQLNumParams`

- `SQLNumParams` returns the marker count for prepared statements (`PreparedPlan::marker_count`),
  `HY010` when the statement is not prepared. For `{? = call}` the return-status marker counts, so
  the totals in §3.5 fall out naturally.
- `SQLDescribeParam` translates the **retained original** text (§5.2) — always, ignoring
  `SQL_ATTR_NOSCAN`, matching `AutoFillIPD`'s `lpstmt == NULL` behaviour — and feeds the resulting
  `EXEC proc @P1,…` to the existing `sp_describe_undeclared_parameters` path. For `{? = call}`,
  describe parameters 2..n from `EXEC proc @P2,…` and report parameter 1 as `SQL_INTEGER`,
  precision 10, matching §3.5.

---

## 6. Delivering it as one PR

One PR on `david/odbc-escape-sequences-46384`, rebased on `main` now that #535 has merged.
Realistically ~2,000–2,800 lines including tests, which is large — so the PR is organised as an
**ordered, individually-reviewable commit sequence**, and the PR description points reviewers at it
commit by commit.

| # | Commit | Content | Rough size |
| --- | --- | --- | --- |
| 0 | `Add the escape-sequence implementation plan` | This document. | — |
| 1 | `Add the ODBC escape scanner` | Shared lexer + `translate_escapes`; `rewrite_param_markers` moved onto the same lexer with its tests unchanged. Recognition, classification, syntax validation, pass-through policy. No caller changes yet. | ~450 |
| 2 | `Translate {escape}, {interval}, {encrypt}` | The three literal translations, to the byte-exact specs in §5.3/§5.4. Pure functions, heavily unit-tested. | ~350 |
| 3 | `Implement SQLNativeSql` | Export `SQLNativeSqlW` (phase 1 only — markers preserved), including the truncation/`01004` buffer contract and the null-out-buffer length query. First user-visible behaviour. | ~200 |
| 4 | `Honour SQL_ATTR_NOSCAN and scan on execute` | Attribute becomes real; statements retain their original SQL (§5.2); `SQLExecDirectW` / `SQLPrepareW` run phase 1 then phase 2; `42000` raised before any network I/O. | ~300 |
| 5 | `Implement SQLNumParams` | Export + `HY010` for unprepared. | ~120 |
| 6 | `Expose the RPC return status in mssql-tds` | Public accessor + tests. | ~120 |
| 7 | `Emit OUTPUT in the sp_executesql parameter declaration` | `rpc_parameters.rs:678` honours `is_output()`, with tests proving an output value round-trips through `sp_executesql`. | ~120 |
| 8 | `Bracket-quote the procedure name in the AE describe path` | The `tds_client.rs:5851` fix + regression test. Standalone so it is reviewable as a security fix. | ~80 |
| 9 | `Translate {call} and dispatch as RPC` | Name validation, RPC path, `EXEC` text fallback with `OUTPUT` at the call site, `;N` group numbers, `07001` binding checks. Output binding still rejected at this commit. | ~500 |
| 10 | `Accept and write back output parameters` | Bind-time acceptance, `BY_REF_VALUE` marking, writeback gated on batch exhaustion, name-then-ordinal matching, indicators, `SQL_NULL_DATA`, `01004` truncation, `{? = call}` return status. Resolves the `more_results.rs:306` TODO. | ~550 |
| 11 | `Describe {call} parameters` | Translated original text into the `sp_describe_undeclared_parameters` path; `{? = call}` parameter 1 reported as `SQL_INTEGER(10)`. | ~200 |
| 12 | `Add escape-sequence e2e tests and parity run` | C++ gtest coverage, msodbcsql parity leg, fuzz target. | ~500 |
| 13 | `Report escape capabilities from SQLGetInfo` | Replace the zeroed masks and add the remaining escape-related info types (§7). | ~150 |

If the security fix in commit 8 needs to ship ahead of the feature, it is self-contained and can be
cherry-picked into its own PR.

---

## 7. `SQLGetInfo` follow-through

#535 deliberately left the scalar-function masks at zero and pointed them here
(`odbc_types.rs:286-291`): *"This driver does not translate escape sequences yet, so ODBC's 'none
supported' is the only honest answer; msodbcsql18 advertises real masks. Tracked by AB#46384."*
`SQL_PROCEDURES` already reports `"Y"` (`get_info.rs:314`), which only becomes fully true once
commit 9 lands.

Measured from the pinned reference driver **[measured, msodbcsql 18.6.2.1 against SQL Server
17.00.4006]**:

| Info type | msodbcsql value | Currently in mssql-odbc |
| --- | --- | --- |
| `SQL_NUMERIC_FUNCTIONS` | `0x00FFFFFF` | `0` |
| `SQL_STRING_FUNCTIONS` | `0x004FFFFF` | `0` |
| `SQL_SYSTEM_FUNCTIONS` | `0x00000007` | `0` |
| `SQL_TIMEDATE_FUNCTIONS` | `0x001FFFFF` | `0` |
| `SQL_CONVERT_FUNCTIONS` | `0x00000003` | not implemented |
| `SQL_TIMEDATE_ADD_INTERVALS` | `0x000001FF` | not implemented |
| `SQL_TIMEDATE_DIFF_INTERVALS` | `0x000001FF` | not implemented |
| `SQL_OJ_CAPABILITIES` | `0x0000007F` | not implemented |
| `SQL_OUTER_JOINS` | `"F"` | not implemented |
| `SQL_LIKE_ESCAPE_CLAUSE` | `"Y"` | not implemented |
| `SQL_ODBC_INTERFACE_CONFORMANCE` | `3` (`SQL_OIC_LEVEL2`) | not implemented |

Commit 13 adopts these, following the rule `attributes_plan.md` §8 established: values are measured
from the pinned msodbcsql build, never copied from the ODBC headers. `SQL_ODBC_INTERFACE_CONFORMANCE`
is listed for completeness but is a broader claim than this story earns on its own — take it only if
the rest of the Level 2 surface is genuinely present, otherwise leave it out and note why.

---

## 8. Testing

- **Unit** — the scanner carries the correctness risk; target ~100%. Every escape family; every
  lexical trap (escape inside `'…'` / `"…"` / `[…]` / `--` / `/* */`, unterminated brace,
  unterminated literal, `{` inside a comment, stray `}`, nested escapes, escape at offset 0 and at
  end of string, empty statement, `?` inside an escape body, `N'…'` prefixes). The `{interval}` and
  `{encrypt}` golden tables in §5.3/§5.4 go in verbatim. Phase separation gets its own test:
  `translate_escapes` must leave every `?` untouched.
- **Fuzz** — add an escape-scanner target alongside the existing ones in `mssql-odbc/fuzz/`. A
  hand-written lexer over attacker-supplied text is exactly what fuzzing pays for: assert no panic,
  no unbounded growth, and that running phase 2 alone still matches today's `rewrite_param_markers`.
- **Integration / e2e** — C++ gtest under `mssql-odbc/tests/e2e/tests/`. Procedures cannot be `#temp`
  across connections, so create them with unique generated names and drop them in teardown. Cover
  input / output / input-output / return-value parameters, NULL outputs, truncation, and an explicit
  assertion that **output values are not visible until result sets are consumed**. Cover the text
  fallback separately from the RPC path — an output parameter must round-trip through both.
- **Parity** — `run_e2e.sh --compare-with-msodbcsql` on the whole corpus; `parity_report.py` fails
  unless both legs reach the same verdict. The `SQLNativeSql` table in §2.2 and the
  `SQLNumParams`/`SQLDescribeParam` table in §3.5 become parity tests.
- **Security review** — commits 8, 9 and 10 need an explicit look at procedure-name handling and at
  buffer handling in output writeback.

---

## 9. Decisions taken

1. **`{interval}` is reproduced exactly**, not rejected — spec and golden values in §5.3.
2. **`{encrypt}` is implemented**, per §5.4, with naming and a comment that make clear it is the TDS
   password scrambler and not a security primitive.
3. **One PR**, structured as the ordered commit sequence in §6.
4. **#535 (AB#47086) has merged**; this branch is rebased on it and finishes the escape-related
   `SQLGetInfo` values it left at zero.
5. **`SQLDescribeParam` for `{call}` is in scope**, §5.8 — cheap, because the translated `EXEC` text
   feeds the existing `sp_describe_undeclared_parameters` path.
6. **The `format!("EXEC {stored_procedure_name}")` injection vector is fixed in this PR**, §5.7,
   as its own commit.

---

## 10. Out of scope

- `sp_prepexecrpc` optimisation for prepared canonical calls (msodbcsql's `PREP_EXEC_RPC_CALL`).
- Cursor-attribute interactions with `{call}` (msodbcsql's `CheckCursorOn`); mssql-odbc is
  forward-only / read-only today, so there is nothing to interact with.
- Batches that mix canonical calls with other RPC-eligible statements beyond the strict single-call
  RPC form — those take the `EXEC` text path.

---
applyTo: "mssql-odbc/**"
---

# mssql-odbc — Rust Guidelines

Rules for writing safe, panic-free Rust in the ODBC driver. This crate produces
a C shared library loaded via `dlopen` into arbitrary host processes — panics
unwinding across the FFI boundary, undefined behavior, and memory errors are all
fatal and unrecoverable.

## Project context

Before making changes, also read [mssql-odbc/README.md](../../mssql-odbc/README.md)
for architecture, supported features, and build/run instructions.

## Parity reference: the classic C++ msodbcsql driver

The classic C++ **msodbcsql** driver (Microsoft ODBC Driver for SQL Server) is the
authoritative parity reference for this crate. Its source lives in the
`SqlClientDrivers` Azure DevOps org (`msodbcsql` project/repo, `master`).

- Before adding, changing, or **rejecting** any behavior for parity reasons — auth
  keywords, connection-string attributes, error/SQLSTATE mapping, Driver Manager
  interaction — verify it against the actual msodbcsql source. Do **not** rely on
  MS Learn docs or sibling drivers (JDBC/.NET/go-sqlcmd), which frequently differ
  from what the C++ driver actually does.
- When reporting a parity finding, cite the msodbcsql source (file + what it does),
  and state explicitly whether the decision **matches**, **exceeds**, or **diverges
  from** msodbcsql so the trade-off is visible.
- **A behavioral parity claim needs a measurement, not just a source reading, and
  must name the build.** Record the driver's `SQL_DRIVER_VER`; retail and debug
  msodbcsql have been observed to disagree with each other and with the source on
  the same binding. CI compares against retail 18.6.2.1, pinned by
  `msodbcsqlVersion` (`.pipeline/validation-pipeline.yml`) and installed by
  `.pipeline/scripts/install-msodbcsql.ps1`; a measurement against any other
  build settles what that build does, not what the compare leg will do. A
  `SKIP_IF_COMPARING_MSODBCSQL()` is itself such a claim - it deletes the only
  check that could contradict the comment above it - so add or keep one only
  against a compare run that actually fails.
- **This driver targets ODBC 3.x only.** That scopes out support for ODBC 2.x
  *applications* — deprecated 2.x entry points, 2.x-only attribute values, and
  the paths msodbcsql keeps for them. The Driver Manager maps a 2.x application
  onto the 3.x interface before the call reaches a 3.x driver, so a msodbcsql
  code path that exists solely for 2.x compatibility is not a parity gap. Say so
  rather than porting it.
  - **It does not scope out 2.x-era identifiers.** `SQL_C_DATE` / `SQL_C_TIME` /
    `SQL_C_TIMESTAMP` are deprecated but still defined in the ODBC 3.x headers, so
    a 3.x application may legally pass them and the DM remaps nothing. Accept
    them and fold them onto the 3.x form (`api::type_rules::canonical_c_type`).
    Check each identifier before assuming the rule applies — the SQL side is not
    symmetric, ODBC 3.x reuses the 2.x date/time SQL values: `9` is both `SQL_DATE`
    (2.x concise) and `SQL_DATETIME` (3.x verbose), and `10` is both `SQL_TIME` and
    `SQL_INTERVAL`. A `ParameterType` of `9` is therefore ambiguous, so it is
    rejected (`HY004`) rather than folded - 3.x applications use `SQL_TYPE_*`
    (91-93), and the DM remaps a 2.x application's spelling first. msodbcsql
    accepts both SQL spellings because it also serves 2.x applications and can
    disambiguate on the declared version.
  - **It does not cover 3.0/3.5 vs 3.8.** `SQL_OV_ODBC3` and `SQL_OV_ODBC3_80`
    are both ODBC 3.x and both in scope. msodbcsql branches on this separately —
    `Sql2CDefault` selects `rgbTRANSTYPE` when `IS351ORLESSAPP(wStatus)` and
    `rgbTRANSTYPE380` otherwise — so a version-keyed branch is only out of scope
    once you have checked *which* version boundary it keys on.
  - Beware that msodbcsql normalizes types to their 2.x values on entry
    (`SQLBindParameter` in `Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp` maps
    `SQL_TYPE_*` down to `SQL_DATE`/`SQL_TIME`/`SQL_TIMESTAMP`, and `SQL_DOUBLE`
    to `SQL_FLOAT`, before validating). Downstream code accepting a 2.x spelling
    therefore does **not** prove it supports 2.x applications, and a branch that
    looks reachable in a validator may be dead once the caller's normalization is
    accounted for. Read the caller before concluding either way.

## Deliberate deviations

Entries here are **decisions**: we know what msodbcsql does, we could match it,
and we chose not to.

Add one only when both are true:

- **An application can tell the difference.** Rejecting something msodbcsql
  accepts always qualifies; accepting something it rejects is milder but counts.
- **The decision is bigger than one function.** It sets a rule other code has to
  follow - an encoding, a connection-string keyword, a length unit - or it
  needed sign-off that a reader must be able to find without reading code.

Two things that look like deviations but are not:

- **A gap** - something not built yet, however deliberately deferred.
  Ex: `SQLBindCol` rejects `SQL_C_DEFAULT` where msodbcsql resolves it at fetch
  time; that is a code comment plus a work item, not an entry here.
- **A difference nothing can observe**, or one that lives in a single function.
  Explain those where they happen.

Each entry gives: what msodbcsql does, with a source citation; what this driver
does instead; why; the work item; and, where applications can regress, who
signed off and when. Keep per-build measurements in the work item so this file
does not grow every time a new msodbcsql build is measured.

1. `ActiveDirectoryManagedIdentity` is accepted as an alias for managed-identity
   authentication. msodbcsql recognizes only `ActiveDirectoryMSI`
   (`Sql/Ntdbms/sqlncli/msdart/inc/dlgattr.h` → `OPTIONADMSI L"ActiveDirectoryMSI"`);
   `ActiveDirectoryManagedIdentity` does not appear anywhere in the msodbcsql source.
   Added to match MS Learn and the sibling drivers (JDBC/.NET/go-sqlcmd). Tracked in AB#46066.
2. `SQL_C_DEFAULT` resolves the wide character SQL types to `SQL_C_WCHAR`, and
   `SQL_GUID` to `SQL_C_GUID`, following the ODBC 3.x default-C-type table.
   Applies to both directions: `SQLBindParameter` resolves at bind time, and
   `SQLFetchScroll` resolves a bound column per fetch from the IRD, through the
   same `type_rules::resolve_default_c_type`.
   msodbcsql's `Sql2CDefault` reads `rgbTRANSTYPE380`
   (`Sql/Ntdbms/sqlncli/odbc/sqlcmisc.cpp`), which resolves both to `SQL_C_CHAR`
   — an ANSI-transfer default this driver has no equivalent for, since its
   `SQL_C_CHAR` is UTF-8. Resolving UTF-16 application input to a UTF-8 buffer
   type would silently corrupt data. On the fetch side the same choice avoids
   transcoding every wide column by default. Confirmed against msodbcsql18 for
   `nvarchar` (three narrow bytes, indicator 3) and `uniqueidentifier` (the
   36-character text form, indicator 36). Note this covers only those two rows:
   `SQL_SS_XML` resolves to `SQL_C_WCHAR` in *both* drivers
   (`sqlcmisc.cpp:179` and `:218`, measured as UTF-16 with indicator 30), so it
   is **not** a deviation. `DefaultCTypeWideCharParam` and
   `DefaultCTypeGuidRoundTrips` carry `SKIP_IF_COMPARING_MSODBCSQL()` for the
   two parameter-side halves. Tracked in AB#47365.
3. `SQL_C_CHAR` is **UTF-8** in both directions; the driver never reads or
   writes the client code page. msodbcsql uses the client code page -
   `dwClientCodePage = SystemLocale::Singleton().AnsiCP()`
   (`odbc/sqlcprot.h:2830`), which is `GetACP()` on Windows
   (`Common/include/Localization.hpp:742`) and `nl_langinfo(CODESET)` elsewhere
   (`LocalizationImpl.hpp:386`); the parameter path reads it directly at
   `sqlcfunc.cpp:2913`. The two therefore agree under a UTF-8 locale and differ
   on a default Windows one. Taken because mssql-python, the only supported
   consumer, is UTF-8 native; the ODBC "C Data Types" appendix fixes no encoding
   for `SQL_C_CHAR`, so neither choice is more conformant. Revisit if a second
   consumer targets this driver on Windows. Tracked in AB#47564 (fetch) and
   AB#47565 (parameters). `SQL_C_WCHAR` is UTF-16LE on both drivers.
4. **Parameter length is measured in UTF-16 units for both character C types.**
   msodbcsql counts UTF-16 units in three of its four arms - both wide-source
   arms, and the narrow-to-wide walk, which counts an astral character as two
   (`odbc/sqlcfunc.cpp:2935`) - but counts source bytes for narrow-to-narrow
   (`cchDest = cbData`, `:2952`). That byte count is the wire length only while
   no client-side transcode happens: TDS carries a collation with char data, so
   the bytes normally ship under a declared collation and the server converts.
   `DoCharToCharConversion` (`odbc/sqlcprot.h:4113`) enables client-side
   conversion for an encoding TDS cannot name - a UTF-8 client against a
   non-UTF-8 server, or the ISO-8859-x range - and translation is on by default
   (`SQL_XL_DEFAULT`). In that configuration msodbcsql transcodes yet still
   measures the *pre-transcode* UTF-8 bytes, so it rejects a four-character
   accented string from a `varchar(4)` that the four bytes it actually sends
   would fit, while accepting the same value as `SQL_C_WCHAR`. Because this
   driver's `SQL_C_CHAR` is always UTF-8, copying the byte rule made that
   latent msodbcsql defect unconditional. The uniform unit is therefore taken
   to stop the two C types disagreeing on one value, not to match msodbcsql -
   it is a divergence in the configuration closest to this driver, on the same
   footing as the narrow-to-wide off-by-one at `sqlcfunc.cpp:2926` that is also
   deliberately not replicated. The count still errs low against a `_UTF8` or
   DBCS collation: a bounded `char`/`varchar` surfaces `HY000` from
   `serialize_char_varchar_direct` rather than `22001`, and the `max` and
   `text`/`ntext` types carry no check at all and send the over-long value.
   **This regresses a subset of inputs rather than being a pure win** - three
   U+2615 into `varchar(3)` was a correct `22001` and is now an opaque failure,
   so CJK and astral input bound with an exact character count is the shape that
   suffers. Taken because over-rejection has no application workaround while
   under-rejection still errors, and because byte-counting both C types would
   break the wide arm that msodbcsql gets right. Exactness needs the collation at
   this layer. Signed off by Theekshna Kotian (product owner) on 2026-08-27.
   Tracked in AB#47584.
5. **An integer parameter bound to a character type is length-checked.**
   msodbcsql length-checks no integer C type (`odbc/sqlcfunc.cpp:2586`, `:2854`,
   `:3165`, `:3177`); what it does instead is undefined per build. Binding
   `12345` as `SQL_C_SLONG` to a `SQL_VARCHAR` of `ColumnSize` 3: retail
   18.05.0002 returns `SQL_SUCCESS` with no diagnostic and sends `varchar(3)`
   holding `"123"`, debug 18.06.0002 aborts on
   `assert(*pstMaxLen > 0 && *pstMaxLen >= stLen)` (`odbc/sqlcmisc.cpp:7458`),
   retail 18.6.2.1 hangs in `SQLExecute`. This driver reports `22001`. The
   fallthrough at `:7459` reads as *widening* the declaration and no measured
   build does that, so do not re-derive this one from source.
   `IntegerParamTooWideForColumnSizeIs22001` and
   `NegativeSignCountsAgainstColumnSize` carry `SKIP_IF_COMPARING_MSODBCSQL()`.
   Signed off by Theekshna Kotian (product owner) on 2026-08-28. Tracked in
   AB#47369.
6. **A `SQL_C_WCHAR` buffer of nothing but blanks bound to an integer type is
   `22018`; msodbcsql answers `HY000`** (retail 18.05.0002). The only input on
   this path where the two differ - the same blanks as `SQL_C_CHAR`, a
   zero-length wide buffer, and every other invalid literal in either width
   answer `22018` on both, so `CharParamInvalidLiteralIs22018` and
   `LocaleFormattedNumbersAreRejected` run unskipped. Mechanism not established;
   only the state is evidence. Do not generalise it - `CVT_ERROR` =
   `IDS_22_005` otherwise resolves to `22018` through the `std_error` branch of
   `SQL_DIAG_SQLSTATE` (`odbc/sqlcerr.cpp:990`) and
   `cli_common/src/clntcomn.cpp:1015`, not the server-keyed table at
   `odbc/sqlcstr.cpp:136`. `BlankOnlyWideLiteralIs22018` carries
   `SKIP_IF_COMPARING_MSODBCSQL()`. Tracked in AB#47369, which is where the
   outstanding 18.6.2.1 measurements land - keep the running record there
   rather than growing this file per build.
7. **A non-NULL parameter with a null `ParameterValuePtr` is `HY009`.**
   Both drivers reject it, with different states: retail 18.6.2.1 answers
   `HY090` ("Invalid string or buffer length") at `SQLExecute`, measured on ADO
   build 172202 on both Build Linux and Build Windows. `read_param_value`
   rejects the null buffer at bind instead, because the indicator already said
   the parameter is not NULL and so a value is owed.
   `sqlcfunc.cpp:2549` reads as though any null buffer is simply taken as NULL -
   an attempt to match that reading failed CI on exactly this input, so the
   reading is incomplete and must not be re-derived from source.
   `NullDataPointerWithZeroLengthIsHy009` carries `SKIP_IF_COMPARING_MSODBCSQL()`.
8. **A bound `max`/LOB text column converted to a typed C target is refused
   above 1 MiB; msodbcsql converts a truncated prefix and warns.** Both drivers
   cap what a typed conversion may materialize - a `varchar(max)` carries up to
   2 GB and the converter needs one contiguous literal. This driver's cap is
   `PLP_TYPED_MATERIALIZE_LIMIT` (`api/fetch_scroll.rs`) at 1 MiB; past it the
   value is drained to keep the row synchronized and answered `HYC00`.
   msodbcsql clamps to `2*CONVBUF_SIZE` (~1244 bytes, sized for the longest
   legal `double` literal) in `EstimateBytesToRead` (`odbc/sqlcdata.cpp`), then
   converts that prefix and reports `01004` rather than failing.
   **Measured on 18.06.0001**, `varchar(max)` bound to a typed target:
   `'0'`×2000 + `'1'` returns `SQL_SUCCESS_WITH_INFO` with `01004` and a value
   of **`0`** - the truncated prefix, not the `1` in the column - for both
   `SQL_C_SBIGINT` and `SQL_C_SLONG`, and the same past 1 MiB; `'x'`×5000
   returns `22018` (the parse fails before truncation is considered, so both
   drivers agree there); a short `'42'` returns `42` on both. The skipped
   case's own payload, `'1'`×1048577 into `SQL_C_SLONG`, overflows even the
   clamped prefix and returns `22003` there against this driver's `HYC00` - so
   both drivers error on it, with different states, and
   `AOversizedBoundVarcharMaxTypedConversionIsRefusedAndDrained` carries
   `SKIP_IF_COMPARING_MSODBCSQL()` on a measured divergence rather than an
   assumed one. Refusing is deliberate: `01004` is "string data, right
   truncated", which application code routinely ignores on a numeric fetch
   because scalars are not expected to be truncatable, so on the prefix-parses
   shape msodbcsql's answer is a silently wrong number. Note the cap keys on
   the column's byte count, not on whether the text parses, so any large
   `varchar(max)` bound to a typed target reaches it - schema drift, not a
   contrived input. CI compares against 18.6.2.1; this measurement is
   18.06.0001, so re-measure there before relying on the exact prefix length.
   Tracked in AB#47767.
9. **Widening a bound narrow `max` column to `SQL_C_WCHAR` truncates on a whole
   character.** A buffer with no room for the final surrogate pair ends before
   it; msodbcsql leaves the lone high surrogate in the last payload slot on this
   narrow-source widening path, though its wide-source path is surrogate-safe
   (`GetColDataSurrogateSafe`, and `TrimPartialCodePt` for partial sequences).
   This driver's existing bound `nvarchar(max)` delivery already trims to a
   character boundary, and handing back text that does not decode from one `max`
   type but not the other would be worse than the divergence.
   `ABoundUtf8VarcharMaxDoesNotSplitASurrogatePairWhenWidening` carries
   `SKIP_IF_COMPARING_MSODBCSQL()`. Tracked in AB#47767.
10. A bound `time` / `datetimeoffset` column strides by
    `sizeof(SQL_SS_TIME2_STRUCT)` (12) and
    `sizeof(SQL_SS_TIMESTAMPOFFSET_STRUCT)` (20) rather than by `BufferLength`.
    Both drivers resolve these to the same C types under ODBC 3.8
    (`rgbTRANSTYPE380`, `sqlcmisc.cpp:220-221`), but msodbcsql's `BindOffset`
    switch has no case for them and falls through to
    `default: dwOffset = lpbindinfo->cbValueMax` (`sqlcfunc.cpp:2280-2283`).
    Measured: a two-row rowset bound `SQL_C_DEFAULT` with `BufferLength` 40 puts
    msodbcsql's second row at byte offset 40, where this driver puts it at 12;
    the indicator is 12 in both, so only the stride differs. This is the safer
    direction — msodbcsql with `BufferLength` 0 strides 0 and stacks every row in
    slot 0 — so the behaviour is kept and registered rather than matched.
    Pre-existing for an explicit `SQL_C_SS_TIME2` bind; deferred
    `SQL_C_DEFAULT` resolution makes it reachable without the application naming
    the C type.
11. A `SQL_C_DEFAULT` binding that resolves to a fixed-width C type wider than
    the application's declared `BufferLength` is left unresolved and fails the
    row (`HYC00`) rather than writing. `BufferLength` is ignored for a
    fixed-width target, which is safe when the application named that type; a
    `SQL_C_DEFAULT` binding names nothing, so honouring the C type's width would
    put 16 bytes into a 4-byte slot for a `uniqueidentifier` column, where
    msodbcsql resolves to `SQL_C_CHAR` and truncates inside `BufferLength`.
    `BufferLength` 0 is exempt — the documented idiom for a fixed-width target,
    carrying no width claim. Whether these should instead report `01004` with a
    truncated value, closer to msodbcsql, is open and untracked.
12. A `varbinary` / `image` / CLR UDT column bound `SQL_C_DEFAULT` resolves to
    `SQL_C_BINARY` (`describe_col.rs` → `SQL_VARBINARY` / `SQL_LONGVARBINARY` /
    `SQL_SS_UDT`, then `resolve_default_c_type`), which bound delivery does not
    implement yet (AB#47239), so it fails per row with `HYC00`. For `varbinary`
    and `image`, deferred resolution exposes the pre-existing explicit
    `SQL_C_BINARY` gap without the application naming the C type. A UDT's former
    `SQL_C_CHAR` default was already unsupported, so the new mapping does not
    regress observable fetch behavior. msodbcsql resolves all three to
    `SQL_C_BINARY` and delivers the bytes.

## No panics

- **Never** use `.unwrap()` or `.expect()` on `Result` or `Option` in
  non-test code. Tests under `#[cfg(test)]` may use them since panics
  there are caught by the test harness.
- Use `.unwrap_or()`, `.unwrap_or_else()`, `.unwrap_or_default()`, or
  pattern matching instead.
- For `Mutex::lock()`, return `SQL_ERROR` on poison — use `let Ok(state) = handle.inner.lock() else { return SQL_ERROR; }`. Do **not** recover via `e.into_inner()`.
- Every FFI entry point must be wrapped in the `crate::ffi_entry!` macro
  (see [FFI boundary conventions](#ffi-boundary-conventions)). The macro is
  a last-resort safety net — write code that cannot panic in the first place.
- Never use `unreachable!()`, `todo!()`, or `unimplemented!()` in non-test code.
  Use explicit error returns instead.
- Array/slice access: prefer `.get()` over indexing (`[]`), which panics on
  out-of-bounds.

## Error handling

- All fallible internal functions should return `Result<T, E>` — never panic on
  failure.
- Map errors early: convert `Result` from external crates into the crate's own
  error types at the call site.
- At FFI boundaries, convert every `Result::Err` into the appropriate
  `SqlReturn` code (`SQL_ERROR`, `SQL_INVALID_HANDLE`, etc.).
- Store diagnostic info on the handle so `SQLGetDiagRec` / `SQLGetDiagField`
  can report it — don't discard error details. Three posters, choose by source:
  - `post_diag(state, DiagMsg)` — **preferred for driver-raised diagnostics
    that have a canonical SQLSTATE + message.** A `DiagMsg` bundles a fixed
    SQLSTATE with its message text into a single `ERR_*` constant in
    `sqlstate.rs` (e.g. `ERR_INVALID_CURSOR_STATE`, `ERR_FUNCTION_SEQUENCE`,
    `ERR_CONNECTION_DOES_NOT_EXIST`). This keeps a call site from pairing a
    message with the wrong SQLSTATE and defines a reused message exactly once,
    mirroring msodbcsql's `IDS_*` resource entries. In new code, prefer
    adding/using an `ERR_*` `DiagMsg` constant over inlining `post_sql_error`
    with a literal — especially when the same `(SQLSTATE, message)` pair
    appears, or could appear, in more than one place.
  - `post_sql_error(state, sqlstate, native, message)` — the lower-level
    primitive behind `post_diag`. Use it directly only for genuinely one-off
    or **dynamic** messages (text computed at runtime) that don't warrant a
    constant. Posts exactly one record.
  - `post_tds_error(state, &tds_err, default_sqlstate)` — for any
    `mssql_tds::TdsError` bubbling up from the protocol layer. For
    `TdsError::SqlServerError` it fans out to one record per server-reported
    error, mapping each error number to a SQLSTATE via the static
    `SERVER_ERROR_TO_SQL_STATE_MAP` and falling back to the message's TDS
    severity class when the number is unmapped (`> 18` → `HY000`, `> 10` →
    `42000`, else `01000`, matching msodbcsql's `sqlcerr.cpp:1385-1401`); for
    other variants it posts a single record using `default_sqlstate`. Pick
    `08001` for connect-time failures and `HY000` for execution/fetch
    failures. Do not add rows to `SERVER_ERROR_TO_SQL_STATE_MAP` to correct a
    single error's SQLSTATE — entries there are a permanent compatibility
    commitment, and the severity fallback already covers unmapped errors.
  Never hand-roll `post_sql_error` over a `TdsError` — you lose the
  per-server-error fan-out and the SQLSTATE mapping.
- Every ODBC entry point must clear the handle's diagnostic records at API
  entry by calling `free_errors(...)` after acquiring the handle lock, so a
  fresh call starts without stale diagnostics.

## Unsafe code

- Minimize `unsafe` blocks — keep them as small as possible and comment
  the safety invariant they rely on.
- All raw-pointer writes must be guarded by a null check first.
- For the ubiquitous "write to caller out-param if non-null" pattern, use
  `crate::api::util::write_if_some(ptr, value)` instead of hand-rolling
  `if !ptr.is_null() { unsafe { ptr.write(v) } }`. The helper is the single
  audited chokepoint for that pattern. Skip the helper only when an outer
  null check guards expensive work that should be elided on null
  (e.g., looking up a value before writing it).
- Never dereference a pointer received from C without validating it.
- **Never assume an application buffer is aligned.** ODBC does not require the
  application to align `ParameterValuePtr`, `TargetValuePtr`, or
  `StrLen_or_IndPtr` for the type being transferred — an app may point at an
  offset inside a packed struct or a byte array. Read and write them with
  `read_unaligned` / `write_unaligned` (or `copy_*` helpers) rather than `*ptr`,
  `ptr::read`, or a reference. This is not defensive: in Rust a misaligned plain
  read is undefined behavior on *every* target, not just the ones that fault, and
  the optimizer is entitled to exploit it. msodbcsql reaches the same conclusion
  in C++ by qualifying every one of these accesses `UNALIGNED` (MSVC's
  `__unaligned`) — see `Sql/Ntdbms/sqlncli/odbc/sqlccnvt.cpp:1677-1714`, where
  each integer source read from an application buffer is
  `*(UNALIGNED SCHAR *)` / `SHORT` / `LONG` and so on.
- Use `#[unsafe(no_mangle)]` only in `exports.rs` — keep implementations
  in separate modules as `pub(crate)` safe functions.

### Safe-core / unsafe-shell split

Push `unsafe` to the edges. Each FFI implementation is split into two layers:

- A thin `unsafe fn sql_xxx_impl(...)` **shim** whose only job is to turn raw
  C pointers into validated Rust references:
  1. Null-check the handle → `SQL_INVALID_HANDLE`.
  2. `unsafe { handle_from_raw::<T>(handle) }` to obtain `&T`.
  3. `debug_assert_eq!(h.object_type, HandleType::X)` to catch DM contract
     violations in debug builds.
  4. Decode any input strings (`read_utf16`, etc.).
  5. Delegate everything else to the safe core.
- A safe `fn sql_xxx_safe(handle: &T, ...) -> SqlReturn` **core** that holds all
  business logic: locking, state mutation, value mapping. It receives validated
  references (never raw handle pointers) and only opens small, locally-justified
  `unsafe { write_if_some(...) }` / `unsafe { copy_with_nul(...) }` blocks to
  write to caller out-pointers.

Rules of thumb:

- A function should be a **safe `fn`** (even if it contains `unsafe {}` blocks)
  whenever it can discharge the safety obligation from its own arguments and
  invariants — e.g. an accessor like `StmtHandle::parent_dbc(&self) -> &DbcHandle`,
  where `&self` already guarantees a valid handle.
- A function should be an **`unsafe fn`** only when it relies on an
  unverifiable caller promise — e.g. the validity of a raw pointer passed
  across the FFI boundary (the `*_impl` shims).
- Validation that only inspects scalar arguments (e.g.
  `debug_assert!(buffer_length >= 0, ...)`) belongs in the safe core, not the
  shim. The shim should be limited to null-checks and pointer→reference
  conversion.
- Preconditions the DM is contractually required to enforce (non-null required
  pointers, valid length/option values, correct handle type) are checked with
  `debug_assert!` only — **do not** promote them to a release-build
  `if`/error-return. The assert documents the DM contract and catches
  violations in debug builds; in release the driver trusts the DM, matching
  msodbcsql (which asserts rather than re-validates). Asserts worded
  *"... — DM should have rejected this"* are intentionally debug-only; leave
  them as `debug_assert!`. Only values the DM does **not** validate (genuine
  application inputs) get a runtime check.

## Memory management

- **Same side allocates and frees.** Whoever produced an allocation owns
  freeing it; the FFI boundary never transfers deallocation responsibility:
  - Rust-allocated memory (`Box`, `Vec`, `String`, anything from
    `Box::into_raw` / `handle_to_raw`) must be freed by Rust via the
    matching `SQLFreeHandle` / `Box::from_raw` path. Never expect the
    caller (DM or app) to `free()` it, and never `mem::forget` it without
    a paired free path.
  - Caller-provided out-buffers (`*mut SQLCHAR` for `SQLGetData`, output
    pointers for `SQLDescribeCol`, etc.) are owned by the caller. Write
    into them, but never `free`, `realloc`, or wrap them in a `Box` —
    doing so hands them to Rust's allocator and corrupts the caller's
    memory.
- Prefer `Box` for single-owner heap objects; use `Arc` only when shared
  ownership is genuinely required.

## Concurrency

- The ODBC spec allows Driver Manager to call functions on the same handle
  from different threads. Protect mutable state with `Mutex` or `RwLock`.
- Keep lock scopes narrow — lock, copy/update, unlock. Never hold a lock
  across an FFI call or I/O operation.
- Handle poison explicitly with `std::sync::Mutex` — see the no-panics
  rule above for the canonical `let Ok(state) = ... else { return SQL_ERROR; }`
  pattern.

### Cross-handle thread safety (alloc / free)

ODBC handles form a parent–child hierarchy (ENV → DBC → STMT → DESC). The
Driver Manager (DM) provides serialization guarantees that the driver relies on
- verified against msodbcsql's behavior:

#### DM guarantees we rely on

- The DM ensures all child handles are freed before freeing a parent:
  all DBCs freed before `SQLFreeEnv`, all STMTs freed before `SQLFreeConnect`.
- `SQLAllocHandle(STMT)` and `SQLFreeHandle(DBC)` cannot race on the same DBC.
  The DM enforces this via the ODBC connection state machine: `SQLAllocStmt`
  requires state C4+ (connected), while `SQLFreeHandle(DBC)` requires state C2
  (disconnected). These are mutually exclusive states, so the DM rejects one
  before it ever reaches the driver. The same logic applies to ENV: `SQLFreeEnv`
  requires no outstanding DBCs, which the DM verifies first. This means the
  parent handle and its mutex are guaranteed alive during child allocation.
- The DM ensures the DBC is disconnected before calling `SQLFreeConnect` via
  call to `SQLDisconnect`, and `SQLDisconnect` automatically drops all
  statements and descriptors.

#### Locking rules (mirroring msodbcsql)

- **Alloc path**: Lock the parent's mutex to register the new child in its list.
- **Free path**: Lock the parent's mutex to unregister from its child list.
- **Lock ordering**: Always lock parent before child (ENV before DBC, DBC before
  STMT) to prevent deadlocks. Always acquire the parent lock before the child lock.
- **DESC is a sibling of STMT, not a child**: a descriptor's parent is the DBC
  (`DescHandle::parent_dbc`), not the statement it happens to be associated
  with — an explicit descriptor can be reassociated across statements, or
  shared by several at once. The free path (`free_desc`, `free_handle.rs`)
  walks DBC → STMT to clear a freed descriptor's association from every
  statement that had it active, so the STMT lock and a DESC lock must never
  nest the other way: **never hold a STMT lock while acquiring a DESC lock**.
  Every entry point that both validates STMT state and writes to a
  descriptor (`SQLBindCol`, `SQLBindParameter`, `SQLFetchScroll`,
  `SQLFreeStmt(SQL_UNBIND | SQL_RESET_PARAMS)`, execute's parameter
  snapshot) follows the same two-phase shape: lock STMT, validate and
  resolve the target descriptor handle (`effective_ard`/`effective_apd`),
  drop the STMT lock, *then* lock the descriptor. A descriptor pointer
  resolved this way can be freed by a concurrent `SQLFreeHandle` before it
  is dereferenced; re-check `handles::live_type` immediately before the
  dereference to fail cleanly instead of touching freed memory.
- **APD before IPD**: `SQLBindParameter`'s `bind_param_records` is the only
  place in this crate that holds two DESC locks at once (writing a
  parameter's APD and IPD records together). It locks APD before IPD, and
  that must stay the only order used anywhere both are locked together —
  `BoundParam::all_from_descriptor_states` (used by
  `snapshot_bound_params`) only ever reads them, never locks both
  simultaneously, so it does not need to follow this rule itself.
- **`debug_assert!` for DM invariants**: The free path uses `debug_assert!` to
  verify the DM upheld its guarantees (e.g., no outstanding children). These
  fire in debug builds only — in release builds the driver trusts the DM and
  frees unconditionally, matching msodbcsql.
- **Known gap: `SQLSetDescRec`/`SQLSetDescFieldW` don't check
  `STMT_STATE_FETCH_IN_PROGRESS`**: `SQLBindCol`, `SQLFreeStmt(SQL_UNBIND)`,
  and `SQLSetStmtAttr` all refuse to touch the ARD while a fetch snapshotted
  it and is still writing through that snapshot — but the descriptor-field
  API writes the same records with no such guard, and (unlike those three)
  would need a DBC → STMT walk to find every statement an explicit,
  possibly-reassociated descriptor is currently associated with. Tracked in
  [#472](https://github.com/microsoft/mssql-rs/issues/472); this is a
  deliberate deferral, not an oversight.

## FFI boundary conventions

- Every exported function goes through `exports.rs` as a thin
  `pub extern "C"` wrapper.
- The wrapper calls a `pub(crate)` implementation function that contains
  the real logic.
- **Every FFI implementation function MUST wrap its body in the
  `crate::ffi_entry!` macro.** This is non-negotiable — it is the single
  panic boundary that converts a Rust panic into `SQL_ERROR` instead of
  unwinding across the C ABI (undefined behavior).
  Shape:

  ```rust
  pub(crate) unsafe fn sql_xxx(/* raw args */) -> SqlReturn {
      debug!(/* all args */, "SQLXxx called");
      crate::ffi_entry!("SQLXxx", unsafe { sql_xxx_impl(/* raw args */) })
  }

  // Thin unsafe shim: raw pointers -> validated references, then delegate.
  unsafe fn sql_xxx_impl(/* raw args */) -> SqlReturn {
      if handle.is_null() { return SQL_INVALID_HANDLE; }
      let h = unsafe { handle_from_raw::<XxxHandle>(handle) };
      debug_assert_eq!(h.object_type, HandleType::Xxx);
      sql_xxx_safe(h, /* scalar/decoded args */)
  }

  // Safe core: all business logic; only small unsafe out-pointer writes.
  fn sql_xxx_safe(handle: &XxxHandle, /* args */) -> SqlReturn {
      // ...
  }
  ```

  See [Safe-core / unsafe-shell split](#safe-core--unsafe-shell-split) for the
  full rationale.

- The first line of every FFI implementation function must be a `debug!` log
  of every argument (pointers logged with `?` — no deref).
- The `pub extern "C"` wrapper in `exports.rs` must call
  `crate::init_tracing()` before delegating to the impl — `ffi_entry!` does
  not initialize tracing itself.
- Never call `std::panic::catch_unwind` directly in this crate; always go
  through `ffi_entry!` so the panic-log message, return-code mapping, and
  trailing trace are uniform.
- Use `SqlReturn` (not raw `i16`) as the return type of internal functions
  to keep intent clear.
- Pointer parameters from C must be treated as potentially null, invalid, or
  misaligned — validate before use.

## Types and casts

- Use explicit types for FFI: `SqlSmallInt`, `SqlHandle`, `SqlReturn` — never
  raw `i16` / `*mut c_void` in business logic.
- Avoid `as` casts for numeric conversions — use `TryFrom` / `TryInto` and
  handle the error. `as` silently truncates.
- Pointer casts between handle types must go through the well-defined
  conversion functions in `crate::handles`: `handle_to_raw`,
  `handle_from_raw`, `handle_from_raw_mut`, `free_handle`.

## Testing

- Unit tests for pure logic go in `#[cfg(test)]` modules inside the source file.
- Allocate ODBC handles in unit tests **only** through
  `crate::test_support::TestHandles`:
  - Use `with_env()`, `with_env_dbc()`, `with_env_dbc_stmt()`, or
    `alloc_extra_stmt()` to get the handle chain you need; access via
    `.env` / `.dbc` / `.stmt`.
  - Never free handles manually — `TestHandles::Drop` frees them
    child-before-parent (the order `SQLFreeHandle` requires). Manual
    `sql_free_handle` calls risk double-frees.
  - If you need a handle shape the constructors don't cover, extend
    `TestHandles` rather than open-coding allocation in the test.
- End-to-end tests that exercise the loadable `.so`/`.dll` through a real
  Driver Manager live in `tests/e2e/` as a CMake-built C++ suite (run via
  `tests/e2e/run_e2e.sh` / `.ps1`).
- Tag any live e2e test that can only assert the observable *outcome* (a value
  round-trips, the connection stays healthy) and cannot see the underlying TDS
  RPC sequence with a `Benefits-from-mock-tds:` comment above the `TEST_F`,
  noting what a byte-level mock TDS server would let it assert (e.g. that an
  `sp_unprepare` / `sp_prepexec` `@handle` drop actually fired). Pin the exact
  behavior with a Rust unit test meanwhile; `grep -rn Benefits-from-mock-tds`
  surfaces every such test to tighten once mock-TDS support lands.
- If an e2e test asserts mssql-odbc-specific behavior the full msodbcsql driver
  does not share (e.g. a Phase-1 "not implemented" response), start it with the
  `SKIP_IF_COMPARING_MSODBCSQL()` macro so it self-skips on the msodbcsql leg of
  a `--compare-with-msodbcsql` run instead of failing the parity binary.
- Every new `SQLXxx` function must have at least:
  - A success-path test.
  - A null-output-handle test.
  - An invalid-handle-type or invalid-input test.
- **A `max`/LOB column only reaches the bound *streaming* path when it is too
  large for the transport to buffer whole.** `try_read_buffered_column` decodes
  any fully buffered column - PLP included - into a `ColumnValues`, so a small
  `varchar(max)`/`varbinary(max)` is delivered by `deliver_bound`, exactly like a
  non-max value, and never enters `deliver_bound_plp`. A few kilobytes still
  buffers; the streaming path is reachable around a megabyte (the existing 1 MiB
  cases in `fetch_scroll_test.cpp` prove it). Size an e2e that targets
  `deliver_bound_plp` accordingly, or it will silently assert the non-PLP path -
  a bound binary `max` column against a typed C target answers `22018` from the
  typed converter when buffered, and `HYC00` when streamed.
- Use `cargo nextest` (via `cargo btest`), not `cargo test`.

## Code style

- Follow the conventions in the repo-level
  [copilot-instructions.md](../.github/copilot-instructions.md).
- Every `.rs` file starts with the copyright header.
- Prefer `pub(crate)` over `pub` for internal APIs.
- No AI-slop comments — don't restate what the code already says.

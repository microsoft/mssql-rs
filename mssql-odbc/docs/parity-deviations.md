# Parity with msodbcsql

This is the registry of deliberate behavioral deviations from msodbcsql. Follow
the [ODBC engineering instructions](../../.github/instructions/mssql-odbc.instructions.md)
for the authoritative parity, evidence, and testing requirements.

## Updating this document

Entries here are **decisions**: we know what msodbcsql does, we could match it,
and we chose not to.

When a change introduces or preserves a deliberate deviation that meets the
criteria below, add or update its numbered entry in this document in the same
change. Do not leave the decision only in code comments, a pull-request
description, or a work item.

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
does instead; why; and, where applications can regress, who signed off and
when. Link a work item when one tracks follow-up work, measurements, or decision
history; a work item is not required for a settled deviation. When one exists,
keep per-build measurements there so this file does not grow every time a new
msodbcsql build is measured.

## Deliberate deviations

1. `ActiveDirectoryManagedIdentity` is accepted as an alias for managed-identity
   authentication. msodbcsql recognizes only `ActiveDirectoryMSI`
   (`Sql/Ntdbms/sqlncli/msdart/inc/dlgattr.h` → `OPTIONADMSI L"ActiveDirectoryMSI"`);
   `ActiveDirectoryManagedIdentity` does not appear anywhere in the msodbcsql source.
   Added to match MS Learn and the sibling drivers (JDBC/.NET/go-sqlcmd). Tracked in AB#46066.
2. `SQL_C_DEFAULT` resolves the wide character SQL types to `SQL_C_WCHAR`, and
   `SQL_GUID` to `SQL_C_GUID`, following the ODBC 3.x default-C-type table.
   Applies to every direction: `SQLBindParameter` resolves at bind time,
   `SQLFetchScroll` resolves a bound column per fetch from the IRD, and
   `SQLGetData` resolves per call from the same column metadata, all through the
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
7. **A `max`/LOB text column converted to a typed C target is refused
   above 1 MiB; msodbcsql converts a truncated prefix and warns.** Both drivers
   cap what a typed conversion may materialize - a `varchar(max)` carries up to
   2 GB and the converter needs one contiguous literal. This driver's cap is
   `PLP_TYPED_MATERIALIZE_LIMIT` (`api/fetch_scroll.rs`) at 1 MiB; past it the
   value is drained to keep the row synchronized and answered `HYC00`.
   This applies to bound fetches and `SQLGetData`; the shared limit counts
   unread source wire bytes, including both bytes of each UTF-16 code unit.
   Earlier character reads do not count against a subsequent typed
   `SQLGetData` call's cap. Decoding
   can expand that bounded input into UTF-8, but allocation never scales with
   an unbounded server value. Below the cap, this driver converts the complete
   remaining literal, not a truncated prefix.
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
   `SQLGetData` was re-measured on Linux with **18.06.0001** for AB#47238:
   `'0'` repeated 1048576 times followed by `'1'`, as either `varchar(max)` or
   `nvarchar(max)`, returns `SQL_SUCCESS_WITH_INFO`, `01004`, integer `0`, and
   indicator `4`. This driver returns `SQL_ERROR` / `HYC00` without changing
   either output, and both can retrieve the following column. The dedicated
   `PlpTypedOversizedValueIsRefusedAndDrained` test asserts each driver's
   result. The native caller clamps fixed conversions in `GetColData`'s
   delivery implementation (`odbc/sqlcdata.h`, `IsFixedOrBinaryWithFixedServerType`
   branch) before `FetchDataWithCopy`, consistent with `EstimateBytesToRead`.
   Reusing the bound-fetch policy for `SQLGetData` was approved by David Engel
   on 2026-09-22. Tracked in AB#47767 and AB#47238.
8. **Widening a bound narrow `max` column to `SQL_C_WCHAR` truncates on a whole
   character.** A buffer with no room for the final surrogate pair ends before
   it; msodbcsql leaves the lone high surrogate in the last payload slot on this
   narrow-source widening path, though its wide-source path is surrogate-safe
   (`GetColDataSurrogateSafe`, and `TrimPartialCodePt` for partial sequences).
   This driver's existing bound `nvarchar(max)` delivery already trims to a
   character boundary, and handing back text that does not decode from one `max`
   type but not the other would be worse than the divergence.
   `ABoundUtf8VarcharMaxDoesNotSplitASurrogatePairWhenWidening` carries
   `SKIP_IF_COMPARING_MSODBCSQL()`. Tracked in AB#47767.

   The same rule applies to a bound UTF-8-collation `varchar(max)` delivered as
   `SQL_C_CHAR`, which is verbatim on both drivers because the wire bytes are
   already UTF-8. Measured on build 173919 with a 3-byte character against an
   8-byte payload slot: msodbcsql fills all 8 and returns
   `"\xE4\xBD\xA0\xE4\xBD\xA0\xE4\xBD"`, ending mid-character, where this driver
   stops at 6. `ABoundUtf8CollationVarcharMaxTruncatesOnACharacterBoundary`
   splits per-leg on `ODBC_TEST_TARGET` rather than skipping, so the shared part
   — both truncate, report `01004`, and deliver a prefix — stays measured.
9. A bound `time` / `datetimeoffset` column strides by
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
10. A `SQL_C_DEFAULT` binding that resolves to a fixed-width C type wider than
    the application's declared `BufferLength` is left unresolved and fails the
    row (`HYC00`) rather than writing. `BufferLength` is ignored for a
    fixed-width target, which is safe when the application named that type; a
    `SQL_C_DEFAULT` binding names nothing, so honouring the C type's width would
    put 16 bytes into a 4-byte slot for a `uniqueidentifier` column, where
    msodbcsql resolves to `SQL_C_CHAR` and truncates inside `BufferLength`.
    `BufferLength` 0 is exempt — the documented idiom for a fixed-width target,
    carrying no width claim. `SQLGetData` refuses the same shape but does **not**
    carry that zero exemption, because there 0 is *also* how an application asks
    for a length without wanting a value written (the `SQL_C_BINARY` probe), so
    honouring the C type's width would put up to 20 bytes into a buffer the
    caller declared as holding none. Whether these should instead report `01004`
    with a truncated value, closer to msodbcsql, is open and untracked.
11. A zero-length `SQL_C_BINARY` `SQLGetData` on a column whose **source SQL
    type is fixed-length** reports `01004` / `SQL_SUCCESS_WITH_INFO`, where
    msodbcsql reports `22003` / `SQL_ERROR` and leaves the indicator untouched.
    The indicator carries a byte count where `binary_length` has an explicit arm
    (`int` → 4, `money` → 8, `uniqueidentifier` → 16) and `SQL_NO_TOTAL` where it
    falls through to the catch-all. That `SQL_NO_TOTAL` class is **decimal and
    numeric as well as every temporal type** (`date`, `time`, `datetime2`,
    `datetimeoffset`) — this driver has no binary encoding for any of them to
    promise a length for.
    **Source-verified.** msodbcsql selects between two policy classes on
    `IsFixedSqlType()` (`Sql/Ntdbms/sqlncli/odbc/sqlcprot.h`), which
    deliberately classifies `SQL_BINARY`, `SQL_CHAR`, `SQL_WCHAR` and every
    partial-length type (`sqlcprot.h`, `IsPartialLenType`) as *not* fixed —
    so the boundary is the **SQL** type, not the C type, and `binary(9)` sits
    on the variable side, while `decimal` / `numeric` sit on the fixed side
    (absent from both the `IsPartialLenType` case list and the
    `IsFixedSqlType` exclusion set). `ColDataRetriever<>::GetColData`
    (`odbc/sqlcdata.h`) instantiates `BinaryOutputWithFixedLengthSqlType` for a
    fixed source type, where delivery to `SQL_C_BINARY` is all-or-nothing (the
    source comments that such data "is fetched in one call"), so a short buffer
    is a data overflow rather than a truncation:
    `if (... == BinaryWithFixedLengthSqlType && (SIZE_T)cbBuf < cbDataAvail)
    { wError = IDS_22_003; }` in `InternalGetColData`, and `Error = CVT_PREC`
    (`#define CVT_PREC IDS_22_003`, `sqlcprot.h`) out of `ConvertToBinary`
    (`odbc/sqlccnvt.cpp`) on the converting route that `datetime2` takes.
    `IDS_22_003` maps to `22003` in `cli_common/src/clntcomn.cpp`. A variable
    source type instead reads `min(cbBuf, avail)` bytes and reports
    `if (!IsFixedOrBinaryWithFixedServerType() && cbDataAvail)
    wError = IDS_01_004;` with the full remaining length in the indicator.
    Line numbers are deliberately omitted: the reading is from `master`
    (`7a0c3d59`), not the 18.6.2.1 release branch, so the file + function +
    condition are the durable part of the citation.
    **Measured against 18.6.2.1** (`SQL_DRIVER_VER` `18.06.0002`) — every
    fixed-source type below answers `SQL_ERROR` / `22003` there, and `01004` /
    `SQL_SUCCESS_WITH_INFO` here, differing only in the indicator this driver
    reports:

    | column | msodbcsql | this driver |
    |---|---|---|
    | `int`, `money`, `smallmoney`, `uniqueidentifier` | `22003` | `01004`, byte count (4 / 8 / 4 / 16) |
    | `decimal(10,2)`, `decimal(38,10)`, `numeric(18,4)` | `22003` | `01004`, `SQL_NO_TOTAL` |
    | `date`, `time(7)`, `datetime2(3)`, `datetimeoffset(3)` | `22003` | `01004`, `SQL_NO_TOTAL` |
    | `binary(9)`, `varbinary(max)`, `nvarchar(10)` | `01004` + length | `01004` + length (agrees) |

    This driver does not distinguish a `sql_variant` column from the value it
    captured, and mssql-python's `sql_variant` support depends on that same
    zero-length probe succeeding (`ddbc_bindings.cpp`,
    `SQLGetData_ptr(hStmt, i, SQL_C_BINARY, NULL, 0, ...)` gated on
    `SQL_SUCCEEDED`), so matching `22003` would break every integer variant. The
    reported truncation is the important half — it is what stops a caller
    treating an undelivered value as delivered (AB#47537) — and the exact
    fixed-width SQLSTATE is left to AB#47239, which reworks binary delivery.
12. A `datetimeoffset` value converted to any target other than
   `SQL_C_SS_TIMESTAMPOFFSET` retains its written wall-clock fields after the
   offset is validated. msodbcsql instead shifts those fields into the client
   machine's local time zone through `ConvertOffsetToLocal`
   (`odbc/sqlccnvt.cpp`, defined at line 9084 and called from the conversion
   paths at lines 3926 and 4836). Matching that
   behavior would make the returned value depend on the client machine's time
   zone, so this driver deliberately ignores the offset for non-offset
   targets. `offset_is_ignored_for_non_offset_targets` pins parsed character
   input, and `DatetimeoffsetIntoSsTime2Succeeds` pins a native
   `datetimeoffset` column while skipping the msodbcsql comparison leg.

   Evidence level: source reading only. No `SQL_DRIVER_VER` or tested build is
   recorded, and unlike entry 14 nothing prevents measuring this — the
   divergence is reachable through a Driver Manager, so a
   `--compare-with-msodbcsql` case that fetches a `datetimeoffset` into a
   non-offset target on an agent whose time zone is not UTC would close it.
   That case does not exist yet: the Rust test covers only this driver, and the
   e2e case skips the reference leg. The claim predates this change — it was
   carried over verbatim from the deviation table in
   `docs/typed-columnar-fetch-plan.md`, which recorded the same behavior, the
   same citations and the same skipped leg — so promoting it into this registry
   neither introduced nor closed the gap.

   This policy applies to both bound-column and `SQLGetData` conversions.
13. A zero-length `SQL_C_BINARY` probe of a `sql_variant` wrapping an empty
   value reports `SQL_SUCCESS`, while msodbcsql reports
   `SQL_SUCCESS_WITH_INFO` / `01004`. Measured against msodbcsql
   `18.6.2.1` (`SQL_DRIVER_VER` `18.06.0002`), the build pinned by
   `msodbcsqlVersion` in
   `.pipeline/validation-pipeline.yml`. A bare empty `varbinary(8)` reports
   `SQL_SUCCESS` in both drivers. Matching the variant-only warning would
   require preserving wrapper identity after the value has been captured;
   this driver deliberately treats the captured value like its base type in
   both `SQLGetData` delivery and the subsequent
   `SQLColAttribute(SQL_CA_SS_VARIANT_TYPE)` lookup. In
   `InternalGetColData` (`odbc/sqlcdata.h`, around line 542), msodbcsql posts
   `IDS_01_004` when the output policy is binary, `cbBuf == 0`, and
   `IsVariantColumn(pColInfo)`; the warning is gated by the zero user buffer
   and variant wrapper, not by the amount of data available.
   The difference is invisible to mssql-python, whose probe is gated on
   `SQL_SUCCEEDED`. `EmptyVariantProbeConsumesValueButKeepsBaseType` accepts
   either successful return so its parity leg can still compare the base type
   and the subsequent `SQL_NO_DATA` read;
   `EmptyVariantProbeReturnsSuccessWithoutWarning` asserts each driver's exact
   return rather than skipping the reference leg, so the measurement is
   re-taken on every `--compare-with-msodbcsql` run and a future msodbcsql
   build that stops warning here fails that test instead of going unnoticed.
   Signed off by Theekshna Kotian on 2026-09-17.
14. `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC2)` is rejected with
   `SQL_ERROR` / `HY024`, and the previously selected version is left
   unchanged. msodbcsql accepts it: `SQLSetEnvAttr`
   (`odbc/sqlcmisc.cpp`, line 1021) validates only the attribute's range and
   then stores the value verbatim
   (`lpEnv->dwOptionsE[fAttribute] = (UINT_PTR)rgbValue;`), with no check on
   the version itself; `IS2xAPPE` (`odbc/sqlcprot.h`, line 1546) reads it back
   as `SQL_OV_ODBC2`, and the driver then branches on it throughout — for
   example `odbc/sqlcconn.cpp`, line 585.
   Evidence level: this rests on a source reading, not a direct-export
   measurement. `SQLSetEnvAttr` performs no validation of the value at all, so
   the reading is unambiguous, but no observed `SQL_DRIVER_VER` is recorded
   here because the claim is about the *driver's* exported entry point and the
   Driver Manager intercepts `SQL_ATTR_ODBC_VERSION` before the driver is
   loaded. `SetGetOdbcVersion2` does not close this: its own comment notes that
   no driver is loaded at that point, so it measures the DM, not msodbcsql.
   Loading msodbcsql directly and calling its exported `SQLSetEnvAttr` with
   `SQL_OV_ODBC2` would close it.
   This driver supports no ODBC 2.x application contract, so accepting the
   declaration and then behaving as 3.x would be the worse outcome: the
   application would be told its request succeeded while silently receiving
   3.x identifiers, column names, and defaults. The rule is recorded in
   §2.2 of the engineering instructions ("Do not add ODBC 2.x application
   behavior to the driver"), and removing it took version branches out of
   `catalog.rs`, `describe_param.rs`, `get_type_info.rs`, `type_rules.rs`,
   and `handles/env.rs`.
   The refusal is carried through to the connection, which is the part an
   application actually notices. The Driver Manager does **not** map a 2.x
   declaration onto 3.x on the driver's behalf — cited, not inferred: unixODBC
   replays the application's value verbatim onto the driver environment,
   passing `connection->environment->requested_version` straight into the
   driver's `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION, ...)`
   (`DriverManager/SQLConnect.c`, lines 1532-1538).
   The sequence is therefore: the DM stores `SQL_OV_ODBC2` and answers the
   application; `SQLAllocHandle(SQL_HANDLE_DBC)` also succeeds, because
   unixODBC services that call entirely inside the DM — its only gate is
   `requested_version == 0`, and no driver is consulted — so it says nothing
   about this driver. The driver is loaded at `SQLDriverConnect`, the DM
   replays the environment at `:1532`, this driver's `SQLSetEnvAttr` rejects
   `SQL_OV_ODBC2`, nothing is recorded, and the driver-side allocation at
   `:1599` reaches this driver's `SQLAllocHandle(SQL_HANDLE_DBC)`, which
   refuses with `HY010` — the SQLSTATE ODBC defines for allocating a
   connection before `SQL_ATTR_ODBC_VERSION` is set.
   What the application finally reads is Driver-Manager-specific, measured in
   build 176958: unixODBC posts its own `IM005` (`:1613-1616`), "Driver's
   SQLAllocHandle on SQL_HANDLE_DBC failed", wrapping the `HY010`; the Windows
   Driver Manager instead propagates this driver's `HY024` from the rejected
   `SQLSetEnvAttr`. Both platforms fail the connect — that part is invariant —
   so `Odbc2ApplicationIsRefused` asserts the failure plus either SQLSTATE
   rather than pinning one Driver Manager's wrapping. `HY010` remains what
   this driver posts on its own environment handle, which no Driver Manager
   application holds.
   One side effect worth knowing: unixODBC treats the rejection as evidence
   about the *driver* rather than the application — `if (ret) {
   connection->driver_version = SQL_OV_ODBC2; }`, commented "if it don't set
   then assume a 2.x driver" — so for that connection the DM classifies this
   driver as 2.x. It does not change the outcome here, since the connection is
   refused moments later, but it is the DM's reading of an `HY024` from this
   attribute.
   A 3.x application is unaffected by the gate: the env-attr replay at `:1532`
   runs before the driver-side allocation at `:1599`, so a supported version is
   always recorded first.
   Refusing beats proceeding, because proceeding would hand the application
   the 3.x contract it never asked for — `COLUMN_SIZE` where it expects
   `PRECISION`, `91`/`92`/`93` where it expects `9`/`10`/`11` — which it would
   read as its own. Failing at connect time is diagnosable; wrong metadata at
   fetch time is not.
   msodbcsql serves such an application instead: it accepts the declaration,
   and `SQLAllocConnect` asserts in debug builds only
   (`odbc/sqlcconn.cpp`, line 527) before setting the 2.x/3.5.1 flags on an
   exact match (line 587). **A real ODBC 2.x application therefore works
   against msodbcsql and cannot connect at all against this driver.** That is
   the intended consequence of not supporting ODBC 2.x, not an oversight.
   `Odbc2ApplicationIsRefused` measures both halves rather than skipping the
   reference leg: on the msodbcsql leg it asserts the connect *succeeds*, on
   the mssql-odbc leg that it is refused. The bolded claim above is therefore
   re-measured on every `--compare-with-msodbcsql` run instead of resting on a
   source reading, which is what §2.1 prefers for a test that exists solely to
   pin one registered divergence. It also doubles as the proof
   that the Driver Manager does not convert 2 to 3 — were it to convert, the
   version would arrive as `SQL_OV_ODBC3` and both legs would connect.
   `Odbc3ApplicationConnectsAndQueries` runs the identical sequence under
   `SQL_OV_ODBC3_80` to show the refusal is keyed on the declared version
   rather than the fixture, and `SetGetOdbcVersion2` covers only the Driver
   Manager's own bookkeeping, since it stops before a driver is loaded.
   One further residue: after the rejection this driver's `SQLGetEnvAttr`
   reports `0` for `SQL_ATTR_ODBC_VERSION`, where msodbcsql reports back the
   `2` it stored verbatim. Through a Driver Manager the DM answers the
   application, so this is visible only to a caller that loads the driver
   directly. Tracked in AB#48256.
   Signed off by Theekshna Kotian on 2026-09-18.
15. The TDS 8 user-agent `Driver Name` field is `MS-ODBCRS`; msodbcsql sends
   `MS-ODBC` (`tds/TdsSend.cpp`, around line 299). The classic Login7
   `ClientInterfaceName` field is not part of this deviation: this driver sends
   `ODBC` there to match msodbcsql's `pwszClientInterface` assignment in
   `odbc/sqlcconn.cpp` and the corresponding `L"ODBC"` assertion in
   `tds/TdsSend.cpp`, preserving `sys.dm_exec_sessions.client_interface_name`
   parity.

   The user-agent name intentionally distinguishes this Rust ODBC driver from
   the classic C++ driver in server-side telemetry while staying within the same
   Microsoft ODBC driver family. This mirrors the sibling binding precedent where
   Python sets a distinct user-agent driver name (`MS-PYTHON`) instead of using
   the generic TDS default. Tracked in #634.

   Evidence level: source reading only. No observed `SQL_DRIVER_VER` or tested
   msodbcsql build is recorded here because the claim is about a Login7 feature
   extension field that this driver's current parity suite can capture through
   `mssql-mock-tds`, but the comparison leg has no equivalent server-side user
   agent capture for msodbcsql. A parity measurement that connects msodbcsql
   18.6.2.1 (the build pinned in CI) to a server or proxy that records the TDS 8
   user-agent feature would close this evidence gap.
16. **Direct IPD field edits invalidate a cached plan when its SQL definition
    changes.** msodbcsql's `ParamInfoSnapshot::FHasChanged` in
    `Sql/Ntdbms/sqlncli/odbc/sqlcprot.h` is used by `SetIPDRec` in `sqlcdesc.cpp`,
    not by the direct `SQLSetDescFieldW` route. On retail 18.06.0001 through the
    Windows Driver Manager, an IPD INTEGER-to-SMALLINT field edit retained
    `sp_execute` and an INTEGER result; `SQLSetDescRec` reparsed as SMALLINT.
    This driver handles both routes consistently so the next execute reflects
    the changed SQL definition. Approved by David Engel on 2026-09-17 in the
    scope of [PR #564](https://github.com/microsoft/mssql-rs/pull/564).
    Retail 18.6.2.1 was not measured for this distinction; do not infer it from
    the driver's compatibility version string or add a parity-test skip.
17. **Special SQL types retain a conservative plan-invalidation comparison.**
    `DescRecord::parameter_definition` compares length, precision, and scale
    for types outside its named character/binary, numeric, and fixed/temporal
    arms, in addition to direction and SQL type. This differs from the
    reference-source comparison, independently of the setter-route difference
    in entry 16: at msodbcsql source `aa19092c`,
    `Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp` maps the public SQL identifiers before
    `SetIPDRec` invokes `ParamInfoSnapshot::FHasChanged` in `sqlcprot.h`.
    `IsSQLBinary` includes mapped UDT and `IsSQLWCHAR` includes mapped XML, so
    that comparison considers only their length; mapped vector is outside all
    shape-comparison groups, so it considers only direction and SQL type.
    This driver deliberately retains the broader fallback because special
    types can encode SQL shape in these fields, such as vector dimensions and
    element type. It may re-prepare for an irrelevant field change rather
    than risk retaining an obsolete declaration. This policy is part of the
    selective design in [PR #564](https://github.com/microsoft/mssql-rs/pull/564),
    not a change to which parameter types or conversions are supported.
    `special_parameter_definitions_keep_size_precision_and_scale` pins the
    vector and UDT projections.
    The UDT projection also carries the type's catalog, schema, and name, which
    `sp_executesql` spells out in the declaration; msodbcsql's `IsSQLBinary`
    grouping compares only length, so an application that rewrites
    `SQL_CA_SS_UDT_TYPE_NAME` between executes keeps the old declaration there.
    This driver re-prepares instead, for the same reason as the rest of this
    entry. The assembly-qualified name is excluded because it never reaches the
    wire. `the_udt_name_is_part_of_the_prepared_parameter_definition` pins both
    halves.
    **Evidence limit:** this is a source comparison and a Rust unit test,
    not a measured retail reuse/re-prepare claim. The earlier 18.06.0001 RPC
    measurements did not cover these special-type edits. A supported
    Driver Manager bind/record-edit sequence with RPC capture and a recorded
    `SQL_DRIVER_VER` is still needed to establish shipping-build behavior;
    do not infer retail parity or add a comparison-test skip from this entry.
18. **`Authentication=ActiveDirectoryPassword` is refused.** msodbcsql accepts
    it: `OPTIONADPASSWORD L"ActiveDirectoryPassword"`
    (`Sql/Ntdbms/sqlncli/msdart/inc/dlgattr.h`), carried through as
    `IntegratedSecurity::ActiveDirectoryPassword` (`tds/TdsParser.h:411`) and
    dispatched by the `authMode` ternary at `tds/Parse.cpp:3661`, which selects
    `AKVCFG_AUTHMODE_PASSWORD` and feeds `AzureADAuth::GetAccessTokenW`.
    This driver parses and validates the keyword - including the rule that it
    requires both `UID` and `PWD` - and then returns `HYC00` from
    `SQLDriverConnectW`, because `configure_auth` (`src/auth/entra.rs`) has no
    arm for it and returns `UnsupportedAuth`. The refusal
    happens before any network activity.
    Excluded by design rather than deferred: the ratified authentication design
    scopes the driver to "full msodbcsql parity except AD Password" (mssql-rs
    wiki, `Design/mssql-odbc-Authentication`, commit `072f280f`). The flow sends
    plaintext credentials to Entra, supports neither MFA nor conditional access,
    is deprecated by the Microsoft identity platform, is blocked in many
    tenants, and was deprecated in SqlClient 7.0.
    **Application-visible regression:** mssql-python does not map this keyword,
    so it stays in the connection string and reaches whichever driver is
    loaded. An application using `Authentication=ActiveDirectoryPassword`
    connects today against msodbcsql; pointing that same application at this
    driver turns a working connection into `HYC00`. Signed off by Vahid
    Beiranvand on 2026-07-16 on AB#45486, which records the reconciliation with
    the auth parity review and was closed as Removed rather than implemented.
    **Evidence limit:** this is a source comparison, not a measured retail
    acceptance claim. A comparison run that records `SQL_DRIVER_VER` and the
    tested msodbcsql build while calling `SQLDriverConnectW` with
    `Authentication=ActiveDirectoryPassword` is still required to establish
    shipping-build behavior.
19. **A cross-identity orphan is released before streaming an already-live
    prepared statement, and a release error fails that execute.** The source
    reference is msodbcsql's `DropPrepHandle` (`Sql/Ntdbms/sqlncli/odbc/sqlcfunc.cpp`), which
    defers the drop in `hPrepDropDeferred`; `BuildSPPrepExec` (`odbc/sqlccmd.cpp`)
    passes that handle by reference on the next prepare, saving a round trip.
    `ProcessDAEParam` clears the deferred slot only after `SendRPCFromStmt`
    succeeds. This driver's streamed `sp_prepexec` likewise piggybacks the
    orphan and defers eviction until the complete message is sent; cancellation
    retains the orphan without assigning a new statement identity.
    The separate-release policy applies only when `begin_execute_prepared`
    reuses a live handle through `sp_execute`, whose handle parameter has no
    drop slot for another identity. It sends and drains `sp_unprepare` before
    parking that stream, analogous to the reference's forced-drop route.
    `execute.rs` propagates a release failure and restores any retained orphan.
    Do not swallow that error and continue: `StmtState::pending_unprepare`
    holds only one orphan, so returning a live prepared statement while retaining
    a separate orphan can overflow that slot at the next rebind, recreating #598.
    This is a source-verified policy difference, not a measured claim about a
    retail driver's diagnostics or wire sequence; a build-specific reference
    comparison remains outstanding. No parity test is skipped for it.
    Recorded following automated review feedback on #599 (2026-09-22);
    narrowed to the cross-identity case following review on 2026-09-24.
    Human parity sign-off has not been recorded. Tracked in #598.
20. **Catalog fallback shares the original query-timeout budget.** If a
    qualified catalog call fails with a server error, `run_catalog` retries
    unqualified across the seven implemented catalog functions. This driver
    deducts cumulative elapsed time from the original `SQL_ATTR_QUERY_TIMEOUT`
    before that retry; an exhausted budget reports the qualified attempt's
    server error instead of attempting the fallback. Whole-second truncation
    means this is not an exact 1x wall-clock cap: a sub-second remainder can
    extend the call by less than one second. msodbcsql's `DoDD` recursively
    retries through `SQLExecDirectW` (`sqlcdd.cpp:1894`), which re-reads the
    undeducted `GetQueryTimeOut(lpstmt)` (`sqlcprot.h:1607`), predicting a
    fresh full budget and up to 2x the configured timeout. Sharing the budget
    avoids doubling an application-visible call's deadline for a fallback the
    application did not request. The local mock-server test
    `catalog_retry_budget_exhausted_reports_the_original_server_error`
    establishes this driver's behavior; the reference behavior is source-only,
    not a measured retail claim. A comparison that records `SQL_DRIVER_VER`
    and the tested msodbcsql build would close that evidence gap. Decision
    recorded in #547. Human parity sign-off has not been recorded.
21. **An oversized `sql_variant` payload with a non-zero overflow is refused by
    the driver, not the server.** `sql_variant` cannot hold a `max` type
    (server error 529), so a payload past the 8000-byte ceiling has to be
    refused somewhere. msodbcsql sends it and surfaces the server's refusal as
    `42000`; this driver declares the inner type at its non-max ceiling
    (`variant_column_size`) and refuses during parameter conversion with
    `22001`, saving the round trip.
    Scope: only a *non-zero* overflow is measured here. A payload whose bytes
    past the ceiling are all zero takes `trim_zero_overflow`, which trims and
    sends rather than refusing, and
    `a_binary_variant_payload_past_the_byte_ceiling_is_truncation` pins that
    half of this driver's behavior.
    **Evidence limit:** whether msodbcsql also trims that case is a source
    reading only - `trim_zero_overflow` mirrors `CheckTrailingZeros`
    (`sqlccnvt.cpp:8690`) - and is *not* measured. Do not infer parity for the
    zero-overflow boundary from this entry. Closing it needs a both-leg case
    binding an oversized zero-filled binary `sql_variant` with `SQL_DRIVER_VER`
    recorded; if the reference refuses it, the deviation here is wider than
    stated.
    The rule is not new to binary payloads - `variant_column_size` already
    governed the character variants - but binary `sql_variant` parameters make
    it reachable for a second family of C types, so it is recorded here rather
    than left in a code comment.
    Measured against msodbcsql 18.6.2.1 (`SQL_DRIVER_VER` `18.06.0002`) on SQL
    Server 2022, Windows Driver Manager, 2026-09-25, with a `0xAB`-filled
    payload - i.e. the non-zero overflow this entry describes.
    `BinaryVariantPayloadPastTheCeilingIsRefused` asserts both legs, so the
    reference side stays measured rather than skipped. Tracked in AB#48248.
    **Evidence limit:** only the binary leg is measured. The character leg is
    covered by unit tests and shares `variant_column_size`, but no comparison
    run records msodbcsql's SQLSTATE for an oversized character `sql_variant`;
    do not infer that half from this entry.
22. **A character the target code page cannot represent is always substituted
    with `?`; msodbcsql best-fit maps many of them.** Both drivers substitute
    rather than reject, and agree on `0x3F` for a character with no mapping at
    all. They differ on the characters Windows NLS can *transliterate*:
    msodbcsql converts through `SystemLocale::FromUtf16` →
    `WideCharToMultiByte(cp, 0, ...)` (`Common/include/LocalizationImpl.hpp:1442`),
    where `dwFlags = 0` leaves best-fit mapping on and a best-fit result does
    not even set the `lpUsedDefaultChar` loss flag. `encoding_rs` is a strict
    WHATWG encoder with no best-fit tables. Measured under
    `SQL_Latin1_General_CP1_CI_AS` — `WideCharToMultiByte` and the engine's own
    `CAST(N'…' AS varchar)` agree on every row:

    | Input | msodbcsql / engine | This driver |
    |---|---|---|
    | `Ā` U+0100, `Ć` U+0106, `ě` U+011B, `Ł` U+0141, `‐` U+2010 | `A` `C` `e` `L` `-`, no loss flag | `?` |
    | `日` U+65E5 | `?`, loss flag set | `?` |
    | `€` U+20AC (in CP1252) | `0x80` | `0x80` |

    So the deviation is confined to characters with a best-fit mapping but no
    true code-page representation — Latin Extended-A, General Punctuation.
    Polish, Czech, Croatian, Turkish and Baltic text bound to a CP1252
    `varchar` transliterates on msodbcsql and becomes `?` here.

    **Not CP1252-specific.** Measured on the DBCS and OEM code pages too:
    `WideCharToMultiByte(932, ...)` substitutes `U+0141` (`3F`, loss flag set)
    while glibc `iconv -t CP932//TRANSLIT` best-fits it to `4C`, and CP437
    best-fits it to `4C` on Windows. The table above is CP1252 because that is
    where the application impact is widest, not because the behaviour is
    confined to it.

    **Not replicated because msodbcsql has no single behaviour to replicate.**
    Its non-Windows legs take transliteration from `iconv`: `cp_iconv::g_cp_iconv`
    appends `//TRANSLIT` (`LocalizationImpl.hpp:59`), which glibc honours with
    its own table and which is compiled out entirely under musl
    (`#define TRANSLIT ""`, `:51`). `Ł` is therefore `L` on Windows, `L` from a
    possibly different table on glibc, and `?` on musl — one driver, one
    version. Matching the Windows column would mean shipping and maintaining
    per-code-page best-fit tables in `mssql-tds` (`encoding_rs` will not supply
    them; `WideCharToMultiByte` is Windows-only) to chase a behaviour the
    reference driver does not hold stable across its own platforms. The `?` we
    emit is the musl leg's answer and the ODBC specification's substitution
    wording.

    **The same platform split decides the substitution *width* for an astral
    character.** Windows converts per UTF-16 code unit, so `U+1F600` becomes two
    `0x3F` bytes; so does the engine
    (`DATALENGTH(CAST(N'😀' AS varchar(4)))` is 2). **Measured on glibc 2.35
    (Ubuntu 22.04, the CI container's base): one byte.**
    `iconv -f UTF-16LE -t CP1252//TRANSLIT` emits a single `3f`, because
    `//TRANSLIT` makes iconv itself resolve the character and msodbcsql's own
    per-`WCHAR` `EILSEQ` loop (`Globalization.h`, `SkipSingleCh` + `AddDefault`)
    never runs.

    **musl is not measured.** `TRANSLIT` is empty there (`:51`), so iconv should
    instead return `EILSEQ` and hand the character back to that per-`WCHAR`
    loop — a different mechanism, and therefore possibly a different width. The
    claim here is deliberately limited to the leg that was measured; do not
    infer musl's byte count from this entry, and measure it before relying on
    one either way.

    This driver follows the Windows and engine answer, which is also the one
    that cannot let a `varchar(n)` accept a string those two reject, and it does
    so identically on every platform.
    `AstralUnmappableCharacterSubstitutesPerUtf16Unit` carries
    `SKIP_IF_COMPARING_MSODBCSQL()` for the glibc leg.

    Second-order consequence: under `SQL_COPT_SS_WARN_ON_CP_ERROR` (entry 23)
    we warn for a best-fit character where msodbcsql would not, since it does
    not count a best-fit result as loss.

    Distinct from the AB#47598 defect this entry is the residue of, where
    `encoding_rs`'s WHATWG *form-submission* semantics emitted a numeric
    character reference — `U+65E5` as the eight ASCII bytes `&#26085;`, markup
    stored in place of the value and one character counted as eight against the
    column — behind only a `tracing::warn!`. `BestFitMappableCharacterDeviates`
    carries `SKIP_IF_COMPARING_MSODBCSQL()` and pins the disagreement;
    `UnmappableCharacterIsSubstituted` and its siblings run unskipped on the
    rows both drivers agree on. Tracked in AB#47598.

    **Human parity sign-off has not been recorded.** This entry describes an
    application-visible regression against msodbcsql — Polish, Czech, Croatian,
    Turkish and Baltic text bound to a CP1252 `varchar` transliterates there and
    is substituted here — so the registry's "who signed off and when" applies.
    The decision is evidenced (measured three ways, with the musl leg explicitly
    unmeasured) but not approved; record the approver and date in AB#47598
    before relying on this entry as settled.

23. **`SQL_COPT_SS_WARN_ON_CP_ERROR` reports code-page loss on input
    parameters; msodbcsql reports it only on retrieval.** msodbcsql posts
    `IDS_01_000_16` ("Warning: Code page translation caused loss of data",
    SQLSTATE `01000` via `cli_common/src/clntcomn.cpp:1183`) from exactly two
    sites, both in `odbc/sqlcdata.h`: the output-parameter arm (`:1297`) and the
    column arm (`:1310`). The input-parameter paths discard the loss flag
    outright — `Xlat(..., TOSERVER, NULL, ...)` at `odbc/sqlcmisc.cpp:7364` and
    `odbc/sqlccnvt.cpp:995`, and `FromUtf16(..., NULL)` at
    `odbc/sqlccmd.cpp:10975`. With the attribute on and an unmappable bound
    parameter, msodbcsql returns `SQL_SUCCESS` and posts nothing; this driver
    returns `SQL_SUCCESS_WITH_INFO` and posts `01000`.

    Taken because the retrieval direction msodbcsql instruments cannot lose
    anything here: this driver's `SQL_C_CHAR` is UTF-8 (entry 3), so any
    character the server sends has a representation in the target buffer and
    there is nothing to substitute. Applying the attribute to the direction
    where loss actually occurs keeps it meaningful rather than inert. The
    substitution itself is silent by default on both drivers, so an application
    that never sets the attribute cannot tell them apart.

    Four tests carry `SKIP_IF_COMPARING_MSODBCSQL()` for this entry:
    `UnmappableCharacterWarnsWhenAsked`,
    `DataAtExecutionUnmappableCharacterWarnsWhenAsked` and
    `DataAtExecutionTruncatedTailWarnsWhenAsked` in
    `param_char_conversions_test.cpp`, and `ArrayUnmappableCharacterWarnsWhenAsked`
    in `param_array_test.cpp`. The substitution they sit alongside is asserted
    unskipped by `UnmappableCharacterIsSubstituted` and
    `DataAtExecutionUnmappableCharacterIsSubstituted`.

    `ArrayUnmappableCharacterWarnsWhenAsked` carries a second obligation worth
    recording here: its follow-up statement, asserting plain `SQL_SUCCESS` on a
    value with nothing unmappable, is the only assertion in the suite that
    catches a missing `take_code_page_conversion_loss` in
    `finish_parameter_array` — an undrained verdict would warn again on an
    unrelated statement. Tracked in AB#47598.

    **Value validation matches, with one measured exception.** msodbcsql
    rejects anything but `SQL_WARN_NO`/`SQL_WARN_YES` with `HY024`
    (`odbc/sqlcmisc.cpp:2473`), and so does this driver. Measured on retail
    18.6.2.1: `SQLSetConnectAttr(dbc, 1243, (SQLPOINTER)7, 0)` answers `HY024`
    after connect but is **accepted silently before** connect — the
    `pAttributeValue > SQL_IS_ON` check at `odbc/dbcinfotoken.cpp:171` guards a
    path `SQLSetConnectAttr` does not reach pre-connect. This driver validates
    in both states; silently storing an out-of-range value is not behaviour
    worth reproducing.

24. **A collation naming no encoding this crate maps is given a fallback
    encoding; msodbcsql fails the conversion instead.** msodbcsql's
    `CodePageFromTDSCollation` (`cli_common/src/clntcomn.cpp:103-160`) seeds
    `*puiCodePage = CP_ACP`, takes `CP_UTF8` for a UTF8-flagged collation, the
    sort-ID table entry when `bSortid` is non-zero, and otherwise looks the LCID
    up in `x_rgLocaleMap` and then `GetLocaleInfoA`
    (`SystemLocale::Singleton().AnsiCP()` on non-Windows builds). If that leaves
    the code page at `CP_ACP` it returns `E_FAIL` (`:152-157`) — there is no
    fallback encoding anywhere in that function. This driver always produces
    bytes, and by **two different rules**:

    - `resolve_collation` (`mssql-tds/src/datatypes/sql_string.rs`) falls back
      to **Windows-1252**. This is the shared resolver: it backs
      `EncodingType::resolved_encoding`, so it covers decoding a fetched value,
      and `encode_narrow`, whose production callers are the `sql_variant`
      serializer and ODBC's data-at-execution transcoder
      (`DaeTarget::Narrow`).
    - `serialize_string`'s `VARCHAR | CHAR | TEXT` arm
      (`mssql-tds/src/datatypes/tds_value_serializer.rs`) falls back to a
      **Latin-1-like** mapping: a scalar at or below U+00FF is its own byte,
      anything above becomes `?` — one per UTF-16 unit, so a supplementary
      character becomes `??`. The same arm uses it when no collation is known
      at all.

    Both rules substitute per UTF-16 unit, so the width agrees between them and
    with deviation 22; measured, U+1F600 is `3F 3F` under either. What the two
    disagree on is U+0080: `0x80` under the Latin-1 mapping, but unmappable in
    Windows-1252, whose `0x80` is the Euro sign, so it substitutes to `?`.

    An application can tell the difference from msodbcsql in both cases — it
    stores a value where the reference driver would fail the bind — so this is
    recorded rather than left to code comments, even though neither rule is new.

    **Evidence level: source citation only; not measured.** The msodbcsql
    branch above is read from `clntcomn.cpp`, not observed on a retail build,
    so no `SQL_DRIVER_VER` is recorded. Reaching it needs a TDS collation whose
    LCID `GetLocaleInfoA` cannot give an ANSI code page for, which an ordinary
    server collation does not produce; the Driver Manager is not the obstacle.
    The measurement that would close this: bind a narrow parameter under such a
    collation against both drivers and record `SQL_DRIVER_VER` with each
    result, or exercise `CodePageFromTDSCollation` directly with a synthesised
    `TDSCOLLATION`.

    The same evidence gap cuts the other way and is worth stating, because it
    is the more reachable half: this crate's `lcid_to_encoding` table is
    narrower than `GetLocaleInfoA`, so there are LCIDs msodbcsql resolves
    correctly and this driver does not. For those, the fallbacks above send
    *wrong bytes* where msodbcsql sends right ones — a silent divergence rather
    than the error-versus-value one this entry's title describes. Which LCIDs
    those are is likewise unmeasured.

    Taken because erroring on a collation the driver merely does not map is a
    poor trade for an application that would otherwise round-trip its data
    correctly: the LCID tables are this crate's coverage limit, not a statement
    about the value. The split between the two rules is **not** itself
    deliberate design — it is two fallbacks that grew independently. AB#48437
    deliberately preserved it rather than unifying it, because changing the
    serializer arm's default to Windows-1252 is a behaviour change on a path
    that fix did not otherwise touch, and
    `an_unmapped_collation_keeps_the_latin1_fallback`
    (`mssql-tds/src/datatypes/tds_value_serializer.rs`) pins the current split,
    so a future unification is a visible, deliberate edit rather than a silent
    drift. `the_sql_variant_narrow_path_still_encodes_through_the_shared_encoder`
    (`mssql-tds/tests/test_narrow_param_encoding.rs`) pins the other side of it:
    the unmapped collation is the only probe that can still tell the two
    helpers apart, so it is what keeps the `sql_variant` route from being
    merged into the serializer arm.

    No application regresses at this entry's introduction: both rules predate
    it and AB#48437 preserved them unchanged, so no sign-off is recorded.
    Unifying the two defaults needs one, as does closing the table gap above.
    Decision history in AB#48437.

    **Scope is the shared serializer, not only parameters.**
    `TdsValueSerializer::serialize_value` is also what bulk copy
    (`mssql-tds/src/message/bulk_load.rs`, per column from
    `col_meta.collation`) and TVP rows (`mssql-tds/src/datatypes/sql_tvp.rs`,
    from `db_collation`) write through, so the fallbacks above and the
    resolution order AB#48437 corrected apply on those routes too. Both
    previously sent the LCID's single-byte encoding under a collation they had
    declared as `_UTF8` or a CP437/CP850 sort ID, storing corrupt cells rather
    than merely mis-framed ones; that is fixed. The corollary is that
    deviation 4's over-length behaviour now reaches them as well -- a cell that
    fit before can exceed its declared length once the encoding grows it, and
    on those routes it surfaces mid-stream rather than before the first row is
    written. Deviation 4's sign-off was given for the ODBC parameter layer and
    does not extend to bulk copy or TVP; AB#47584 owns closing that gap for all
    three.

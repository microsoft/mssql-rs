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
7. **A bound `max`/LOB text column converted to a typed C target is refused
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
   machine's local time zone through `ConvertOffsetToLocal`. Matching that
   behavior would make the returned value depend on the client machine's time
   zone, so this driver deliberately ignores the offset for non-offset
   targets. `offset_is_ignored_for_non_offset_targets` pins parsed character
   input, and `DatetimeoffsetIntoSsTime2Succeeds` pins a native
   `datetimeoffset` column while skipping the msodbcsql comparison leg.

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
   declaration onto 3.x on the driver's behalf — measured, not assumed. It
   stores `SQL_OV_ODBC2` and answers the application, and
   `SQLAllocHandle(SQL_HANDLE_DBC)` also succeeds, because that handle is the
   Driver Manager's own and no driver has been loaded yet. The driver is
   loaded at `SQLDriverConnect`, and only then does the DM replay the
   environment onto it: this driver's `SQLSetEnvAttr` rejects `SQL_OV_ODBC2`,
   nothing is recorded, and this driver's `SQLAllocHandle(SQL_HANDLE_DBC)`
   then refuses with `HY010` — the SQLSTATE ODBC defines for allocating a
   connection before `SQL_ATTR_ODBC_VERSION` is set. unixODBC surfaces that to
   the application as `IM005`, "Driver's SQLAllocHandle on SQL_HANDLE_DBC
   failed"; `HY010` is what this driver posts, `IM005` is what the application
   reads. Measured on unixODBC in build 176929.
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
   `Odbc2ApplicationIsRefused` pins it end to end and doubles as the proof
   that the Driver Manager does not convert 2 to 3 — were it to convert, the
   version would arrive as `SQL_OV_ODBC3` and the connect would succeed.
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

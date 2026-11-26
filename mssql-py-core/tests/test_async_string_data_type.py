# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Live SQL Server string insertion/retrieval through asynchronous queries.

Character inventory:
* ASCII letters, digits, punctuation, SQL/parameter-marker syntax, and escapes.
* Every C0 control (including NUL), DEL, whitespace, and line separators.
* Accented Latin, Greek, Cyrillic, Hebrew, Arabic, Indic, and East Asian scripts.
* Combining marks, normalization variants, format/bidi controls, and private use.
* Symbols, emoji, supplementary planes, variation selectors, and ZWJ sequences.
* Empty/NULL values, padding, UTF-16 limits, and long multi-packet strings.
* Explicit casts, truncation errors, and recovery after failed inserts.
* Collation comparisons, Unicode sorting, UTF-8 storage, and LIKE patterns.
* String ownership across awaits and cursor/connection teardown.
* Caller-side buffer replacement and abandoned/cancelled pending executions.
* Targeted CRUD matrices across six string types and both execution APIs.
* Multi-thousand-row executemany integrity for mixed VARCHAR/NVARCHAR values.
* CP1252 replacement rules, undefined bytes, and explicit VARCHAR rebinding.
* Japanese, Chinese, Korean, Cyrillic, Arabic, Central European, and Thai code pages.
* Cross-collation transfers, conflicts, and non-CP1252 ANSI parameter binding.
* Explicit fixed-to-variable and MAX-to-bounded string conversions.
* Pending-query responsiveness, independent completion, and string result transitions.
* Cancellation isolation, transaction rollback, and native fetch timeouts.
* Concurrent code-page conversion and conversion after recoverable fetch cancellation.
* Multi-MiB column isolation and server-initiated interruption of large result retrieval.
* One hundred pending string queries and shared-session execute-overlap rejection.

Unicode scalars use NCHAR/NVARCHAR/NTEXT, or VARCHAR with a UTF-8 collation.
Other VARCHAR/CHAR/TEXT cases default to an explicit Windows-1252 collation;
legacy-code-page cases override it with the corresponding language collation.
Lone surrogates are tested separately for rejection during parameter binding.
Connection-collation tests require CREATE DATABASE permission and clean up their
uniquely named databases.
Lock-wait fault-injection tests also require visibility of their requests in
sys.dm_exec_requests. Session-termination tests require permission to KILL their
own SQL sessions.
"""

import asyncio
import gc
import string
import threading
import uuid
import warnings
import weakref
from concurrent.futures import ThreadPoolExecutor
from contextlib import asynccontextmanager

import mssql_py_core
import pytest


pytestmark = [
    pytest.mark.integration,
    pytest.mark.parametrize("use_prepare", [True, False], ids=["prepared", "direct"]),
]

ASCII_CASES = [
    pytest.param("", id="empty"),
    pytest.param(None, id="null"),
    pytest.param(string.ascii_letters, id="ascii-letters"),
    pytest.param("A", id="single-character"),
    pytest.param(string.digits, id="digits"),
    pytest.param(string.punctuation, id="all-ascii-punctuation"),
    pytest.param("".join(chr(code) for code in range(32, 127)), id="printable-ascii"),
    pytest.param(" ", id="single-space"),
    pytest.param("   ", id="spaces-only"),
    pytest.param("  leading", id="leading-spaces"),
    pytest.param("trailing  ", id="trailing-spaces"),
    pytest.param("  both  ", id="surrounding-spaces"),
    pytest.param("\t\r\n", id="whitespace-only"),
    pytest.param("line1\nline2\r\nline3\rline4", id="line-endings"),
    pytest.param("O'Brien's \"quoted\" text", id="quotes"),
    pytest.param(r"C:\path\file\n\t\\", id="literal-backslashes"),
    pytest.param("? %s %(value)s :value @value %%", id="parameter-like-text"),
    pytest.param("'; DROP TABLE #async_strings; -- /* ? */", id="sql-like-text"),
    pytest.param("100% _ [a-z] ^ $ .* + ?", id="wildcards-and-regex"),
    pytest.param('{"key": "<tag>&value</tag>"}', id="json-and-xml-like-text"),
    pytest.param("\x00", id="nul-only"),
    pytest.param("\x00start", id="leading-nul"),
    pytest.param("end\x00", id="trailing-nul"),
    pytest.param("a\x00\x00b", id="repeated-nul"),
] + [
    pytest.param(f"before{chr(code)}after", id=f"control-{code:02x}")
    for code in (*range(32), 127)
]

UNICODE_CASES = [
    pytest.param("caf\u00e9 Stra\u00dfe \u00c5ngstr\u00f6m", id="accented-latin"),
    pytest.param("\u0395\u03bb\u03bb\u03b7\u03bd\u03b9\u03ba\u03ac", id="greek"),
    pytest.param("\u041f\u0440\u0438\u0432\u0435\u0442 \u043c\u0438\u0440", id="cyrillic"),
    pytest.param("\u05e9\u05dc\u05d5\u05dd \u05e2\u05d5\u05dc\u05dd", id="hebrew"),
    pytest.param("\u0645\u0631\u062d\u0628\u0627 \u0628\u0627\u0644\u0639\u0627\u0644\u0645", id="arabic"),
    pytest.param("\u0645\u064e\u0631\u0652\u062d\u064e\u0628\u064b\u0627", id="arabic-diacritics"),
    pytest.param("\u0939\u093f\u0928\u094d\u0926\u0940", id="devanagari"),
    pytest.param("\u09ac\u09be\u0982\u09b2\u09be", id="bengali"),
    pytest.param("\u0ba4\u0bae\u0bbf\u0bb4\u0bcd", id="tamil"),
    pytest.param("\u0e20\u0e32\u0e29\u0e32\u0e44\u0e17\u0e22", id="thai"),
    pytest.param("\u4e2d\u6587\u6f22\u5b57", id="chinese"),
    pytest.param("\u65e5\u672c\u8a9e \u3072\u3089\u304c\u306a \u30ab\u30bf\u30ab\u30ca", id="japanese"),
    pytest.param("\ud55c\uad6d\uc5b4", id="korean"),
    pytest.param("\u1100\u1161\u11a8", id="hangul-jamo"),
    pytest.param("\uff21\uff22\uff23 \uff11\uff12\uff13 \uff76\uff80\uff76\uff85", id="fullwidth-and-halfwidth"),
    pytest.param("\u00e9 \u00c5 \u00f6", id="precomposed"),
    pytest.param("e\u0301 A\u030a o\u0308", id="decomposed"),
    pytest.param("a\u0301\u0327\u0308", id="stacked-combining-marks"),
    pytest.param("\u0301\u0327", id="combining-marks-only"),
    pytest.param("\u00a0\u1680\u2000\u2001\u2002\u2003\u2009\u202f\u205f\u3000", id="unicode-spaces"),
    pytest.param("\u0085\u2028\u2029", id="unicode-line-separators"),
    pytest.param("a\u200bb\u200cc\u200dd\u2060e\ufeff", id="zero-width-and-bom"),
    pytest.param("\u202aLTR\u202c \u202bRTL\u202c \u2067\u05d0\u05d1\u2069", id="bidi-controls"),
    pytest.param("abc \u05e9\u05dc\u05d5\u05dd 123 \u0645\u0631\u062d\u0628\u0627", id="mixed-directions"),
    pytest.param("\u00a3\u00a5\u20ac\u20b9\u20bf", id="currency"),
    pytest.param("\u00b1\u00d7\u00f7\u2211\u221a\u221e\u2260\u2264\u2265", id="math"),
    pytest.param("\u2190\u2191\u2192\u2193 \u2500\u2502\u253c \u2588\u2591", id="arrows-and-box-drawing"),
    pytest.param("\u2018\u2019\u201c\u201d\u2013\u2014\u2026", id="typographic-punctuation"),
    pytest.param("\U0001f600\U0001f680\U0001f389", id="emoji"),
    pytest.param("\U0001f44d\U0001f3fd", id="emoji-skin-tone"),
    pytest.param("\U0001f469\u200d\U0001f4bb", id="emoji-zwj"),
    pytest.param("\U0001f468\u200d\U0001f469\u200d\U0001f467\u200d\U0001f466", id="emoji-family"),
    pytest.param("\U0001f1ee\U0001f1f3\U0001f1fa\U0001f1f8", id="regional-indicator-flags"),
    pytest.param("\u2764\ufe0e \u2764\ufe0f 1\ufe0f\u20e3", id="variation-selectors-and-keycap"),
    pytest.param("\U00020000\U0002a6d6", id="supplementary-cjk"),
    pytest.param("\U0001d11e\U0001d122 \U0001d400\U0001d7d8", id="supplementary-music-and-math"),
    pytest.param("\ue000\uf8ff\U000f0000\U0010fffd", id="private-use"),
    pytest.param("\u007f\u0080\u07ff\u0800\ud7ff\ue000\ufffd\U00010000", id="encoding-boundaries"),
    pytest.param("\ufffe\uffff\U0010ffff", id="noncharacters"),
    pytest.param(
        "Hello \u0928\u092e\u0938\u094d\u0924\u0947 \u4f60\u597d \U0001f600",
        id="mixed-language",
    ),
    pytest.param("A\x00\u4e2d\x00\U0001f600\x00Z", id="mixed-unicode-and-nul"),
] + [
    pytest.param(f"before{chr(code)}after", id=f"c1-control-{code:02x}")
    for code in range(0x80, 0xA0)
]

CP1252_CASES = [
    pytest.param("caf\u00e9 na\u00efve fa\u00e7ade \u00c5ngstr\u00f6m", id="latin-accents"),
    pytest.param("\u00a3\u00a5\u00a2\u20ac", id="currency"),
    pytest.param("\u2018\u2019\u201c\u201d\u2013\u2014\u2026", id="typographic-punctuation"),
    pytest.param("\u0152\u0153\u0160\u0161\u017d\u017e\u0178\u0192", id="extended-letters"),
    pytest.param("\u00a0\u00ad", id="nbsp-and-soft-hyphen"),
    pytest.param("Price: \u20ac100 \u2122", id="price-and-trademark"),
]


LEGACY_CODE_PAGE_CASES = [
    pytest.param(
        932, "Japanese_CI_AS", "cp932",
        "\u65e5\u672c\u8a9e \u3042\u3044\u3046 \uff76\uff80\uff76\uff85",
        id="cp932-japanese",
    ),
    pytest.param(
        936, "Chinese_PRC_CI_AS", "cp936",
        "\u7b80\u4f53\u4e2d\u6587 \u4f60\u597d",
        id="cp936-simplified-chinese",
    ),
    pytest.param(
        949, "Korean_Wansung_CI_AS", "cp949",
        "\ud55c\uad6d\uc5b4 \ud7a3",
        id="cp949-korean",
    ),
    pytest.param(
        950, "Chinese_Taiwan_Stroke_CI_AS", "cp950",
        "\u7e41\u9ad4\u4e2d\u6587 \u4f60\u597d",
        id="cp950-traditional-chinese",
    ),
    pytest.param(
        1251, "Cyrillic_General_CI_AS", "cp1251",
        "\u041f\u0440\u0438\u0432\u0435\u0442 \u043c\u0438\u0440 \u0401\u0451",
        id="cp1251-cyrillic",
    ),
    pytest.param(
        1256, "Arabic_CI_AS", "cp1256",
        "\u0645\u0631\u062d\u0628\u0627 \u0628\u0627\u0644\u0639\u0627\u0644\u0645",
        id="cp1256-arabic",
    ),
    pytest.param(
        1250, "Polish_CI_AS", "cp1250",
        "\u017dlu\u0165ou\u010dk\u00fd k\u016f\u0148",
        id="cp1250-central-european",
    ),
    pytest.param(
        874, "Thai_CI_AS", "cp874",
        "\u0e20\u0e32\u0e29\u0e32\u0e44\u0e17\u0e22",
        id="cp874-thai",
    ),
]


ENCODING_CONVERSION_CASES = [
    (
        "varchar(max)", "Latin1_General_100_BIN2", "cp1252",
        "Caf\u00e9 \u20ac \u2122",
    ),
    (
        "varchar(max)", "Latin1_General_100_BIN2_UTF8", "utf-8",
        "\u0928\u092e\u0938\u094d\u0924\u0947 \u4f60\u597d \U0001f600e\u0301",
    ),
    (
        "nvarchar(max)", "Latin1_General_100_BIN2", "utf-16-le",
        "\u6f22 \u0645\u0631\u062d\u0628\u0627 \U0001f600e\u0301",
    ),
] + [
    ("varchar(max)", collation, encoding, text)
    for _, collation, encoding, text in (
        case.values for case in LEGACY_CODE_PAGE_CASES
    )
]


async def connect(client_context, *, autocommit=True, **overrides):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            {**client_context, **overrides}, autocommit=autocommit
        )


async def round_trip(
    client_context, sql_type, value, use_prepare, *, collation="Latin1_General_100_BIN2"
):
    conn = await connect(client_context)
    try:
        cursor = conn.cursor()
        # A local temp table created inside a prepared RPC would leave scope.
        await cursor.execute(
            f"SET ANSI_WARNINGS ON; CREATE TABLE #async_strings "
            f"(value {sql_type} COLLATE {collation} NULL)",
            use_prepare=False,
        )
        await cursor.execute(
            "INSERT INTO #async_strings (value) VALUES (?)",
            value,
            use_prepare=use_prepare,
        )
        cast_type = "nvarchar(max)" if sql_type.startswith("n") else "varchar(max)"
        await cursor.execute(
            "SELECT value, DATALENGTH(value), "
            f"CONVERT(varbinary(max), CONVERT({cast_type}, value)) "
            "FROM #async_strings",
            use_prepare=use_prepare,
        )
        row = await cursor.fetchone()
        assert await cursor.fetchone() is None
        return row
    finally:
        await conn.close()


async def fetch_query(client_context, operation, parameters, use_prepare):
    conn = await connect(client_context)
    try:
        cursor = conn.cursor()
        await cursor.execute(operation, *parameters, use_prepare=use_prepare)
        return await cursor.fetchall()
    finally:
        await conn.close()


async def wait_for_lock_wait(cursor, session_id, pending):
    for _ in range(500):
        await cursor.execute(
            "SELECT wait_type FROM sys.dm_exec_requests WHERE session_id = ?",
            session_id,
            use_prepare=False,
        )
        waits = await cursor.fetchall()
        if waits and waits[0][0] and waits[0][0].startswith("LCK_M_"):
            return
        assert not pending.done(), "Query ended before reaching its lock wait"
        await asyncio.sleep(0.01)
    pytest.fail("Query did not reach its server-side lock wait")


@asynccontextmanager
async def locked_string_table(client_context):
    conn = await connect(client_context)
    async with conn:
        control = conn.cursor()
        table = f"##async_strings_{uuid.uuid4().hex}"
        await control.execute(
            f"CREATE TABLE {table} (id int PRIMARY KEY)", use_prepare=False
        )
        try:
            await control.execute(
                f"INSERT INTO {table} VALUES (1)", use_prepare=False
            )
            await control.execute(
                f"BEGIN TRANSACTION; SELECT id FROM {table} WITH (TABLOCKX, HOLDLOCK)",
                use_prepare=False,
            )
            assert await control.fetchall() == [(1,)]
            yield control, table
        finally:
            try:
                await control.execute(
                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                )
            finally:
                await control.execute(
                    f"DROP TABLE IF EXISTS {table}", use_prepare=False
                )


def test_delayed_string_query_allows_independent_completion(client_context, use_prepare):
    async def run():
        connections = []
        delayed = None
        value = "delayed:\u6f22\U0001f600\x00e\u0301"
        table = f"##async_strings_{uuid.uuid4().hex}"
        try:
            for _ in range(3):
                connections.append(await connect(client_context))
            control, slow_cursor, fast_cursor = [
                conn.cursor() for conn in connections
            ]
            await control.execute(
                f"CREATE TABLE {table} (id int PRIMARY KEY, value nvarchar(max))",
                use_prepare=False,
            )
            try:
                await control.execute(
                    f"INSERT INTO {table} VALUES (0, N'baseline')", use_prepare=False
                )
                await slow_cursor.execute("SELECT @@SPID", use_prepare=False)
                [(session_id,)] = await slow_cursor.fetchall()
                try:
                    await control.execute(
                        f"BEGIN TRANSACTION; SELECT id FROM {table} "
                        "WITH (TABLOCKX, HOLDLOCK)",
                        use_prepare=False,
                    )
                    assert await control.fetchall() == [(0,)]
                    delayed = slow_cursor.execute(
                        f"INSERT INTO {table} VALUES (1, ?); "
                        f"SELECT value FROM {table} WHERE id = 1",
                        value,
                        use_prepare=use_prepare,
                    )
                    await wait_for_lock_wait(control, session_id, delayed)

                    assert await fast_cursor.execute(
                        "SELECT CAST(? AS nvarchar(32))",
                        "independent:\u4f60\u597d",
                        use_prepare=use_prepare,
                    ) is fast_cursor
                    assert await fast_cursor.fetchall() == [("independent:\u4f60\u597d",)]
                    assert not delayed.done()
                finally:
                    await control.execute(
                        "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                    )

                assert await delayed is slow_cursor
                if slow_cursor.description is None:
                    assert await slow_cursor.nextset() is True
                assert await slow_cursor.fetchall() == [(value,)]
                await slow_cursor.execute(
                    "SELECT CAST(? AS nvarchar(max)) WHERE 1 = 0",
                    "must not reuse the previous string",
                    use_prepare=use_prepare,
                )
                assert await slow_cursor.fetchone() is None
                assert await slow_cursor.fetchall() == []
            finally:
                try:
                    if delayed is not None:
                        await delayed
                finally:
                    await control.execute(
                        f"DROP TABLE IF EXISTS {table}", use_prepare=False
                    )
        finally:
            await asyncio.gather(*(conn.close() for conn in connections))

    asyncio.run(run())


def test_string_result_set_transitions(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            value = "\u6f22\U0001f600\x00e\u0301" * 2048
            await cursor.execute(
                "SELECT CAST(? AS varchar(32)) AS ascii_value; "
                "SELECT CAST(? AS nvarchar(max)) AS missing_value WHERE 1 = 0; "
                "SELECT CAST(? AS nvarchar(max)) AS null_value; "
                "SELECT CAST(? AS nvarchar(max)) AS empty_value; "
                "SELECT CAST(? AS nvarchar(max)) AS unicode_value",
                "first", "not returned", None, "", value,
                use_prepare=use_prepare,
            )
            expected = [
                ("ascii_value", [("first",)]),
                ("missing_value", []),
                ("null_value", [(None,)]),
                ("empty_value", [("",)]),
                ("unicode_value", [(value,)]),
            ]
            for index, (name, rows) in enumerate(expected):
                assert cursor.description[0][:2] == (name, str)
                assert await cursor.fetchall() == rows
                assert await cursor.nextset() is (index + 1 < len(expected))
            assert cursor.description is None
            assert await cursor.nextset() is False
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("code_page", "collation", "encoding", "text"), LEGACY_CODE_PAGE_CASES
)
@pytest.mark.parametrize("sql_type", ["varchar(200)", "varchar(max)"])
def test_legacy_code_page_round_trip(
    client_context, use_prepare, code_page, collation, encoding, text, sql_type
):
    rows = asyncio.run(fetch_query(
        client_context,
        "SELECT CONVERT(int, COLLATIONPROPERTY(?, 'CodePage'))",
        (collation,),
        use_prepare,
    ))
    assert rows == [(code_page,)]

    repeats = 2048 if sql_type == "varchar(max)" else 1
    value = "ASCII " + text * repeats + "\x00 tail  "
    encoded = value.encode(encoding)
    row = asyncio.run(round_trip(
        client_context, sql_type, value, use_prepare, collation=collation
    ))
    assert row == (value, len(encoded), encoded)


@pytest.mark.parametrize(
    ("code_page", "collation", "encoding", "text"), LEGACY_CODE_PAGE_CASES
)
def test_legacy_code_page_lossy_conversion(
    client_context, use_prepare, code_page, collation, encoding, text
):
    value = f"cp{code_page}:{text}\u0378\U0001f600"
    # The unassigned BMP value and two surrogate units each become '?'.
    expected = f"cp{code_page}:{text}???"
    encoded = expected.encode(encoding)
    row = asyncio.run(round_trip(
        client_context, "varchar(200)", value, use_prepare, collation=collation
    ))
    assert row == (expected, len(encoded), encoded)


@pytest.mark.parametrize(
    ("code_page", "collation", "encoding", "text"), LEGACY_CODE_PAGE_CASES
)
def test_legacy_connection_collation_explicit_ansi_binding(
    client_context, use_prepare, code_page, collation, encoding, text
):
    async def run():
        admin = await connect(client_context, database="master")
        database = f"pycore_cp{code_page}_{uuid.uuid4().hex}"
        admin_cursor = admin.cursor()
        database_created = False
        conn = None
        try:
            await admin_cursor.execute(
                f"CREATE DATABASE [{database}] COLLATE {collation}",
                use_prepare=False,
            )
            database_created = True
            conn = await connect(client_context, database=database)
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT CONVERT(int, COLLATIONPROPERTY(CONVERT(nvarchar(128), "
                "DATABASEPROPERTYEX(DB_NAME(), 'Collation')), 'CodePage'))",
                use_prepare=use_prepare,
            )
            assert await cursor.fetchall() == [(code_page,)]
            await cursor.execute(
                "CREATE TABLE #async_strings (id int PRIMARY KEY, "
                "value varchar(max) COLLATE DATABASE_DEFAULT NULL, parameter_bytes bigint)",
                use_prepare=False,
            )
            values = [None, "", f"ASCII {text}", f"ASCII {text * 512}", "", f"{text}\x00tail"]
            for row_id, value in enumerate(values):
                cursor.setinputsizes([(4, 0, 0), (-1, 0, 0)])
                await cursor.execute(
                    "INSERT INTO #async_strings "
                    "SELECT ?, value, DATALENGTH(value) FROM (VALUES (?)) AS source(value)",
                    row_id,
                    value,
                    use_prepare=use_prepare,
                    reset_cursor=False,
                )
            await cursor.execute(
                "SELECT id, value, parameter_bytes, CONVERT(varbinary(max), value) "
                "FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            expected = []
            for row_id, value in enumerate(values):
                encoded = None if value is None else value.encode(encoding)
                expected.append((
                    row_id, value, None if encoded is None else len(encoded), encoded,
                ))
            assert await cursor.fetchall() == expected
        finally:
            try:
                if conn is not None:
                    await conn.close()
            finally:
                try:
                    if database_created:
                        await admin_cursor.execute(
                            f"DROP DATABASE [{database}]", use_prepare=False
                        )
                finally:
                    await admin.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        pytest.param("Caf\u00e9 \u20ac \u2122", "Caf\u00e9 \u20ac \u2122", id="lossless"),
        pytest.param("Caf\u00e9 \u4f60\u597d \U0001f600", "Caf\u00e9 ?? ??", id="lossy"),
    ],
)
def test_cross_code_page_column_conversion(client_context, use_prepare, value, expected):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings ("
                "source_value varchar(200) COLLATE Latin1_General_100_BIN2_UTF8, "
                "implicit_value varchar(200) COLLATE Latin1_General_100_BIN2, "
                "explicit_value varchar(200) COLLATE Latin1_General_100_BIN2)",
                use_prepare=False,
            )
            await cursor.execute(
                "INSERT INTO #async_strings (source_value) VALUES (?)",
                value,
                use_prepare=use_prepare,
            )
            await cursor.execute(
                "UPDATE #async_strings SET implicit_value = source_value, "
                "explicit_value = source_value COLLATE Latin1_General_100_BIN2",
                use_prepare=use_prepare,
            )
            await cursor.execute(
                "SELECT source_value, implicit_value, explicit_value, "
                "CONVERT(varbinary(max), source_value), "
                "CONVERT(varbinary(max), implicit_value), "
                "CONVERT(varbinary(max), explicit_value), "
                "implicit_value COLLATE Latin1_General_100_BIN2_UTF8 "
                "FROM #async_strings",
                use_prepare=use_prepare,
            )
            encoded = expected.encode("cp1252")
            assert await cursor.fetchall() == [(
                value, expected, expected, value.encode("utf-8"), encoded, encoded, expected,
            )]
        finally:
            await conn.close()

    asyncio.run(run())


def test_cross_collation_conflict_and_explicit_resolution(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings ("
                "cs_value nvarchar(10) COLLATE Latin1_General_100_CS_AS, "
                "ci_value nvarchar(10) COLLATE Latin1_General_100_CI_AS)",
                use_prepare=False,
            )
            await cursor.execute(
                "INSERT INTO #async_strings VALUES (?, ?)",
                "ABC", "abc",
                use_prepare=use_prepare,
            )
            with pytest.raises(mssql_py_core.DatabaseError, match="(?i)collation conflict"):
                await cursor.execute(
                    "SELECT CASE WHEN cs_value = ci_value THEN 1 ELSE 0 END "
                    "FROM #async_strings",
                    use_prepare=use_prepare,
                )
            await cursor.execute(
                "SELECT CASE WHEN cs_value COLLATE Latin1_General_100_CI_AS = ci_value "
                "THEN 1 ELSE 0 END, "
                "CASE WHEN ci_value COLLATE Latin1_General_100_CS_AS = cs_value "
                "THEN 1 ELSE 0 END FROM #async_strings",
                use_prepare=use_prepare,
            )
            assert await cursor.fetchall() == [(1, 0)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize("sql_type", ["varchar(100)", "varchar(max)"])
@pytest.mark.parametrize(
    ("value", "expected"),
    [
        pytest.param("\u4f60\u597d", "??", id="chinese"),
        pytest.param("\U0001f600", "??", id="emoji"),
        pytest.param("\u0928\u092e\u0938\u094d\u0924\u0947", "??????", id="hindi"),
    ],
)
def test_cp1252_unsupported_unicode_conversion(
    client_context, use_prepare, sql_type, value, expected
):
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    assert row == (expected, len(expected), expected.encode("ascii"))


@pytest.mark.parametrize("byte", [0x81, 0x8D, 0x8F, 0x90, 0x9D], ids=lambda byte: f"{byte:02x}")
def test_cp1252_undefined_byte_decoding(client_context, use_prepare, byte):
    rows = asyncio.run(fetch_query(
        client_context,
        "SELECT value, CONVERT(nvarchar(10), value), CONVERT(varbinary(10), value) "
        "FROM (VALUES (CONVERT(varchar(1), ?) COLLATE Latin1_General_100_BIN2)) "
        "AS source(value)",
        (bytes([byte]),),
        use_prepare,
    ))
    # SQL Server preserves these bytes as their corresponding C1 code points.
    assert rows == [(chr(byte), chr(byte), bytes([byte]))]


def test_cp1252_rebinding_and_nvarchar_conversion(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT CONVERT(int, COLLATIONPROPERTY(CONVERT(nvarchar(128), "
                "DATABASEPROPERTYEX(DB_NAME(), 'Collation')), 'CodePage'))",
                use_prepare=False,
            )
            [(code_page,)] = await cursor.fetchall()
            if code_page != 1252:
                pytest.skip("Explicit CP1252 binding requires a CP1252 database")
            await cursor.execute(
                "CREATE TABLE #async_strings (id int PRIMARY KEY, "
                "value varchar(max) COLLATE Latin1_General_100_BIN2 NULL)",
                use_prepare=False,
            )
            values = [
                None,
                "",
                "Caf\u00e9 \u20ac \u2122",
                "\u00a3" * 31,
                "\u20ac" * 8000,
                "",
                "Caf\u00e9\x00Test",
                None,
                "Price: \u20ac100",
            ]
            for row_id, value in enumerate(values):
                # SQL_INTEGER and SQL_LONGVARCHAR keep the declaration stable.
                cursor.setinputsizes([(4, 0, 0), (-1, 0, 0)])
                await cursor.execute(
                    "INSERT INTO #async_strings VALUES (?, ?)",
                    row_id,
                    value,
                    use_prepare=use_prepare,
                    reset_cursor=False,
                )
            await cursor.execute(
                "SELECT id, value, CONVERT(nvarchar(max), value), DATALENGTH(value), "
                "CONVERT(varbinary(max), value) FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            expected = []
            for row_id, value in enumerate(values):
                encoded = None if value is None else value.encode("cp1252")
                expected.append((
                    row_id, value, value,
                    None if encoded is None else len(encoded), encoded,
                ))
            assert await cursor.fetchall() == expected
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    "character", ["a", "\u00e9", "\u6f22", "\U0001f600"],
    ids=["ascii", "latin1", "bmp", "supplementary"],
)
@pytest.mark.parametrize("length", [4097, 262145], ids=["multi-packet", "large"])
def test_string_ownership_survives_await_and_connection_close(
    client_context, use_prepare, character, length
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            value = character * length + "\x00tail"
            parameters = [value]
            pending = cursor.execute(
                "WAITFOR DELAY '00:00:00.200'; SELECT CAST(? AS nvarchar(max))",
                parameters,
                use_prepare=use_prepare,
            )
            value += character * length
            parameters[0] = value
            parameters.extend(["replacement"] * 64)
            del value, parameters
            assert await pending is cursor
            row = await cursor.fetchone()
            assert await cursor.fetchone() is None
            await cursor.close()
            return row
        finally:
            await conn.close()

    row = asyncio.run(run())
    assert row == (character * length + "\x00tail",)


def test_dropped_execute_future_preserves_string_and_releases_resources(
    client_context, use_prepare
):
    async def run():
        async with locked_string_table(client_context) as (control, table):
            conn = await connect(client_context)
            try:
                cursor = conn.cursor()
                await cursor.execute(
                    "CREATE TABLE #async_strings (value nvarchar(max))",
                    use_prepare=False,
                )
                await cursor.execute("SELECT @@SPID", use_prepare=False)
                [(session_id,)] = await cursor.fetchall()
                sample = "\u6f22\U0001f600e\u0301\x00"
                value = sample * 65536 + "tail"
                parameters = [value]
                try:
                    pending = cursor.execute(
                        f"UPDATE {table} SET id = id WHERE id = 1; "
                        "INSERT INTO #async_strings VALUES (?)",
                        parameters,
                        use_prepare=use_prepare,
                    )
                    del value, parameters
                    await wait_for_lock_wait(control, session_id, pending)
                    reference = weakref.ref(pending)
                    del pending
                    gc.collect()
                finally:
                    await control.execute(
                        "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                    )

                for _ in range(600):
                    if reference() is None:
                        break
                    await asyncio.sleep(0.01)
                else:
                    pytest.fail("Abandoned execute future was not released after completion")

                await cursor.execute(
                    "SELECT value, DATALENGTH(value) FROM #async_strings",
                    use_prepare=use_prepare,
                )
                expected = sample * 65536 + "tail"
                assert await cursor.fetchall() == [
                    (expected, len(expected.encode("utf-16-le")))
                ]
            finally:
                await conn.close()

    asyncio.run(run())


def test_cancelled_execute_with_large_string_closes_safely(client_context, use_prepare):
    async def run():
        independent = await connect(client_context)
        async with independent:
            independent_cursor = independent.cursor()
            await independent_cursor.execute("SELECT @@SPID", use_prepare=False)
            [(independent_session_id,)] = await independent_cursor.fetchall()
            references = []
            for cycle in range(3):
                async with locked_string_table(client_context) as (control, table):
                    conn = await connect(client_context)
                    async with conn:
                        cursor = conn.cursor()
                        await cursor.execute("SELECT @@SPID", use_prepare=False)
                        [(session_id,)] = await cursor.fetchall()
                        pending = None
                        other = None
                        try:
                            try:
                                value = "\u6f22\U0001f600e\u0301\x00" * 65536
                                parameters = [value]
                                pending = cursor.execute(
                                    f"UPDATE {table} SET id = id WHERE id = 1; "
                                    "SELECT CAST(? AS nvarchar(max))",
                                    parameters,
                                    use_prepare=use_prepare,
                                )
                                marker = f"independent-{cycle}:\u4f60\u597d"
                                other = independent_cursor.execute(
                                    f"UPDATE {table} SET id = id WHERE id = 1; "
                                    "SELECT CAST(? AS nvarchar(64))",
                                    marker,
                                    use_prepare=use_prepare,
                                )
                                await wait_for_lock_wait(control, session_id, pending)
                                await wait_for_lock_wait(
                                    control, independent_session_id, other
                                )
                                del value, parameters
                                assert pending.cancel()
                                assert not pending.cancel()
                                with pytest.raises(asyncio.CancelledError):
                                    await pending

                                await asyncio.wait_for(
                                    asyncio.shield(pending._cancel_and_settle()), timeout=6
                                )
                                probe = conn.cursor()
                                await probe.execute(
                                    "SELECT N'after cancellation', @@SPID",
                                    use_prepare=use_prepare,
                                )
                                assert await probe.fetchall() == [
                                    ("after cancellation", session_id)
                                ]
                                assert not other.done()
                            finally:
                                await control.execute(
                                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                                )

                            assert await other is independent_cursor
                            if independent_cursor.description is None:
                                assert await independent_cursor.nextset() is True
                            assert await independent_cursor.fetchall() == [(marker,)]
                            await conn.close()
                            assert conn.closed
                            with pytest.raises(mssql_py_core.InterfaceError, match="Connection is closed"):
                                await cursor.execute(
                                    "SELECT N'after close'", use_prepare=use_prepare
                                )
                            references.append(weakref.ref(pending))
                        finally:
                            try:
                                if pending is not None and not pending.done():
                                    pending.cancel()
                                    with pytest.raises(asyncio.CancelledError):
                                        await pending
                            finally:
                                if other is not None:
                                    await other
                        del pending, other

            gc.collect()
            for _ in range(600):
                if all(reference() is None for reference in references):
                    break
                await asyncio.sleep(0.01)
            else:
                pytest.fail("Cancelled execute futures were not released")

    asyncio.run(run())


def test_cancel_after_completion_does_not_cancel_next_string_query(
    client_context, use_prepare
):
    async def run():
        async with locked_string_table(client_context) as (control, table):
            conn = await connect(client_context)
            async with conn:
                cursor = conn.cursor()
                await cursor.execute("SELECT @@SPID", use_prepare=False)
                [(session_id,)] = await cursor.fetchall()
                completed_execute = cursor.execute(
                    "SELECT CAST(? AS nvarchar(max))",
                    "completed:\u6f22\U0001f600",
                    use_prepare=use_prepare,
                )
                assert await completed_execute is cursor
                completed_fetch = cursor.fetchall()
                assert await completed_fetch == [("completed:\u6f22\U0001f600",)]

                value = "next:\u6f22\U0001f600\x00" * 4097
                pending = cursor.execute(
                    f"UPDATE {table} SET id = id WHERE id = 1; "
                    "SELECT CAST(? AS nvarchar(max))",
                    value,
                    use_prepare=use_prepare,
                )
                try:
                    try:
                        await wait_for_lock_wait(control, session_id, pending)
                        assert not completed_execute.cancel()
                        assert not completed_fetch.cancel()
                        assert not pending.done()
                    finally:
                        await control.execute(
                            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                        )
                    assert await pending is cursor
                    if cursor.description is None:
                        assert await cursor.nextset() is True
                    assert await cursor.fetchall() == [(value,)]
                    assert await completed_fetch == [("completed:\u6f22\U0001f600",)]
                finally:
                    await pending

    asyncio.run(run())


def test_encoding_conversion_after_fetch_cancellation(client_context, use_prepare):
    async def run():
        owner = await connect(client_context)
        async with owner:
            control = owner.cursor()
            table = f"##async_strings_{uuid.uuid4().hex}"
            await control.execute(
                f"CREATE TABLE {table} (id int PRIMARY KEY, "
                "value varchar(max) COLLATE Japanese_CI_AS)",
                use_prepare=False,
            )
            try:
                await control.execute(
                    f"INSERT INTO {table} VALUES (1, ?), (2, ?)",
                    "\u65e5\u672c\u8a9e" * 262144, "\u65e5\u672c\u8a9e",
                    use_prepare=False,
                )
                conn = await connect(client_context)
                async with conn:
                    pending = None
                    try:
                        cursor = conn.cursor()
                        await cursor.execute("SELECT @@SPID", use_prepare=False)
                        [(session_id,)] = await cursor.fetchall()
                        await control.execute(
                            f"BEGIN TRANSACTION; UPDATE {table} WITH (ROWLOCK) "
                            "SET value = value WHERE id = 2",
                            use_prepare=False,
                        )
                        await cursor.execute(
                            f"SELECT value FROM {table} "
                            "WITH (READCOMMITTEDLOCK, ROWLOCK, INDEX(1)) "
                            "ORDER BY id OPTION (MAXDOP 1)",
                            use_prepare=use_prepare,
                        )
                        pending = cursor.fetchall()
                        await wait_for_lock_wait(control, session_id, pending)
                        assert pending.cancel()
                        with pytest.raises(asyncio.CancelledError):
                            await pending

                        probe = conn.cursor()
                        for sql_type, collation, encoding, text in ENCODING_CONVERSION_CASES:
                            value = f"{encoding}:{text * 1024}\x00tail"
                            statement = (
                                "SELECT @@SPID, value, DATALENGTH(value), "
                                "CONVERT(varbinary(max), value) FROM (VALUES "
                                f"(CAST(? COLLATE {collation} AS {sql_type}))) AS source(value)"
                            )
                            deadline = asyncio.get_running_loop().time() + 6
                            while True:
                                try:
                                    await probe.execute(
                                        statement, value, use_prepare=use_prepare
                                    )
                                    break
                                except mssql_py_core.InterfaceError as error:
                                    if "busy" not in str(error).lower():
                                        raise
                                    if asyncio.get_running_loop().time() >= deadline:
                                        pytest.fail("Cancelled fetch left the connection permanently busy")
                                    await asyncio.sleep(0.01)
                            encoded = value.encode(encoding)
                            assert await probe.fetchall() == [
                                (session_id, value, len(encoded), encoded)
                            ]
                    finally:
                        try:
                            if pending is not None and not pending.done():
                                pending.cancel()
                                with pytest.raises(asyncio.CancelledError):
                                    await pending
                        finally:
                            await control.execute(
                                "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION",
                                use_prepare=False,
                            )
            finally:
                await control.execute(f"DROP TABLE IF EXISTS {table}", use_prepare=False)

    asyncio.run(run())


def test_cancelled_large_string_insert_rolls_back_on_close(client_context, use_prepare):
    async def run():
        owner = await connect(client_context)
        async with owner:
            control = owner.cursor()
            table = f"##async_strings_{uuid.uuid4().hex}"
            rollback_table = f"{table}_rollback"
            await control.execute(
                f"CREATE TABLE {table} (id int PRIMARY KEY, value nvarchar(max))",
                use_prepare=False,
            )
            try:
                await control.execute(
                    f"CREATE TABLE {rollback_table} (id int PRIMARY KEY)", use_prepare=False
                )
                await control.execute(
                    f"INSERT INTO {table} VALUES (0, N'baseline')", use_prepare=False
                )
                writer = await connect(client_context, autocommit=False)
                async with writer:
                    pending = None
                    try:
                        cursor = writer.cursor()
                        await cursor.execute("SELECT @@SPID", use_prepare=False)
                        [(session_id,)] = await cursor.fetchall()
                        # Keep prior uncommitted work outside the table locked by the controller.
                        await cursor.execute(
                            f"INSERT INTO {rollback_table} VALUES (1)",
                            use_prepare=use_prepare,
                        )
                        await control.execute(
                            f"SELECT id FROM {rollback_table} WITH (NOLOCK)",
                            use_prepare=False,
                        )
                        assert await control.fetchall() == [(1,)]
                        await control.execute(
                            f"BEGIN TRANSACTION; SELECT id FROM {table} "
                            "WITH (TABLOCKX, HOLDLOCK)",
                            use_prepare=False,
                        )
                        assert await control.fetchall() == [(0,)]
                        pending = cursor.execute(
                            f"INSERT INTO {table} VALUES (1, ?)",
                            "\u6f22\U0001f600\x00" * 262144,
                            use_prepare=use_prepare,
                        )
                        await wait_for_lock_wait(control, session_id, pending)

                        assert pending.cancel()
                        with pytest.raises(asyncio.CancelledError):
                            await pending
                        await asyncio.wait_for(
                            asyncio.shield(pending._cancel_and_settle()), timeout=6
                        )
                    finally:
                        if pending is not None and not pending.done():
                            pending.cancel()
                            with pytest.raises(asyncio.CancelledError):
                                await pending
                        await control.execute(
                            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                        )
                    await control.execute(
                        f"SELECT COUNT(*) FROM {table} WHERE id = 1", use_prepare=False
                    )
                    assert await control.fetchall() == [(0,)]
                    await control.execute(
                        f"SELECT id FROM {rollback_table} WITH (NOLOCK)", use_prepare=False
                    )
                    assert await control.fetchall() == [(1,)]
                    await writer.close()

                await control.execute(f"SELECT id FROM {rollback_table}", use_prepare=False)
                assert await control.fetchall() == []
                await control.execute(
                    f"SELECT id, value FROM {table} ORDER BY id", use_prepare=False
                )
                assert await control.fetchall() == [(0, "baseline")]
            finally:
                await control.execute(
                    f"DROP TABLE IF EXISTS {table}; DROP TABLE IF EXISTS {rollback_table}",
                    use_prepare=False,
                )

    asyncio.run(run())


@pytest.mark.parametrize("interruption", ["timeout", "terminate-session"])
def test_interrupted_large_string_fetch(client_context, use_prepare, interruption):
    async def run():
        owner = await connect(client_context)
        async with owner:
            control = owner.cursor()
            table = f"##async_strings_{uuid.uuid4().hex}"
            await control.execute(
                f"CREATE TABLE {table} (id int PRIMARY KEY, value nvarchar(max))",
                use_prepare=False,
            )
            try:
                await control.execute(
                    f"INSERT INTO {table} VALUES (1, ?), (2, ?)",
                    "\u6f22\U0001f600" * 262144, "\u4f60\U0001f680" * 262144,
                    use_prepare=False,
                )
                reader = await connect(client_context, connect_retry_count=0)
                async with reader:
                    fetching = None
                    try:
                        await control.execute(
                            f"BEGIN TRANSACTION; UPDATE {table} WITH (ROWLOCK) "
                            "SET value = value WHERE id = 2",
                            use_prepare=False,
                        )
                        reader.timeout = 1 if interruption == "timeout" else 0
                        cursor = reader.cursor()
                        await cursor.execute("SELECT @@SPID", use_prepare=False)
                        [(session_id,)] = await cursor.fetchall()
                        await cursor.execute(
                            f"SELECT value FROM {table} "
                            "WITH (READCOMMITTEDLOCK, ROWLOCK, INDEX(1)) "
                            "ORDER BY id OPTION (MAXDOP 1)",
                            use_prepare=use_prepare,
                        )
                        fetching = cursor.fetchall()
                        if interruption == "timeout":
                            with pytest.raises(
                                mssql_py_core.OperationalError,
                                match=r"PyAsyncCursor\.fetchall.*Timeout Error",
                            ):
                                await asyncio.wait_for(fetching, timeout=6)

                            await cursor.execute(
                                "SELECT CAST(? AS nvarchar(64))",
                                "fresh after timeout:\u6f22\U0001f600",
                                use_prepare=use_prepare,
                            )
                            assert await cursor.fetchall() == [
                                ("fresh after timeout:\u6f22\U0001f600",)
                            ]
                        else:
                            await wait_for_lock_wait(control, session_id, fetching)
                            assert not fetching.done()
                            actor = await connect(client_context)
                            async with actor:
                                await actor.cursor().execute(
                                    f"KILL {session_id}", use_prepare=False
                                )
                            with pytest.raises(
                                mssql_py_core.DatabaseError,
                                match=r"PyAsyncCursor\.fetchall.*kill state",
                            ) as exc_info:
                                await asyncio.wait_for(fetching, timeout=6)
                            assert exc_info.value.sql_errors[0]["number"] == 596
                            await reader.close()
                            assert reader.closed
                            with pytest.raises(mssql_py_core.InterfaceError, match="Connection is closed"):
                                await cursor.execute(
                                    "SELECT N'after close'", use_prepare=use_prepare
                                )
                    finally:
                        if fetching is not None and not fetching.done():
                            fetching.cancel()
                            with pytest.raises(asyncio.CancelledError):
                                await fetching
                        await control.execute(
                            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                        )
            finally:
                await control.execute(f"DROP TABLE IF EXISTS {table}", use_prepare=False)

    asyncio.run(run())


def test_dropped_connection_reference_keeps_pending_string_valid(
    client_context, use_prepare
):
    async def run():
        async with locked_string_table(client_context) as (control, table):
            conn = await connect(client_context)
            cursor = conn.cursor()
            pending = None
            try:
                try:
                    await cursor.execute("SELECT @@SPID", use_prepare=False)
                    [(session_id,)] = await cursor.fetchall()
                    sample = "\u6f22\U0001f600e\u0301\x00"
                    value = sample * 65536 + "tail"
                    parameters = [value]
                    pending = cursor.execute(
                        f"UPDATE {table} SET id = id WHERE id = 1; "
                        "SELECT CAST(? AS nvarchar(max))",
                        parameters,
                        use_prepare=use_prepare,
                    )
                    del conn, value, parameters
                    gc.collect()

                    await wait_for_lock_wait(control, session_id, pending)
                    assert not pending.done()
                finally:
                    await control.execute(
                        "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                    )
                assert await pending is cursor
                if cursor.description is None:
                    assert await cursor.nextset() is True
                assert await cursor.fetchall() == [(sample * 65536 + "tail",)]
            finally:
                try:
                    if pending is not None:
                        await pending
                finally:
                    await cursor.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("collation", "left", "right", "equal"),
    [
        pytest.param("Latin1_General_100_CS_AS", "ABC", "abc", 0, id="case-sensitive"),
        pytest.param("Latin1_General_100_CI_AS", "ABC", "abc", 1, id="case-insensitive"),
        pytest.param("Latin1_General_100_CI_AS", "cafe", "caf\u00e9", 0, id="accent-sensitive"),
        pytest.param("Latin1_General_100_CI_AI", "cafe", "caf\u00e9", 1, id="accent-insensitive"),
        pytest.param("Latin1_General_100_BIN2", "ABC", "abc", 0, id="binary-case"),
        pytest.param("Latin1_General_100_BIN2", "\u00e9", "e\u0301", 0, id="binary-normalization"),
        pytest.param("Latin1_General_100_BIN2", "abc", "abc ", 1, id="trailing-space-right"),
        pytest.param("Latin1_General_100_BIN2", "abc ", "abc", 1, id="trailing-space-left"),
        pytest.param("Latin1_General_100_BIN2", "", " ", 1, id="empty-versus-space"),
        pytest.param("Latin1_General_100_BIN2", "abc", " abc", 0, id="leading-space-differs"),
    ],
)
def test_collation_comparison(client_context, use_prepare, collation, left, right, equal):
    rows = asyncio.run(fetch_query(
        client_context,
        f"SELECT CASE WHEN CAST(? AS nvarchar(100)) COLLATE {collation} = "
        f"CAST(? AS nvarchar(100)) COLLATE {collation} THEN 1 ELSE 0 END",
        (left, right),
        use_prepare,
    ))
    assert rows == [(equal,)]


@pytest.mark.parametrize("sql_type", ["varchar(100)", "varchar(max)"])
def test_utf8_collation_varchar_round_trip(client_context, use_prepare, sql_type):
    value = "Caf\u00e9 \u4f60\u597d \u0928\u092e\u0938\u094d\u0924\u0947 \U0001f600"
    row = asyncio.run(round_trip(
        client_context, sql_type, value, use_prepare,
        collation="Latin1_General_100_BIN2_UTF8",
    ))
    encoded = value.encode("utf-8")
    assert row == (value, len(encoded), encoded)


def test_supplementary_character_aware_collation(client_context, use_prepare):
    value = "A\U0001f600\U00010437B"
    rows = asyncio.run(fetch_query(
        client_context,
        "SELECT value, LEN(value), DATALENGTH(value), SUBSTRING(value, 2, 2), "
        "UNICODE(SUBSTRING(value, 2, 1)) FROM "
        "(VALUES (CAST(? AS nvarchar(100)) COLLATE Latin1_General_100_CI_AS_SC)) "
        "AS source(value)",
        (value,),
        use_prepare,
    ))
    assert rows == [(value, 4, 12, "\U0001f600\U00010437", 0x1F600)]


def test_unicode_sorting_under_binary_collation(client_context, use_prepare):
    values = ("\u4e2d", "\u03a9", "\u00e9", "a", "Z", "A")
    markers = ", ".join("(CAST(? AS nvarchar(100)))" for _ in values)
    rows = asyncio.run(fetch_query(
        client_context,
        f"SELECT value FROM (VALUES {markers}) AS source(value) "
        "ORDER BY value COLLATE Latin1_General_100_BIN2",
        values,
        use_prepare,
    ))
    assert rows == [(value,) for value in ("A", "Z", "a", "\u00e9", "\u03a9", "\u4e2d")]


@pytest.mark.parametrize(
    ("value", "pattern", "matches"),
    [
        pytest.param("\u4f60\u597d\u4e16\u754c", "\u4f60\u597d%", 1, id="chinese-prefix"),
        pytest.param("\u0928\u092e\u0938\u094d\u0924\u0947 \u0926\u0941\u0928\u093f\u092f\u093e",
                     "\u0928\u092e\u0938\u094d\u0924\u0947%", 1, id="hindi-prefix"),
        pytest.param("Hello \U0001f600\U0001f680", "%\U0001f600_", 1, id="emoji-underscore"),
        pytest.param("\u4f60\u597d\u4e16\u754c", "\u3053\u3093\u306b\u3061\u306f%", 0, id="unicode-nonmatch"),
        pytest.param("100% done", "100!% done", 1, id="escaped-percent"),
        pytest.param("100X done", "100!% done", 0, id="escaped-percent-nonmatch"),
        pytest.param("100X done", "100% done", 1, id="percent-wildcard"),
        pytest.param("a_b", "a!_b", 1, id="escaped-underscore"),
        pytest.param("acb", "a!_b", 0, id="escaped-underscore-nonmatch"),
        pytest.param("acb", "a_b", 1, id="underscore-wildcard"),
        pytest.param("a[b", "a![b", 1, id="escaped-bracket"),
        pytest.param("look!", "look!!", 1, id="escaped-escape"),
    ],
)
def test_unicode_like_patterns(client_context, use_prepare, value, pattern, matches):
    rows = asyncio.run(fetch_query(
        client_context,
        "SELECT CASE WHEN CAST(? AS nvarchar(100)) COLLATE Latin1_General_100_CI_AS_SC "
        "LIKE CAST(? AS nvarchar(100)) COLLATE Latin1_General_100_CI_AS_SC "
        "ESCAPE N'!' THEN 1 ELSE 0 END",
        (value, pattern),
        use_prepare,
    ))
    assert rows == [(matches,)]


@pytest.mark.parametrize("sql_type", ["nvarchar(4000)", "nvarchar(max)", "ntext"])
@pytest.mark.parametrize("value", ASCII_CASES + UNICODE_CASES)
def test_unicode_character_round_trip(client_context, use_prepare, sql_type, value):
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    encoded = None if value is None else value.encode("utf-16-le")
    assert row == (value, None if encoded is None else len(encoded), encoded)


@pytest.mark.parametrize(
    "value",
    [
        pytest.param("\ud800", id="lone-high-surrogate"),
        pytest.param("\udfff", id="lone-low-surrogate"),
        pytest.param("a\ud800b", id="embedded-surrogate"),
        pytest.param("\udc00\ud800", id="reversed-surrogate-pair"),
    ],
)
def test_rejects_invalid_unicode_without_inserting(client_context, use_prepare, value):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings (value nvarchar(max))",
                use_prepare=False,
            )
            with pytest.raises(TypeError, match="surrogates not allowed"):
                await cursor.execute(
                    "INSERT INTO #async_strings VALUES (?)",
                    value,
                    use_prepare=use_prepare,
                )
            await cursor.execute(
                "SELECT value FROM #async_strings", use_prepare=use_prepare
            )
            assert await cursor.fetchall() == []

            valid = "valid \U0001f600"
            await cursor.execute(
                "INSERT INTO #async_strings VALUES (?)",
                valid,
                use_prepare=use_prepare,
            )
            await cursor.execute(
                "SELECT value FROM #async_strings", use_prepare=use_prepare
            )
            assert await cursor.fetchall() == [(valid,)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize("sql_type", ["varchar(8000)", "varchar(max)", "text"])
@pytest.mark.parametrize("value", ASCII_CASES + CP1252_CASES)
def test_varchar_character_round_trip(client_context, use_prepare, sql_type, value):
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    encoded = None if value is None else value.encode("cp1252")
    assert row == (value, None if encoded is None else len(encoded), encoded)


@pytest.mark.parametrize(
    ("sql_type", "value"),
    [
        pytest.param("nvarchar(4000)", "a" * 3999, id="nvarchar-below-limit"),
        pytest.param("nvarchar(4000)", "a" * 4000, id="nvarchar-at-limit"),
        pytest.param("nvarchar(4000)", "\u6f22" * 4000, id="unicode-at-limit"),
        pytest.param("nvarchar(4000)", "\U0001f600" * 1999 + "a", id="surrogate-below-limit"),
        pytest.param("nvarchar(4000)", "\U0001f600" * 2000, id="surrogate-at-limit"),
        pytest.param("varchar(8000)", "a" * 7999, id="varchar-below-limit"),
        pytest.param("varchar(8000)", "a" * 8000, id="varchar-at-limit"),
        pytest.param("varchar(8000)", "\u20ac" * 8000, id="cp1252-at-limit"),
    ],
)
def test_bounded_string_length_round_trip(client_context, use_prepare, sql_type, value):
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    encoded = value.encode("utf-16-le" if sql_type.startswith("n") else "cp1252")
    assert row == (value, len(encoded), encoded)


@pytest.mark.parametrize("sql_type", ["nvarchar(max)", "ntext"])
@pytest.mark.parametrize("length", [255, 256, 3999, 4000, 4001, 4095, 4096, 4097, 32769])
@pytest.mark.parametrize("character", ["\u6f22", "\U0001f600"], ids=["bmp", "surrogate-pair"])
def test_long_unicode_round_trip(client_context, use_prepare, sql_type, length, character):
    suffix = "\x00e\u0301 end  "
    value = character * (length - len(suffix)) + suffix
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    encoded = value.encode("utf-16-le")
    assert row == (value, len(encoded), encoded)


@pytest.mark.parametrize("sql_type", ["varchar(max)", "text"])
@pytest.mark.parametrize("length", [255, 256, 7999, 8000, 8001, 8191, 8192, 8193, 65537])
def test_long_varchar_round_trip(client_context, use_prepare, sql_type, length):
    suffix = "\x00 end  "
    value = "x" * (length - len(suffix)) + suffix
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    encoded = value.encode("cp1252")
    assert row == (value, len(encoded), encoded)


@pytest.mark.parametrize(
    ("sql_type", "capacity", "character", "encoding"),
    [
        ("char(8000)", 8000, "x", "cp1252"),
        ("nchar(4000)", 4000, "\u6f22", "utf-16-le"),
    ],
)
@pytest.mark.parametrize("unused_capacity", [1, 0], ids=["below-limit", "at-limit"])
def test_fixed_width_capacity_boundary(
    client_context, use_prepare, sql_type, capacity, character, encoding, unused_capacity
):
    value = character * (capacity - unused_capacity)
    expected = value + " " * unused_capacity
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    encoded = expected.encode(encoding)
    assert row == (expected, len(encoded), encoded)


@pytest.mark.parametrize(
    ("sql_type", "value"),
    [
        pytest.param("char(8000)", "x" * 8001, id="char-overflow"),
        pytest.param("varchar(8000)", "x" * 8001, id="varchar-overflow"),
        pytest.param("varchar(8000)", "\u20ac" * 8001, id="cp1252-overflow"),
        pytest.param("nchar(4000)", "\u6f22" * 4001, id="nchar-overflow"),
        pytest.param("nvarchar(4000)", "\u6f22" * 4001, id="nvarchar-overflow"),
        pytest.param(
            "nvarchar(4000)", "\U0001f600" * 2000 + "x", id="utf16-unit-overflow"
        ),
    ],
)
def test_bounded_string_overflow(client_context, use_prepare, sql_type, value):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                f"SET ANSI_WARNINGS ON; CREATE TABLE #async_strings "
                f"(value {sql_type} COLLATE Latin1_General_100_BIN2)",
                use_prepare=False,
            )
            source_type = "nvarchar(max)" if sql_type.startswith("n") else "varchar(max)"
            statement = (
                "INSERT INTO #async_strings VALUES "
                f"(CAST(? COLLATE Latin1_General_100_BIN2 AS {source_type}))"
            )
            with pytest.raises(mssql_py_core.DatabaseError, match="(?i)truncat"):
                await cursor.execute(statement, value, use_prepare=use_prepare)
            await cursor.execute(
                "SELECT value FROM #async_strings", use_prepare=use_prepare
            )
            assert await cursor.fetchall() == []

            valid = value[:-1]
            await cursor.execute(statement, valid, use_prepare=use_prepare)
            await cursor.execute(
                "SELECT value FROM #async_strings", use_prepare=use_prepare
            )
            assert await cursor.fetchall() == [(valid,)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("source_type", "target_type", "encoding", "sample"),
    [
        ("char(8)", "varchar(8)", "cp1252", "Caf\u00e9"),
        ("nchar(8)", "nvarchar(8)", "utf-16-le", "\u6f22\U0001f600"),
    ],
)
@pytest.mark.parametrize("value_state", ["null", "empty", "sample", "trailing-space", "full"])
def test_fixed_to_variable_string_conversion(
    client_context, use_prepare, source_type, target_type, encoding, sample, value_state
):
    value = {
        "null": None,
        "empty": "",
        "sample": sample,
        "trailing-space": sample + " ",
        "full": "12345678",
    }[value_state]
    if value is None:
        expected = encoded = byte_length = None
    else:
        width = len(value.encode(encoding)) // (2 if encoding == "utf-16-le" else 1)
        expected = value + " " * (8 - width)
        encoded = expected.encode(encoding)
        byte_length = len(encoded)
    rows = asyncio.run(fetch_query(
        client_context,
        "SELECT source_value, value, DATALENGTH(source_value), DATALENGTH(value), "
        "CONVERT(varbinary(max), value) "
        f"FROM (VALUES (CAST(? COLLATE Latin1_General_100_BIN2 AS {source_type}))) "
        "AS source(source_value) "
        f"CROSS APPLY (VALUES (CAST(source_value AS {target_type}))) AS target(value)",
        (value,),
        use_prepare,
    ))
    assert rows == [(expected, expected, byte_length, byte_length, encoded)]


@pytest.mark.parametrize("sql_type", ["char(4)", "varchar(4)", "nchar(4)", "nvarchar(4)"])
@pytest.mark.parametrize("length", [3, 4, 5], ids=["below-limit", "at-limit", "over-limit"])
def test_explicit_max_to_bounded_string_conversion(
    client_context, use_prepare, sql_type, length
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SET ANSI_WARNINGS ON", use_prepare=False)
            text = "\u6f22\u5b57\u3042\u3044\u3046" if sql_type.startswith("n") else "abcdef"
            value = text[:length]
            expected = value[:4]
            if sql_type.startswith(("char(", "nchar(")):
                expected = expected.ljust(4)
            encoding = "utf-16-le" if sql_type.startswith("n") else "cp1252"
            source_type = "nvarchar(max)" if sql_type.startswith("n") else "varchar(max)"
            await cursor.execute(
                "SELECT value, DATALENGTH(value), CONVERT(varbinary(max), value) "
                f"FROM (VALUES (CAST(CAST(? COLLATE Latin1_General_100_BIN2 "
                f"AS {source_type}) AS {sql_type}))) AS source(value)",
                value,
                use_prepare=use_prepare,
            )
            encoded = expected.encode(encoding)
            assert await cursor.fetchall() == [(expected, len(encoded), encoded)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("sql_type", "pattern", "encoding", "bytes_per_character"),
    [
        ("varchar(max)", "Ab9!xY2?", "cp1252", 1),
        (
            "nvarchar(max)",
            "\u6f22\u5b57\u03a9\u0416\u3042\u20ac\u0915\u05d0",
            "utf-16-le",
            2,
        ),
    ],
    ids=["ascii", "unicode"],
)
@pytest.mark.parametrize(
    "size_bytes", [64 * 1024, 1024 * 1024, 10 * 1024 * 1024],
    ids=["64KiB", "1MiB", "10MiB"],
)
def test_large_string_payload_round_trip(
    client_context, use_prepare, sql_type, pattern, encoding, bytes_per_character,
    size_bytes
):
    repeats, remainder = divmod(size_bytes // bytes_per_character, len(pattern))
    value = pattern * repeats + pattern[:remainder]
    encoded = value.encode(encoding)
    assert len(encoded) == size_bytes
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    assert row == (value, size_bytes, encoded)


def test_multiple_large_string_columns_are_independent(client_context, use_prepare):
    values = (
        "first:" + "A\u00e9\u20ac\x00" * 262144 + ":tail",
        "second:" + "\u6f22\U0001f600e\u0301\x00" * 131072 + ":tail",
        "third:" + "\u0928\u4f60\U0001f680e\u0301\x00" * 131072 + ":tail",
    )
    sizes = tuple(
        len(value.encode(encoding))
        for value, encoding in zip(values, ("cp1252", "utf-16-le", "utf-8"))
    )
    assert all(size >= 1024 * 1024 for size in sizes)
    rows = asyncio.run(fetch_query(
        client_context,
        "SELECT ansi_value, unicode_value, utf8_value, "
        "DATALENGTH(ansi_value), DATALENGTH(unicode_value), DATALENGTH(utf8_value) "
        "FROM (VALUES ("
        "CAST(? COLLATE Latin1_General_100_BIN2 AS varchar(max)), "
        "CAST(? AS nvarchar(max)), "
        "CAST(? COLLATE Latin1_General_100_BIN2_UTF8 AS varchar(max)))) "
        "AS source(ansi_value, unicode_value, utf8_value)",
        values,
        use_prepare,
    ))
    assert rows == [(*values, *sizes)]


@pytest.mark.parametrize("sql_type", ["char(16)", "nchar(16)"])
@pytest.mark.parametrize(
    "value",
    [
        pytest.param(None, id="null"),
        pytest.param("", id="empty"),
        pytest.param(" ", id="space"),
        pytest.param("  padded  ", id="spaces"),
        pytest.param("O'Brien ? %", id="punctuation"),
        pytest.param("a\x00b\t\r\n", id="controls"),
        pytest.param("caf\u00e9 \u20ac", id="cp1252"),
        pytest.param("x" * 16, id="full-width"),
    ],
)
def test_fixed_width_string_padding(client_context, use_prepare, sql_type, value):
    row = asyncio.run(round_trip(client_context, sql_type, value, use_prepare))
    if value is None:
        assert row == (None, None, None)
    else:
        expected = value.ljust(16)
        encoded = expected.encode("utf-16-le" if sql_type.startswith("n") else "cp1252")
        assert row == (expected, len(encoded), encoded)


@pytest.mark.parametrize(
    "value",
    [
        pytest.param("\u4e2d\u6587", id="cjk"),
        pytest.param("\U0001f600" * 8, id="full-utf16-width"),
        pytest.param("A\U0001f600B", id="mixed-utf16-width"),
        pytest.param("e\u0301", id="decomposed"),
    ],
)
def test_nchar_padding_counts_utf16_units(client_context, use_prepare, value):
    expected = value + " " * (16 - len(value.encode("utf-16-le")) // 2)
    row = asyncio.run(round_trip(client_context, "nchar(16)", value, use_prepare))
    assert row == (expected, 32, expected.encode("utf-16-le"))


@pytest.mark.parametrize("parameter_style", ["positional", "named"])
def test_executemany_string_batch_integrity(client_context, use_prepare, parameter_style):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings (id int PRIMARY KEY, "
                "ansi_value varchar(max) COLLATE Latin1_General_100_BIN2 NULL, "
                "unicode_value nvarchar(max) COLLATE Latin1_General_100_BIN2 NULL)",
                use_prepare=False,
            )
            cases = [
                ("ASCII", "\u6f22\u5b57 \U0001f600"),
                ("", ""),
                (None, None),
                ("ANSI only", None),
                (None, "\u0928\u092e\u0938\u094d\u0924\u0947"),
                ("caf\u00e9 ' \" \\\r\n\t\x00%_[]?", "e\u0301 \u4f60\u597d \U0001f680\x00"),
                ("x" * 255, "\u6f22" * 256),
                ("Price: \u20ac100 \u2122 \u0152\u0160\u017d", "Caf\u00e9 \u20ac \u2122"),
            ]
            rows = []
            for row_id in range(2048):
                ansi_value, unicode_value = cases[row_id % len(cases)]
                if row_id % 257 == 0:
                    ansi_value = "x" * 8001
                    unicode_value = "\u6f22\U0001f600" * 3000
                if ansi_value not in (None, ""):
                    ansi_value = f"{row_id}:{ansi_value}"
                if unicode_value not in (None, ""):
                    unicode_value = f"{row_id}:{unicode_value}"
                rows.append((row_id, ansi_value, unicode_value))

            if parameter_style == "named":
                statement = (
                    "INSERT INTO #async_strings VALUES (%(id)s, %(ansi)s, %(unicode)s)"
                )
                parameters = (
                    {"unicode": unicode_value, "id": row_id, "ansi": ansi_value}
                    for row_id, ansi_value, unicode_value in rows
                )
            else:
                statement = "INSERT INTO #async_strings VALUES (?, ?, ?)"
                parameters = rows
            await cursor.executemany(statement, parameters, use_prepare=use_prepare)
            assert cursor.rowcount == len(rows)

            await cursor.execute(
                "SELECT id, ansi_value, unicode_value, "
                "DATALENGTH(ansi_value), DATALENGTH(unicode_value) "
                "FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            total = 0
            while batch := await cursor.fetchmany(127):
                expected = [
                    (
                        row_id, ansi_value, unicode_value,
                        None if ansi_value is None else len(ansi_value.encode("cp1252")),
                        None if unicode_value is None else len(unicode_value.encode("utf-16-le")),
                    )
                    for row_id, ansi_value, unicode_value in rows[total:total + len(batch)]
                ]
                assert batch == expected
                total += len(batch)
            assert total == len(rows)
        finally:
            await conn.close()

    asyncio.run(run())


def test_fetch_100000_string_rows(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            prefix = "row-\u6f22\U0001f600-"
            await cursor.execute(
                """
                WITH digits(value) AS (
                    SELECT value FROM (VALUES (0), (1), (2), (3), (4),
                                              (5), (6), (7), (8), (9)) AS d(value)
                ), numbers(value) AS (
                    SELECT a.value + 10 * b.value + 100 * c.value
                         + 1000 * d.value + 10000 * e.value
                    FROM digits AS a CROSS JOIN digits AS b CROSS JOIN digits AS c
                    CROSS JOIN digits AS d CROSS JOIN digits AS e
                )
                SELECT value, CAST(? AS nvarchar(50)) + CONVERT(nvarchar(10), value)
                FROM numbers ORDER BY value
                """,
                prefix,
                use_prepare=use_prepare,
            )
            total = 0
            while batch := await cursor.fetchmany(2048):
                assert batch == [
                    (row_id, f"{prefix}{row_id}")
                    for row_id in range(total, total + len(batch))
                ]
                total += len(batch)
            assert total == 100000
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize("concurrency_model", ["asyncio", "threads"])
def test_concurrent_unicode_inserts_and_retrieval(
    client_context, use_prepare, concurrency_model
):
    worker_count = len(ENCODING_CONVERSION_CASES)
    barrier = threading.Barrier(worker_count, timeout=30)

    async def worker(worker_id):
        sql_type, collation, encoding, text = ENCODING_CONVERSION_CASES[worker_id]
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings (id int PRIMARY KEY, value nvarchar(max), "
                f"encoded_value {sql_type} COLLATE {collation})",
                use_prepare=False,
            )
            await cursor.execute("SELECT @@SPID", use_prepare=False)
            [(session_id,)] = await cursor.fetchall()
            values = []
            for cycle in range(4):
                prefix = f"worker-{worker_id}:cycle-{cycle}:"
                values.extend([
                    prefix + "ASCII",
                    prefix + "\u6f22\U0001f600e\u0301" * 2048,
                    "",
                    None,
                    prefix + "\u0928\u092e\u0938\u094d\u0924\u0947\x00\u4f60\u597d",
                ])
            rows = []
            expected = []
            for row_id, value in enumerate(values):
                if value is None or value == "":
                    converted = value
                else:
                    repeats = 1024 if len(value) > 1000 else 1
                    converted = f"worker-{worker_id}:row-{row_id}:{text * repeats}\x00tail"
                encoded = None if converted is None else converted.encode(encoding)
                rows.append((row_id, value, converted))
                expected.append((
                    row_id, value, converted,
                    None if encoded is None else len(encoded), encoded,
                ))

            await asyncio.to_thread(barrier.wait)
            await cursor.executemany(
                "INSERT INTO #async_strings VALUES (?, ?, ?)",
                rows,
                use_prepare=use_prepare,
            )
            assert cursor.rowcount == len(expected)

            await asyncio.to_thread(barrier.wait)
            await cursor.execute(
                "SELECT id, value, encoded_value, DATALENGTH(encoded_value), "
                "CONVERT(varbinary(max), encoded_value) "
                "FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            total = 0
            while batch := await cursor.fetchmany(7):
                assert batch == expected[total:total + len(batch)]
                total += len(batch)
            assert total == len(expected)
            return session_id
        finally:
            await conn.close()

    async def run_tasks():
        asyncio.get_running_loop().set_default_executor(
            ThreadPoolExecutor(max_workers=worker_count)
        )
        return await asyncio.gather(*(worker(index) for index in range(worker_count)))

    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        if concurrency_model == "asyncio":
            session_ids = asyncio.run(run_tasks())
        else:
            with ThreadPoolExecutor(max_workers=worker_count) as executor:
                futures = [
                    executor.submit(asyncio.run, worker(index))
                    for index in range(worker_count)
                ]
                session_ids = [future.result(timeout=90) for future in futures]
    assert len(set(session_ids)) == worker_count


def test_100_concurrent_string_queries(client_context, use_prepare):
    async def run():
        worker_count = 100
        dispatched = asyncio.Event()
        issued = 0
        tasks = []
        owner = await connect(client_context)
        async with owner:
            control = owner.cursor()
            table = f"##async_strings_{uuid.uuid4().hex}"
            await control.execute(
                f"CREATE TABLE {table} (id int PRIMARY KEY)", use_prepare=False
            )

            async def worker(worker_id):
                nonlocal issued
                conn = await connect(client_context)
                async with conn:
                    conn.timeout = 60
                    cursor = conn.cursor()
                    state = worker_id % 4
                    if state == 0:
                        value = None
                    elif state == 1:
                        value = ""
                    else:
                        repeats = 1024 if state == 3 else 1
                        value = f"worker-{worker_id}:\u6f22\U0001f600e\u0301\x00" * repeats
                    pending = cursor.execute(
                        "SELECT @@SPID, CAST(? AS int), value, DATALENGTH(value) "
                        f"FROM {table} WITH (READCOMMITTEDLOCK) "
                        "CROSS APPLY (VALUES (CAST(? AS nvarchar(max)))) AS source(value)",
                        worker_id, value,
                        use_prepare=use_prepare,
                    )
                    issued += 1
                    if issued == worker_count:
                        dispatched.set()
                    assert await pending is cursor
                    rows = await cursor.fetchall()
                    assert len(rows) == 1
                    expected_size = None if value is None else len(value.encode("utf-16-le"))
                    assert rows[0][1:] == (worker_id, value, expected_size)
                    return rows[0][0]

            try:
                await control.execute(
                    f"INSERT INTO {table} VALUES (1); "
                    f"BEGIN TRANSACTION; SELECT id FROM {table} WITH (TABLOCKX, HOLDLOCK)",
                    use_prepare=False,
                )
                if control.description is None:
                    assert await control.nextset() is True
                assert await control.fetchall() == [(1,)]
                tasks = [
                    asyncio.create_task(worker(worker_id))
                    for worker_id in range(worker_count)
                ]
                waiter = asyncio.create_task(dispatched.wait())
                try:
                    done, _ = await asyncio.wait(
                        [waiter, *tasks],
                        timeout=45,
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                    for task in done - {waiter}:
                        task.result()
                    assert waiter in done, "Not all workers dispatched their queries"
                finally:
                    waiter.cancel()
                    await asyncio.gather(waiter, return_exceptions=True)
                assert issued == worker_count
                assert all(not task.done() for task in tasks)

                await control.execute("ROLLBACK TRANSACTION", use_prepare=False)
                session_ids = await asyncio.wait_for(asyncio.gather(*tasks), timeout=30)
                assert len(set(session_ids)) == worker_count
            finally:
                try:
                    await control.execute(
                        "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                    )
                finally:
                    try:
                        # Let every worker close its connection after an earlier failure.
                        await asyncio.gather(*tasks, return_exceptions=True)
                    finally:
                        await control.execute(
                            f"DROP TABLE IF EXISTS {table}", use_prepare=False
                        )

    asyncio.run(run())


@pytest.mark.parametrize("same_cursor", [False, True], ids=["other-cursor", "same-cursor"])
def test_overlapping_string_execute_is_rejected(client_context, use_prepare, same_cursor):
    async def run():
        async with locked_string_table(client_context) as (control, table):
            conn = await connect(client_context)
            async with conn:
                owner = conn.cursor()
                contender = owner if same_cursor else conn.cursor()
                await owner.execute("SELECT @@SPID", use_prepare=False)
                [(session_id,)] = await owner.fetchall()
                statement = (
                    f"UPDATE {table} SET id = id WHERE id = 1; "
                    "SELECT CAST(? AS nvarchar(max))"
                )
                original = "original:\u6f22\U0001f600\x00" * 2048
                replacement = "after rejection:\u0928\u4f60\U0001f680"
                pending = owner.execute(statement, original, use_prepare=use_prepare)
                try:
                    try:
                        await wait_for_lock_wait(control, session_id, pending)
                        with pytest.raises(mssql_py_core.InterfaceError, match="busy with another cursor operation"):
                            await contender.execute(
                                statement,
                                replacement,
                                use_prepare=use_prepare,
                                reset_cursor=False,
                            )
                    finally:
                        await control.execute(
                            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", use_prepare=False
                        )
                    assert await pending is owner
                    if owner.description is None:
                        assert await owner.nextset() is True
                    assert await owner.fetchall() == [(original,)]
                    assert await owner.nextset() is False

                    assert await contender.execute(
                        statement,
                        replacement,
                        use_prepare=use_prepare,
                        reset_cursor=False,
                    ) is contender
                    if contender.description is None:
                        assert await contender.nextset() is True
                    assert await contender.fetchall() == [(replacement,)]
                finally:
                    await pending

    asyncio.run(run())


@pytest.mark.parametrize("parameter_style", ["positional", "named"])
def test_explicit_varchar_and_nvarchar_parameter_mapping(
    client_context, use_prepare, parameter_style
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            ansi_value = "ASCII ? ' \\ tail  "
            unicode_value = "\u4e2d\u6587 \U0001f600"
            if parameter_style == "named":
                markers = "%(ansi)s, %(unicode)s"
                parameters = ({"unicode": unicode_value, "ansi": ansi_value},)
            else:
                markers = "?, ?"
                parameters = (ansi_value, unicode_value)

            # SQL_VARCHAR(32), SQL_WVARCHAR(32); the latter occupies 64 bytes.
            cursor.setinputsizes([(12, 32, 0), (-9, 32, 0)])
            await cursor.execute(
                "SELECT ansi_value, unicode_value, "
                "CONVERT(varchar(20), SQL_VARIANT_PROPERTY("
                "CAST(ansi_value AS sql_variant), 'BaseType')), "
                "CONVERT(int, SQL_VARIANT_PROPERTY("
                "CAST(ansi_value AS sql_variant), 'MaxLength')), "
                "CONVERT(varchar(20), SQL_VARIANT_PROPERTY("
                "CAST(unicode_value AS sql_variant), 'BaseType')), "
                "CONVERT(int, SQL_VARIANT_PROPERTY("
                "CAST(unicode_value AS sql_variant), 'MaxLength')) "
                f"FROM (VALUES ({markers})) AS source(ansi_value, unicode_value)",
                *parameters,
                use_prepare=use_prepare,
            )
            assert await cursor.fetchall() == [
                (ansi_value, unicode_value, "varchar", 32, "nvarchar", 64)
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    "sql_type",
    ["char(64)", "varchar(64)", "varchar(max)", "nchar(64)", "nvarchar(64)", "nvarchar(max)"],
)
@pytest.mark.parametrize("execution", ["execute", "executemany"])
def test_string_update_and_delete_matrix(client_context, use_prepare, sql_type, execution):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings (id int PRIMARY KEY, "
                f"value {sql_type} COLLATE Latin1_General_100_BIN2 NULL)",
                use_prepare=False,
            )

            async def apply(statement, rows):
                if execution == "executemany":
                    await cursor.executemany(statement, rows, use_prepare=use_prepare)
                    assert cursor.rowcount == len(rows)
                else:
                    for row in rows:
                        await cursor.execute(statement, row, use_prepare=use_prepare)
                        assert cursor.rowcount == 1

            original = [(1, None), (2, ""), (3, "ASCII"), (4, "a\x00\tb"), (5, "caf\u00e9")]
            updated = [
                (1, "updated ASCII"),
                (2, None),
                (3, ""),
                (4, "changed\x00\x02\r\n"),
                (5, "Caf\u00e9 updated"),
            ]
            if sql_type.startswith("n"):
                original.append((6, "\u6f22\U0001f600e\u0301"))
                updated.append((6, "e\u0301\U0001f680\u5b57"))

            await apply("INSERT INTO #async_strings VALUES (?, ?)", original)
            await apply(
                "UPDATE #async_strings SET value = ? WHERE id = ?",
                [(value, row_id) for row_id, value in updated],
            )

            expected = []
            for row_id, value in updated:
                if value is not None and sql_type.startswith(("char(", "nchar(")):
                    encoding = "utf-16-le" if sql_type.startswith("n") else "cp1252"
                    units = len(value.encode(encoding)) // (2 if sql_type.startswith("n") else 1)
                    value += " " * (64 - units)
                expected.append((row_id, value))
            await cursor.execute(
                "SELECT id, value FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            assert await cursor.fetchone() == expected[0]
            assert await cursor.fetchmany(2) == expected[1:3]
            assert await cursor.fetchall() == expected[3:]

            await apply(
                "DELETE FROM #async_strings WHERE id = ? AND value = ?",
                [(row_id, value) for row_id, value in updated if value not in (None, "")],
            )
            await cursor.execute(
                "SELECT id, value FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            assert await cursor.fetchall() == expected[1:3]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize("parameter_style", ["positional", "named"])
def test_mixed_strings_across_rows_and_fetch_methods(
    client_context, use_prepare, parameter_style
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_strings (id int PRIMARY KEY, value nvarchar(max))",
                use_prepare=False,
            )
            values = [
                None,
                "",
                "\x00",
                "  trailing  ",
                "\u00e9",
                "e\u0301",
                "\u4e2d\u6587",
                "\U0001f469\u200d\U0001f4bb",
                "? %(value)s :value @value ' -- /* */",
                "\u6f22\U0001f600" * 5000,
                "",
                "short after long",
                None,
                "",
            ]
            for cycle in range(3):
                values.extend([
                    f"short-{cycle}",
                    f"long-{cycle}:\u6f22\U0001f600" * 1000,
                ])
            for row_id, value in enumerate(values):
                if parameter_style == "named":
                    await cursor.execute(
                        "INSERT INTO #async_strings VALUES (%(id)s, %(value)s)",
                        {"id": row_id, "value": value},
                        use_prepare=use_prepare,
                        reset_cursor=False,
                    )
                else:
                    await cursor.execute(
                        "INSERT INTO #async_strings VALUES (?, ?)",
                        row_id,
                        value,
                        use_prepare=use_prepare,
                        reset_cursor=False,
                    )

            await cursor.execute(
                "SELECT id, value FROM #async_strings ORDER BY id",
                use_prepare=use_prepare,
            )
            expected = list(enumerate(values))
            assert await cursor.fetchone() == expected[0]
            assert await cursor.fetchmany(4) == expected[1:5]
            assert await cursor.fetchall() == expected[5:]
            assert await cursor.fetchone() is None
            assert await cursor.fetchmany(4) == []
            assert await cursor.fetchall() == []
        finally:
            await conn.close()

    asyncio.run(run())

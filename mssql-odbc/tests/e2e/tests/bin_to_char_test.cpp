// Copyright (c) Microsoft Corporation. All rights reserved.
// bin_to_char_test.cpp  –  binary sources into character targets (AB#47240).
//
// ODBC Appendix D makes binary -> character a mandatory conversion. msodbcsql
// renders upper-case hex with no 0x prefix, and every expectation here was
// measured against it before being asserted.
//
// The rule worth stating is that a byte's two hex characters are atomic: msodbcsql
// fills whole bytes and leaves an odd trailing slot empty rather than splitting a
// pair, so a 4-byte buffer takes "01" and not "010". That is why these tests check
// several buffer sizes rather than one.

#include "odbc_test_fixture.h"

#include <cstring>
#include <string>

class BinToCharLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
    SQLRETURN ExecDirect(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }
    void FetchOne(const std::string& sql) {
        ASSERT_SQL_OK(ExecDirect(sql), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    }
    // Reads column 1 as SQL_C_CHAR into a buffer of exactly `bufLen` bytes.
    void ReadChar(SQLLEN bufLen, SQLRETURN* rc, SQLLEN* ind, std::string* text) {
        char buf[512];
        ASSERT_GE(bufLen, 0);
        ASSERT_LE(bufLen, static_cast<SQLLEN>(sizeof(buf)));
        std::memset(buf, 0x7E, sizeof(buf));
        *rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, bufLen, ind);
        text->clear();
        if (SQL_SUCCEEDED(*rc) && bufLen > 0) {
            const char* end =
                static_cast<const char*>(std::memchr(buf, '\0', static_cast<size_t>(bufLen)));
            ASSERT_NE(nullptr, end) << "missing terminator within BufferLength";
            text->assign(buf, static_cast<size_t>(end - buf));
        }
    }
};

// ---------------------------------------------------------------------------
// Non-PLP: binary(n), varbinary(n), image.
// ---------------------------------------------------------------------------

TEST_F(BinToCharLiveTest, FixedBinaryRendersUpperCaseHex) {
    FetchOne("SELECT CAST(0x0102AB AS BINARY(3))");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(6, ind) << "two characters per byte";
    EXPECT_EQ("0102AB", text);
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, VarbinaryRendersUpperCaseHex) {
    FetchOne("SELECT CAST(0xDEADBEEF AS VARBINARY(8))");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(8, ind);
    EXPECT_EQ("DEADBEEF", text);
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, EmptyBinaryRendersEmptyString) {
    FetchOne("SELECT CAST(0x AS VARBINARY(8))");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(0, ind);
    EXPECT_EQ("", text);
    SQLCloseCursor(stmt_);
}

// The pair-atomicity rule, across every interesting buffer size. A buffer of n
// bytes holds floor((n-1)/2)*2 characters: one is reserved for the terminator and
// the remainder rounds down to a whole number of bytes.
TEST_F(BinToCharLiveTest, HexNeverSplitsAByteAcrossTheBufferEdge) {
    struct Case {
        SQLLEN bufLen;
        const char* expected;
    };
    const Case cases[] = {{1, ""}, {2, ""},     {3, "01"},   {4, "01"},
                          {5, "0102"}, {6, "0102"}, {7, "0102AB"}};

    for (const auto& c : cases) {
        FetchOne("SELECT CAST(0x0102AB AS BINARY(3))");
        SQLRETURN rc;
        SQLLEN ind = -1;
        std::string text;
        ReadChar(c.bufLen, &rc, &ind, &text);
        const bool complete = std::strlen(c.expected) == 6;
        EXPECT_EQ(complete ? SQL_SUCCESS : SQL_SUCCESS_WITH_INFO, rc)
            << "bufLen " << c.bufLen;
        EXPECT_EQ(6, ind) << "the untruncated length, bufLen " << c.bufLen;
        EXPECT_EQ(c.expected, text) << "bufLen " << c.bufLen;
        SQLCloseCursor(stmt_);
    }
}

TEST_F(BinToCharLiveTest, ChunkedHexReportsADecreasingRemainder) {
    FetchOne("SELECT CAST(0x0102AB40 AS BINARY(4))");

    const char* expected[] = {"01", "02", "AB", "40"};
    const SQLLEN remaining[] = {8, 6, 4, 2};
    for (int i = 0; i < 4; ++i) {
        SQLRETURN rc;
        SQLLEN ind = -1;
        std::string text;
        ReadChar(4, &rc, &ind, &text);
        EXPECT_EQ(i == 3 ? SQL_SUCCESS : SQL_SUCCESS_WITH_INFO, rc) << "chunk " << i;
        EXPECT_EQ(remaining[i], ind) << "chunk " << i;
        EXPECT_EQ(expected[i], text) << "chunk " << i;
    }
    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(4, &rc, &ind, &text);
    EXPECT_EQ(SQL_NO_DATA, rc) << "drained";
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, NonPlpZeroLengthHexProbePreservesBufferAndValue) {
    const auto check = [&](auto& buf, SQLSMALLINT target) {
        for (const char* source : {"BINARY(3)", "VARBINARY(8)"}) {
            SCOPED_TRACE(::testing::Message() << source << ", target " << target);
            FetchOne(std::string("SELECT CAST(0x0102AB AS ") + source + ")");
            for (auto& unit : buf) unit = '~';
            SQLLEN ind = -1;
            for (int probe = 0; probe < 2; ++probe) {
                ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
                          SQLGetData(stmt_, 1, target, buf, 0, &ind));
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
                EXPECT_EQ(static_cast<SQLLEN>(6 * sizeof(buf[0])), ind);
                for (const auto unit : buf) EXPECT_EQ('~', unit);
            }
            ASSERT_EQ(SQL_SUCCESS,
                      SQLGetData(stmt_, 1, target, buf, sizeof(buf), &ind));
            EXPECT_EQ(static_cast<SQLLEN>(6 * sizeof(buf[0])), ind);
            const char* expected = "0102AB";
            for (int i = 0; i < 6; ++i) EXPECT_EQ(expected[i], buf[i]);
            EXPECT_EQ(0, buf[6]);
            EXPECT_EQ('~', buf[7]);
            EXPECT_EQ(SQL_NO_DATA,
                      SQLGetData(stmt_, 1, target, buf, sizeof(buf), &ind));
            ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        }
    };
    SQLCHAR narrow[8];
    SQLWCHAR wide[8];
    ASSERT_NO_FATAL_FAILURE(check(narrow, SQL_C_CHAR));
    ASSERT_NO_FATAL_FAILURE(check(wide, SQL_C_WCHAR));
}

TEST_F(BinToCharLiveTest, WideTargetRendersTheSameHex) {
    FetchOne("SELECT CAST(0x0102AB AS BINARY(3))");

    SQLWCHAR wbuf[64];
    for (auto& unit : wbuf) unit = '~';
    SQLLEN ind = -1;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, wbuf, sizeof(wbuf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(12, ind) << "characters are reported in bytes for a wide target";
    std::string narrow;
    for (int i = 0; i < 64 && wbuf[i]; ++i) narrow.push_back(static_cast<char>(wbuf[i]));
    EXPECT_EQ("0102AB", narrow);
    SQLCloseCursor(stmt_);
}

// Pair atomicity on the wide target, where the capacity is in code units.
TEST_F(BinToCharLiveTest, WideTargetAlsoKeepsBytesWhole) {
    struct Case {
        SQLLEN bufBytes;
        const char* expected;
    };
    const Case cases[] = {{4, ""}, {6, "01"}, {8, "01"}, {10, "0102"}};

    for (const auto& c : cases) {
        FetchOne("SELECT CAST(0x0102AB AS BINARY(3))");
        SQLWCHAR wbuf[64];
        for (auto& unit : wbuf) unit = '~';
        SQLLEN ind = -1;
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO,
                  SQLGetData(stmt_, 1, SQL_C_WCHAR, wbuf, c.bufBytes, &ind))
            << "bufBytes " << c.bufBytes;
        EXPECT_EQ(12, ind) << "bufBytes " << c.bufBytes;
        std::string narrow;
        for (int i = 0; i < 64 && wbuf[i]; ++i) narrow.push_back(static_cast<char>(wbuf[i]));
        EXPECT_EQ(c.expected, narrow) << "bufBytes " << c.bufBytes;
        SQLCloseCursor(stmt_);
    }
}

// ---------------------------------------------------------------------------
// PLP: varbinary(max) and the UDT types, which stream rather than materialize.
// ---------------------------------------------------------------------------

TEST_F(BinToCharLiveTest, VarbinaryMaxRendersHex) {
    FetchOne("SELECT CAST(0x4142 AS VARBINARY(MAX))");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(4, ind);
    EXPECT_EQ("4142", text);
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, EmptyVarbinaryMaxRendersEmptyString) {
    FetchOne("SELECT CAST(0x AS VARBINARY(MAX))");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(0, ind);
    EXPECT_EQ("", text);
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, NullVarbinaryMaxReportsNull) {
    FetchOne("SELECT CAST(NULL AS VARBINARY(MAX))");

    char buf[16];
    std::memset(buf, 0x7E, sizeof(buf));
    SQLLEN ind = -1;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_NULL_DATA, ind);
    // No comparison skip: only the shared NULL indicator is asserted.
    // The pre-existing NULL terminator difference is tracked in #555;
    // buffer-content parity belongs with that fix, not hex conversion.
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, StreamedHexChunksWithADecreasingRemainder) {
    FetchOne("SELECT CAST(0x0102AB40 AS VARBINARY(MAX))");

    const char* expected[] = {"01", "02", "AB", "40"};
    const SQLLEN remaining[] = {8, 6, 4, 2};
    for (int i = 0; i < 4; ++i) {
        SQLRETURN rc;
        SQLLEN ind = -1;
        std::string text;
        ReadChar(4, &rc, &ind, &text);
        EXPECT_EQ(i == 3 ? SQL_SUCCESS : SQL_SUCCESS_WITH_INFO, rc) << "chunk " << i;
        EXPECT_EQ(remaining[i], ind) << "chunk " << i;
        EXPECT_EQ(expected[i], text) << "chunk " << i;
    }
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, StreamedWideHexChunksWithADecreasingByteRemainder) {
    const char* expected[] = {"01", "02", "AB", "40"};
    for (const SQLLEN bufUnits : {3, 4}) {
        SCOPED_TRACE(bufUnits);
        FetchOne("SELECT CAST(0x0102AB40 AS VARBINARY(MAX))");

        for (int i = 0; i < 4; ++i) {
            SCOPED_TRACE(i);
            SQLWCHAR buf[5] = {'~', '~', '~', '~', '~'};
            SQLLEN ind = -1;
            const SQLRETURN rc = SQLGetData(
                stmt_, 1, SQL_C_WCHAR, buf, bufUnits * sizeof(SQLWCHAR), &ind);
            ASSERT_EQ(i == 3 ? SQL_SUCCESS : SQL_SUCCESS_WITH_INFO, rc)
                << StmtDiagState();
            EXPECT_EQ(static_cast<SQLLEN>((8 - 2 * i) * sizeof(SQLWCHAR)), ind);
            EXPECT_EQ(static_cast<SQLWCHAR>(expected[i][0]), buf[0]);
            EXPECT_EQ(static_cast<SQLWCHAR>(expected[i][1]), buf[1]);
            EXPECT_EQ(0, buf[2]);
            EXPECT_EQ('~', buf[bufUnits]) << "must not write past BufferLength";
            if (i < 3) {
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
            }
        }

        SQLWCHAR buf[5] = {};
        SQLLEN ind = -1;
        EXPECT_EQ(SQL_NO_DATA,
                  SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind));
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

TEST_F(BinToCharLiveTest, StreamedHexPairSizedProbesDoNotConsumeValue) {
    const auto check = [&](auto& buf, SQLSMALLINT target) {
        const SQLLEN unitBytes = sizeof(buf[0]);
        for (SQLLEN bufBytes = 0; bufBytes < 3 * unitBytes; ++bufBytes) {
            SCOPED_TRACE(::testing::Message() << "target " << target
                                             << ", buffer bytes " << bufBytes);
            FetchOne("SELECT CAST(0x0102AB AS VARBINARY(MAX))");
            SQLLEN ind = -1;
            for (int probe = 0; probe < 2; ++probe) {
                for (auto& unit : buf) unit = '~';
                ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
                          SQLGetData(stmt_, 1, target, buf, bufBytes, &ind));
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
                EXPECT_EQ(6 * unitBytes, ind);
                EXPECT_EQ(bufBytes >= unitBytes ? 0 : '~', buf[0]);
                EXPECT_EQ('~', buf[1]);
            }

            ASSERT_EQ(SQL_SUCCESS,
                      SQLGetData(stmt_, 1, target, buf, sizeof(buf), &ind));
            EXPECT_EQ(6 * unitBytes, ind);
            const char* expected = "0102AB";
            for (int i = 0; i < 6; ++i) EXPECT_EQ(expected[i], buf[i]);
            EXPECT_EQ(0, buf[6]);
            EXPECT_EQ('~', buf[7]);
            EXPECT_EQ(SQL_NO_DATA,
                      SQLGetData(stmt_, 1, target, buf, sizeof(buf), &ind));
            ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        }
    };
    SQLCHAR narrow[8];
    SQLWCHAR wide[8];
    ASSERT_NO_FATAL_FAILURE(check(narrow, SQL_C_CHAR));
    ASSERT_NO_FATAL_FAILURE(check(wide, SQL_C_WCHAR));
}

// A value far larger than any single wire chunk, so the remaining count comes
// from the declared total rather than from what happens to be buffered.
TEST_F(BinToCharLiveTest, LargeStreamedValueReportsTheFullCharacterCount) {
    FetchOne(
        "SELECT CAST(REPLICATE(CAST(0xAB AS VARBINARY(MAX)), 1100000) AS VARBINARY(MAX))");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(16, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_EQ(2200000, ind) << "characters, which is twice the byte count";
    EXPECT_EQ("ABABABABABABAB", text) << "14 characters: 15 payload bytes rounded to 7 whole bytes";
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, LargeStreamedWideValueReportsTheFullByteCount) {
    FetchOne(
        "SELECT CAST(REPLICATE(CAST(0xAB AS VARBINARY(MAX)), 1100000) AS VARBINARY(MAX))");

    SQLWCHAR buf[17];
    for (auto& unit : buf) unit = '~';
    SQLLEN ind = -1;
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, 16 * sizeof(SQLWCHAR), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(static_cast<SQLLEN>(2200000 * sizeof(SQLWCHAR)), ind);
    for (int i = 0; i < 14; ++i) {
        EXPECT_EQ(i % 2 == 0 ? 'A' : 'B', buf[i]) << "unit " << i;
    }
    EXPECT_EQ(0, buf[14]) << "the odd payload slot cannot split a hex pair";
    EXPECT_EQ('~', buf[16]) << "must not write past BufferLength";
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The UDT types arrive as PLP binary too, so they render the same way.
TEST_F(BinToCharLiveTest, HierarchyidRendersItsWireBytesAsHex) {
    FetchOne("SELECT CAST('/1/' AS HIERARCHYID)");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(2, ind);
    EXPECT_EQ("58", text);
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, GeometryRendersItsWireBytesAsHex) {
    FetchOne("SELECT geometry::Point(1, 2, 0)");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(128, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(44, ind) << "22 wire bytes";
    EXPECT_EQ("00000000010C000000000000F03F0000000000000040", text);
    SQLCloseCursor(stmt_);
}

TEST_F(BinToCharLiveTest, GeographyRendersItsWireBytesAsHex) {
    FetchOne("SELECT geography::Point(47.651, -122.349, 4326)");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(128, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(44, ind) << "22 wire bytes";
    EXPECT_EQ("E6100000010C17D9CEF753D347407593180456965EC0", text);
    SQLCloseCursor(stmt_);
}

// Bound delivery must not depend on how big the value happens to be. A small
// varbinary(max) arrives buffered and goes through deliver_bound; a large one
// streams through deliver_bound_plp. Both have to render hex.
TEST_F(BinToCharLiveTest, BoundVarbinaryMaxRendersHexWhetherBufferedOrStreamed) {
    struct Case {
        const char* repeat;
        SQLLEN expectedIndicator;
        SQLRETURN expectedRc;
        size_t expectedChars;
    };
    // 10 bytes is buffered; 1,100,000 crosses into the streaming path, where the
    // 65-byte buffer takes 64 characters and the terminator.
    const Case cases[] = {{"10", 20, SQL_SUCCESS, 20},
                          {"1100000", 2200000, SQL_SUCCESS_WITH_INFO, 64}};

    for (const auto& c : cases) {
        const std::string sql = std::string("SELECT CAST(REPLICATE(CAST(0xAB AS VARBINARY(MAX)), ") +
                                c.repeat + ") AS VARBINARY(MAX))";
        ASSERT_SQL_OK(ExecDirect(sql), SQL_HANDLE_STMT, stmt_);

        char buf[65];
        std::memset(buf, 0x7E, sizeof(buf));
        SQLLEN ind = -1;
        ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                      stmt_);
        const SQLRETURN fetchRc = SQLFetch(stmt_);
        EXPECT_EQ(c.expectedRc, fetchRc)
            << "repeat " << c.repeat << " state "
            << ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.expectedIndicator, ind) << "repeat " << c.repeat;
        std::string expected;
        while (expected.size() < c.expectedChars) expected += "AB";
        EXPECT_EQ(0, buf[c.expectedChars]) << "repeat " << c.repeat;
        EXPECT_EQ(expected, std::string(buf, c.expectedChars)) << "repeat " << c.repeat;

        SQLFreeStmt(stmt_, SQL_UNBIND);
        SQLCloseCursor(stmt_);
    }
}

TEST_F(BinToCharLiveTest, BoundWideVarbinaryMaxRendersHexWhetherBufferedOrStreamed) {
    struct Case {
        const char* repeat;
        SQLLEN expectedChars;
        SQLRETURN expectedRc;
    };
    // Above 1 MiB, the value cannot take the fully buffered delivery path.
    const Case cases[] = {{"10", 20, SQL_SUCCESS},
                          {"1100000", 2200000, SQL_SUCCESS_WITH_INFO}};

    for (const auto& c : cases) {
        for (const SQLLEN bufUnits : {21, 64}) {
            SCOPED_TRACE(::testing::Message() << "repeat " << c.repeat
                                             << ", buffer units " << bufUnits);
            const std::string sql =
                std::string("SELECT CAST(REPLICATE(CAST(0xAB AS VARBINARY(MAX)), ") +
                c.repeat + ") AS VARBINARY(MAX)), CAST(42 AS INT)";
            ASSERT_SQL_OK(ExecDirect(sql), SQL_HANDLE_STMT, stmt_);

            SQLWCHAR buf[65];
            for (auto& unit : buf) unit = '~';
            SQLLEN ind = -1;
            SQLINTEGER following = -1;
            SQLLEN followingInd = -1;
            ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf,
                                    bufUnits * sizeof(SQLWCHAR), &ind),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &following,
                                    sizeof(following), &followingInd),
                          SQL_HANDLE_STMT, stmt_);

            ASSERT_EQ(c.expectedRc, SQLFetch(stmt_)) << StmtDiagState();
            EXPECT_EQ(c.expectedChars * static_cast<SQLLEN>(sizeof(SQLWCHAR)), ind);
            if (c.expectedRc == SQL_SUCCESS_WITH_INFO) {
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
            }
            const SQLLEN copiedChars =
                c.expectedRc == SQL_SUCCESS ? c.expectedChars : (bufUnits - 1) & ~SQLLEN{1};
            for (SQLLEN i = 0; i < copiedChars; ++i) {
                EXPECT_EQ(i % 2 == 0 ? 'A' : 'B', buf[i]) << "unit " << i;
            }
            EXPECT_EQ(0, buf[copiedChars]);
            EXPECT_EQ('~', buf[bufUnits]) << "must not write past BufferLength";
            EXPECT_EQ(42, following) << "the truncated stream must be drained";
            EXPECT_EQ(static_cast<SQLLEN>(sizeof(following)), followingInd);
            EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        }
    }
}

TEST_F(BinToCharLiveTest, BoundHexNeverSplitsAByteAcrossTheBufferEdge) {
    const auto check = [&](auto& buf, SQLSMALLINT target) {
        for (const char* source : {"BINARY(3)", "VARBINARY(8)", "VARBINARY(MAX)"}) {
            for (const SQLLEN bufUnits : {0, 1, 2, 3, 4, 5, 6, 7}) {
                SCOPED_TRACE(::testing::Message() << source << ", target " << target
                                                 << ", buffer units " << bufUnits);
                ASSERT_SQL_OK(ExecDirect(std::string("SELECT CAST(0x0102AB AS ") + source + ")"),
                              SQL_HANDLE_STMT, stmt_);
                for (auto& unit : buf) unit = '~';
                SQLLEN ind = -1;
                ASSERT_SQL_OK(SQLBindCol(stmt_, 1, target, buf,
                                        bufUnits * sizeof(buf[0]), &ind),
                              SQL_HANDLE_STMT, stmt_);
                ASSERT_EQ(bufUnits == 7 ? SQL_SUCCESS : SQL_SUCCESS_WITH_INFO,
                          SQLFetch(stmt_)) << StmtDiagState();
                EXPECT_EQ(static_cast<SQLLEN>(6 * sizeof(buf[0])), ind);
                if (bufUnits < 7) {
                    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
                }
                const SQLLEN copiedChars = bufUnits == 0 ? 0 : (bufUnits - 1) & ~SQLLEN{1};
                const char* expected = "0102AB";
                for (SQLLEN i = 0; i < copiedChars; ++i) EXPECT_EQ(expected[i], buf[i]);
                if (bufUnits > 0) EXPECT_EQ(0, buf[copiedChars]);
                EXPECT_EQ('~', buf[bufUnits]);
                ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
                ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
            }
        }
    };
    SQLCHAR narrow[8];
    SQLWCHAR wide[8];
    ASSERT_NO_FATAL_FAILURE(check(narrow, SQL_C_CHAR));
    ASSERT_NO_FATAL_FAILURE(check(wide, SQL_C_WCHAR));
}

// A uniqueidentifier is not hex-rendered: it has its own string form, which the
// driver already produced before this change.
TEST_F(BinToCharLiveTest, UniqueidentifierKeepsItsGuidRendering) {
    FetchOne("SELECT CAST('0BA9F6C6-7B1A-4A2C-9C31-1E2E1D5B0C55' AS UNIQUEIDENTIFIER)");

    SQLRETURN rc;
    SQLLEN ind = -1;
    std::string text;
    ReadChar(64, &rc, &ind, &text);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(36, ind);
    EXPECT_EQ("0BA9F6C6-7B1A-4A2C-9C31-1E2E1D5B0C55", text);
    SQLCloseCursor(stmt_);
}

// Copyright (c) Microsoft Corporation. All rights reserved.
// binary_fetch_test.cpp  –  SQL_C_BINARY result delivery (AB#47239).
//
// Every expectation here was measured against msodbcsql 18 before being
// asserted. The indicator rule is the part worth stating: it carries the bytes
// remaining *before* the call, not the bytes written, so a chunked read sees a
// decreasing count and the final chunk still reports what it delivered.
//
// Binary carries no terminator, so the whole buffer is payload -- which is why
// these do not reuse the character-target helpers.

#include "odbc_test_fixture.h"

#include <cstring>
#include <string>
#include <vector>

class BinaryFetchLiveTest : public ODBCTest {
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
    void AssertColumnSqlType(SQLSMALLINT expected) {
        SQLSMALLINT actual = 0;
        ASSERT_SQL_OK(SQLDescribeCol(stmt_, 1, nullptr, 0, nullptr, &actual, nullptr, nullptr,
                                     nullptr),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, actual);
    }
};

// ---------------------------------------------------------------------------
// Fixed-length binary, whole and chunked.
// ---------------------------------------------------------------------------

TEST_F(BinaryFetchLiveTest, FixedBinaryDeliversWholeValue) {
    FetchOne("SELECT CAST(0x010203040506070809 AS BINARY(9))");

    unsigned char buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(9, ind);
    const unsigned char expected[9] = {1, 2, 3, 4, 5, 6, 7, 8, 9};
    EXPECT_EQ(0, std::memcmp(buf, expected, sizeof(expected)));
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, FixedBinaryPreservesEmbeddedZero) {
    FetchOne("SELECT CAST(0x01000203 AS BINARY(4))");

    unsigned char buf[4] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(4, ind);
    const unsigned char expected[4] = {1, 0, 2, 3};
    EXPECT_EQ(0, std::memcmp(buf, expected, sizeof(expected)));
    SQLCloseCursor(stmt_);
}

// The indicator counts down because it reports what was left before each call,
// not what the call delivered.
TEST_F(BinaryFetchLiveTest, FixedBinaryChunksWithARemainingCount) {
    FetchOne("SELECT CAST(0x010203040506070809 AS BINARY(9))");

    unsigned char buf[4] = {};
    SQLLEN ind = 0;

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(9, ind) << "bytes remaining before the first call";
    EXPECT_EQ(0, std::memcmp(buf, "\x01\x02\x03\x04", 4));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(5, ind);
    EXPECT_EQ(0, std::memcmp(buf, "\x05\x06\x07\x08", 4));

    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(1, ind);
    EXPECT_EQ(0x09, buf[0]);

    // Drained: the value is gone, so asking again is SQL_NO_DATA.
    EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, FixedBinaryProbeReportsRemainingBytesWithoutConsumingThem) {
    FetchOne("SELECT CAST(0x010203040506070809 AS BINARY(9))");

    unsigned char buf[4] = {};
    SQLLEN ind = 0;

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(9, ind);
    EXPECT_EQ(0, std::memcmp(buf, "\x01\x02\x03\x04", 4));

    // A real buffer even at length 0: the Driver Manager rejects a null
    // TargetValuePtr with HY009 before the call reaches the driver.
    SQLCHAR probeBuf[1] = {};
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, probeBuf, 0, &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(5, ind);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(5, ind);
    EXPECT_EQ(0, std::memcmp(buf, "\x05\x06\x07\x08", 4));

    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(1, ind);
    EXPECT_EQ(0x09, buf[0]);
    SQLCloseCursor(stmt_);
}

// The PLP half of the same rule. This path computes its remainder from the
// stream rather than from `remaining_binary_length`, so it needs its own case.
TEST_F(BinaryFetchLiveTest, StreamedBinaryProbeReportsRemainingBytesWithoutConsumingThem) {
    FetchOne("SELECT CAST(REPLICATE(CAST(0x41 AS VARBINARY(MAX)), 10) AS VARBINARY(MAX))");
    AssertColumnSqlType(SQL_VARBINARY);

    unsigned char buf[4] = {};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(10, ind);

    SQLCHAR probeBuf[1] = {};
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, probeBuf, 0, &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(6, ind);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(6, ind);
    EXPECT_EQ(0x41, buf[0]);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, EmptyVarbinaryReportsZeroLength) {
    FetchOne("SELECT CAST(0x AS VARBINARY(20))");

    unsigned char buf[8];
    std::memset(buf, 0xEE, sizeof(buf));
    SQLLEN ind = -1;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(0, ind);
    EXPECT_EQ(0xEE, buf[0]) << "an empty value must not disturb the buffer";
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, NullVarbinaryReportsNull) {
    FetchOne("SELECT CAST(NULL AS VARBINARY(8))");

    unsigned char buf[8];
    std::memset(buf, 0xEE, sizeof(buf));
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_NULL_DATA, ind);
    EXPECT_EQ(0xEE, buf[0]) << "a NULL value must not disturb the buffer";
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// PLP: varbinary(max) and the UDT types, which arrive as a wire stream rather
// than a materialized value.
// ---------------------------------------------------------------------------

TEST_F(BinaryFetchLiveTest, VarbinaryMaxDeliversWholeValue) {
    FetchOne("SELECT CAST(REPLICATE(CAST(0x41 AS VARBINARY(MAX)), 10) AS VARBINARY(MAX))");
    AssertColumnSqlType(SQL_VARBINARY);

    unsigned char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(10, ind);
    for (int i = 0; i < 10; ++i) EXPECT_EQ(0x41, buf[i]) << "byte " << i;
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, EmptyVarbinaryMaxReportsZeroLength) {
    FetchOne("SELECT CAST(0x AS VARBINARY(MAX))");
    AssertColumnSqlType(SQL_VARBINARY);

    unsigned char buf[8];
    std::memset(buf, 0xEE, sizeof(buf));
    SQLLEN ind = -1;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(0, ind);
    EXPECT_EQ(0xEE, buf[0]) << "an empty stream must not disturb the buffer";
    SQLCloseCursor(stmt_);
}

// The chunked stream is the case that regressed twice while this was built:
// the first chunk and the resumed chunk are admitted by two separate gates, so
// a read could start and then be refused half way through.
TEST_F(BinaryFetchLiveTest, VarbinaryMaxChunksAcrossCalls) {
    FetchOne("SELECT CAST(REPLICATE(CAST(0x41 AS VARBINARY(MAX)), 10) AS VARBINARY(MAX))");
    AssertColumnSqlType(SQL_VARBINARY);

    unsigned char buf[4] = {};
    SQLLEN ind = 0;

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(10, ind);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(6, ind);

    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_EQ(2, ind);
    EXPECT_EQ(0x41, buf[0]);

    // Drained: the stream is exhausted, so asking again is SQL_NO_DATA.
    EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, HierarchyidDeliversItsWireBytes) {
    FetchOne("SELECT CAST(hierarchyid::Parse('/1/2/') AS hierarchyid)");

    unsigned char buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(2, ind);
    EXPECT_EQ(0x5B, buf[0]);
    EXPECT_EQ(0x40, buf[1]);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, GeometryDeliversItsWireBytes) {
    FetchOne("SELECT geometry::Point(1, 2, 0)");

    unsigned char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(22, ind) << "SRID 0 point, well-known binary";
    SQLCloseCursor(stmt_);
}

// A character column read as binary yields its wire bytes, not its text.
TEST_F(BinaryFetchLiveTest, NvarcharAsBinaryYieldsUtf16WireBytes) {
    FetchOne("SELECT CAST('hi' AS NVARCHAR(10))");

    unsigned char buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(4, ind);
    const unsigned char expected[4] = {0x68, 0x00, 0x69, 0x00};
    EXPECT_EQ(0, std::memcmp(buf, expected, sizeof(expected)));
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Bound delivery. mssql-python's block fetch binds binary columns, so this is
// the path its arrow reader drives.
// ---------------------------------------------------------------------------

TEST_F(BinaryFetchLiveTest, BoundBinaryDeliversWholeValue) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0x010203040506070809 AS BINARY(9))"), SQL_HANDLE_STMT,
                  stmt_);

    unsigned char buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(9, ind);
    const unsigned char expected[9] = {1, 2, 3, 4, 5, 6, 7, 8, 9};
    EXPECT_EQ(0, std::memcmp(buf, expected, sizeof(expected)));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, BoundBinaryTruncatesWithTheFullLength) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0x010203040506070809 AS BINARY(9))"), SQL_HANDLE_STMT,
                  stmt_);

    unsigned char buf[4] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_EQ(9, ind) << "the untruncated length, as the character targets report";
    EXPECT_EQ(0, std::memcmp(buf, "\x01\x02\x03\x04", 4));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, BoundVarbinaryMaxDeliversWholeValue) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(REPLICATE(CAST(0x41 AS VARBINARY(MAX)), 10) AS VARBINARY(MAX))"),
        SQL_HANDLE_STMT, stmt_);
    AssertColumnSqlType(SQL_VARBINARY);

    unsigned char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(10, ind);
    for (int i = 0; i < 10; ++i) EXPECT_EQ(0x41, buf[i]) << "byte " << i;
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, BoundBinaryNullsReportNullWithoutDisturbingBuffers) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(NULL AS VARBINARY(8)), CAST(NULL AS VARBINARY(MAX))"),
        SQL_HANDLE_STMT, stmt_);

    unsigned char fixedBuf[8];
    unsigned char plpBuf[8];
    std::memset(fixedBuf, 0xEE, sizeof(fixedBuf));
    std::memset(plpBuf, 0xDD, sizeof(plpBuf));
    SQLLEN fixedInd = 0;
    SQLLEN plpInd = 0;
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 1, SQL_C_BINARY, fixedBuf, sizeof(fixedBuf), &fixedInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_BINARY, plpBuf, sizeof(plpBuf), &plpInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, fixedInd);
    EXPECT_EQ(SQL_NULL_DATA, plpInd);
    EXPECT_EQ(0xEE, fixedBuf[0]);
    EXPECT_EQ(0xDD, plpBuf[0]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, AdjacentBoundBinaryColumnsStayWithinTheirBuffers) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(0x01020304 AS BINARY(4)), CAST(0x05060708 AS BINARY(4))"),
        SQL_HANDLE_STMT, stmt_);

    struct {
        unsigned char first[4];
        unsigned char second[4];
    } buffers = {};
    SQLLEN firstInd = 0;
    SQLLEN secondInd = 0;
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 1, SQL_C_BINARY, buffers.first, sizeof(buffers.first), &firstInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 2, SQL_C_BINARY, buffers.second, sizeof(buffers.second), &secondInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(4, firstInd);
    EXPECT_EQ(4, secondInd);
    EXPECT_EQ(0, std::memcmp(buffers.first, "\x01\x02\x03\x04", 4));
    EXPECT_EQ(0, std::memcmp(buffers.second, "\x05\x06\x07\x08", 4));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(BinaryFetchLiveTest, BoundBinaryRowArrayUsesTheBinaryStride) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT b FROM (VALUES (1, CAST(0x01020304 AS BINARY(4))),"
                   " (2, CAST(0x05060708 AS BINARY(4)))) AS t(n, b) ORDER BY n"),
        SQL_HANDLE_STMT, stmt_);

    unsigned char buffers[2][4] = {};
    SQLLEN indicators[2] = {};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_BINARY, buffers, sizeof(buffers[0]), indicators),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);
    EXPECT_EQ(4, indicators[0]);
    EXPECT_EQ(4, indicators[1]);
    EXPECT_EQ(0, std::memcmp(buffers[0], "\x01\x02\x03\x04", 4));
    EXPECT_EQ(0, std::memcmp(buffers[1], "\x05\x06\x07\x08", 4));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Still deliberately refused. msodbcsql converts the fixed-width kinds to
// binary, and does so inconsistently -- int and money give wire bytes, date
// gives the 6-byte SQL_DATE_STRUCT, decimal is refused with 22003. Rather than
// guess that table, those keep answering HYC00 until each is measured. This
// pins the boundary so the gap is visible rather than assumed closed.
// ---------------------------------------------------------------------------

TEST_F(BinaryFetchLiveTest, FixedWidthKindsAreStillRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();
    FetchOne("SELECT CAST(258 AS INT)");

    unsigned char buf[16] = {};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Binary is not text, even when the column is. A bound SQL_C_BINARY read of a
// UTF-8 encoded PLP column shares the accumulator with the character targets,
// and those trim a truncated tail back to a whole UTF-8 character. Doing that
// to a binary caller silently drops bytes it asked for verbatim.
// ---------------------------------------------------------------------------

TEST_F(BinaryFetchLiveTest, ABoundBinaryReadOfAUtf8ColumnKeepsTheTruncatedTail) {
    // Every character is 2 bytes, so an odd-sized buffer must split one. The
    // whole slot is payload: character trimming would give back 8, not 9.
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(N'[\"' + REPLICATE(NCHAR(233), 40) + N'\"]' AS JSON) AS c1"),
        SQL_HANDLE_STMT, stmt_);

    unsigned char buf[9] = {};
    SQLLEN ind = -99;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));

    // A streamed value whose total the server never declares: both drivers
    // report SQL_NO_TOTAL rather than a count.
    EXPECT_EQ(SQL_NO_TOTAL, ind);
    EXPECT_EQ('[', buf[0]);
    EXPECT_EQ('"', buf[1]);
    // buf[2..] is the UTF-8 for U+00E9: 0xC3 0xA9 repeating. The last byte of
    // the slot lands mid-character and must survive.
    for (size_t i = 2; i < sizeof(buf); ++i) {
        EXPECT_EQ((i % 2 == 0) ? 0xC3 : 0xA9, buf[i]) << "byte " << i << " must not be trimmed";
    }

    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

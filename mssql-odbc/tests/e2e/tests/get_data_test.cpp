// Copyright (c) Microsoft Corporation. All rights reserved.
// get_data_test.cpp  –  E2E tests for column-wise SQLGetData (msodbcsql style).
//
// SQLFetch positions on a row without materializing any column; each SQLGetData
// decodes exactly the requested column, draining the columns in between. PLP
// (VARCHAR(MAX)/NVARCHAR(MAX)/VARBINARY(MAX)) columns are streamed across
// repeated SQLGetData calls.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <algorithm>
#include <cstring>
#include <string>
#include <vector>

namespace {

// Builds `token` repeated `count` times.
std::string RepeatToken(const std::string& token, size_t count) {
    std::string out;
    out.reserve(token.size() * count);
    for (size_t i = 0; i < count; ++i) {
        out += token;
    }
    return out;
}

// Streams one SQL_C_CHAR column across as many SQLGetData calls as it takes,
// using a small buffer. Returns the fully assembled value. Sets `*final_ind`
// to the indicator reported on the final (SQL_SUCCESS) call when provided.
std::string ReadCharDataInChunks(SQLHSTMT stmt, SQLUSMALLINT col, size_t buf_size,
                                 SQLLEN* final_ind = nullptr) {
    std::string value;
    std::vector<SQLCHAR> buf(buf_size, 0);
    while (true) {
        std::fill(buf.begin(), buf.end(), 0);
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt, col, SQL_C_CHAR, buf.data(),
                                  static_cast<SQLLEN>(buf.size()), &ind);
        EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "SQLGetData failed rc=" << rc;
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        value.append(reinterpret_cast<const char*>(buf.data()));
        if (rc == SQL_SUCCESS) {
            if (final_ind != nullptr) {
                *final_ind = ind;
            }
            break;
        }
    }
    return value;
}

}  // namespace

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

TEST(GetDataTest, NullHandle) {
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(SQL_NULL_HSTMT, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class GetDataLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN ExecDirect(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Read one column as a narrow string via a single SQLGetData call.
    std::string GetChar(SQLUSMALLINT col, SQLRETURN* rc_out = nullptr,
                        SQLLEN* ind_out = nullptr) {
        SQLCHAR buf[512] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, col, SQL_C_CHAR, buf, sizeof(buf), &ind);
        if (rc_out) {
            *rc_out = rc;
        }
        if (ind_out) {
            *ind_out = ind;
        }
        if (ind == SQL_NULL_DATA) {
            return std::string();
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }
};

// SQLGetData without a positioned row (no SQLFetch yet) fails with 24000.
TEST_F(GetDataLiveTest, NoCurrentRow) {
    ASSERT_SQL_OK(ExecDirect("SELECT 1 AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    SQLCloseCursor(stmt_);
}

// Column-wise retrieval: request columns in ascending order; intervening
// columns are drained transparently.
TEST_F(GetDataLiveTest, ColumnWiseAscending) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST(1 AS INT) AS c1, "
                      "CAST('two' AS VARCHAR(10)) AS c2, "
                      "CAST(3 AS INT) AS c3, "
                      "CAST('four' AS VARCHAR(10)) AS c4, "
                      "CAST(5 AS INT) AS c5"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc;
    EXPECT_EQ("two", GetChar(2, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("four", GetChar(4, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("5", GetChar(5, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// Re-requesting a column strictly earlier than the last one retrieved is
// backward retrieval, which this driver rejects (SQLSTATE 07009). Re-requesting
// the column just retrieved reports end-of-data (SQL_NO_DATA).
TEST_F(GetDataLiveTest, BackwardColumnRejectedRereadIsNoData) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST(10 AS INT) AS c1, "
                      "CAST(20 AS INT) AS c2, "
                      "CAST(30 AS INT) AS c3"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc;
    EXPECT_EQ("20", GetChar(2, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Column 1 was drained while reaching column 2; requesting it now is a
    // backward access and returns SQL_ERROR with SQLSTATE 07009.
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");

    // Re-requesting the just-retrieved column 2 returns SQL_NO_DATA.
    rc = SQLGetData(stmt_, 2, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_NO_DATA, rc);

    SQLCloseCursor(stmt_);
}

// PLP streaming: a large VARCHAR(MAX) column is delivered across repeated
// SQLGetData calls. Each partial call returns SQL_SUCCESS_WITH_INFO (01004);
// the final call returns SQL_SUCCESS.
TEST_F(GetDataLiveTest, PlpVarcharMaxStreamed) {
    const int kTotal = 9000;
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT REPLICATE(CAST('A' AS VARCHAR(MAX)), 9000) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::string assembled;
    SQLCHAR buf[1024];
    SQLLEN ind = 0;
    SQLRETURN rc;
    int guard = 0;
    do {
        rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;
        assembled += std::string(reinterpret_cast<const char*>(buf));
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    } while (rc == SQL_SUCCESS_WITH_INFO);

    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(static_cast<size_t>(kTotal), assembled.size());
    EXPECT_EQ(std::string(kTotal, 'A'), assembled);

    // Stream exhausted: a further call for the same column yields SQL_NO_DATA.
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_NO_DATA, rc);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// A NULL value reports SQL_NULL_DATA in the indicator with SQL_SUCCESS.
TEST_F(GetDataLiveTest, NullColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS VARCHAR(10)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, ind);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// A scalar column followed by a PLP column: the scalar is delivered in one shot,
// then the PLP value streams to completion. Exercises the scalar→PLP transition
// within a single row.
TEST_F(GetDataLiveTest, MixedPlpAndNonPlpColumns) {
    const std::string expected_plp = RepeatToken("x", 128);
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST(42 AS INT) AS c1, "
                      "CAST(REPLICATE(CAST('x' AS VARCHAR(MAX)), 128) AS VARCHAR(MAX)) AS c2"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR num_buf[16] = {0};
    SQLLEN num_ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, num_buf, sizeof(num_buf), &num_ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, num_ind);
    EXPECT_STREQ("42", reinterpret_cast<const char*>(num_buf));

    EXPECT_EQ(expected_plp, ReadCharDataInChunks(stmt_, 2, 16));

    SQLCloseCursor(stmt_);
}

// Requesting a later column skips an intervening PLP column: the driver drains
// the unread VARCHAR(MAX) in the middle while advancing to column 3.
TEST_F(GetDataLiveTest, SkipsPlpMiddleColumn) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST('first' AS VARCHAR(10)) AS c1, "
                      "CAST(REPLICATE(CAST('y' AS VARCHAR(MAX)), 64) AS VARCHAR(MAX)) AS c2, "
                      "CAST('third' AS VARCHAR(10)) AS c3"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc;
    SQLLEN ind = 0;
    EXPECT_EQ("first", GetChar(1, &rc, &ind));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, ind);

    // Column 2 (VARCHAR(MAX)) is never read; requesting column 3 drains it.
    EXPECT_EQ("third", GetChar(3, &rc, &ind));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, ind);

    SQLCloseCursor(stmt_);
}

// Two PLP columns in the same row, both streamed: read column 2 (VARCHAR(MAX))
// to completion, skip the PLP column 3, then stream column 4 (VARCHAR(MAX)).
TEST_F(GetDataLiveTest, TwoPlpColumnsStreamedWithSkippedPlpBetween) {
    const std::string expected_c2 = RepeatToken("ab", 500);   // 1000 bytes
    const std::string expected_c4 = RepeatToken("wxyz", 300);  // 1200 bytes
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST(1 AS INT) AS c1, "
                      "REPLICATE(CAST('ab' AS VARCHAR(MAX)), 500) AS c2, "
                      "REPLICATE(CAST('q' AS VARCHAR(MAX)), 128) AS c3, "
                      "REPLICATE(CAST('wxyz' AS VARCHAR(MAX)), 300) AS c4"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected_c2, ReadCharDataInChunks(stmt_, 2, 16));

    // Column 3 (also PLP) is never read; requesting column 4 must drain it.
    EXPECT_EQ(expected_c4, ReadCharDataInChunks(stmt_, 4, 16));

    SQLCloseCursor(stmt_);
}

// Multiple rows: loop SQLFetch and read scalar columns on each row.
TEST_F(GetDataLiveTest, MultiRowScalarColumns) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT n, CAST(n * 10 AS INT) AS m FROM (VALUES (1), (2), (3)) AS v(n) "
                      "ORDER BY n"),
                  SQL_HANDLE_STMT, stmt_);

    for (int row = 1; row <= 3; ++row) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLRETURN rc;
        EXPECT_EQ(std::to_string(row), GetChar(1, &rc)) << "row " << row << " col 1";
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

        EXPECT_EQ(std::to_string(row * 10), GetChar(2, &rc)) << "row " << row << " col 2";
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    }

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// Multiple rows each carrying a PLP column, streamed to completion on every row.
TEST_F(GetDataLiveTest, MultiRowPlpStreamedPerRow) {
    const std::string expected_r1 = RepeatToken("row1", 250);  // 1000 bytes
    const std::string expected_r2 = RepeatToken("row2", 300);  // 1200 bytes
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT 1 AS n, REPLICATE(CAST('row1' AS VARCHAR(MAX)), 250) AS c "
                      "UNION ALL "
                      "SELECT 2 AS n, REPLICATE(CAST('row2' AS VARCHAR(MAX)), 300) AS c "
                      "ORDER BY n"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLRETURN rc;
    EXPECT_EQ("1", GetChar(1, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(expected_r1, ReadCharDataInChunks(stmt_, 2, 16));

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetChar(1, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(expected_r2, ReadCharDataInChunks(stmt_, 2, 16));

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// Advancing to the next row while the current row still has an in-progress PLP
// stream: SQLFetch must drain the unfinished PLP value and position on row 2.
TEST_F(GetDataLiveTest, FetchDrainsPartiallyReadPlpFromPriorRow) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT 1 AS n, REPLICATE(CAST('aaaa' AS VARCHAR(MAX)), 500) AS c "
                      "UNION ALL "
                      "SELECT 2 AS n, CAST('second' AS VARCHAR(MAX)) AS c "
                      "ORDER BY n"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLCHAR buf[8] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 2, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) << "rc=" << rc;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc) << "partial read of a 2000-byte value";

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetChar(1, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("second", GetChar(2, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// Requesting a column past the end of the row returns SQLSTATE 07009.
TEST_F(GetDataLiveTest, ColumnBeyondEndReturns07009) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(123 AS INT) AS c1"), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 2, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");

    SQLCloseCursor(stmt_);
}

// An empty VARCHAR(MAX) reports a 0-length indicator with SQL_SUCCESS.
TEST_F(GetDataLiveTest, EmptyVarcharMaxChar) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('' AS VARCHAR(MAX)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, ind);

    SQLCloseCursor(stmt_);
}

// An empty NVARCHAR(MAX) read as SQL_C_WCHAR reports a 0-length indicator.
TEST_F(GetDataLiveTest, EmptyNvarcharMaxWchar) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(N'' AS NVARCHAR(MAX)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, ind);

    SQLCloseCursor(stmt_);
}

// A tiny caller buffer forces many continuation calls; the reassembled value
// must equal the full payload and at least one call reports truncation.
TEST_F(GetDataLiveTest, PlpTinyBufferManyCalls) {
    const std::string expected = RepeatToken("abc", 200);  // 600 bytes
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::string observed;
    bool saw_success_with_info = false;
    int guard = 0;
    while (true) {
        SQLCHAR buf[4] = {0};  // 3 usable bytes per call after NUL
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;
        observed.append(reinterpret_cast<const char*>(buf));
        if (rc == SQL_SUCCESS_WITH_INFO) {
            saw_success_with_info = true;
        } else {
            break;
        }
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    }

    EXPECT_TRUE(saw_success_with_info);
    EXPECT_EQ(expected, observed);

    SQLCloseCursor(stmt_);
}

// For a value that fits in a single wire pump (<= 8 KiB), the length indicator
// reports the exact number of bytes still available before each copy.
TEST_F(GetDataLiveTest, PlpSmallValueIndicatorDecrements) {
    const SQLLEN kTotal = 400;
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT REPLICATE(CAST('abcdefgh' AS VARCHAR(MAX)), 50) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLLEN expected_remaining = kTotal;
    SQLLEN total_fetched = 0;
    while (true) {
        SQLCHAR buf[16] = {0};  // 15 usable bytes per call
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;

        SQLLEN copied = static_cast<SQLLEN>(std::strlen(reinterpret_cast<const char*>(buf)));
        total_fetched += copied;

        if (rc == SQL_SUCCESS_WITH_INFO) {
            EXPECT_EQ(expected_remaining, ind)
                << "indicator should report exact remaining bytes";
            expected_remaining -= copied;
            continue;
        }
        break;
    }

    EXPECT_EQ(kTotal, total_fetched);

    SQLCloseCursor(stmt_);
}

// For a value larger than one wire pump (> 8 KiB), the indicator is SQL_NO_TOTAL.
TEST_F(GetDataLiveTest, PlpLargeValueIndicatorNoTotal) {
    const int kTotal = 20000;
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT REPLICATE(CAST('A' AS VARCHAR(MAX)), 20000) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    bool saw_no_total = false;
    std::string assembled;
    int guard = 0;
    while (true) {
        SQLCHAR buf[4096] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;
        assembled.append(reinterpret_cast<const char*>(buf));
        if (ind == SQL_NO_TOTAL) {
            saw_no_total = true;
        }
        if (rc == SQL_SUCCESS) {
            break;
        }
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    }

    EXPECT_TRUE(saw_no_total)
        << "a >8 KiB streamed value should report SQL_NO_TOTAL before the wire drains";
    EXPECT_EQ(static_cast<size_t>(kTotal), assembled.size());

    SQLCloseCursor(stmt_);
}

// NVARCHAR(MAX) delivered as SQL_C_WCHAR round-trips the UTF-16 content.
TEST_F(GetDataLiveTest, NvarcharMaxWideRoundTrip) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(N'wide chars' AS NVARCHAR(MAX)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR buf[64] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    // 10 UTF-16 code units × 2 bytes.
    EXPECT_EQ(20, ind);

    const SQLWCHAR expected[] = {'w', 'i', 'd', 'e', ' ', 'c', 'h', 'a', 'r', 's', 0};
    EXPECT_EQ(0, std::memcmp(buf, expected, sizeof(expected)));

    SQLCloseCursor(stmt_);
}

// An unsupported C target type is rejected with HYC00 and does not consume the
// column, so a follow-up call with a supported type still returns the value.
TEST_F(GetDataLiveTest, UnsupportedCTypeReturnsHyc00ThenValueReadable) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('hello' AS VARCHAR(20)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT sbuf = 0;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_SSHORT, &sbuf, 0, &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    SQLRETURN rc2;
    EXPECT_EQ("hello", GetChar(1, &rc2, &ind));
    EXPECT_SQL_OK(rc2, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, ind);

    SQLCloseCursor(stmt_);
}

// VARBINARY(MAX) to a character target is not yet implemented; it must report
// HYC00 rather than corrupt the stream.
TEST_F(GetDataLiveTest, VarbinaryMaxToCharReturnsHyc00) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0x41424344 AS VARBINARY(MAX)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[64] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    SQLCloseCursor(stmt_);
}


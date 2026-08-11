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
// backward retrieval, which this forward-only driver rejects (SQLSTATE 07009).
// Re-requesting the column just retrieved is not backward movement, but its data
// has already been consumed, so the driver reports end-of-data (SQL_NO_DATA)
// rather than replaying the value. This is the spec-compliant result: the ODBC
// SQLGetData contract permits a re-request of the same column (the ordering rule
// requires Col_or_Param_Num to be non-decreasing) and mandates SQL_NO_DATA once
// the column has no more data to return.
//
// The reference msodbcsql driver returns SQL_ERROR for the re-request instead of
// SQL_NO_DATA. That deviation is incidental, not a deliberate contract, and it
// only appears in this specific three-step sequence. In isolation the two
// drivers agree: a bare "drain col 1, then re-request col 1" returns SQL_NO_DATA
// on msodbcsql too. The difference here is the intervening rejected backward
// GetData(col 1): on msodbcsql that failed call resets the "just finished a
// column" state, so the col 2 re-request is no longer treated as an already-read
// column (which would return SQL_NO_DATA) and is instead reported as a
// backward-access error. Because that value is msodbcsql-specific (and
// non-conformant), skip this assertion on the msodbcsql comparison leg.
//
// Scope: this covers only the *fully consumed* re-read (returns SQL_NO_DATA). A
// *partially* consumed column must instead resume from where it stopped -- that
// truncation-recovery path is covered by NonPlpChunkedReadAccumulatesFullValue,
// not here.
TEST_F(GetDataLiveTest, BackwardColumnRejectedRereadIsNoData) {
    SKIP_IF_COMPARING_MSODBCSQL();
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

// A known-length PLP value (server sends the total up front) reports the
// concrete bytes-still-available indicator on every SQLGetData call, counting
// down as the value drains — never SQL_NO_TOTAL. On each call StrLen_or_Ind is
// the bytes available *before* that call's copy, so it equals
// `kTotal - bytes_consumed_by_prior_calls` for both the truncated
// (SQL_SUCCESS_WITH_INFO) chunks and the final (SQL_SUCCESS) chunk. This matches
// the reference msodbcsql driver, so the assertion runs on both legs.
TEST_F(GetDataLiveTest, PlpKnownLengthIndicatorCountsDown) {
    const int kTotal = 20000;
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT REPLICATE(CAST('A' AS VARCHAR(MAX)), 20000) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    bool saw_success_with_info = false;
    std::string assembled;
    int guard = 0;
    while (true) {
        const size_t consumed_before = assembled.size();
        SQLCHAR buf[4096] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;
        EXPECT_NE(ind, SQL_NO_TOTAL)
            << "a known-length value must report a concrete remaining count";
        EXPECT_EQ(static_cast<SQLLEN>(kTotal - consumed_before), ind)
            << "indicator must be bytes-available-before-this-call";
        assembled.append(reinterpret_cast<const char*>(buf));
        if (rc == SQL_SUCCESS_WITH_INFO) {
            saw_success_with_info = true;
        } else {
            break;
        }
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    }

    EXPECT_TRUE(saw_success_with_info)
        << "a 20 KB value must truncate at least once before draining";
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

// Character text that is not a valid literal for a numeric C target is rejected
// with 22018 and does not consume the column, so a follow-up call with a
// supported type still returns the value. Before the P1a source-type
// conversions this pairing was simply unimplemented and reported HYC00.
//
// Still skipped on the msodbcsql leg: msodbcsql implements this conversion and
// its CVT_CAST_ERROR carries the "Invalid character value for cast
// specification" message, so it very likely agrees, but the constant is spelled
// IDS_22_005 in its source and that has not been confirmed against a live run.
TEST_F(GetDataLiveTest, InvalidCharacterForNumericTargetIs22018ThenValueReadable) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('hello' AS VARCHAR(20)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT sbuf = 0;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_SSHORT, &sbuf, 0, &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");

    SQLRETURN rc2;
    EXPECT_EQ("hello", GetChar(1, &rc2, &ind));
    EXPECT_SQL_OK(rc2, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, ind);

    SQLCloseCursor(stmt_);
}

// An unsupported C target type is rejected with HYC00 and does not consume the
// column. SQL_C_NUMERIC is the durable anchor for this: emitting the
// SQL_NUMERIC_STRUCT is an explicit non-goal (decimal is delivered as character
// data, which is what mssql-python requests), so unlike the other C targets it
// is not scheduled to become supported.
TEST_F(GetDataLiveTest, UnsupportedCTypeReturnsHyc00ThenValueReadable) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('hello' AS VARCHAR(20)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_NUMERIC_STRUCT nbuf{};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_NUMERIC, &nbuf, sizeof(nbuf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    SQLRETURN rc2;
    EXPECT_EQ("hello", GetChar(1, &rc2, &ind));
    EXPECT_SQL_OK(rc2, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, ind);

    SQLCloseCursor(stmt_);
}

// VARBINARY(MAX) to a character target is not yet implemented; it must report
// HYC00 rather than corrupt the stream. The reference msodbcsql driver supports
// binary-to-char (hex) conversion, so this is mssql-odbc-specific — skip it on
// the msodbcsql comparison leg.
TEST_F(GetDataLiveTest, VarbinaryMaxToCharReturnsHyc00) {
    SKIP_IF_COMPARING_MSODBCSQL();
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

// Jumping to a later column while a PLP stream is still open is incorrect usage
// per the ODBC spec. The driver must clear the stale stream, drain the partially
// read column, and return the later column's value rather than corrupt the row.
TEST_F(GetDataLiveTest, PartialPlpReadThenJumpToLaterColumnClearsStaleStream) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1, "
                   "CAST(42 AS INT) AS c2"),
        SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // One tiny read of c1 opens the PLP stream but leaves it mid-value.
    SQLCHAR buf[4] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);

    // Jumping to c2 must discard the stale c1 stream, drain the remaining c1
    // bytes off the wire, and yield c2's value.
    SQLRETURN rc2;
    SQLLEN c2_ind = 0;
    EXPECT_EQ("42", GetChar(2, &rc2, &c2_ind));
    EXPECT_SQL_OK(rc2, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, c2_ind);

    SQLCloseCursor(stmt_);
}

// A PLP (streamed max-type) column requested with a non-character C type is
// rejected with HYC00 before any stream state is created. The reference
// msodbcsql driver implements numeric conversions from character data, so the
// HYC00 assertion is mssql-odbc-specific — skip it on the msodbcsql leg.
TEST_F(GetDataLiveTest, PlpColumnUnsupportedCTypeReturnsHyc00) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('123' AS VARCHAR(MAX)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT sbuf = 0;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_SSHORT, &sbuf, 0, &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    SQLCloseCursor(stmt_);
}

// ===================================================================
// Non-PLP resumable SQLGetData (regression coverage for column-wise
// truncated reads). A fixed-size varchar(n)/nvarchar(n) column larger than
// the caller buffer must be deliverable across repeated calls, and a length
// probe must not consume the column — exactly as a PLP column behaves.
// ===================================================================

// A length probe must report the total length with 01004 and leave the column
// readable, so the app can re-call with a right-sized buffer. BufferLength 1
// (room for the terminator only) is the portable probe form: the Windows Driver
// Manager rejects a NULL pointer and a 0 length for character C types.
TEST_F(GetDataLiveTest, NonPlpProbeThenFetchReturnsValue) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(REPLICATE('0123456789', 10) AS VARCHAR(100)) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // Probe: 1-byte buffer holds only the terminator, so this truncates and
    // reports the full length without consuming the column.
    SQLCHAR probe[1] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, probe, sizeof(probe), &ind);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_EQ(100, ind) << "probe must report the full length";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    // A second call with a real buffer must still return the whole value.
    SQLCHAR buf[256] = {0};
    SQLLEN ind2 = 0;
    SQLRETURN rc2 = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind2);
    ASSERT_SQL_OK(rc2, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(RepeatToken("0123456789", 10),
              std::string(reinterpret_cast<const char*>(buf)));

    SQLCloseCursor(stmt_);
}

// A non-PLP character column larger than the caller's buffer must be delivered
// across repeated calls, exactly as a PLP column is.
TEST_F(GetDataLiveTest, NonPlpChunkedReadAccumulatesFullValue) {
    const std::string expected = RepeatToken("0123456789", 100);  // 1000 bytes
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(REPLICATE('0123456789', 100) AS VARCHAR(1000)) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // 100-byte buffer over a 1000-byte value: 11 calls, ten SUCCESS_WITH_INFO
    // then one SUCCESS. Before the fix this delivered one chunk then SQL_NO_DATA.
    EXPECT_EQ(expected, ReadCharDataInChunks(stmt_, 1, 100));

    SQLCloseCursor(stmt_);
}

// ===================================================================
// Chunked PLP transcoding into SQL_C_CHAR / SQL_C_WCHAR (regression coverage
// for the UTF-16->UTF-8 framing defect). The SQL_C_WCHAR path is the control;
// the SQL_C_CHAR path is where the byte-shift/overflow bug lived.
// ===================================================================

// Control: nvarchar(max) delivered as SQL_C_WCHAR in small chunks round-trips.
TEST_F(GetDataLiveTest, NvarcharMaxToWcharChunkedRoundTrip) {
    const std::string ascii = RepeatToken("0123456789", 300);  // 3000 chars
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(N'0123456789' AS NVARCHAR(MAX)), 300) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::u16string observed;
    int guard = 0;
    while (true) {
        SQLWCHAR wbuf[17] = {0};  // 16 code units + terminator
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_WCHAR, wbuf, sizeof(wbuf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;
        for (size_t i = 0; wbuf[i] != 0; ++i) {
            observed.push_back(static_cast<char16_t>(wbuf[i]));
        }
        if (rc == SQL_SUCCESS) {
            break;
        }
        ASSERT_LT(++guard, 10000);
    }
    ASSERT_EQ(ascii.size(), observed.size());
    for (size_t i = 0; i < ascii.size(); ++i) {
        ASSERT_EQ(static_cast<char16_t>(ascii[i]), observed[i])
            << "first mismatch at code unit " << i;
    }

    SQLCloseCursor(stmt_);
}

// nvarchar(max) delivered as SQL_C_CHAR (UTF-8) in small chunks must reassemble
// byte-for-byte. ASCII content keeps UTF-16 -> UTF-8 1:1 so any framing error
// surfaces as a shifted or dropped byte. Buffer size 1024 is the one the
// reviewer used to pin the original defect (first mismatch at byte 511).
TEST_F(GetDataLiveTest, NvarcharMaxToCharChunkedAsciiRoundTrip) {
    const std::string expected = RepeatToken("0123456789", 300);  // 3000 bytes
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(N'0123456789' AS NVARCHAR(MAX)), 300) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadCharDataInChunks(stmt_, 1, 1024));

    SQLCloseCursor(stmt_);
}

// Astral (non-BMP) content forces surrogate pairs, and a small buffer makes a
// pair straddle a chunk boundary. U+1F600 is NCHAR(0xD83D) + NCHAR(0xDE00) on
// the wire (two UTF-16 code units) and F0 9F 98 80 in UTF-8 (four bytes). With a
// 16-byte SQL_C_CHAR buffer the transcode reads an odd number of code units per
// chunk, so a high surrogate is left without its low half at the boundary; the
// driver must carry it to the next chunk rather than emit U+FFFD.
//
// This asserts mssql-odbc-specific behavior and is skipped on the msodbcsql
// comparison leg: mssql-odbc delivers SQL_C_CHAR as UTF-8 (the emoji round-trips
// as F0 9F 98 80), whereas msodbcsql on Windows converts SQL_C_CHAR to the
// client's ANSI codepage, where U+1F600 has no representation and best-fits to
// '?'. On Linux msodbcsql also delivers UTF-8, so the two agree there; the
// divergence is Windows-only. This is the same intentional UTF-8-vs-ANSI
// SQL_C_CHAR difference already documented for other tests in this file.
TEST_F(GetDataLiveTest, NvarcharMaxToCharChunkedAstralRoundTrip) {
    SKIP_IF_COMPARING_MSODBCSQL();
    const std::string emoji = "\xF0\x9F\x98\x80";       // U+1F600, 4 UTF-8 bytes
    const std::string expected = RepeatToken(emoji, 500);  // 2000 bytes
    ASSERT_SQL_OK(
        ExecDirect(
            "SELECT REPLICATE(NCHAR(0xD83D) + NCHAR(0xDE00), 500) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadCharDataInChunks(stmt_, 1, 16));

    SQLCloseCursor(stmt_);
}

// A PLP read into a buffer too small to hold even one whole character plus the
// null terminator must fail deterministically with HY090 rather than spin
// forever making zero-length reads. buffer_length 2 leaves one payload byte
// once the terminator is reserved, which cannot hold a complete multibyte unit.
//
// This is a truncate-vs-reject policy difference, not purely an encoding one:
// on a UTF-8 client locale both drivers deliver SQL_C_CHAR as UTF-8, so both
// face the same variable-width (1-4 byte) character problem. On a sub-minimal
// buffer the reference msodbcsql driver returns SQL_SUCCESS_WITH_INFO/01004
// (truncation) and expects the app to keep calling, whereas mssql-odbc rejects
// with HY090 to guarantee forward progress and never split a multibyte unit.
// TODO(convergence): mssql-odbc will eventually adopt the msodbcsql
// truncate-and-continue contract for sub-minimal buffers (deliver whole bytes
// that fit and carry the unflushed UTF-8 tail across calls), at which point this
// HY090 rejection goes away and the assertion can run on both legs. Until then
// it is mssql-odbc-specific — skip it on the msodbcsql comparison leg.
TEST_F(GetDataLiveTest, PlpZeroCapacityBufferDoesNotSpin) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(N'abcd' AS NVARCHAR(MAX)), 50) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR tiny[2] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, tiny, sizeof(tiny), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY090");

    SQLCloseCursor(stmt_);

    // buffer_length 1 (room for the NUL only) is the portable length-probe shape
    // applications actually send, and it exercises the non-transcode PLP branch
    // (varchar(max) -> SQL_C_CHAR) where max_read collapses to 0 directly rather
    // than through the UTF-16 sizing above. It must also be rejected with HY090,
    // never spun on.
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR one[1] = {0};
    SQLLEN ind1 = 0;
    SQLRETURN rc1 = SQLGetData(stmt_, 1, SQL_C_CHAR, one, sizeof(one), &ind1);
    EXPECT_EQ(SQL_ERROR, rc1);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY090");

    SQLCloseCursor(stmt_);
}

// An integer column delivered to its natural fixed-width C target, rather than
// being rendered as text.
TEST_F(GetDataLiveTest, IntColumnToSlongTarget) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(-2000000 AS INT) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-2000000, value);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), ind);

    SQLCloseCursor(stmt_);
}

// DATETIME2(7) into SQL_C_TYPE_TIMESTAMP. The fractional field is the guard
// against a units mismatch in the wire value (it is carried in 100 ns ticks, not
// nanoseconds), which no unit test that builds SqlTime by hand can catch.
TEST_F(GetDataLiveTest, Datetime2ToTimestampTargetKeepsFraction) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('2023-06-15 12:34:56.1234567' AS DATETIME2(7)) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_TIMESTAMP_STRUCT ts{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &ts, sizeof(ts), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2023, ts.year);
    EXPECT_EQ(6, ts.month);
    EXPECT_EQ(15, ts.day);
    EXPECT_EQ(12, ts.hour);
    EXPECT_EQ(34, ts.minute);
    EXPECT_EQ(56, ts.second);
    EXPECT_EQ(123456700u, ts.fraction);

    SQLCloseCursor(stmt_);
}

// A non-PLP column whose type has no character conversion (e.g. a short
// VARBINARY) must fail with HYC00 and leave the column readable, so a retry with
// a compatible C type still works. The reference msodbcsql driver renders binary
// as hex, so the HYC00 assertion is mssql-odbc-specific.
//
// Maintenance note: this relies on the column type having no
// column_value_to_text arm. It was originally anchored on DATETIME, which became
// convertible when the typed conversion core landed; binary is the remaining
// non-PLP type with no character rendering. If binary→hex is ever implemented,
// re-point this again, or assert the recovery via the target-type HYC00 path (an
// unsupported SQL_C target) with a type that will stay unsupported.
TEST_F(GetDataLiveTest, UnsupportedColumnTypeHyc00PreservesValue) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(0x4142434445464748 AS VARBINARY(8)) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // First attempt with an unsupported target for this column type fails soft.
    SQLCHAR buf[64] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    // The column is still addressable: a retry (again HYC00, not 24000) proves
    // the value was not consumed by the failed attempt.
    SQLCHAR buf2[64] = {0};
    SQLLEN ind2 = 0;
    SQLRETURN rc2 = SQLGetData(stmt_, 1, SQL_C_CHAR, buf2, sizeof(buf2), &ind2);
    EXPECT_EQ(SQL_ERROR, rc2);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    SQLCloseCursor(stmt_);
}


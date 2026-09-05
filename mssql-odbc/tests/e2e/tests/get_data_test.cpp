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

// Encodes UTF-16 code units as UTF-8 without <codecvt> (deprecated), so a
// fetched value can be compared against a plain string literal.
//
// Only a well-formed surrogate pair is combined; a lone surrogate is passed
// through as-is rather than repaired, so a genuinely malformed value is not
// silently cleaned up here.
//
// This says nothing about chunk boundaries: the units arrive already flattened,
// so a pair whose halves came from two SQLGetData calls is indistinguishable
// from one delivered whole -- and correctly so, because splitting a pair across
// calls is legal (SQL_C_WCHAR chunks in code units). Callers that need to see
// boundaries take the per-call counts from ReadWCharDataInChunksAsUtf8.
std::string Utf16ToUtf8(const std::u16string& units) {
    std::string out;
    for (size_t i = 0; i < units.size(); ++i) {
        char32_t cp = units[i];
        if (cp >= 0xD800 && cp <= 0xDBFF && i + 1 < units.size() && units[i + 1] >= 0xDC00 &&
            units[i + 1] <= 0xDFFF) {
            cp = 0x10000 + ((cp - 0xD800) << 10) + (units[i + 1] - 0xDC00);
            ++i;
        }
        if (cp < 0x80) {
            out.push_back(static_cast<char>(cp));
        } else if (cp < 0x800) {
            out.push_back(static_cast<char>(0xC0 | (cp >> 6)));
            out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
        } else if (cp < 0x10000) {
            out.push_back(static_cast<char>(0xE0 | (cp >> 12)));
            out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
            out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
        } else {
            out.push_back(static_cast<char>(0xF0 | (cp >> 18)));
            out.push_back(static_cast<char>(0x80 | ((cp >> 12) & 0x3F)));
            out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
            out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
        }
    }
    return out;
}

// Streams one SQL_C_WCHAR column across as many SQLGetData calls as it takes,
// using a small buffer, and returns the assembled value as UTF-8 so it can be
// compared against a plain string literal.
//
// Chunks are appended up to the terminator rather than by the indicator: on a
// transcoding path the indicator is SQL_NO_TOTAL (wire bytes are not delivered
// code units), so the terminator is the only length signal an application has.
//
// When `per_call_units` is given, it receives the code units delivered by each
// SQLGetData call, so a caller can assert on the chunk boundaries the assembled
// value hides.
std::string ReadWCharDataInChunksAsUtf8(SQLHSTMT stmt, SQLUSMALLINT col, size_t buf_bytes,
                                        std::vector<size_t>* per_call_units = nullptr) {
    std::u16string units;
    std::vector<SQLCHAR> buf(buf_bytes, 0);
    int guard = 0;
    while (true) {
        const size_t units_before = units.size();
        std::fill(buf.begin(), buf.end(), 0);
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt, col, SQL_C_WCHAR, buf.data(),
                                  static_cast<SQLLEN>(buf.size()), &ind);
        EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "SQLGetData failed rc=" << rc;
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        // The byte buffer is deliberately untyped -- that is the shape an ODBC
        // application passes -- so each code unit is copied out rather than read
        // through a SQLWCHAR* alias, which would be an unaligned load.
        for (size_t i = 0; i + sizeof(SQLWCHAR) <= buf_bytes; i += sizeof(SQLWCHAR)) {
            SQLWCHAR unit = 0;
            std::memcpy(&unit, buf.data() + i, sizeof(unit));
            if (unit == 0) {
                break;
            }
            units.push_back(static_cast<char16_t>(unit));
        }
        if (per_call_units != nullptr) {
            per_call_units->push_back(units.size() - units_before);
        }
        if (rc == SQL_SUCCESS) {
            break;
        }
        EXPECT_LT(++guard, 100000) << "PLP stream made no forward progress";
        if (guard >= 100000) {
            break;
        }
    }
    return Utf16ToUtf8(units);
}

// Fetches column `col` the way a driver-agnostic client library does when it
// sizes its buffer from SQLDescribeCol: describe first, compute
// `(ColumnSize + 1) * sizeof(SQLWCHAR)`, read into that, and fall back to
// streaming when the value did not fit.
//
// This is mssql-python's `SQLGetData_wrap` shape (ddbc_bindings.cpp, the
// SQL_WCHAR/SQL_WVARCHAR/SQL_WLONGVARCHAR branch), and it is the shape that
// AB#47506 broke: for any MAX / XML / computed-string column SQLDescribeCol
// reports ColumnSize 0, so the computed buffer is exactly 2 bytes -- room for
// the null terminator and no payload at all.
//
// Returns the assembled value as UTF-8. Sets `*first_rc` / `*first_ind` to the
// return code and indicator of that first sized read, which is where the
// regression showed up.
std::string FetchLikeDescribeColClient(SQLHSTMT stmt, SQLUSMALLINT col,
                                       SQLRETURN* first_rc = nullptr,
                                       SQLLEN* first_ind = nullptr) {
    // SQLTCHAR, not SQLWCHAR: the suite builds in the platform default TCHAR
    // mode, so SQLDescribeCol resolves to the narrow entry point here. Only the
    // column name is affected, and it is not used.
    SQLTCHAR name[256] = {};
    SQLSMALLINT name_len = 0, data_type = 0, dec_digits = 0, nullable = 0;
    SQLULEN column_size = 0;
    SQLRETURN rc =
        SQLDescribeCol(stmt, col, name, static_cast<SQLSMALLINT>(sizeof(name) / sizeof(SQLTCHAR)),
                       &name_len, &data_type, &column_size, &dec_digits, &nullable);
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) << "SQLDescribeCol rc=" << rc;

    // A client cannot size a buffer from an unbounded column, so it asks for the
    // minimum and reads the indicator. buffer_length is in bytes.
    const size_t buf_units = static_cast<size_t>(column_size) + 1;
    std::vector<SQLWCHAR> buf(buf_units, 0);
    SQLLEN ind = 0;
    rc = SQLGetData(stmt, col, SQL_C_WCHAR, buf.data(),
                    static_cast<SQLLEN>(buf_units * sizeof(SQLWCHAR)), &ind);
    if (first_rc != nullptr) {
        *first_rc = rc;
    }
    if (first_ind != nullptr) {
        *first_ind = ind;
    }
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
        << "sized read rc=" << rc << " (buffer " << buf_units * sizeof(SQLWCHAR) << " bytes)";
    if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
        return "<error>";
    }
    if (ind == SQL_NULL_DATA) {
        return "<null>";
    }

    std::u16string units;
    if (rc == SQL_SUCCESS) {
        for (size_t i = 0; i < buf_units && buf[i] != 0; ++i) {
            units.push_back(static_cast<char16_t>(buf[i]));
        }
    } else {
        // Truncated: the indicator gave the length (or SQL_NO_TOTAL), so the
        // client re-reads the column through its streaming path. This only works
        // if the sized read left the value intact.
        return ReadWCharDataInChunksAsUtf8(stmt, col, 8192);
    }

    return Utf16ToUtf8(units);
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

    // The native `json` type is not on every supported target. Probe rather
    // than parse a version string, so the check reflects what the server will
    // actually accept.
    bool ServerSupportsNativeJson() {
        const bool ok = SQL_SUCCEEDED(ExecDirect("SELECT CAST(N'{}' AS JSON)"));
        SQLCloseCursor(stmt_);
        return ok;
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

// AB#47507: a bare SELECT NULL is a nullable INT column with no CAST. When the
// caller supplies no indicator, SQLGetData must fail with SQLSTATE 22002
// rather than silently succeed and leave the target buffer untouched. This is
// the exact regression reported: mssql-python fetches SQL_INTEGER columns via
// SQL_C_SLONG with a null indicator and relies on the ODBC-mandated error to
// detect NULL, falling back to None only when SQLGetData fails.
TEST_F(GetDataLiveTest, NullColumnWithoutIndicatorReturns22002) {
    ASSERT_SQL_OK(ExecDirect("SELECT NULL"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 7;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22002");
    EXPECT_EQ(7, value) << "a NULL must not disturb the data slot";

    SQLCloseCursor(stmt_);
}

// The same rule holds on the PLP arrival path: VARCHAR(MAX) NULL decodes via
// the distinct SQL_PLP_NULL wire marker and RowWriter::write_null, not the
// fixed/var-length NULL length prefix NullColumn above exercises. Both paths
// must converge on the identical SQLGetData guard.
TEST_F(GetDataLiveTest, PlpNullColumnWithoutIndicatorReturns22002) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS VARCHAR(MAX)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16];
    std::memset(buf, 'Z', sizeof(buf));
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22002");
    EXPECT_TRUE(std::all_of(buf, buf + sizeof(buf),
                            [](SQLCHAR b) { return b == 'Z'; }))
        << "a NULL must not disturb the data slot";

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
// msodbcsql consumes the column after the failed conversion and returns
// SQL_NO_DATA from the follow-up call.
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
// SQL_NUMERIC_STRUCT is not implemented, recorded as a tracked gap (AB#47816)
// in the "Known divergences from msodbcsql" table in
// docs/typed-columnar-fetch-plan.md. Retarget this test at another unimplemented
// C type when that gap closes, rather than deleting the coverage.
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


// A buffer with room for the terminator but no payload is a length probe, not a
// caller error. It must report the available length with 01004 and leave the
// value re-readable, so the application can size a real buffer and fetch again.
//
// AB#47506: this is not a hypothetical shape. `SQLDescribeCol` reports
// ColumnSize 0 for a MAX column, so a caller sizing its buffer as
// `(ColumnSize + 1) * sizeof(SQLWCHAR)` gets exactly 2 bytes. mssql-python does
// that for every NVARCHAR(MAX) column, which is why rejecting the probe with
// HY090 failed every such fetch -- including short values nowhere near a chunk
// boundary.
//
// Runs on both legs: msodbcsql answers the probe the same way.
TEST_F(GetDataLiveTest, PlpLengthProbeBufferReportsLengthAndKeepsValue) {
    // Spelled as NCHAR() rather than a UTF-8 literal: ExecDirect widens the
    // narrow SQL text byte by byte, so a multi-byte literal would reach the
    // server as one character per UTF-8 byte.
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(N'Hello ' + NCHAR(0xD83D) + NCHAR(0xDE04) AS NVARCHAR(MAX)) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // 2 bytes: room for the terminator only. 7 chars -> 8 UTF-16 code units
    // (the emoji is a surrogate pair) -> 16 wire bytes.
    SQLWCHAR probe[1] = {0xFFFF};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_WCHAR, probe, sizeof(probe), &ind);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(16, ind) << "probe must report the bytes available";
    EXPECT_EQ(0, probe[0]) << "a zero-payload probe writes only the terminator";

    // The probe consumed nothing: the value is still fully readable.
    const std::string expected = "Hello \xF0\x9F\x98\x84";
    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 8192));

    SQLCloseCursor(stmt_);
}

// AB#47506: a varchar(max)/json column requested as SQL_C_WCHAR used to be
// rejected with HYC00 on the first streamed chunk, because the PLP delivery
// gate had no narrow-wire -> wide-target arm. mssql-python fetches every
// character column with its default charCtype of SQL_C_WCHAR, so this rejected
// every varchar(max) column it read.
//
// ASCII content keeps the wire bytes 1:1 with code units, so a framing error
// shows up as a shifted or dropped character rather than a decode artifact.
TEST_F(GetDataLiveTest, VarcharMaxToWcharChunkedRoundTrip) {
    const std::string expected = RepeatToken("0123456789", 3000);  // 30000 chars
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST('0123456789' AS VARCHAR(MAX)), 3000) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 1024));

    SQLCloseCursor(stmt_);
}

// The widening decodes through the column's own collation, so a non-ASCII
// CP1252 value must come back as the original characters and not as raw bytes
// zero-extended into code units. A 26-byte buffer delivers 12 characters per
// call, so the value is reassembled over hundreds of continuations.
TEST_F(GetDataLiveTest, VarcharMaxCp1252ToWcharChunkedRoundTrip) {
    // UTF-8 spelling of "café René Größe naïve " — the expected decoded value.
    const std::string token = "caf\xC3\xA9 Ren\xC3\xA9 Gr\xC3\xB6\xC3\x9F"
                              "e na\xC3\xAF"
                              "ve ";
    const std::string expected = RepeatToken(token, 400);
    // NCHAR() rather than a UTF-8 literal: the fixture widens narrow SQL text
    // byte by byte, so a multi-byte literal would reach the server as one
    // character per UTF-8 byte.
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(N'caf' + NCHAR(0xE9) + N' Ren' + NCHAR(0xE9) "
                   "+ N' Gr' + NCHAR(0xF6) + NCHAR(0xDF) + N'e na' + NCHAR(0xEF) + N've ' "
                   "COLLATE SQL_Latin1_General_CP1_CI_AS AS VARCHAR(MAX)), 400) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 26));

    SQLCloseCursor(stmt_);
}

// The pinning case for the chunk-boundary carry. Under a Chinese_PRC collation
// the wire encoding is GBK, where each CJK character is two bytes. A 30-byte
// buffer makes the driver read an odd number of wire bytes per call, so a
// character is split across the boundary on most calls; its two halves must be
// rejoined rather than each becoming U+FFFD.
//
// Skipped on the msodbcsql leg because the two drivers disagree about the
// conversion itself, not the chunking. mssql-odbc decodes the wire bytes through
// the column's own collation (GBK here), so the value round-trips. msodbcsql on
// Linux converts through the client locale instead: with a UTF-8 locale and no
// GBK support it best-fits every CJK character to '?', so the measured result is
// "????????abc..." -- silent data loss. Asserting the round-trip on that leg
// would only re-report a divergence this driver is deliberately on the right
// side of.
TEST_F(GetDataLiveTest, VarcharMaxDbcsToWcharSplitsCharacterAcrossChunks) {
    SKIP_IF_COMPARING_MSODBCSQL();
    const std::string token = "\xE4\xBD\xA0\xE5\xA5\xBD\xE4\xB8\x96\xE7\x95\x8C"
                              "abc";  // 你好世界abc
    const std::string expected = RepeatToken(token, 400);
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(NCHAR(0x4F60) + NCHAR(0x597D) + NCHAR(0x4E16) "
                   "+ NCHAR(0x754C) + N'abc' "
                   "COLLATE Chinese_PRC_CI_AS AS VARCHAR(MAX)), 400) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 30));

    SQLCloseCursor(stmt_);
}

// A UTF-8 collation puts 1-4 byte sequences on the wire for the same column
// type, and an astral character has to survive both the UTF-8 chunk boundary
// and the surrogate-pair encode on the way out.
TEST_F(GetDataLiveTest, VarcharMaxUtf8CollationToWcharChunkedRoundTrip) {
    const std::string token = "\xE4\xBD\xA0\xE5\xA5\xBD"
                              "caf\xC3\xA9\xF0\x9F\x98\x80";  // 你好café😀
    const std::string expected = RepeatToken(token, 300);
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(NCHAR(0x4F60) + NCHAR(0x597D) + N'caf' "
                   "+ NCHAR(0xE9) + NCHAR(0xD83D) + NCHAR(0xDE00) "
                   "COLLATE Latin1_General_100_CI_AS_SC_UTF8 AS VARCHAR(MAX)), 300) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 30));

    SQLCloseCursor(stmt_);
}

// Chunking must be invisible to the caller: the same column read in one call
// and in many must produce the same value.
TEST_F(GetDataLiveTest, VarcharMaxToWcharChunkSizeDoesNotChangeValue) {
    const char* kQuery =
        "SELECT REPLICATE(CAST(N'caf' + NCHAR(0xE9) + N' Ren' + NCHAR(0xE9) "
        "+ N' Gr' + NCHAR(0xF6) + NCHAR(0xDF) + N'e na' + NCHAR(0xEF) + N've ' "
        "COLLATE SQL_Latin1_General_CP1_CI_AS AS VARCHAR(MAX)), 400) AS c1";

    ASSERT_SQL_OK(ExecDirect(kQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    const std::string one_shot = ReadWCharDataInChunksAsUtf8(stmt_, 1, 65536);
    SQLCloseCursor(stmt_);

    ASSERT_SQL_OK(ExecDirect(kQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    const std::string chunked = ReadWCharDataInChunksAsUtf8(stmt_, 1, 34);
    SQLCloseCursor(stmt_);

    EXPECT_FALSE(one_shot.empty());
    EXPECT_EQ(one_shot, chunked);
}

// The smallest buffer the widening path must serve: 4 bytes is one SQLWCHAR of
// payload plus the terminator, so the driver delivers exactly one character per
// call. Verified against msodbcsql, which drains the same value the same way.
//
// This is the boundary where sizing the decode by the caller's capacity breaks
// down. Some decoders refuse to emit anything with a single code unit of room —
// `encoding_rs::GBK` returns OutputFull having consumed nothing — so the DBCS
// case below is the one that actually pins the behaviour; the ASCII case is here
// to separate "one character per call" from "multi-byte character handling".
TEST_F(GetDataLiveTest, VarcharMaxToWcharSingleCharacterBuffer) {
    const std::string expected = RepeatToken("0123456789", 20);  // 200 chars
    ASSERT_SQL_OK(ExecDirect("SELECT REPLICATE(CAST('0123456789' AS VARCHAR(MAX)), 20) AS c1"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 4));

    SQLCloseCursor(stmt_);
}

// A DBCS column into a one-character buffer. Each character is two wire bytes
// and one code unit, so it fits, but only if the decode is not confined to the
// caller's buffer.
//
// Skipped on the msodbcsql leg for the same reason as
// VarcharMaxDbcsToWcharSplitsCharacterAcrossChunks: msodbcsql on Linux best-fits
// the CJK characters to '?' rather than decoding the column's GBK collation.
TEST_F(GetDataLiveTest, VarcharMaxDbcsToWcharSingleCharacterBuffer) {
    SKIP_IF_COMPARING_MSODBCSQL();
    const std::string token = "\xE4\xBD\xA0\xE5\xA5\xBD\xE4\xB8\x96\xE7\x95\x8C"
                              "abc";  // 你好世界abc
    const std::string expected = RepeatToken(token, 40);
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(NCHAR(0x4F60) + NCHAR(0x597D) + NCHAR(0x4E16) "
                   "+ NCHAR(0x754C) + N'abc' "
                   "COLLATE Chinese_PRC_CI_AS AS VARCHAR(MAX)), 40) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 4));

    SQLCloseCursor(stmt_);
}

// `json` is the other narrow PLP type that widens to SQL_C_WCHAR, and it takes a
// different route to the decoder than `varchar(max)` does: it is UTF-8 on the
// wire and carries no collation, so the encoding is chosen from PlpEncoding
// rather than from the column collation. Deriving it through `get_encoding_type`
// would unwrap the absent collation and panic across the FFI boundary, which is
// UB -- so this covers the branch that avoids that.
//
// Non-ASCII and a small buffer are both deliberate: they force multi-byte
// sequences to straddle chunk boundaries on the no-collation path.
TEST_F(GetDataLiveTest, JsonToWcharChunkedRoundTrip) {
    if (!ServerSupportsNativeJson()) {
        GTEST_SKIP() << "server has no native json type";
    }
    const std::string token = "\xE4\xBD\xA0\xE5\xA5\xBD"
                              "caf\xC3\xA9\xF0\x9F\x98\x80";  // 你好café😀
    const std::string expected = "{\"k\":\"" + RepeatToken(token, 200) + "\"}";
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(N'{\"k\":\"' + REPLICATE(CAST(NCHAR(0x4F60) "
                             "+ NCHAR(0x597D) + N'caf' + NCHAR(0xE9) + NCHAR(0xD83D) "
                             "+ NCHAR(0xDE00) AS NVARCHAR(MAX)), 200) + N'\"}' AS JSON) AS c1"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 30));

    SQLCloseCursor(stmt_);
}

// The same json value through a length probe: ColumnSize is 0 for json exactly
// as it is for a MAX column, so mssql-python sends the same 2-byte buffer here.
TEST_F(GetDataLiveTest, JsonLengthProbeReportsTruncationAndKeepsValue) {
    if (!ServerSupportsNativeJson()) {
        GTEST_SKIP() << "server has no native json type";
    }
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(N'{\"k\":\"' + NCHAR(0xE9) + N'\"}' AS JSON) AS c1"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR probe[1] = {0xFFFF};
    SQLLEN ind = 0;
    const SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_WCHAR, probe, sizeof(probe), &ind);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_EQ(0, probe[0]) << "a zero-payload buffer still gets its terminator";

    // Nothing was consumed: the value reads back whole.
    EXPECT_EQ("{\"k\":\"\xC3\xA9\"}", ReadWCharDataInChunksAsUtf8(stmt_, 1, 64));

    SQLCloseCursor(stmt_);
}

// A two-code-unit buffer is the tightest one an astral character fits in, so it
// is where the widening read is most likely to consume wire bytes without being
// able to emit anything.
//
// What that has to guarantee is forward progress, not surrogate atomicity: every
// truncated call must carry a non-empty payload, because an application reading
// to the terminator cannot tell an empty chunk apart from a stream that has
// stopped advancing. A surrogate pair may still span two calls -- SQL_C_WCHAR
// chunking is in code units, and the UTF-16 passthrough path splits on the same
// rule -- so it is the assembled value, not the chunk boundaries, that has to
// round-trip.
TEST_F(GetDataLiveTest, VarcharMaxAstralToWcharSurrogatePairBuffer) {
    const std::string token = "\xE4\xBD\xA0\xE5\xA5\xBD"
                              "caf\xC3\xA9\xF0\x9F\x98\x80";  // 你好café😀
    const std::string expected = RepeatToken(token, 60);
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(NCHAR(0x4F60) + NCHAR(0x597D) + N'caf' "
                   "+ NCHAR(0xE9) + NCHAR(0xD83D) + NCHAR(0xDE00) "
                   "COLLATE Latin1_General_100_CI_AS_SC_UTF8 AS VARCHAR(MAX)), 60) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::vector<size_t> per_call;
    EXPECT_EQ(expected, ReadWCharDataInChunksAsUtf8(stmt_, 1, 6, &per_call));

    // Only the final call, which reports SQL_SUCCESS, may deliver nothing.
    ASSERT_FALSE(per_call.empty());
    for (size_t i = 0; i + 1 < per_call.size(); ++i) {
        EXPECT_GT(per_call[i], 0u) << "truncated call " << i << " delivered an empty payload";
    }

    SQLCloseCursor(stmt_);
}

// The two shapes either side of the probe boundary, which differ by one byte of
// payload room.
//
// A buffer with payload room too small to carry one whole character cannot make
// progress: the conservative read sizing rounds it to zero, so returning
// truncation would let an application looping on an unchanged buffer spin
// forever. That is HY090, not a probe.
//
// A buffer with no payload room at all is a probe, and is answered.
//
// msodbcsql instead delivers one payload byte per call for the first shape --
// 'a' as 0x61, 'e-acute' as 0xE9, CJK as 0x3F ('?') -- because it converts
// SQL_C_CHAR to the client codepage, and the codepage measured here is
// single-byte, so every character is one byte (and unrepresentable ones are
// best-fit away, losing data). mssql-odbc delivers UTF-8, where a character is
// 1-4 bytes. Matching it needs an unflushed-tail buffer in ActivePlpStream;
// tracked separately. Hence the skip on the comparison leg.
TEST_F(GetDataLiveTest, PlpSubMinimalBufferIsRejectedButProbeIsAnswered) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(N'abcd' AS NVARCHAR(MAX)), 50) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // 2 bytes for SQL_C_CHAR: one payload byte once the terminator is reserved,
    // which cannot hold a complete transcoded character.
    SQLCHAR tiny[2] = {0xFF, 0xFF};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, tiny, sizeof(tiny), &ind);
    EXPECT_EQ(SQL_ERROR, rc) << "a buffer that cannot make progress must not report truncation";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY090");

    // The rejection consumed nothing, so the value still reads back in full.
    EXPECT_EQ(RepeatToken("abcd", 50), ReadCharDataInChunks(stmt_, 1, 8192));

    SQLCloseCursor(stmt_);

    // buffer_length 1 (room for the NUL only) is the portable length-probe
    // shape, and it exercises the non-transcode PLP branch (varchar(max) ->
    // SQL_C_CHAR) where the payload capacity collapses to 0 directly.
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR one[1] = {0xFF};
    SQLLEN ind1 = 0;
    SQLRETURN rc1 = SQLGetData(stmt_, 1, SQL_C_CHAR, one, sizeof(one), &ind1);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc1);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    EXPECT_EQ(RepeatToken("abc", 200), ReadCharDataInChunks(stmt_, 1, 8192));

    SQLCloseCursor(stmt_);
}

// The same boundary on the widening path (varchar(max) -> SQL_C_WCHAR), where
// the read is sized from output code units rather than byte capacity.
//
// 2 bytes is the probe: room for the terminator, no code unit. 1 byte cannot
// hold even the terminator, and 3 bytes has a spare byte that no whole code
// unit fits in -- neither can make progress, so both are HY090 rather than a
// truncation an application would retry forever.
TEST_F(GetDataLiveTest, WidenedPlpRejectsBuffersThatCannotHoldACodeUnit) {
    SKIP_IF_COMPARING_MSODBCSQL();
    for (SQLLEN len : {SQLLEN{1}, SQLLEN{3}}) {
        ASSERT_SQL_OK(
            ExecDirect("SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1"),
            SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        unsigned char buf[4];
        std::memset(buf, 0xFF, sizeof(buf));
        SQLLEN ind = 0;
        EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, len, &ind))
            << "buffer_length=" << len;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY090");

        // The rejection consumed nothing: the value still reads back whole.
        EXPECT_EQ(RepeatToken("abc", 200), ReadWCharDataInChunksAsUtf8(stmt_, 1, 8192));
        SQLCloseCursor(stmt_);
    }

    // 2 bytes is the probe shape and must still be answered.
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR probe[1] = {0xFFFF};
    SQLLEN probe_ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetData(stmt_, 1, SQL_C_WCHAR, probe, sizeof(probe), &probe_ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(0, probe[0]) << "a zero-payload buffer still gets its terminator";
    EXPECT_EQ(RepeatToken("abc", 200), ReadWCharDataInChunksAsUtf8(stmt_, 1, 8192));

    SQLCloseCursor(stmt_);
}

// An invalid TargetType is a property of the request, so it must report HY003
// whether the column is delivered from the captured value or from an open PLP
// stream. Before this was hoisted ahead of the dispatch, the PLP path reached
// its own compatibility gate first and reported HYC00 instead.
TEST_F(GetDataLiveTest, InvalidTargetTypeOnPlpStreamReportsHy003) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST('abc' AS VARCHAR(MAX)), 200) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // Open the stream with a legitimate partial read first, so the next call
    // takes the PLP continuation path rather than the captured-value path.
    SQLCHAR buf[8] = {0};
    SQLLEN ind = 0;
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind));

    SQLCHAR out[16] = {0};
    SQLLEN ind2 = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, 9999, out, sizeof(out), &ind2));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY003");

    SQLCloseCursor(stmt_);
}

// ===================================================================
// AB#47506 regression: columns whose SQLDescribeCol ColumnSize is 0.
//
// A client that sizes its fetch buffer as `(ColumnSize + 1) * sizeof(SQLWCHAR)`
// -- which is what mssql-python does -- computes a 2-byte buffer for any such
// column: room for the null terminator and no payload. Rejecting that request
// broke every fetch of a MAX, XML, or computed-string column, including values
// only a few characters long.
//
// Each case below mirrors a specific mssql-python test that failed. They run on
// both legs: msodbcsql answers these the same way.
// ===================================================================

// tests/test_004_cursor.py::test_emoji_round_trip -- NVARCHAR(MAX) holding
// short strings with astral characters, ZWJ sequences and combining marks.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnNvarcharMaxEmoji) {
    struct Case {
        const char* sql_literal;
        const char* expected_utf8;
    };
    // Spelled as NCHAR() so the payload does not depend on the source file
    // encoding surviving the compiler and the ODBC driver manager.
    const Case cases[] = {
        // "Hello " + U+1F604
        {"N'Hello ' + NCHAR(0xD83D) + NCHAR(0xDE04)", "Hello \xF0\x9F\x98\x84"},
        // "Accented " + e-acute u-diaeresis n-tilde c-cedilla
        {"N'Accented ' + NCHAR(0xE9) + NCHAR(0xFC) + NCHAR(0xF1) + NCHAR(0xE7)",
         "Accented \xC3\xA9\xC3\xBC\xC3\xB1\xC3\xA7"},
        // "Chinese: " + U+4E2D U+6587
        {"N'Chinese: ' + NCHAR(0x4E2D) + NCHAR(0x6587)", "Chinese: \xE4\xB8\xAD\xE6\x96\x87"},
        // U+1F468 ZWJ U+1F469 -- a ZWJ sequence, two surrogate pairs plus U+200D
        {"NCHAR(0xD83D) + NCHAR(0xDC68) + NCHAR(0x200D) + NCHAR(0xD83D) + NCHAR(0xDC69)",
         "\xF0\x9F\x91\xA8\xE2\x80\x8D\xF0\x9F\x91\xA9"},
    };

    for (const auto& c : cases) {
        const std::string query =
            std::string("SELECT CAST(") + c.sql_literal + " AS NVARCHAR(MAX)) AS content";
        ASSERT_SQL_OK(ExecDirect(query), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLRETURN first_rc = SQL_SUCCESS;
        SQLLEN first_ind = 0;
        const std::string got = FetchLikeDescribeColClient(stmt_, 1, &first_rc, &first_ind);

        // The sized read must report truncation with a usable length, never fail.
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, first_rc) << "literal: " << c.sql_literal;
        EXPECT_GT(first_ind, 0) << "indicator must give the byte count to re-fetch with";
        EXPECT_EQ(std::string(c.expected_utf8), got) << "literal: " << c.sql_literal;

        SQLCloseCursor(stmt_);
    }
}

// tests/test_015_pyformat_parameters.py::test_unicode_single_param -- a bound
// parameter echoed straight back, so the result column has no declared width.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnBoundParameterEcho) {
    // "Hello " + U+4E16 U+754C + " " + U+1F30D
    const std::string expected = "Hello \xE4\xB8\x96\xE7\x95\x8C \xF0\x9F\x8C\x8D";
    const std::vector<SQLWCHAR> param = {'H',    'e',    'l',    'l',    'o',   ' ',
                                         0x4E16, 0x754C, ' ',    0xD83C, 0xDF0D};
    SQLLEN param_len = static_cast<SQLLEN>(param.size() * sizeof(SQLWCHAR));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR,
                                   param.size(), 0,
                                   const_cast<SQLWCHAR*>(param.data()), param_len, &param_len),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(ExecDirect("SELECT ?"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(expected, FetchLikeDescribeColClient(stmt_, 1));

    SQLCloseCursor(stmt_);
}

// tests/test_017_spatial_types.py::test_geography_as_text and
// ::test_geometry_as_text -- STAsText() converts the UDT server-side, so the
// column is nvarchar with no declared width. The UDT itself never crosses the
// wire, which is why these are in scope even though UDT fetch is not.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnSpatialAsText) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT geography::STGeomFromText('POINT(-122.34900 47.65100)', 4326)"
                   ".STAsText() AS wkt"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    const std::string geography = FetchLikeDescribeColClient(stmt_, 1);
    EXPECT_EQ(0u, geography.rfind("POINT", 0)) << "got: " << geography;
    EXPECT_NE(std::string::npos, geography.find("-122.349"));
    EXPECT_NE(std::string::npos, geography.find("47.651"));
    SQLCloseCursor(stmt_);

    ASSERT_SQL_OK(
        ExecDirect("SELECT geometry::STGeomFromText('POINT(100 200)', 0).STAsText() AS wkt"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    const std::string geometry = FetchLikeDescribeColClient(stmt_, 1);
    EXPECT_EQ(0u, geometry.rfind("POINT", 0)) << "got: " << geometry;
    EXPECT_NE(std::string::npos, geometry.find("100"));
    EXPECT_NE(std::string::npos, geometry.find("200"));
    SQLCloseCursor(stmt_);
}

// tests/test_018_polars_pandas_integration.py::test_all_types_are_isclass and
// tests/test_024_bulkcopy_arrow.py::test_xml -- an XML column, both directly and
// through CAST(... AS NVARCHAR(MAX)). The isclass test reads cursor.description
// for many columns but still has to fetchall() at the end, which is where it
// died.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnXmlColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('<r/>' AS XML) AS x"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("<r/>", FetchLikeDescribeColClient(stmt_, 1));
    SQLCloseCursor(stmt_);

    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(CAST('<r a=\"1\">hi</r>' AS XML) AS NVARCHAR(MAX)) AS x"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    const std::string xml = FetchLikeDescribeColClient(stmt_, 1);
    EXPECT_NE(std::string::npos, xml.find("hi")) << "got: " << xml;
    SQLCloseCursor(stmt_);
}

// The same client shape over a value far larger than one chunk, so the probe is
// followed by a real multi-call stream rather than a single follow-up read.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnLargeNvarcharMax) {
    const std::string expected = RepeatToken("0123456789", 3000);  // 30000 chars
    ASSERT_SQL_OK(
        ExecDirect("SELECT REPLICATE(CAST(N'0123456789' AS NVARCHAR(MAX)), 3000) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN first_rc = SQL_SUCCESS;
    SQLLEN first_ind = 0;
    const std::string got = FetchLikeDescribeColClient(stmt_, 1, &first_rc, &first_ind);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, first_rc);
    EXPECT_EQ(60000, first_ind) << "indicator must report the whole value, not the chunk";
    EXPECT_EQ(expected, got);

    SQLCloseCursor(stmt_);
}

// A sized column takes the other branch: the computed buffer has real room, so
// the value arrives in one call with SQL_SUCCESS. This is why the bug never
// showed up on ordinary columns.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnSizedColumnIsSingleCall) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(N'Hello ' + NCHAR(0xD83D) + NCHAR(0xDE04) AS NVARCHAR(50)) AS c"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN first_rc = SQL_ERROR;
    SQLLEN first_ind = 0;
    const std::string got = FetchLikeDescribeColClient(stmt_, 1, &first_rc, &first_ind);
    EXPECT_EQ(SQL_SUCCESS, first_rc) << "a sized column must not need a second call";
    EXPECT_EQ(16, first_ind);
    EXPECT_EQ("Hello \xF0\x9F\x98\x84", got);

    SQLCloseCursor(stmt_);
}

// tests/test_004_cursor.py::test_varbinarymax_insert_fetch_null -- the NULL leg
// of the varbinary(max) test. A NULL MAX column must report SQL_NULL_DATA on the
// probe rather than failing; binary *data* delivery is still unimplemented
// (AB#47239), which is why only the NULL case is covered here.
TEST_F(GetDataLiveTest, DescribeColSizedFetchOnNullMaxColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS NVARCHAR(MAX)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN first_rc = SQL_ERROR;
    SQLLEN first_ind = 0;
    EXPECT_EQ("<null>", FetchLikeDescribeColClient(stmt_, 1, &first_rc, &first_ind));
    EXPECT_EQ(SQL_SUCCESS, first_rc);
    EXPECT_EQ(SQL_NULL_DATA, first_ind);

    SQLCloseCursor(stmt_);
}

// tests/test_004_cursor.py::test_varbinarymax_insert_fetch_null -- the read that
// actually failed. mssql-python fetches a nullable varbinary(max) with a real
// SQL_C_BINARY buffer, not the zero-length probe, so the request reaches the
// target-type check. Binary *data* delivery is still unimplemented (AB#47239);
// a NULL carries no data, so it must be answered rather than rejected.
//
// The nonzero buffer is the point of this test: with a zero-length buffer the
// read is admitted as a length probe and the NULL gate is never consulted.
TEST_F(GetDataLiveTest, NullVarbinaryMaxToBinaryTargetReportsNull) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS VARBINARY(MAX)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    unsigned char buf[8];
    std::memset(buf, 0xAA, sizeof(buf));
    SQLLEN ind = 0;
    EXPECT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_NULL_DATA, ind);

    SQLCloseCursor(stmt_);
}

// tests/test_004_cursor_arrow.py::test_arrow_lob_wide (AB#47537) -- the shape
// that crashed the interpreter. mssql-python's Arrow fetch takes the SQLGetData
// branch whenever the result set holds a MAX column, and reads every column of
// the row that way, including a fixed `binary(9)`. Its GetDataVar helper starts
// each SQL_C_BINARY read with an empty buffer, so the first call arrives with
// BufferLength 0, and it grows and retries only while the driver reports there
// is more to come.
//
// Reporting SQL_SUCCESS there says the value fits in a zero-length buffer, so
// the caller stops retrying and copies `indicator` bytes out of a buffer it
// never grew. A truncation warning is what tells it to retry. Measured on
// msodbcsql 18.6.2.1 (SQL_DRIVER_VER 18.06.0002): SQL_SUCCESS_WITH_INFO with
// 01004 and indicator 9.
//
// This only asserts the probe contract; delivering the bytes on the retry is
// AB#47239.
TEST_F(GetDataLiveTest, ZeroLengthBinaryProbeReportsTruncationWhenBytesRemain) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('asdfghjkl' AS BINARY(9)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // The probe is keyed on a zero buffer length, not on a null pointer:
    // mssql-python passes NULL because it dlopen's the driver directly, while
    // these tests go through the Driver Manager, which rejects a null
    // TargetValuePtr with HY009 before the driver ever sees the call.
    SQLCHAR probe = 0;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(9, ind);

    SQLCloseCursor(stmt_);
}

// The other half of the contract: an empty value has nothing left to deliver, so
// the same probe is a plain success, and because that call delivered the whole
// value the column is consumed -- a repeat reports SQL_NO_DATA. Measured
// identically on msodbcsql 18.6.2.1 (SQL_SUCCESS with indicator 0 and no
// diagnostic, then SQL_NO_DATA).
TEST_F(GetDataLiveTest, ZeroLengthBinaryProbeOnEmptyValueSucceedsAndConsumesColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('' AS VARBINARY(8)) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR probe = 0;
    SQLLEN ind = -1;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind));
    EXPECT_EQ(0, ind);
    EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind));

    SQLCloseCursor(stmt_);
}

// AB#47537, second site. The two tests above run on a single-column result set,
// which does not produce a buffered row, so they only ever exercise the
// `write_captured_column` probe. #446 later added a second probe on the buffered
// fast path in `sql_get_data_safe`, which returns before that one is reached --
// and it is the path mssql-python's `arrow_batch` actually takes, because its
// unbound single-row fetch is precisely what makes a row buffered.
//
// The two blocks sit ~750 lines apart in different functions, so the merge that
// brought them together was textually clean and every existing test stayed
// green while the reachable path silently regressed to the original SIGSEGV.
// This test reproduces `test_arrow_lob_wide`'s actual shape -- a fixed
// `binary(9)` alongside an `nvarchar(max)`, fetched unbound -- so the buffered
// probe is covered on its own terms rather than by inference from the captured
// one.
//
// The MAX column is what forces the whole result set onto SQLGetData and makes
// the driver buffer the inline prefix; the `binary(9)` is the column that
// crashed. It is read without ever binding a column, exactly as mssql-python
// does.
TEST_F(GetDataLiveTest, ZeroLengthBinaryProbeReportsTruncationOnBufferedRow) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('asdfghjkl' AS BINARY(9)) AS c1,"
                             " CAST(N'hey' AS NVARCHAR(MAX)) AS c2"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR probe = 0;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind))
        << "a buffered-row probe must report truncation just like a captured one";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(9, ind);

    // Bytes remain, so the column stays readable rather than being retired.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind));
    EXPECT_EQ(9, ind);

    SQLCloseCursor(stmt_);
}

// The empty-value half of the buffered path: nothing remains, so the probe is a
// plain success and the column is consumed.
TEST_F(GetDataLiveTest, ZeroLengthBinaryProbeOnEmptyBufferedValueConsumesColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('' AS VARBINARY(8)) AS c1,"
                             " CAST(N'hey' AS NVARCHAR(MAX)) AS c2"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR probe = 0;
    SQLLEN ind = -1;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind));
    EXPECT_EQ(0, ind);
    EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind));

    // The MAX column that follows must still be readable -- retiring column 1
    // must not disturb the rest of the row.
    std::vector<SQLCHAR> buf(64, 0);
    SQLLEN text_ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 2, SQL_C_CHAR, buf.data(),
                              static_cast<SQLLEN>(buf.size()), &text_ind);
    EXPECT_TRUE(SQL_SUCCEEDED(rc)) << "rc=" << rc;
    EXPECT_STREQ("hey", reinterpret_cast<const char*>(buf.data()));

    SQLCloseCursor(stmt_);
}

// AB#47537 parity table, the *divergence* row -- deliberately guarded on the
// reference leg.
//
// The probe tests above all cover the agreement row (`binary`/`varbinary`/
// `nvarchar`), where this driver and msodbcsql answer identically. A
// fixed-source-type column is where the two intentionally part company:
// msodbcsql treats a short buffer for a fixed SQL type as a data overflow
// (`22003` / `SQL_ERROR`, indicator untouched), while this driver reports the
// truncation (`01004` / `SQL_SUCCESS_WITH_INFO`) so a caller is told to grow its
// buffer instead of believing an undelivered value landed.
//
// That divergence was only covered by unit tests built from synthetic
// `ColumnValues` until now, which cannot catch a change in how a real column
// reaches the probe. It is the row most worth a live guard precisely because it
// is the deliberate disagreement, and the parity note in
// `.github/instructions/mssql-odbc.instructions.md` is otherwise its only record.
//
// Both indicator sub-classes are covered: a type `binary_length` has an explicit
// arm for (`int` -> 4) and one that falls through to `SQL_NO_TOTAL`
// (`decimal`, `datetime2`).
//
// SKIP_IF_COMPARING_MSODBCSQL is justified by measurement, not assumption:
// against msodbcsql 18.6.2.1 (`SQL_DRIVER_VER` `18.06.0002`) each query below
// answers `SQL_ERROR` with `22003`, so the comparison leg genuinely fails rather
// than the macro papering over an untested guess.
TEST_F(GetDataLiveTest, ZeroLengthBinaryProbeOnFixedSourceTypesReportsTruncation) {
    SKIP_IF_COMPARING_MSODBCSQL();

    struct Case {
        const char* query;
        SQLLEN expected_indicator;
    };
    const Case cases[] = {
        {"SELECT CAST(7 AS INT) AS c1", 4},
        {"SELECT CAST(1.23 AS DECIMAL(10,2)) AS c1", SQL_NO_TOTAL},
        {"SELECT CAST('2025-01-01 12:00:05.123' AS DATETIME2(3)) AS c1", SQL_NO_TOTAL},
    };

    for (const auto& c : cases) {
        SCOPED_TRACE(c.query);
        ASSERT_SQL_OK(ExecDirect(c.query), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLCHAR probe = 0;
        SQLLEN ind = 12345;
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &ind))
            << "a fixed-source type must report truncation, not claim delivery";
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        EXPECT_EQ(c.expected_indicator, ind);

        SQLCloseCursor(stmt_);
    }
}

// AB#47482 / AB#47537 -- pins the crash *shape*, stated as the invariant the
// consumer actually relies on rather than as this driver's return codes.
//
// `FetchArrowBatch_wrap` (mssql-python `ddbc_bindings.cpp`) does:
//
//     while (target_vec->size() < start + dataLen) target_vec->resize(...);
//     std::memcpy(&(*target_vec)[start], &buffers.charBuffers[idxCol][...], dataLen);
//
// It grows the *destination* to the indicator and then memcpy's that many bytes
// out of the *source* buffer it handed us -- without ever checking the source
// actually received them. So the indicator is a promise about bytes delivered
// into the caller's buffer, and a call that delivered none must not report a
// nonzero indicator alongside plain success. That is precisely what this driver
// used to do, and the memcpy read off the end of an empty vector:
//
//     #0  __memcpy_avx_unaligned_erms ()
//     #1  FetchArrowBatch_wrap(...)
//
// Shape matters and is the cheap part to get wrong: neither column crashes
// alone. The `nvarchar(max)` must be present and must come *after* the
// `binary(9)`, because a LOB anywhere in the result set makes mssql-python drop
// to fetchSize 1 and pull every column row-by-row through `SQLGetData`, which is
// what routes the fixed-length binary column through `GetDataVar`.
//
// Deliberately mirrors `GetDataVar`'s zero-length-probe-then-grow-and-retry loop
// rather than doing one clean sized read, because the crash lived in that loop.
TEST_F(GetDataLiveTest, BinaryColumnBesideALobStaysWithinItsBuffer) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0x010203040506070809 AS BINARY(9)) AS b,"
                             " CAST(N'hey' AS NVARCHAR(MAX)) AS lob"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // 1. The probe, exactly as GetDataVar opens: an empty buffer.
    SQLCHAR empty = 0xCC;
    SQLLEN probe_ind = -999;
    const SQLRETURN probe_rc = SQLGetData(stmt_, 1, SQL_C_BINARY, &empty, 0, &probe_ind);

    // The invariant. Zero bytes reached the caller, so reporting plain success
    // would tell it those `probe_ind` bytes are sitting in its buffer. Whatever
    // else changes here, this must not.
    EXPECT_NE(SQL_SUCCESS, probe_rc)
        << "delivered 0 bytes but claimed success -- this is the AB#47537 crash: the caller "
           "memcpy's the indicator out of the empty buffer it passed";
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, probe_rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(9, probe_ind) << "the probe should still report the full length";
    EXPECT_EQ(0xCC, empty) << "a zero-length buffer must not be written to";

    // 2. Grow to the reported length and read for real, as GetDataVar then does.
    std::vector<SQLCHAR> buf(static_cast<size_t>(probe_ind), 0xCC);
    SQLLEN read_ind = -999;
    const SQLRETURN read_rc =
        SQLGetData(stmt_, 1, SQL_C_BINARY, buf.data(), static_cast<SQLLEN>(buf.size()), &read_ind);

    if (SQL_SUCCEEDED(read_rc)) {
        // Delivering is fine; over-promising is not.
        EXPECT_LE(read_ind, static_cast<SQLLEN>(buf.size()))
            << "indicator exceeds the caller's buffer -- the memcpy would run off the end";
        EXPECT_EQ(9, read_ind);
    } else {
        // Binary *data* delivery is still AB#47239, so a refusal is expected
        // today. Refusing is safe; the crash came from claiming success.
        EXPECT_EQ(SQL_ERROR, read_rc);
    }

    // 3. The LOB after it must still decode. A desync left behind by the binary
    //    column would otherwise surface as silent corruption rather than a
    //    failure, which is how this stayed hidden as a crash for so long.
    std::vector<SQLWCHAR> wbuf(64, 0);
    SQLLEN lob_ind = 0;
    const SQLRETURN lob_rc = SQLGetData(stmt_, 2, SQL_C_WCHAR, wbuf.data(),
                                        static_cast<SQLLEN>(wbuf.size() * sizeof(SQLWCHAR)),
                                        &lob_ind);
    EXPECT_TRUE(SQL_SUCCEEDED(lob_rc)) << "rc=" << lob_rc;
    std::u16string units;
    for (size_t i = 0; i < wbuf.size() && wbuf[i] != 0; ++i) {
        units.push_back(static_cast<char16_t>(wbuf[i]));
    }
    EXPECT_EQ("hey", Utf16ToUtf8(units));

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

// An embedded NUL ends the number on this side too. The column value is real
// data whose length is authoritative, but the parser is the one msodbcsql uses
// in both directions - CharToBigint's loop stops at the NUL whichever way the
// data moves (sqlccnvt.cpp:7800) - so "1\0 2" reads as 1 rather than 22018.
TEST_F(GetDataLiveTest, EmbeddedNulEndsANumericColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(CHAR(49) + CHAR(0) + CHAR(50) AS VARCHAR(8))"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER out = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &out, sizeof(out), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, out);

    SQLCloseCursor(stmt_);
}

// A character column holding more digits than an exact i128 mantissa still
// reaches a float target at full precision. The parser is shared with the
// parameter direction, which reduces such a literal to an integer part plus a
// dropped-fraction flag (param_conversions_test.cpp,
// WideDecimalLiteralReportsTruncation); routing that reduction to a double would
// yield about 1.1 here.
TEST_F(GetDataLiveTest, WideDecimalColumnKeepsPrecisionForADoubleTarget) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('1.234567890123456789012345678901234567890'"
                             " AS VARCHAR(64))"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    double out = 0.0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_DOUBLE, &out, sizeof(out), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_NEAR(out, 1.2345678901234567, 1e-15);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(double)), ind);

    SQLCloseCursor(stmt_);
}

// AB#47815: SQLGetData resolves SQL_C_DEFAULT from the column's SQL type
// instead of rejecting it with HYC00, so the same placeholder means the same
// thing whether an application reads a column bound or unbound. msodbcsql keeps
// that invariant by consulting Sql2CDefault on the GetColData path that serves
// both.
//
// Deliberately uses the two types both drivers resolve identically — int to
// SQL_C_SLONG and narrow varchar to SQL_C_CHAR — so this runs on the msodbcsql
// leg too and compares. The deviating types are covered separately below.
TEST_F(GetDataLiveTest, DefaultTargetResolvesFromTheColumnType) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(4242 AS INT), CAST('hello' AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER n = 0;
    SQLLEN nInd = -99;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_DEFAULT, &n, sizeof(n), &nInd),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(4242, n);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), nInd);

    SQLCHAR text[16] = {};
    SQLLEN textInd = -99;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_DEFAULT, text, sizeof(text), &textInd),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("hello", reinterpret_cast<const char*>(text));
    EXPECT_EQ(5, textInd);

    SQLCloseCursor(stmt_);
}

// The resolver's two registered deviations from msodbcsql's Sql2CDefault reach
// SQLGetData as well, because it is the one the bound path already uses: an
// NVARCHAR column resolves to SQL_C_WCHAR and a uniqueidentifier to SQL_C_GUID,
// where msodbcsql resolves both to its ANSI SQL_C_CHAR. See
// mssql-odbc/docs/typed-columnar-fetch-plan.md for the measured msodbcsql
// values these assertions diverge from.
//
// Skipped on the reference leg by construction: asserting a deviation is the
// point, so comparing it would always report a divergence.
TEST_F(GetDataLiveTest, DefaultTargetResolvesWideAndGuidToTypedTargets) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST(N'one' AS NVARCHAR(8)), "
                   "CAST('01020304-0506-0708-090A-0B0C0D0E0F10' AS UNIQUEIDENTIFIER)"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR wide[8] = {};
    SQLLEN wideInd = -99;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_DEFAULT, wide, sizeof(wide), &wideInd),
                  SQL_HANDLE_STMT, stmt_);
    const SQLWCHAR one[] = {'o', 'n', 'e', 0};
    for (int i = 0; i < 4; ++i) {
        EXPECT_EQ(one[i], wide[i]) << "unit " << i;
    }
    // Bytes of UTF-16, which is what makes the wide resolution observable: the
    // narrow default would report 3.
    EXPECT_EQ(static_cast<SQLLEN>(3 * sizeof(SQLWCHAR)), wideInd);

    SQLGUID guid{};
    SQLLEN guidInd = -99;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_DEFAULT, &guid, sizeof(guid), &guidInd),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0x01020304u, guid.Data1);
    EXPECT_EQ(0x0506u, guid.Data2);
    EXPECT_EQ(0x0708u, guid.Data3);
    const unsigned char tail[8] = {0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10};
    EXPECT_EQ(0, std::memcmp(guid.Data4, tail, sizeof(tail)));
    // sizeof(SQLGUID), not the 36 characters msodbcsql's SQL_C_CHAR would give.
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLGUID)), guidInd);

    SQLCloseCursor(stmt_);
}

// A SQL_C_DEFAULT retrieval names no C type, so it carries no width contract:
// resolving a uniqueidentifier column to SQL_C_GUID must not write
// sizeof(SQLGUID) into a buffer the application declared as 4 bytes. The
// placeholder is kept instead and the existing target gate reports HYC00, which
// is what SQLFetchScroll does with the same shape. The backing array is
// deliberately larger than the declared length so a regression shows up as
// bytes written past it rather than as a crash.
//
// msodbcsql resolves to SQL_C_CHAR and truncates inside BufferLength, so this
// asserts a deviation and does not run on the reference leg.
TEST_F(GetDataLiveTest, DefaultTargetTooNarrowForItsFixedTargetIsRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('01020304-0506-0708-090A-0B0C0D0E0F10' AS UNIQUEIDENTIFIER)"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    unsigned char backing[64];
    std::memset(backing, 0xEE, sizeof(backing));
    SQLLEN ind = -99;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_DEFAULT, backing, 4, &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
    for (size_t i = 0; i < sizeof(backing); ++i) {
        EXPECT_EQ(0xEE, backing[i]) << "byte " << i << " was written";
    }

    // The value stays resident, so a retry naming a wide enough target still
    // reads it.
    SQLGUID guid{};
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_DEFAULT, &guid, sizeof(guid), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0x01020304u, guid.Data1);

    SQLCloseCursor(stmt_);
}

// A varbinary column resolves to SQL_C_BINARY, which this path still serves
// only as the zero-length length probe (AB#47239), so a real read keeps
// returning HYC00 through the resolved target. That is the same posture the
// bound path took when it started resolving the placeholder; msodbcsql resolves
// identically and delivers the bytes, so this does not run on the reference
// leg.
//
// Scoped to a non-PLP varbinary(n) deliberately: a VARBINARY(MAX) refuses the
// probe too, which the next test pins.
TEST_F(GetDataLiveTest, DefaultTargetOnABinaryColumnIsStillUnimplemented) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0x4142434445464748 AS VARBINARY(8))"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    // The zero-length probe the resolved SQL_C_BINARY does answer. The buffer
    // is real because the Driver Manager rejects a null TargetValuePtr with
    // HY009 before the call reaches a driver.
    SQLCHAR probeBuf[1] = {};
    SQLLEN probe = -99;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_DEFAULT, probeBuf, 0, &probe),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(8, probe);

    SQLCHAR buf[64] = {};
    SQLLEN ind = -99;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_DEFAULT, buf, sizeof(buf), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    SQLCloseCursor(stmt_);
}

// The PLP half of the same story, and the boundary the test above does not
// cross. A non-NULL VARBINARY(MAX) also resolves to SQL_C_BINARY, but there
// even the zero-length probe is HYC00: stream_active_plp_chunk admits only
// SQL_C_CHAR/SQL_C_WCHAR and rejects everything else before it looks at
// BufferLength, so there is no probe branch to reach. The non-PLP
// VARBINARY(8) above answers that same probe with a length, so the two spell
// out where the difference actually lies.
//
// Asserted as "the defaulted spelling agrees with the explicit one" rather than
// against a hardcoded state, because that agreement is what this PR is
// responsible for; the HYC00 itself is pre-existing and owned by AB#47239,
// which is expected to turn both into real binary delivery. Each spelling gets
// its own result set: a PLP column whose stream was begun and then refused
// cannot be re-read on the same row (the second call reports 07009 from the
// cursor's forward-only guard, not the target gate), so reusing one row would
// measure that instead.
//
// NULL is deliberately not covered here: it never enters the streaming path
// (see NullVarbinaryMaxToBinaryTargetReportsNull).
//
// msodbcsql delivers the bytes on both, so this does not run on the reference
// leg.
TEST_F(GetDataLiveTest, DefaultTargetOnABinaryMaxColumnRefusesEvenTheProbe) {
    SKIP_IF_COMPARING_MSODBCSQL();
    const char* kQuery = "SELECT CAST(0x4142434445464748 AS VARBINARY(MAX))";

    // Resolved from SQL_C_DEFAULT.
    ASSERT_SQL_OK(ExecDirect(kQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLCHAR probeBuf[1] = {};
    SQLLEN probe = -99;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_DEFAULT, probeBuf, 0, &probe));
    const std::string defaulted = ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
    SQLCloseCursor(stmt_);

    // The same probe with the C type named explicitly.
    ASSERT_SQL_OK(ExecDirect(kQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLLEN explicitProbe = -99;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_BINARY, probeBuf, 0, &explicitProbe));
    const std::string named = ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
    SQLCloseCursor(stmt_);

    EXPECT_EQ("HYC00", defaulted) << "a MAX binary column refuses even the probe";
    EXPECT_EQ(named, defaulted)
        << "resolving SQL_C_DEFAULT must give the same answer as naming SQL_C_BINARY";
}

// The placeholder is resolved ahead of the captured/PLP dispatch, so it reaches
// the streaming path too and stays stable across the continuation calls that
// re-enter with the same column. A VARCHAR(MAX) resolves to SQL_C_CHAR in both
// drivers, so this runs on the reference leg and compares.
//
// The size is for chunk count, not to force streaming: unlike a bound fetch —
// where try_read_buffered_column materializes whatever the transport already
// holds, so a small max column never streams — a paused row read pauses on
// ColumnMetadata::is_plp() alone (token_stream.rs, `stop_here && meta.is_plp()`),
// which is a property of the declared type. The buffered fast path this cursor
// does consult bails out first and unconditionally on the same predicate
// (decoder.rs `try_decode_buffered`: `if metadata.is_plp() ... return Ok(None)`),
// so no amount of buffering — and no connection-string packet size — can divert
// a max column away from the streaming path. Every VARCHAR(MAX) therefore takes
// it at any size, which PlpColumnUnsupportedCTypeReturnsHyc00 above demonstrates
// on a three-byte value: it answers HYC00 from the stream's target gate rather
// than converting the text the way a captured value would.
TEST_F(GetDataLiveTest, DefaultTargetStreamsAVarcharMaxAcrossChunks) {
    const size_t kTotal = 9000;
    ASSERT_SQL_OK(ExecDirect("SELECT REPLICATE(CAST('A' AS VARCHAR(MAX)), 9000)"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::string assembled;
    SQLCHAR buf[1024];
    SQLLEN ind = 0;
    SQLRETURN rc;
    int guard = 0;
    do {
        rc = SQLGetData(stmt_, 1, SQL_C_DEFAULT, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) << "unexpected rc=" << rc;
        assembled += std::string(reinterpret_cast<const char*>(buf));
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    } while (rc == SQL_SUCCESS_WITH_INFO);

    EXPECT_GT(guard, 1) << "the value must span more than one call to exercise the continuation";
    EXPECT_EQ(std::string(kTotal, 'A'), assembled);
    EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, SQL_C_DEFAULT, buf, sizeof(buf), &ind));

    SQLCloseCursor(stmt_);
}

// The wide half of the same streaming path: an NVARCHAR(MAX) resolves to
// SQL_C_WCHAR and is delivered as UTF-16 across chunks, which also exercises
// the per-stream decoder that widening builds once at stream start. That
// resolution is the registered deviation — msodbcsql resolves the wide types to
// its ANSI SQL_C_CHAR — so this does not run on the reference leg. See the
// preceding test for why any max column reaches the streaming path here.
TEST_F(GetDataLiveTest, DefaultTargetStreamsAnNvarcharMaxAsWideChunks) {
    SKIP_IF_COMPARING_MSODBCSQL();
    const size_t kTotal = 9000;
    ASSERT_SQL_OK(ExecDirect("SELECT REPLICATE(CAST(N'A' AS NVARCHAR(MAX)), 9000)"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::u16string assembled;
    SQLWCHAR buf[512];
    SQLLEN ind = 0;
    SQLRETURN rc;
    int guard = 0;
    do {
        rc = SQLGetData(stmt_, 1, SQL_C_DEFAULT, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) << "unexpected rc=" << rc;
        for (size_t i = 0; i < sizeof(buf) / sizeof(buf[0]) && buf[i] != 0; ++i) {
            assembled.push_back(static_cast<char16_t>(buf[i]));
        }
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    } while (rc == SQL_SUCCESS_WITH_INFO);

    EXPECT_GT(guard, 1) << "the value must span more than one call to exercise the continuation";
    EXPECT_EQ(kTotal, assembled.size()) << "code units, not the narrow bytes msodbcsql would give";
    EXPECT_EQ(std::u16string(kTotal, u'A'), assembled);

    SQLCloseCursor(stmt_);
}

// The underflow half of the `real` range check, on the fetch direction. Runs
// unskipped on the msodbcsql parity leg, so retail is what pins the answer: a
// `float` column at 1e-40 read into a `SQL_C_FLOAT` buffer is 22003 there too,
// not a silent subnormal write.
TEST_F(GetDataLiveTest, FloatTargetRejectsUnderflowAsWellAsOverflow) {
    struct Case {
        const char* literal;
        const char* what;
    };
    for (const Case& c : {Case{"1e-40", "positive underflow"},
                          Case{"-1e-40", "negative underflow"},
                          Case{"1e40", "positive overflow"},
                          Case{"-1e40", "negative overflow"}}) {
        ASSERT_SQL_OK(ExecDirect(std::string("SELECT CAST(") + c.literal + " AS FLOAT)"),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        float out = 9.0f;
        SQLLEN ind = 0;
        EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_FLOAT, &out, sizeof(out), &ind))
            << c.what;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
        EXPECT_EQ(9.0f, out) << c.what << ": a rejected conversion must not write the buffer";

        SQLCloseCursor(stmt_);
    }

    // Zero is not underflow, and the same value reaches SQL_C_DOUBLE intact -
    // only the 32-bit target narrows.
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0 AS FLOAT), CAST(1e-40 AS FLOAT)"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    float zero = 9.0f;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_FLOAT, &zero, sizeof(zero), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0.0f, zero);

    double wide = 0.0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_DOUBLE, &wide, sizeof(wide), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_DOUBLE_EQ(1e-40, wide);

    SQLCloseCursor(stmt_);
}

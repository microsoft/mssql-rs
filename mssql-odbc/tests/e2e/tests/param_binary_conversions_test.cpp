// Copyright (c) Microsoft Corporation. All rights reserved.
// param_binary_conversions_test.cpp  -  E2E tests for binary parameter
// conversion: SQL_C_BINARY bound against binary, varbinary and image.
//
// Character parameters live in param_char_conversions_test.cpp and the
// cross-family quadrants in param_cross_conversions_test.cpp.
//
// Values are read back through server-side CONVERT(..., 2) as hex text rather
// than SQLGetData with SQL_C_BINARY, which this driver does not implement yet
// (api::get_data reports HYC00). That keeps these tests about the parameter
// direction and lets them run on both drivers.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>
#include <vector>

class BinaryConversionLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN Prepare(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // The buffer is a member so it outlives the SQLExecute that reads it, and it
    // always keeps room for one byte so `data()` is never null even when empty -
    // a null pointer is a different binding case, covered by NullIndicatorBindsNull.
    SQLRETURN BindBytes(const std::vector<SQLCHAR>& value, SQLSMALLINT sql_type,
                        SQLULEN column_size) {
        bytes_ = value;
        bytes_.reserve(bytes_.size() + 1);
        indicator_ = static_cast<SQLLEN>(value.size());
        return SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, sql_type,
                                column_size, 0, bytes_.data(), indicator_, &indicator_);
    }

    // Reads column 1 of the current row as a narrow string.
    std::string GetColumnChar() {
        SQLCHAR buf[256] = {0};
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind),
                      SQL_HANDLE_STMT, stmt_);
        if (ind == SQL_NULL_DATA) {
            return "<null>";
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }

    // Runs the prepared statement with the bound parameter and returns column 1.
    std::string ExecuteAndReadBack() {
        EXPECT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        std::string v = GetColumnChar();
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        return v;
    }

    // Hex of the bytes the server actually received. Style 2 is hex digits with
    // no 0x prefix.
    std::string HexOfBoundParam(const std::vector<SQLCHAR>& value, SQLSMALLINT sql_type,
                                SQLULEN column_size) {
        EXPECT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(128), ?, 2)"), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(BindBytes(value, sql_type, column_size), SQL_HANDLE_STMT, stmt_);
        std::string hex = ExecuteAndReadBack();
        ResetParams();
        return hex;
    }

    // Byte count the server received, as text.
    std::string DataLengthOfBoundParam(const std::vector<SQLCHAR>& value,
                                       SQLSMALLINT sql_type, SQLULEN column_size) {
        EXPECT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(16), DATALENGTH(?))"), SQL_HANDLE_STMT,
                      stmt_);
        EXPECT_SQL_OK(BindBytes(value, sql_type, column_size), SQL_HANDLE_STMT, stmt_);
        std::string len = ExecuteAndReadBack();
        ResetParams();
        return len;
    }

    void ResetParams() {
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }

    std::vector<SQLCHAR> bytes_;
    SQLLEN indicator_ = 0;
};

// The wire type follows ParameterType, not the C type - every binary value used
// to go out as varbinary(max) whatever the application declared.
//
// sql_variant cannot hold a varbinary(max) at all (server error 529), so the max
// spelling is asserted by length in UnboundedColumnSizeCarriesOversizedValues
// rather than here.
TEST_F(BinaryConversionLiveTest, BinaryParamDeclaresTheParameterType) {
    struct Case {
        SQLSMALLINT sql_type;
        const char* base_type;
    };
    for (const Case& c : {Case{SQL_BINARY, "binary"}, Case{SQL_VARBINARY, "varbinary"}}) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT),"
                              " 'BaseType') AS VARCHAR(32))"),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindBytes({1, 2, 3, 4}, c.sql_type, 4), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.base_type, ExecuteAndReadBack()) << "sql type " << c.sql_type;
        ResetParams();
    }
}

// The declared length reaches the server, not just the type name.
TEST_F(BinaryConversionLiveTest, DeclaredLengthReachesTheServer) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT),"
                          " 'MaxLength') AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindBytes({1, 2}, SQL_VARBINARY, 40), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("40", ExecuteAndReadBack());
}

TEST_F(BinaryConversionLiveTest, BinaryParamRoundTrips) {
    EXPECT_EQ("01FF7E80", HexOfBoundParam({0x01, 0xFF, 0x7E, 0x80}, SQL_VARBINARY, 8));
}

// A zero inside the value is data, not padding, so it survives.
TEST_F(BinaryConversionLiveTest, InteriorZerosAreNotPadding) {
    EXPECT_EQ("01000002", HexOfBoundParam({0x01, 0x00, 0x00, 0x02}, SQL_VARBINARY, 4));
}

// Overflow that is entirely 0x00 is padding and is dropped silently, the way
// overflowing blanks are on the character path - CheckTrailingZeros driving
// sqlcfunc.cpp:2611.
TEST_F(BinaryConversionLiveTest, AllZeroOverflowIsTrimmedSilently) {
    EXPECT_EQ("ABCD", HexOfBoundParam({0xAB, 0xCD, 0x00, 0x00, 0x00}, SQL_VARBINARY, 2));
}

// Anything else past the declared length is data, so it is 22001 rather than a
// silent loss.
TEST_F(BinaryConversionLiveTest, OverflowCarryingDataIs22001) {
    for (SQLSMALLINT sql_type : {SQL_BINARY, SQL_VARBINARY, SQL_LONGVARBINARY}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindBytes({1, 2, 3, 4, 5}, sql_type, 4), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "sql type " << sql_type;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001") << "sql type " << sql_type;
        ResetParams();
    }
}

// The scan stops at the first non-pad byte wherever it sits, so a mostly-zero
// overflow still errors.
TEST_F(BinaryConversionLiveTest, PartiallyZeroOverflowIs22001) {
    const std::vector<std::vector<SQLCHAR>> cases = {
        {1, 2, 3, 9, 0, 0},  // non-zero first
        {1, 2, 3, 0, 0, 9},  // non-zero last
        {1, 2, 3, 0, 9, 0},  // non-zero in the middle
    };
    for (const auto& value : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindBytes(value, SQL_VARBINARY, 3), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
        ResetParams();
    }
}

// An unbounded ColumnSize selects varbinary(max), so nothing is length-checked
// and a value past the 8000-byte non-max bound reaches the server whole.
TEST_F(BinaryConversionLiveTest, UnboundedColumnSizeCarriesOversizedValues) {
    std::vector<SQLCHAR> value(9000, 0xEE);
    EXPECT_EQ("9000", DataLengthOfBoundParam(value, SQL_VARBINARY, 0));
}

// binary(n) is fixed width, so a short value is zero-padded out to n.
TEST_F(BinaryConversionLiveTest, FixedBinaryIsZeroPaddedToTheDeclaredLength) {
    EXPECT_EQ("AABB0000", HexOfBoundParam({0xAA, 0xBB}, SQL_BINARY, 4));
}

// A zero-length value is empty, not NULL - SQL_NULL_DATA is the only way to
// bind a NULL.
TEST_F(BinaryConversionLiveTest, EmptyBinaryParamIsNotNull) {
    EXPECT_EQ("0", DataLengthOfBoundParam({}, SQL_VARBINARY, 4));
}

TEST_F(BinaryConversionLiveTest, NullIndicatorBindsNull) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(16), DATALENGTH(?))"), SQL_HANDLE_STMT,
                  stmt_);
    indicator_ = SQL_NULL_DATA;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_VARBINARY, 4,
                                   0, nullptr, 0, &indicator_),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("<null>", ExecuteAndReadBack());
}

// binary(0) is not legal T-SQL and has no max spelling, so ColumnSize 0 is
// HY104 - rejected at bind by both drivers, msodbcsql through CheckSqlPrec
// (sqlcdesc.cpp:11783, which groups SQL_BINARY with SQL_CHAR).
TEST_F(BinaryConversionLiveTest, ZeroColumnSizeOnFixedBinaryIsRejected) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, BindBytes({1, 2}, SQL_BINARY, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY104");
}

// varbinary reads 0 as the max spelling, so the same ColumnSize is accepted
// there - the split msodbcsql draws at sqlcdesc.cpp:11748.
TEST_F(BinaryConversionLiveTest, ZeroColumnSizeOnVarbinaryIsTheMaxSpelling) {
    EXPECT_EQ("0102", HexOfBoundParam({1, 2}, SQL_VARBINARY, 0));
}

// image carries its value like the other two. Asserted by length rather than by
// CONVERT: msodbcsql declares a real image, and the server refuses an explicit
// image -> varchar conversion (error 529). This driver substitutes
// varbinary(max) for image (AB#47592), so only the length assertion is common
// to both.
TEST_F(BinaryConversionLiveTest, ImageParamCarriesItsValue) {
    EXPECT_EQ("4", DataLengthOfBoundParam({0xDE, 0xAD, 0xBE, 0xEF}, SQL_LONGVARBINARY, 16));
}

// image is the one binding whose declared type is unbounded while its enforced
// bound is not, so both halves of the overflow rule need asserting on it
// separately from binary and varbinary.
TEST_F(BinaryConversionLiveTest, ImageParamIsStillBoundedByColumnSize) {
    // Zero overflow trims rather than reaching the server.
    EXPECT_EQ("2", DataLengthOfBoundParam({1, 2, 0, 0, 0}, SQL_LONGVARBINARY, 2));

    // Anything else is 22001, covered for all three types by
    // OverflowCarryingDataIs22001; repeated here against the max declaration.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindBytes({1, 2, 3}, SQL_LONGVARBINARY, 2), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ResetParams();
}

// And past the 8000-byte varbinary(n) ceiling image is genuinely unbounded, so a
// large ColumnSize carries a large value rather than capping it.
TEST_F(BinaryConversionLiveTest, ImageParamCarriesValuesPastTheNonMaxCeiling) {
    std::vector<SQLCHAR> value(9000, 0xEE);
    EXPECT_EQ("9000", DataLengthOfBoundParam(value, SQL_LONGVARBINARY, 20000));
}

// 8000 is the last accepted ColumnSize for varbinary; one past it is HY104 at
// bind on both drivers, so `variable_length`'s widen-to-max branch is defence in
// depth rather than a reachable path. 0 remains the way to ask for max.
TEST_F(BinaryConversionLiveTest, ColumnSizeAtTheNonMaxBoundary) {
    EXPECT_EQ("0102", HexOfBoundParam({1, 2}, SQL_VARBINARY, 8000));

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, BindBytes({1, 2}, SQL_VARBINARY, 8001));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY104");
    ResetParams();

    // Over 8000 bytes therefore needs the max spelling, not a bigger ColumnSize.
    std::vector<SQLCHAR> value(8500, 0x5A);
    EXPECT_EQ("8500", DataLengthOfBoundParam(value, SQL_VARBINARY, 0));
}

// The buffer stays bound and only the indicator changes - the shape msodbcsql's
// own regression for a NULL binary parameter uses (BindPara.CPP:3102). A NULL
// is typed from ParameterType alone, so it must still declare varbinary(4).
TEST_F(BinaryConversionLiveTest, NullIndicatorOnABoundBufferSendsNull) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(16), DATALENGTH(?))"), SQL_HANDLE_STMT,
                  stmt_);
    bytes_ = {0xAA, 0xBB};
    indicator_ = static_cast<SQLLEN>(bytes_.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_VARBINARY, 4,
                                   0, bytes_.data(), indicator_, &indicator_),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", ExecuteAndReadBack());

    // Same binding, indicator flipped to NULL.
    indicator_ = SQL_NULL_DATA;
    EXPECT_EQ("<null>", ExecuteAndReadBack());

    // And back to a value, to prove the NULL did not latch.
    indicator_ = static_cast<SQLLEN>(bytes_.size());
    EXPECT_EQ("2", ExecuteAndReadBack());
}

// Re-executing with a longer value against the same declaration must re-run the
// overflow check rather than reuse the previous verdict.
TEST_F(BinaryConversionLiveTest, RebindingChangesTheTruncationVerdict) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindBytes({1, 2}, SQL_VARBINARY, 2), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    ResetParams();

    // Now one byte of real data past the same declared length.
    ASSERT_SQL_OK(BindBytes({1, 2, 3}, SQL_VARBINARY, 2), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ResetParams();

    // And an all-zero overflow against the same declaration still trims.
    ASSERT_SQL_OK(BindBytes({1, 2, 0}, SQL_VARBINARY, 2), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Two binary parameters with different declarations in one statement, so a
// per-parameter length cannot be leaking from one slot into the other.
TEST_F(BinaryConversionLiveTest, TwoBinaryParamsKeepSeparateDeclarations) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 2)"
                          " + '|' + CONVERT(VARCHAR(32), ?, 2)"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> first = {0x11, 0x22, 0x00};
    std::vector<SQLCHAR> second = {0x33, 0x44, 0x55, 0x66};
    SQLLEN first_ind = static_cast<SQLLEN>(first.size());
    SQLLEN second_ind = static_cast<SQLLEN>(second.size());

    // The first trims its zero overflow against binary(2); the second fits
    // varbinary(4) whole.
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_BINARY, 2, 0,
                                   first.data(), first_ind, &first_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_VARBINARY, 4,
                                   0, second.data(), second_ind, &second_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1122|33445566", ExecuteAndReadBack());
}

// Divergence: ColumnSize does not bound a data-at-execution value here, and it
// does in msodbcsql. dae_placeholder_type declares varbinary(max) from the C
// type alone, so a streamed value is never length-checked; msodbcsql applies the
// same cbMaxPrec bound and CheckTrailingZeros rule to each SQLPutData chunk,
// accumulating cbDataSentToServer across calls (sqlccmd.cpp:11192-11218), and
// answers 22001 for the binding below. Measured on retail 18.6.2.1.
//
// Not narrowed to binary: the character DAE path has the same gap, and closing
// either needs ColumnSize threaded into the placeholder type plus a running
// total in SQLPutData. Tracked as AB#47775; skipped here rather than left
// asserting the wrong direction.
TEST_F(BinaryConversionLiveTest, DataAtExecutionIgnoresColumnSize) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 2)"), SQL_HANDLE_STMT, stmt_);

    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    // ColumnSize 2 against a 4-byte streamed value: no 22001.
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_VARBINARY, 2,
                                   0, &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    const SQLCHAR chunk[] = {0xAA, 0xBB, 0xCC, 0xDD};
    ASSERT_SQL_OK(SQLPutData(stmt_, const_cast<SQLCHAR*>(chunk), sizeof(chunk)),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLParamData(stmt_, &value_ptr), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("AABBCCDD", GetColumnChar());
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

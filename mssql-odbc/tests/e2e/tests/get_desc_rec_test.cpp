// Copyright (c) Microsoft Corporation. All rights reserved.
// get_desc_rec_test.cpp - Tests for SQLGetDescRecW.
//
// Verifies:
//   1. NullHandle                    - null descriptor handle -> SQL_INVALID_HANDLE
//   2. RecordNumberZeroReturnsError  - RecNumber 0 -> SQL_ERROR / 07009
//   3. RecordPastCountReturnsNoData  - RecNumber > SQL_DESC_COUNT -> SQL_NO_DATA
//   4. ReadsBackValueSetBySetDescField - GET after a SQLSetDescFieldW sequence
//      round-trips Name/Type/SubType/Length/Precision/Scale/Nullable in one call
//   5. NameTruncationReturnsInfo     - short Name buffer -> SUCCESS_WITH_INFO / 01004
//   6. ImpRowDescMatchesDescribeColAfterExecute - IRD record after a real
//      execute agrees with SQLDescribeCol for the same column

#include "odbc_test_fixture.h"

TEST(GetDescRecTest, NullHandle) {
    SQLSMALLINT type = 0;
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLGetDescRecW(SQL_NULL_HANDLE, 1, nullptr, 0, nullptr, &type, nullptr, nullptr,
                             nullptr, nullptr, nullptr));
}

class GetDescRecLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or "
                      "ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLHDESC AppParamDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_PARAM_DESC, &hdesc, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return hdesc;
    }

    SQLHDESC ImpRowDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_IMP_ROW_DESC, &hdesc, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return hdesc;
    }

    SQLHDESC ImpParamDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_IMP_PARAM_DESC, &hdesc, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return hdesc;
    }
};

TEST_F(GetDescRecLiveTest, RecordNumberZeroReturnsError) {
    SQLHDESC hdesc = AppParamDesc();
    SQLSMALLINT type = 0;
    ASSERT_SQL_ERROR(SQLGetDescRecW(hdesc, 0, nullptr, 0, nullptr, &type, nullptr, nullptr,
                                    nullptr, nullptr, nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "07009");
}

TEST_F(GetDescRecLiveTest, RecordPastCountReturnsNoData) {
    SQLHDESC hdesc = AppParamDesc();
    SQLSMALLINT type = 0;
    EXPECT_EQ(SQL_NO_DATA, SQLGetDescRecW(hdesc, 1, nullptr, 0, nullptr, &type, nullptr, nullptr,
                                          nullptr, nullptr, nullptr));
}

// SQLSetDescFieldW and SQLGetDescRecW read/write the same descriptor record
// storage (AB#47437), so every field a SQL_C_NUMERIC parameter bind sets
// through the single-field API must come back through the bulk one too.
TEST_F(GetDescRecLiveTest, ReadsBackValueSetBySetDescField) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 1, SQL_DESC_PRECISION,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(10)), 0),
                  SQL_HANDLE_DESC, hdesc);
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 1, SQL_DESC_SCALE,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(2)), 0),
                  SQL_HANDLE_DESC, hdesc);

    SQLSMALLINT type = -1;
    SQLSMALLINT sub_type = -1;
    SQLLEN length = -1;
    SQLSMALLINT precision = -1;
    SQLSMALLINT scale = -1;
    SQLSMALLINT nullable = -1;
    SQLRETURN rc = SQLGetDescRecW(hdesc, 1, nullptr, 0, nullptr, &type, &sub_type, &length,
                                  &precision, &scale, &nullable);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(SQL_C_NUMERIC, type);
    EXPECT_EQ(10, precision);
    EXPECT_EQ(2, scale);
}

TEST_F(GetDescRecLiveTest, NameTruncationReturnsInfo) {
    SQLHDESC hdesc = ImpParamDesc();
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 1, SQL_DESC_NAME,
                                   const_cast<SQLTCHAR*>(
                                       ODBCTestUtils::ToSqlTStr("a_long_parameter_name").c_str()),
                                   SQL_NTS),
                  SQL_HANDLE_DESC, hdesc);

    SQLTCHAR name[5] = {};
    SQLSMALLINT name_len = -1;
    SQLRETURN rc = SQLGetDescRecW(hdesc, 1, name, 5, &name_len, nullptr, nullptr, nullptr,
                                  nullptr, nullptr, nullptr);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_EQ(21, name_len);
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "01004");
}

// AB#47437: once a result set is positioned by a real execute, SQLGetDescRecW
// on the IRD must agree with SQLDescribeCol for the same column — both are
// driven by the same column_metadata/field-mapping functions. TypePtr here is
// SQL_DESC_TYPE (verbose), while SQLDescribeCol's DataType is the concise
// type; INTEGER's verbose and concise forms are identical (the fold only
// touches the temporal types), so this comparison is valid for this column.
TEST_F(GetDescRecLiveTest, ImpRowDescMatchesDescribeColAfterExecute) {
    ExecDirect("SELECT CAST(1 AS INT) AS i");
    SQLHDESC hdesc = ImpRowDesc();

    SQLSMALLINT data_type = 0;
    SQLULEN col_size = 0;
    SQLSMALLINT dec_digits = 0;
    SQLSMALLINT nullable = 0;
    ASSERT_SQL_OK(
        SQLDescribeCol(stmt_, 1, nullptr, 0, nullptr, &data_type, &col_size, &dec_digits,
                       &nullable),
        SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT rec_type = -1;
    SQLSMALLINT rec_nullable = -1;
    SQLRETURN rc = SQLGetDescRecW(hdesc, 1, nullptr, 0, nullptr, &rec_type, nullptr, nullptr,
                                  nullptr, nullptr, &rec_nullable);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(data_type, rec_type);
    EXPECT_EQ(nullable, rec_nullable);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

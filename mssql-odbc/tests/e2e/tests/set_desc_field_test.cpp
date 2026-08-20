// Copyright (c) Microsoft Corporation. All rights reserved.
// set_desc_field_test.cpp - Tests for SQLSetDescFieldW.
//
// Verifies:
//   1. NullHandle                    - null descriptor handle -> SQL_INVALID_HANDLE
//   2. MssqlPythonNumericParameterSequence - the exact sequence mssql-python's
//      ddbc_bindings.cpp runs for a SQL_C_NUMERIC input parameter: this is
//      the regression anchor for AB#47297.
//   3. CountGrowsAndShrinks          - SQL_DESC_COUNT write grows/shrinks the record plex
//   4. IrdRejectsFieldWrite          - any field write on the IRD -> SQL_ERROR / HY016
//   5. IrdAllowsRowsProcessedPtr     - IRD's two exempted pointer fields remain writable
//   6. InvalidCTypeOnApdReturnsError - unrecognized ValueType on APD -> SQL_ERROR / HY003
//   7. NumericPrecisionOutOfRangeReturnsError - SQL_C_NUMERIC precision outside
//      1..=38 -> SQL_ERROR / HY094

#include "odbc_test_fixture.h"

TEST(SetDescFieldTest, NullHandle) {
    SQLLEN value = SQL_C_NUMERIC;
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLSetDescFieldW(SQL_NULL_HANDLE, 1, SQL_DESC_TYPE,
                                reinterpret_cast<SQLPOINTER>(value), 0));
}

class SetDescFieldLiveTest : public ODBCTest {
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

    SQLSMALLINT GetSmallInt(SQLHDESC hdesc, SQLSMALLINT record, SQLSMALLINT field) {
        SQLSMALLINT value = -1;
        SQLRETURN rc = SQLGetDescFieldW(hdesc, record, field, &value, sizeof(value), nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
        return value;
    }
};

// Mirrors mssql-python's ddbc_bindings.cpp BindParameters (lines 1003-1048):
// SQLBindParameter(..., SQL_C_NUMERIC, ...) already succeeded by the time this
// runs; the driver then binds SQLGetStmtAttr(APP_PARAM_DESC) and four
// SQLSetDescField calls on record 1, in this exact order.
TEST_F(SetDescFieldLiveTest, MssqlPythonNumericParameterSequence) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_NE(hdesc, static_cast<SQLHDESC>(SQL_NULL_HDESC));

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

    SQL_NUMERIC_STRUCT numeric_buf = {};
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 1, SQL_DESC_DATA_PTR, &numeric_buf, 0),
                  SQL_HANDLE_DESC, hdesc);

    EXPECT_EQ(SQL_C_NUMERIC, GetSmallInt(hdesc, 1, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(10, GetSmallInt(hdesc, 1, SQL_DESC_PRECISION));
    EXPECT_EQ(2, GetSmallInt(hdesc, 1, SQL_DESC_SCALE));

    SQLPOINTER data_ptr = nullptr;
    ASSERT_SQL_OK(
        SQLGetDescFieldW(hdesc, 1, SQL_DESC_DATA_PTR, &data_ptr, sizeof(data_ptr), nullptr),
        SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(static_cast<void*>(&numeric_buf), data_ptr);
}

TEST_F(SetDescFieldLiveTest, CountGrowsAndShrinks) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 0, SQL_DESC_COUNT,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(3)), 0),
                  SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(3, GetSmallInt(hdesc, 0, SQL_DESC_COUNT));

    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 0, SQL_DESC_COUNT,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(1)), 0),
                  SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(1, GetSmallInt(hdesc, 0, SQL_DESC_COUNT));
    EXPECT_EQ(SQL_NO_DATA, SQLGetDescFieldW(hdesc, 2, SQL_DESC_TYPE, nullptr, 0, nullptr));
}

TEST_F(SetDescFieldLiveTest, IrdRejectsFieldWrite) {
    SQLHDESC hdesc = ImpRowDesc();
    ASSERT_SQL_ERROR(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_INTEGER)), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY016");
}

TEST_F(SetDescFieldLiveTest, IrdAllowsRowsProcessedPtr) {
    SQLHDESC hdesc = ImpRowDesc();
    SQLULEN rows = 0;
    EXPECT_SQL_OK(SQLSetDescFieldW(hdesc, 0, SQL_DESC_ROWS_PROCESSED_PTR, &rows, 0),
                  SQL_HANDLE_DESC, hdesc);
}

TEST_F(SetDescFieldLiveTest, InvalidCTypeOnApdReturnsError) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_ERROR(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(9999)), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY003");
}

TEST_F(SetDescFieldLiveTest, NumericPrecisionOutOfRangeReturnsError) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);

    ASSERT_SQL_ERROR(SQLSetDescFieldW(hdesc, 1, SQL_DESC_PRECISION,
                                      reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(39)), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY094");
}

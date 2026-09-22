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
//   8. ChangingTypeToNumericThroughArdUnbindsData - the ARD half of
//      ChangingTypeToNumericResetsDefaultsAndUnbindsData: retyping an
//      already-bound fetch column through the implicit ARD unbinds it too.
//   9. ChangingTypeToNumericThroughExplicitDescUsedAsArdUnbindsData - same,
//      but through an explicitly allocated descriptor associated as the ARD
//      via SQL_ATTR_APP_ROW_DESC (PR #521 review thread PRRT_kwDOPLFXwM6gzXDQ).

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

    SQLHDESC AppRowDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, &hdesc, 0, nullptr),
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

TEST_F(SetDescFieldLiveTest, ChangingTypeToNumericResetsDefaultsAndUnbindsData) {
    SQLHDESC hdesc = AppParamDesc();
    SQLINTEGER old_value = 7;
    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_LONG)), 0),
        SQL_HANDLE_DESC, hdesc);
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 1, SQL_DESC_DATA_PTR, &old_value, 0),
                  SQL_HANDLE_DESC, hdesc);

    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);

    SQLPOINTER data_ptr = &old_value;
    ASSERT_SQL_OK(
        SQLGetDescFieldW(hdesc, 1, SQL_DESC_DATA_PTR, &data_ptr, sizeof(data_ptr), nullptr),
        SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(nullptr, data_ptr);
    EXPECT_EQ(38, GetSmallInt(hdesc, 1, SQL_DESC_PRECISION));
    EXPECT_EQ(0, GetSmallInt(hdesc, 1, SQL_DESC_SCALE));
}

// The ARD half of ChangingTypeToNumericResetsDefaultsAndUnbindsData:
// msodbcsql's SQL_DESC_TYPE/CONCISE_TYPE handler resets `rgbValue` to
// `NOT_BOUND` and calls `SetTypeDefaults` for every `ObjectType ==
// SQL_HANDLE_AD` record (sqlcdesc.cpp:1736-1740), and that one object type
// covers the ARD exactly as it does the APD (sqlsrv.h:542) -- retail never
// special-cases APD over ARD here. Retyping an already-bound fetch column
// through the ARD unbinds it too, and the fetch that follows must not write
// through the stale pointer.
TEST_F(SetDescFieldLiveTest, ChangingTypeToNumericThroughArdUnbindsData) {
    ExecDirect("SELECT 1 AS v");

    SQLINTEGER old_value = 7;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_LONG, &old_value, sizeof(old_value), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    SQLHDESC hdesc = AppRowDesc();
    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);

    SQLPOINTER data_ptr = &old_value;
    ASSERT_SQL_OK(
        SQLGetDescFieldW(hdesc, 1, SQL_DESC_DATA_PTR, &data_ptr, sizeof(data_ptr), nullptr),
        SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(nullptr, data_ptr);
    EXPECT_EQ(38, GetSmallInt(hdesc, 1, SQL_DESC_PRECISION));
    EXPECT_EQ(0, GetSmallInt(hdesc, 1, SQL_DESC_SCALE));

    // The unbound column no longer writes through the stale pointer.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(7, old_value) << "an unbound column must not write through the stale pointer";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Same as above, but through an explicitly allocated descriptor associated as
// the ARD via SQL_ATTR_APP_ROW_DESC: `DescKind::Ad` is the kind such a
// descriptor carries regardless of which role (ARD or APD) it is currently
// plugged into (PR #521 review thread PRRT_kwDOPLFXwM6gzXDQ).
TEST_F(SetDescFieldLiveTest, ChangingTypeToNumericThroughExplicitDescUsedAsArdUnbindsData) {
    ExecDirect("SELECT 1 AS v");

    SQLHDESC hdesc = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DESC, dbc_, &hdesc), SQL_HANDLE_DBC, dbc_);
    ASSERT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, hdesc, 0), SQL_HANDLE_STMT,
                  stmt_);

    SQLINTEGER old_value = 7;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_LONG, &old_value, sizeof(old_value), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);

    SQLPOINTER data_ptr = &old_value;
    ASSERT_SQL_OK(
        SQLGetDescFieldW(hdesc, 1, SQL_DESC_DATA_PTR, &data_ptr, sizeof(data_ptr), nullptr),
        SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(nullptr, data_ptr);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(7, old_value) << "an unbound column must not write through the stale pointer";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);

    ASSERT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, SQL_NULL_HDESC, 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLFreeHandle(SQL_HANDLE_DESC, hdesc);
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
    // msodbcsql's SQLSetDescFieldW rejects an unrecognized APD SQL_DESC_TYPE
    // with HY021 from CheckADDescRecConsistency, not HY003.
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_ERROR(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(9999)), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY003");
}

TEST_F(SetDescFieldLiveTest, NumericPrecisionOutOfRangeReturnsError) {
    // msodbcsql defers SQL_C_NUMERIC precision consistency until the binding is
    // complete (CheckADDescRecConsistency at SQL_DESC_DATA_PTR / bind time), so
    // this eager HY094 assertion is Rust-driver-specific.
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);

    ASSERT_SQL_ERROR(SQLSetDescFieldW(hdesc, 1, SQL_DESC_PRECISION,
                                      reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(39)), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY094");
}

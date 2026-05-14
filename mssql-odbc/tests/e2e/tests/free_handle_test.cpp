// Copyright (c) Microsoft Corporation. All rights reserved.
// free_handle_test.cpp  –  Tests for SQLFreeHandle edge cases
//
// Variations:
//   1. FreeNullHandle        – NULL ENV/DBC handles → SQL_INVALID_HANDLE
//   2. FreeEnvWithOutstandingDbc – ENV fails with live DBC, succeeds after DBC freed
//   3. FreeEnvAfterAllDbcFreed – ENV succeeds once all DBCs are freed
//   4. FreeInvalidHandleType  – Invalid handle type → failure

#include "odbc_test_fixture.h"

class FreeHandleTest : public ::testing::Test {
protected:
    SQLHENV henv_ = SQL_NULL_HENV;

    void SetUp() override {
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv_);
        ASSERT_TRUE(SQL_SUCCEEDED(rc))
            << "SQLAllocHandle(ENV) failed, rc=" << rc;

        rc = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                           reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    }

    void TearDown() override {
        if (henv_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, henv_);
            henv_ = SQL_NULL_HENV;
        }
    }
};

// -------------------------------------------------------------------
// Variation 1 – FreeNullHandle
// Freeing a NULL handle of any type should return SQL_INVALID_HANDLE.
// -------------------------------------------------------------------
TEST_F(FreeHandleTest, FreeNullHandleReturnsInvalidHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE, SQLFreeHandle(SQL_HANDLE_ENV, SQL_NULL_HENV));
    EXPECT_EQ(SQL_INVALID_HANDLE, SQLFreeHandle(SQL_HANDLE_DBC, SQL_NULL_HDBC));
}

// -------------------------------------------------------------------
// Variation 2 – FreeEnvWithOutstandingDbc
// Freeing an ENV while a DBC is still allocated must fail.
// Once the DBC is freed, the ENV free should succeed.
// -------------------------------------------------------------------
TEST_F(FreeHandleTest, FreeEnvWithOutstandingDbcFails) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

    // Try to free ENV while DBC is outstanding — must fail.
    EXPECT_SQL_ERROR(SQLFreeHandle(SQL_HANDLE_ENV, henv_));

    // Free the DBC, then ENV should succeed.
    rc = SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    rc = SQLFreeHandle(SQL_HANDLE_ENV, henv_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    henv_ = SQL_NULL_HENV;
}

// -------------------------------------------------------------------
// Variation 3 – FreeEnvAfterAllDbcFreed
// ENV should succeed once all child DBCs are freed.
// -------------------------------------------------------------------
TEST_F(FreeHandleTest, FreeEnvAfterAllDbcFreedSucceeds) {
    SQLHDBC hdbc1 = SQL_NULL_HDBC;
    SQLHDBC hdbc2 = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc1);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc2);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

    // ENV should fail while both DBCs are live.
    EXPECT_SQL_ERROR(SQLFreeHandle(SQL_HANDLE_ENV, henv_));

    // Free one — ENV should still fail.
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc1);
    EXPECT_SQL_ERROR(SQLFreeHandle(SQL_HANDLE_ENV, henv_));

    // Free the last — now ENV should succeed.
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc2);
    rc = SQLFreeHandle(SQL_HANDLE_ENV, henv_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    henv_ = SQL_NULL_HENV;
}

// -------------------------------------------------------------------
// Variation 4 – FreeInvalidHandleType
// Passing an invalid handle type should fail.
// -------------------------------------------------------------------
TEST_F(FreeHandleTest, FreeInvalidHandleTypeFails) {
    ASSERT_FALSE(SQL_SUCCEEDED(SQLFreeHandle(99, henv_)));
}

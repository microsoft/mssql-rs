// Copyright (c) Microsoft Corporation. All rights reserved.
// alloc_dbc_test.cpp  –  Tests for SQLAllocHandle(SQL_HANDLE_DBC, ...)
//
// Mirrors the alloc_env_test.cpp structure for DBC handle allocation.
//
// Variations:
//   1. AllocDbcNull     – NULL output pointer → SQL_ERROR
//   2. AllocDbcValid    – Allocate and free a single HDBC
//   3. AllocDbcMult     – Allocate two DBCs on the same ENV
//   4. AllocDbcDup      – Re-allocate over an existing HDBC variable (no crash)
//   5. AllocDbcNullEnv  – NULL parent ENV → SQL_INVALID_HANDLE
//   6. AllocDbcIndepEnv – DBCs from independent ENVs are distinct

#include "odbc_test_fixture.h"

// ===================================================================
// AllocDbcTest — manages its own ENV so we control the full lifecycle.
// ===================================================================
class AllocDbcTest : public ::testing::Test {
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
// Variation 1 – AllocDbcNull
// Passing NULL as the output handle pointer should return SQL_ERROR.
// -------------------------------------------------------------------
TEST_F(AllocDbcTest, NullOutputPointerReturnsError) {
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
}

// -------------------------------------------------------------------
// Variation 2 – AllocDbcValid
// A simple alloc → free cycle should succeed.
// -------------------------------------------------------------------
TEST_F(AllocDbcTest, AllocAndFreeSucceeds) {
    SQLHDBC hdbc = SQL_NULL_HDBC;

    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    EXPECT_TRUE(hdbc != SQL_NULL_HDBC);

    rc = SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);
}

// -------------------------------------------------------------------
// Variation 3 – AllocDbcMult
// Allocating two DBCs on the same ENV should both succeed.
// -------------------------------------------------------------------
TEST_F(AllocDbcTest, MultipleDbcHandlesOnSameEnv) {
    SQLHDBC hdbc1 = SQL_NULL_HDBC;
    SQLHDBC hdbc2 = SQL_NULL_HDBC;

    SQLRETURN rc1 = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc1);
    ASSERT_SQL_OK(rc1, SQL_HANDLE_ENV, henv_);
    EXPECT_TRUE(hdbc1 != SQL_NULL_HDBC);

    SQLRETURN rc2 = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc2);
    ASSERT_SQL_OK(rc2, SQL_HANDLE_ENV, henv_);
    EXPECT_TRUE(hdbc2 != SQL_NULL_HDBC);

    EXPECT_NE(hdbc1, hdbc2);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc2);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc1);
}

// -------------------------------------------------------------------
// Variation 4 – AllocDbcDup
// Re-allocating into the same variable (overwriting the pointer)
// should not crash. The original handle is still valid and must be
// freed separately.
// -------------------------------------------------------------------
TEST_F(AllocDbcTest, DuplicateAllocDoesNotCrash) {
    SQLHDBC hdbc = SQL_NULL_HDBC;

    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

    SQLHDBC hdbcOriginal = hdbc;

    rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    EXPECT_TRUE(hdbc != SQL_NULL_HDBC);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbcOriginal);
}

// -------------------------------------------------------------------
// Variation 5 – AllocDbcNullEnv
// Passing SQL_NULL_HANDLE as the parent ENV should fail.
// -------------------------------------------------------------------
TEST_F(AllocDbcTest, NullEnvReturnsInvalidHandle) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, SQL_NULL_HANDLE, &hdbc);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// -------------------------------------------------------------------
// Variation 6 – AllocDbcIndepEnv
// DBCs allocated from independent ENVs should be distinct and
// independently freeable.
// -------------------------------------------------------------------
TEST_F(AllocDbcTest, DbcFromIndependentEnvsAreDistinct) {
    // Allocate a second ENV.
    SQLHENV henv2 = SQL_NULL_HENV;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv2);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));
    rc = SQLSetEnvAttr(henv2, SQL_ATTR_ODBC_VERSION,
                       reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv2);

    SQLHDBC hdbc1 = SQL_NULL_HDBC;
    rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc1);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

    SQLHDBC hdbc2 = SQL_NULL_HDBC;
    rc = SQLAllocHandle(SQL_HANDLE_DBC, henv2, &hdbc2);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv2);

    EXPECT_NE(hdbc1, hdbc2);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc2);
    SQLFreeHandle(SQL_HANDLE_ENV, henv2);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc1);
}

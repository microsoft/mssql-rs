// Copyright (c) Microsoft Corporation. All rights reserved.
// alloc_env_test.cpp  –  Tests for SQLAllocHandle(SQL_HANDLE_ENV, ...)
//
// Original LTM variations:
//   1. AllocEnvNull         – NULL output pointer → SQL_ERROR
//   2. AllocEnvValid        – Allocate and free a single HENV
//   3. AllocEnvMult         – Allocate two independent HENVs
//   4. AllocEnvDup          – Re-allocate over an existing HENV variable (no crash)
//   4b. AllocEnvNonNullInput – Non-null input_handle → SQL_INVALID_HANDLE
//   5. AllocEnvStates       – Allocate HENVs while connection/statement are active
//   6. AllocInvalidType     – Invalid handle type → failure (DM-specific error)

#include "odbc_test_fixture.h"

// ===================================================================
// AllocEnvTest — does NOT use ODBCTest fixture because we need to
// control HENV allocation ourselves (that's what we're testing).
// ===================================================================
class AllocEnvTest : public ::testing::Test {};

// -------------------------------------------------------------------
// Variation 1 – AllocEnvNull
// Passing NULL as the output handle pointer should return SQL_ERROR.
// -------------------------------------------------------------------
TEST_F(AllocEnvTest, NullOutputPointerReturnsError) {
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
}

// -------------------------------------------------------------------
// Variation 2 – AllocEnvValid
// A simple alloc → free cycle should succeed.
// -------------------------------------------------------------------
TEST_F(AllocEnvTest, AllocAndFreeSucceeds) {
    SQLHENV henv = SQL_NULL_HENV;

    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv);
    ASSERT_TRUE(SQL_SUCCEEDED(rc))
        << "SQLAllocHandle(ENV) failed, rc=" << rc;
    EXPECT_TRUE(henv != SQL_NULL_HENV);

    rc = SQLFreeHandle(SQL_HANDLE_ENV, henv);
    EXPECT_TRUE(SQL_SUCCEEDED(rc))
        << "SQLFreeHandle(ENV) failed, rc=" << rc;
}

// -------------------------------------------------------------------
// Variation 3 – AllocEnvMult
// Allocating two independent HENVs should both succeed.
// -------------------------------------------------------------------
TEST_F(AllocEnvTest, MultipleEnvHandlesSucceed) {
    SQLHENV henv1 = SQL_NULL_HENV;
    SQLHENV henv2 = SQL_NULL_HENV;

    SQLRETURN rc1 = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv1);
    ASSERT_TRUE(SQL_SUCCEEDED(rc1));
    EXPECT_TRUE(henv1 != SQL_NULL_HENV);

    SQLRETURN rc2 = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv2);
    ASSERT_TRUE(SQL_SUCCEEDED(rc2));
    EXPECT_TRUE(henv2 != SQL_NULL_HENV);

    // The two handles should be distinct.
    EXPECT_NE(henv1, henv2);

    SQLFreeHandle(SQL_HANDLE_ENV, henv2);
    SQLFreeHandle(SQL_HANDLE_ENV, henv1);
}

// -------------------------------------------------------------------
// Variation 4 – AllocEnvDup
// Re-allocating into the same variable (overwriting the pointer)
// should not crash. The original handle is still valid and must be
// freed separately.
// -------------------------------------------------------------------
TEST_F(AllocEnvTest, DuplicateAllocDoesNotCrash) {
    SQLHENV henv = SQL_NULL_HENV;

    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));

    // Save the first handle so we can free it later.
    SQLHENV henvOriginal = henv;

    // Allocate again into the same variable — the driver must not crash.
    rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_TRUE(henv != SQL_NULL_HENV);

    // Free both — order doesn't matter, just no crash.
    SQLFreeHandle(SQL_HANDLE_ENV, henv);
    SQLFreeHandle(SQL_HANDLE_ENV, henvOriginal);
}

// -------------------------------------------------------------------
// Variation 4b – AllocEnvNonNullInput
// Per ODBC spec, input_handle for ENV must be SQL_NULL_HANDLE.
// Passing a non-null value should fail.
// -------------------------------------------------------------------
TEST_F(AllocEnvTest, NonNullInputHandleReturnsInvalidHandle) {
    // First allocate a valid ENV to use as a bogus input_handle.
    SQLHENV henv = SQL_NULL_HENV;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));

    SQLHENV henv2 = SQL_NULL_HENV;
    rc = SQLAllocHandle(SQL_HANDLE_ENV, henv, &henv2);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);

    SQLFreeHandle(SQL_HANDLE_ENV, henv);
}

// ===================================================================
// AllocEnvStatesTest — Variation 5
// Requires a live SQL Server connection.
// Verifies that allocating new HENVs while a connection and
// statement are active on a *different* HENV does not crash.
// ===================================================================
class DISABLED_AllocEnvStatesTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        auto& cfg = ODBCTestConfig::Instance();
        if (!cfg.HasConnection()) {
            FAIL()
                << "No connection configured – set ODBC_TEST_DSN, "
                   "ODBC_TEST_SERVER, or ODBC_TEST_CONNSTR";
        }
    }
};

// While connected with an active statement, allocating a fresh HENV
// on the side should succeed and not interfere.
TEST_F(DISABLED_AllocEnvStatesTest, AllocEnvWhileConnected) {
    // Connect on the base fixture's env_/dbc_/stmt_.
    Connect();

    // Allocate a second, independent HENV — should succeed.
    SQLHENV henv2 = SQL_NULL_HENV;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv2);
    ASSERT_TRUE(SQL_SUCCEEDED(rc))
        << "Allocating a second HENV while connected failed, rc=" << rc;
    EXPECT_TRUE(henv2 != SQL_NULL_HENV);

    // Do something on the original connection to prove it's still alive.
    ExecDirect("SELECT 1");

    // Allocate yet another HENV — still should work.
    SQLHENV henv3 = SQL_NULL_HENV;
    rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv3);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));

    // Clean up the extra HENVs.
    SQLFreeHandle(SQL_HANDLE_ENV, henv3);
    SQLFreeHandle(SQL_HANDLE_ENV, henv2);
    // Base fixture TearDown() handles env_/dbc_/stmt_.
}

// Allocate a DBC on a second HENV while the first HENV has an active
// connection — both should coexist.
TEST_F(DISABLED_AllocEnvStatesTest, AllocEnvAndDbcWhileConnected) {
    Connect();

    // Second env + dbc (not connected).
    SQLHENV henv2 = SQL_NULL_HENV;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv2);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));

    rc = SQLSetEnvAttr(henv2, SQL_ATTR_ODBC_VERSION,
                       reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv2);

    SQLHDBC hdbc2 = SQL_NULL_HDBC;
    rc = SQLAllocHandle(SQL_HANDLE_DBC, henv2, &hdbc2);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv2);

    // Original connection still works.
    ExecDirect("SELECT @@VERSION");

    // Clean up second env's resources.
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc2);
    SQLFreeHandle(SQL_HANDLE_ENV, henv2);
}

// -------------------------------------------------------------------
// Variation 6 – AllocInvalidHandleType
// Passing an unknown handle type should fail.
// -------------------------------------------------------------------
TEST_F(AllocEnvTest, InvalidHandleTypeReturnsError) {
    SQLHANDLE handle = SQL_NULL_HANDLE;
    SQLRETURN rc = SQLAllocHandle(99, SQL_NULL_HANDLE, &handle);
    ASSERT_FALSE(SQL_SUCCEEDED(rc));
}

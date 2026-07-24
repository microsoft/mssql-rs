// Copyright (c) Microsoft Corporation. All rights reserved.
// alloc_stmt_test.cpp  –  Tests for SQLAllocHandle(SQL_HANDLE_STMT, ...)
//
// Note: STMT allocation through the DM requires a connected DBC — the DM
// only loads the driver at SQLDriverConnect time. Tests that exercise
// the driver's STMT handling are disabled until connection APIs land.
//
// Variations (DM-level, no connection):
//   1. AllocStmtNullOutput   – NULL output pointer → SQL_ERROR
//   2. AllocStmtNullDbc      – NULL parent DBC → SQL_INVALID_HANDLE
//   3. AllocStmtDisconnected – Disconnected DBC → SQL_ERROR (DM rejects)
//
// Variations (connected, disabled):
//   4. AllocStmtValid        – Allocate and free a single HSTMT
//   5. AllocStmtMult         – Allocate two STMTs on the same DBC
//   6. FreeDbcWithStmt       – DBC with outstanding STMT → SQL_ERROR
//   7. FreeStmtThenDbc       – Correct teardown order succeeds

#include "odbc_test_fixture.h"

// ===================================================================
// AllocStmtTest — manages its own ENV + DBC so we control the full lifecycle.
// ===================================================================
class AllocStmtTest : public ::testing::Test {
protected:
    SQLHENV henv_ = SQL_NULL_HENV;
    SQLHDBC hdbc_ = SQL_NULL_HDBC;

    void SetUp() override {
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv_);
        ASSERT_TRUE(SQL_SUCCEEDED(rc))
            << "SQLAllocHandle(ENV) failed, rc=" << rc;

        rc = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                           reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

        rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc_);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    }

    void TearDown() override {
        if (hdbc_ != SQL_NULL_HDBC) {
            SQLFreeHandle(SQL_HANDLE_DBC, hdbc_);
            hdbc_ = SQL_NULL_HDBC;
        }
        if (henv_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, henv_);
            henv_ = SQL_NULL_HENV;
        }
    }
};

// -------------------------------------------------------------------
// Variation 1 – AllocStmtNullOutput
// -------------------------------------------------------------------
TEST_F(AllocStmtTest, NullOutputPointerReturnsError) {
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, hdbc_, nullptr);
    ASSERT_FALSE(SQL_SUCCEEDED(rc));
}

// -------------------------------------------------------------------
// Variation 2 – AllocStmtNullDbc
// -------------------------------------------------------------------
TEST_F(AllocStmtTest, NullDbcReturnsInvalidHandle) {
    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, SQL_NULL_HANDLE, &hstmt);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// -------------------------------------------------------------------
// Variation 3 – AllocStmtDisconnected
// DM rejects STMT allocation on a disconnected DBC.
// -------------------------------------------------------------------
TEST_F(AllocStmtTest, DisconnectedDbcFails) {
    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, hdbc_, &hstmt);
    ASSERT_FALSE(SQL_SUCCEEDED(rc));
    EXPECT_FALSE(ODBCTestUtils::GetDiagState(SQL_HANDLE_DBC, hdbc_).empty());
}

// ===================================================================
// Connected STMT tests — disabled until SQLDriverConnect is implemented.
// These will exercise the driver's STMT alloc/free paths.
// ===================================================================
class DISABLED_AllocStmtConnectedTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        auto& cfg = ODBCTestConfig::Instance();
        if (!cfg.HasConnection()) {
            FAIL()
                << "No connection configured – set ODBC_TEST_DSN, "
                   "ODBC_TEST_SERVER, or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// -------------------------------------------------------------------
// Variation 4 – AllocStmtValid
// -------------------------------------------------------------------
TEST_F(DISABLED_AllocStmtConnectedTest, AllocAndFreeSucceeds) {
    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    EXPECT_TRUE(hstmt != SQL_NULL_HSTMT);

    rc = SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);
}

// -------------------------------------------------------------------
// Variation 5 – AllocStmtMult
// -------------------------------------------------------------------
TEST_F(DISABLED_AllocStmtConnectedTest, MultipleStmtsOnSameDbc) {
    SQLHSTMT hstmt1 = SQL_NULL_HSTMT;
    SQLHSTMT hstmt2 = SQL_NULL_HSTMT;

    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt1);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);

    rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt2);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);

    EXPECT_NE(hstmt1, hstmt2);

    SQLFreeHandle(SQL_HANDLE_STMT, hstmt2);
    SQLFreeHandle(SQL_HANDLE_STMT, hstmt1);
}

// -------------------------------------------------------------------
// Variation 6 – FreeDbcWithStmt
// DBC should refuse to free while a STMT is outstanding.
// -------------------------------------------------------------------
TEST_F(DISABLED_AllocStmtConnectedTest, FreeDbcWithOutstandingStmtFails) {
    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);

    // Disconnect first (DBC can't free while connected either).
    SQLDisconnect(dbc_);

    // DBC should still refuse — STMT is outstanding.
    rc = SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
    ASSERT_FALSE(SQL_SUCCEEDED(rc));

    // Free STMT, then DBC succeeds.
    rc = SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);
    rc = SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    dbc_ = SQL_NULL_HDBC;
}

// -------------------------------------------------------------------
// Variation 7 – FreeStmtThenDbc
// Correct teardown order: free all STMTs, disconnect, free DBC.
// -------------------------------------------------------------------
TEST_F(DISABLED_AllocStmtConnectedTest, FreeStmtThenDbcSucceeds) {
    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);

    // The fixture's TearDown will handle disconnect + free, but we
    // explicitly free our extra STMT here.
    rc = SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);
}

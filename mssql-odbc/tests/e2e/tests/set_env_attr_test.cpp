// Copyright (c) Microsoft Corporation. All rights reserved.
// set_env_attr_test.cpp  -  Tests for SQLSetEnvAttr / SQLGetEnvAttr.
//
// Exercises the driver through the unixODBC Driver Manager, validating:
//   1. SetGetOdbcVersion3_80    - round-trip SQL_OV_ODBC3_80
//   2. SetGetOdbcVersion3       - round-trip SQL_OV_ODBC3
//   3. SetGetOdbcVersion2       - round-trip SQL_OV_ODBC2
//   4. SetOdbcVersionInvalid    - bogus version value -> SQL_ERROR
//   5. SetUnknownAttribute      - unknown attribute -> error
//   6. SetVersionOverwrites     - subsequent SQLSetEnvAttr replaces prior value
//   7. SetVersionThenAllocDbc   - happy path exercising the AllocHandle gate
//   8. SetEnvAttrNullHandle     - DM rejects null henv before reaching driver

#include "odbc_test_fixture.h"

// All tests manage their own HENV - do NOT use the ODBCTest fixture which
// pre-allocates one and pre-sets the ODBC version.
class SetEnvAttrTest : public ::testing::Test {
protected:
    SQLHENV henv_ = SQL_NULL_HENV;

    void SetUp() override {
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv_);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
        ASSERT_NE(henv_, nullptr);
    }

    void TearDown() override {
        if (henv_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, henv_);
            henv_ = SQL_NULL_HENV;
        }
    }

    SQLRETURN SetVersion(SQLULEN ver) {
        return SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                             reinterpret_cast<SQLPOINTER>(ver), 0);
    }

    SQLINTEGER GetVersion() {
        SQLINTEGER v = 0;
        SQLRETURN rc = SQLGetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                                     &v, sizeof(v), nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
        return v;
    }
};

// -------------------------------------------------------------------
// Variation 1 - round-trip SQL_OV_ODBC3_80
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetGetOdbcVersion3_80) {
    EXPECT_SQL_OK(SetVersion(SQL_OV_ODBC3_80), SQL_HANDLE_ENV, henv_);
    EXPECT_EQ(static_cast<SQLINTEGER>(SQL_OV_ODBC3_80), GetVersion());
}

// -------------------------------------------------------------------
// Variation 2 - round-trip SQL_OV_ODBC3
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetGetOdbcVersion3) {
    EXPECT_SQL_OK(SetVersion(SQL_OV_ODBC3), SQL_HANDLE_ENV, henv_);
    EXPECT_EQ(static_cast<SQLINTEGER>(SQL_OV_ODBC3), GetVersion());
}

// -------------------------------------------------------------------
// Variation 3 - round-trip SQL_OV_ODBC2
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetGetOdbcVersion2) {
    EXPECT_SQL_OK(SetVersion(SQL_OV_ODBC2), SQL_HANDLE_ENV, henv_);
    EXPECT_EQ(static_cast<SQLINTEGER>(SQL_OV_ODBC2), GetVersion());
}

// -------------------------------------------------------------------
// Variation 4 - bogus value rejected
// Some DMs (notably unixODBC) intercept SQL_ATTR_ODBC_VERSION and reject
// unknown values themselves with HY024 before the driver ever sees them.
// Either way, the call must NOT succeed.
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetOdbcVersionInvalid) {
    SQLRETURN rc = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                                 reinterpret_cast<SQLPOINTER>(9999), 0);
    EXPECT_NE(SQL_SUCCESS, rc);
    EXPECT_NE(SQL_SUCCESS_WITH_INFO, rc);
}

// -------------------------------------------------------------------
// Variation 5 - unknown attribute id
// Future work: SQLSTATE HY092 (invalid attribute identifier).
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetUnknownAttribute) {
    SQLRETURN rc = SQLSetEnvAttr(henv_, 99999,
                                 reinterpret_cast<SQLPOINTER>(0), 0);
    EXPECT_NE(SQL_SUCCESS, rc);
    EXPECT_NE(SQL_SUCCESS_WITH_INFO, rc);
}

// -------------------------------------------------------------------
// Variation 6 - last write wins
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetVersionOverwrites) {
    EXPECT_SQL_OK(SetVersion(SQL_OV_ODBC2), SQL_HANDLE_ENV, henv_);
    EXPECT_SQL_OK(SetVersion(SQL_OV_ODBC3_80), SQL_HANDLE_ENV, henv_);
    EXPECT_EQ(static_cast<SQLINTEGER>(SQL_OV_ODBC3_80), GetVersion());
}

// -------------------------------------------------------------------
// Variation 7 - allocating a DBC after setting the version should work
// (this is the documented happy path: SetEnvAttr THEN AllocHandle(DBC)).
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetVersionThenAllocDbc) {
    EXPECT_SQL_OK(SetVersion(SQL_OV_ODBC3_80), SQL_HANDLE_ENV, henv_);

    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc);
    EXPECT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    EXPECT_NE(hdbc, nullptr);

    if (hdbc != SQL_NULL_HDBC) {
        SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
    }
}

// -------------------------------------------------------------------
// Variation 8 - null henv
// The Driver Manager intercepts null henv and returns SQL_INVALID_HANDLE
// before the driver is consulted.
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, SetEnvAttrNullHandle) {
    SQLRETURN rc = SQLSetEnvAttr(SQL_NULL_HENV, SQL_ATTR_ODBC_VERSION,
                                 reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80),
                                 0);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

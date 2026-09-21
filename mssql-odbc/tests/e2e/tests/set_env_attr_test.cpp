// Copyright (c) Microsoft Corporation. All rights reserved.
// set_env_attr_test.cpp  -  Tests for SQLSetEnvAttr / SQLGetEnvAttr.
//
// Exercises the driver through the unixODBC Driver Manager, validating:
//   1. SetGetOdbcVersion3_80    - round-trip SQL_OV_ODBC3_80
//   2. SetGetOdbcVersion3       - round-trip SQL_OV_ODBC3
//   3. SetGetOdbcVersion2       - DM accepts an ODBC 2.x application declaration
//   4. SetOdbcVersionInvalid    - bogus version value -> SQL_ERROR
//   5. SetUnknownAttribute      - unknown attribute -> error
//   6. SetVersionOverwrites     - subsequent SQLSetEnvAttr replaces prior value
//   6a. Odbc2ApplicationIsRefused - the DM forwards SQL_OV_ODBC2 rather than
//                                 mapping it, and the driver refuses at
//                                 connect time (surfaced as IM005)
//   6b. Odbc3ApplicationConnectsAndQueries - the same sequence under 3.80
//                                 connects and queries
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
// Variation 3 - the DM accepts an ODBC 2.x application declaration
// No driver is loaded at this point, so this does not test which values the
// driver's exported SQLSetEnvAttr implementation accepts.
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
// Variation 6a - an ODBC 2.x application is refused, through the DM
//
// This is the load-bearing test for the ODBC 3.x-only contract, and it proves
// two things at once.
//
// First, the Driver Manager does *not* map a 2.x declaration onto 3.x on the
// driver's behalf. It stores SQL_OV_ODBC2 and answers the application
// (variation 3), and SQLAllocHandle(SQL_HANDLE_DBC) below also succeeds - that
// handle is the DM's own, allocated before any driver is loaded. The driver is
// loaded at SQLDriverConnect, and only then does the DM replay the environment
// setup onto it: our exported SQLSetEnvAttr sees SQL_OV_ODBC2 and rejects it,
// so no version is recorded, and our SQLAllocHandle(SQL_HANDLE_DBC) refuses.
// unixODBC surfaces that to the application as IM005, "Driver's SQLAllocHandle
// on SQL_HANDLE_DBC failed" - our HY010 wrapped by the DM. Were the DM to
// convert 2 -> 3, the version would arrive as SQL_OV_ODBC3 and the connect
// would succeed.
//
// Second, the driver refuses to serve such an application rather than handing
// it the 3.x contract it never asked for - COLUMN_SIZE where it expects
// PRECISION, 91/92/93 where it expects 9/10/11.
//
// msodbcsql accepts the declaration and connects, so this asserts
// mssql-odbc-specific behavior the reference does not share at all; that is the
// first admissible reason for the skip macro in instructions.md 2.1. Registry
// entry 14 records the divergence.
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, Odbc2ApplicationIsRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();
    if (!ODBCTestConfig::Instance().HasConnection()) {
        GTEST_SKIP() << "No connection configured - set ODBC_TEST_DSN, "
                        "ODBC_TEST_SERVER, or ODBC_TEST_CONNSTR";
    }

    // The DM stores the declaration and reports success to the application.
    ASSERT_SQL_OK(SetVersion(SQL_OV_ODBC2), SQL_HANDLE_ENV, henv_);

    // Still the DM's own handle - no driver has been selected or loaded yet,
    // so this says nothing about the driver and must succeed.
    SQLHDBC hdbc = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc), SQL_HANDLE_ENV, henv_);

    // Connecting loads the driver and replays the environment onto it. This is
    // where the refusal becomes observable.
    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    SQLRETURN rc = SQLDriverConnect(hdbc, nullptr,
                                    const_cast<SQLTCHAR*>(connstr.c_str()),
                                    static_cast<SQLSMALLINT>(connstr.size()),
                                    outStr,
                                    static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                                    &outLen, SQL_DRIVER_NOPROMPT);
    EXPECT_EQ(SQL_ERROR, rc)
        << "a 2.x declaration must not yield a usable connection; if this "
           "succeeded, the Driver Manager mapped SQL_OV_ODBC2 to SQL_OV_ODBC3 "
           "before the driver saw it";
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, hdbc, "IM005");

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// -------------------------------------------------------------------
// Variation 6b - the same application succeeds once it declares 3.x
//
// Pins that variation 6a rejects the declared version, not the fixture: the
// identical sequence with SQL_OV_ODBC3_80 connects and queries.
// -------------------------------------------------------------------
TEST_F(SetEnvAttrTest, Odbc3ApplicationConnectsAndQueries) {
    if (!ODBCTestConfig::Instance().HasConnection()) {
        GTEST_SKIP() << "No connection configured - set ODBC_TEST_DSN, "
                        "ODBC_TEST_SERVER, or ODBC_TEST_CONNSTR";
    }

    ASSERT_SQL_OK(SetVersion(SQL_OV_ODBC3_80), SQL_HANDLE_ENV, henv_);

    SQLHDBC hdbc = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc), SQL_HANDLE_ENV, henv_);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    SQLRETURN rc = SQLDriverConnect(hdbc, nullptr,
                                    const_cast<SQLTCHAR*>(connstr.c_str()),
                                    static_cast<SQLSMALLINT>(connstr.size()),
                                    outStr,
                                    static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                                    &outLen, SQL_DRIVER_NOPROMPT);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, hdbc, &hstmt), SQL_HANDLE_DBC, hdbc);

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    ASSERT_SQL_OK(SQLExecDirect(hstmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, hstmt);
    ASSERT_SQL_OK(SQLFetch(hstmt), SQL_HANDLE_STMT, hstmt);

    SQLINTEGER value = 0;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(hstmt, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, hstmt);
    EXPECT_EQ(1, value);

    SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    SQLDisconnect(hdbc);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
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

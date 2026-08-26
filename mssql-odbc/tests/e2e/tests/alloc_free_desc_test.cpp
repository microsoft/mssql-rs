// Copyright (c) Microsoft Corporation. All rights reserved.
// alloc_free_desc_test.cpp - Tests for SQLAllocHandle(SQL_HANDLE_DESC, ...) /
// SQLFreeHandle(SQL_HANDLE_DESC, ...) and the SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC
// / SQL_ATTR_APP_PARAM_DESC) association they support (AB#47436).
//
// Verifies:
//   1. AllocNullDbcReturnsInvalidHandle - null parent DBC -> SQL_INVALID_HANDLE
//   2. FreeNullHandleReturnsInvalidHandle - null descriptor handle -> SQL_INVALID_HANDLE
//   3. AllocAndFreeExplicitDescriptor - alloc succeeds, SQL_DESC_ALLOC_TYPE is
//      SQL_DESC_ALLOC_USER, free succeeds
//   4. AssociatesAsActiveRowDescriptor - SQLSetStmtAttrW(APP_ROW_DESC) makes
//      SQLGetStmtAttrW(APP_ROW_DESC) return the explicit descriptor; APD is
//      untouched
//   5. ResetToImplicitViaNull - SQL_NULL_HDESC reverts to the implicit ARD
//   6. ResetToImplicitViaOwnHandle - passing back the original implicit ARD
//      handle reverts to it too (the other ODBC-legal reset spelling)
//   7. FreeingAssociatedDescriptorResetsStatementToImplicit - freeing an
//      explicit descriptor while it is a statement's active ARD reverts that
//      statement to its implicit ARD instead of leaving a dangling handle
//   8. FreeingImplicitDescriptorIsRejected - SQLFreeHandle on an implicit ARD
//      handle -> SQL_ERROR / HY017, and the ARD stays valid
//
// Note on "reverts to implicit" assertions: these check SQL_DESC_ALLOC_TYPE
// (SQL_DESC_ALLOC_AUTO) on the current ARD rather than comparing its SQLHDESC
// against a value captured from an earlier SQLGetStmtAttrW call. unixODBC (the
// Linux/macOS Driver Manager this suite runs against) synthesizes a fresh
// wrapper handle for an *implicitly* allocated descriptor on every
// SQLGetStmtAttrW call rather than returning the same value each time -- a
// documented unixODBC behavior, not a driver bug: msodbcsql reproduces the
// identical non-identity through the same DM, and both drivers pass an
// identity-based version of these assertions cleanly through the Windows
// native odbc32 Driver Manager. The wrapper still routes correctly to the
// real underlying descriptor, so field queries through it are reliable even
// when its own address is not.

#include "odbc_test_fixture.h"

TEST(AllocFreeDescTest, AllocNullDbcReturnsInvalidHandle) {
    SQLHDESC hdesc = SQL_NULL_HDESC;
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLAllocHandle(SQL_HANDLE_DESC, SQL_NULL_HANDLE, &hdesc));
}

TEST(AllocFreeDescTest, FreeNullHandleReturnsInvalidHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE, SQLFreeHandle(SQL_HANDLE_DESC, SQL_NULL_HANDLE));
}

class AllocFreeDescLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or "
                      "ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLHDESC AllocDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DESC, dbc_, &hdesc), SQL_HANDLE_DBC, dbc_);
        EXPECT_NE(hdesc, static_cast<SQLHDESC>(SQL_NULL_HDESC));
        return hdesc;
    }

    SQLHDESC AppRowDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, &hdesc, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return hdesc;
    }

    SQLHDESC AppParamDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_PARAM_DESC, &hdesc, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return hdesc;
    }

    // True if `hdesc` reports SQL_DESC_ALLOC_AUTO -- see the file-level
    // comment above for why this, not a captured-handle identity comparison,
    // is how these tests confirm a statement reverted to its implicit ARD.
    bool IsImplicitAlloc(SQLHDESC hdesc) {
        SQLSMALLINT alloc_type = -1;
        EXPECT_SQL_OK(SQLGetDescFieldW(hdesc, 0, SQL_DESC_ALLOC_TYPE, &alloc_type,
                                        sizeof(alloc_type), nullptr),
                      SQL_HANDLE_DESC, hdesc);
        return alloc_type == SQL_DESC_ALLOC_AUTO;
    }
};

TEST_F(AllocFreeDescLiveTest, AllocAndFreeExplicitDescriptor) {
    SQLHDESC hdesc = AllocDesc();

    SQLSMALLINT alloc_type = -1;
    EXPECT_SQL_OK(
        SQLGetDescFieldW(hdesc, 0, SQL_DESC_ALLOC_TYPE, &alloc_type, sizeof(alloc_type), nullptr),
        SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(SQL_DESC_ALLOC_USER, alloc_type);

    EXPECT_SQL_OK(SQLFreeHandle(SQL_HANDLE_DESC, hdesc), SQL_HANDLE_DESC, hdesc);
}

TEST_F(AllocFreeDescLiveTest, AssociatesAsActiveRowDescriptor) {
    SQLHDESC hdesc = AllocDesc();

    EXPECT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, hdesc, 0), SQL_HANDLE_STMT, stmt_);
    // hdesc is an application-owned handle from SQLAllocHandle, not one
    // synthesized by SQLGetStmtAttrW for an implicit descriptor, so the
    // Driver Manager must preserve its identity -- comparing it directly is
    // reliable (see the file-level comment for the distinction).
    EXPECT_EQ(hdesc, AppRowDesc());
    // APD is untouched by an ARD association -- checked by allocation type,
    // not by comparing two separate AppParamDesc() calls for raw identity
    // (see the file-level comment).
    EXPECT_TRUE(IsImplicitAlloc(AppParamDesc()));

    SQLFreeHandle(SQL_HANDLE_DESC, hdesc);
}

TEST_F(AllocFreeDescLiveTest, ResetToImplicitViaNull) {
    SQLHDESC hdesc = AllocDesc();
    ASSERT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, hdesc, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(hdesc, AppRowDesc());

    EXPECT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, SQL_NULL_HDESC, 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_TRUE(IsImplicitAlloc(AppRowDesc()));

    SQLFreeHandle(SQL_HANDLE_DESC, hdesc);
}

TEST_F(AllocFreeDescLiveTest, ResetToImplicitViaOwnHandle) {
    SQLHDESC implicit_ard = AppRowDesc();
    SQLHDESC hdesc = AllocDesc();
    ASSERT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, hdesc, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(hdesc, AppRowDesc());

    // ODBC: "If the value of this attribute is set to SQL_NULL_DESC or the
    // handle originally allocated for the descriptor" both revert to
    // implicit -- `implicit_ard` (captured before any association existed)
    // is the input under test here, not a value re-checked for identity
    // afterward.
    EXPECT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, implicit_ard, 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_TRUE(IsImplicitAlloc(AppRowDesc()));

    SQLFreeHandle(SQL_HANDLE_DESC, hdesc);
}

TEST_F(AllocFreeDescLiveTest, FreeingAssociatedDescriptorResetsStatementToImplicit) {
    SQLHDESC hdesc = AllocDesc();
    ASSERT_SQL_OK(SQLSetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, hdesc, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(hdesc, AppRowDesc());

    EXPECT_SQL_OK(SQLFreeHandle(SQL_HANDLE_DESC, hdesc), SQL_HANDLE_DESC, hdesc);
    EXPECT_TRUE(IsImplicitAlloc(AppRowDesc()));
}

TEST_F(AllocFreeDescLiveTest, FreeingImplicitDescriptorIsRejected) {
    SQLHDESC implicit_ard = AppRowDesc();

    SQLRETURN rc = SQLFreeHandle(SQL_HANDLE_DESC, implicit_ard);
    ASSERT_FALSE(SQL_SUCCEEDED(rc));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, implicit_ard, "HY017");

    // The ARD is untouched: still usable as the statement's own descriptor.
    EXPECT_TRUE(IsImplicitAlloc(AppRowDesc()));
}

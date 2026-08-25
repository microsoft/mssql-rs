// Copyright (c) Microsoft Corporation. All rights reserved.
// get_desc_field_test.cpp - Tests for SQLGetDescFieldW.
//
// Verifies:
//   1. NullHandle                  - null descriptor handle -> SQL_INVALID_HANDLE
//   2. AppParamDescIsImplicitApd   - SQLGetStmtAttr(APP_PARAM_DESC) returns a usable HDESC
//   3. FreshDescriptorHasZeroCount - SQL_DESC_COUNT is 0 before any binding
//   4. AllocTypeIsAuto             - SQL_DESC_ALLOC_TYPE is SQL_DESC_ALLOC_AUTO (implicit)
//   5. InvalidFieldReturnsError    - unrecognized FieldIdentifier -> SQL_ERROR / HY091
//   6. InvalidRecordNumberErrors   - RecNumber 0/negative -> SQL_ERROR / 07009
//   7. RecordPastCountReturnsNoData - RecNumber > SQL_DESC_COUNT -> SQL_NO_DATA
//   8. ReadsBackValueSetBySetDescField - GET after SET round-trips SQL_C_NUMERIC
//   9. ImpRowDescRecordPastCountReturnsNoData - IRD RecNumber > SQL_DESC_COUNT
//      -> SQL_NO_DATA, on a prepared statement (see the test for why prepare
//      is required)

#include "odbc_test_fixture.h"

#include <cstring>

TEST(GetDescFieldTest, NullHandle) {
    SQLSMALLINT value = 0;
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLGetDescFieldW(SQL_NULL_HANDLE, 1, SQL_DESC_TYPE, &value,
                                sizeof(value), nullptr));
}

class GetDescFieldLiveTest : public ODBCTest {
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
};

TEST_F(GetDescFieldLiveTest, AppParamDescIsImplicitApd) {
    SQLHDESC hdesc = AppParamDesc();
    EXPECT_NE(hdesc, static_cast<SQLHDESC>(SQL_NULL_HDESC));
}

TEST_F(GetDescFieldLiveTest, FreshDescriptorHasZeroCount) {
    SQLHDESC hdesc = AppParamDesc();
    SQLSMALLINT count = -1;
    SQLRETURN rc = SQLGetDescFieldW(hdesc, 0, SQL_DESC_COUNT, &count, sizeof(count), nullptr);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(0, count);
}

TEST_F(GetDescFieldLiveTest, AllocTypeIsAuto) {
    SQLHDESC hdesc = AppParamDesc();
    SQLSMALLINT alloc_type = -1;
    SQLRETURN rc =
        SQLGetDescFieldW(hdesc, 0, SQL_DESC_ALLOC_TYPE, &alloc_type, sizeof(alloc_type), nullptr);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(SQL_DESC_ALLOC_AUTO, alloc_type);
}

TEST_F(GetDescFieldLiveTest, InvalidFieldReturnsError) {
    // msodbcsql checks whether RecNumber refers to an existing record before
    // it decides whether a record-field identifier is recognized, so an
    // unrecognized field on a record that doesn't exist yet reports "no such
    // record" (SQL_NO_DATA on RecNumber 1 of an empty descriptor, 07009 on
    // RecNumber 0) instead of "unrecognized field". Grow record 1 first so
    // the record-existence check passes and the field-recognition check is
    // what actually runs.
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_OK(SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_LONG)),
                                   0),
                  SQL_HANDLE_DESC, hdesc);
    ASSERT_SQL_ERROR(SQLGetDescFieldW(hdesc, 1, 0x7FFF, nullptr, 0, nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY091");
}

TEST_F(GetDescFieldLiveTest, InvalidRecordNumberErrors) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_ERROR(SQLGetDescFieldW(hdesc, 0, SQL_DESC_TYPE, nullptr, 0, nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "07009");

    ASSERT_SQL_ERROR(SQLGetDescFieldW(hdesc, -1, SQL_DESC_TYPE, nullptr, 0, nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "07009");
}

TEST_F(GetDescFieldLiveTest, RecordPastCountReturnsNoData) {
    SQLHDESC hdesc = AppParamDesc();
    EXPECT_EQ(SQL_NO_DATA, SQLGetDescFieldW(hdesc, 1, SQL_DESC_TYPE, nullptr, 0, nullptr));
}

TEST_F(GetDescFieldLiveTest, ReadsBackValueSetBySetDescField) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_OK(
        SQLSetDescFieldW(hdesc, 1, SQL_DESC_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_NUMERIC)), 0),
        SQL_HANDLE_DESC, hdesc);

    SQLSMALLINT concise_type = -1;
    SQLRETURN rc = SQLGetDescFieldW(hdesc, 1, SQL_DESC_CONCISE_TYPE, &concise_type,
                                     sizeof(concise_type), nullptr);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(SQL_C_NUMERIC, concise_type);
}

// IRD supports GET on the common record fields (via the same field-access
// model as ARD/APD/IPD) even though every SET on it is rejected — see
// set_desc_field_test.cpp's IrdRejectsFieldWrite. This driver's IRD storage
// is independent of the live result set: reconciling it against a prepared
// statement's real column metadata is AB#47437, not this PR (see
// handles/desc.rs's module docs), so record 1 does not exist here even once
// prepared. msodbcsql fully implements that wiring already and reports the
// real column type for a one-column "SELECT 1" — a genuine, intentional
// scope-boundary divergence, not a bug, so this only asserts on the Rust
// leg.
//
// The statement must be prepared first: unixODBC's Driver Manager gates
// SQLGetDescField on an IRD to statements past STATE_S1 ("allocated, not
// yet prepared") and returns HY007 itself — never even reaching the
// driver — for an unprepared statement's IRD. That is a Driver-Manager
// -level, spec-mandated check (ODBC state transition tables), not
// something the driver controls, so querying the IRD immediately after
// SQLAllocHandle(STMT) does not reach this driver at all on Linux.
TEST_F(GetDescFieldLiveTest, ImpRowDescRecordPastCountReturnsNoData) {
    SKIP_IF_COMPARING_MSODBCSQL();
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    SQLHDESC hdesc = ImpRowDesc();
    EXPECT_EQ(SQL_NO_DATA, SQLGetDescFieldW(hdesc, 1, SQL_DESC_TYPE, nullptr, 0, nullptr));
}

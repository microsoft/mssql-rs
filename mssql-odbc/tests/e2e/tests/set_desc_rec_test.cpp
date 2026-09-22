// Copyright (c) Microsoft Corporation. All rights reserved.
// set_desc_rec_test.cpp - Tests for SQLSetDescRec.
//
// Unlike SQLGetDescRecW, this function has no W/A split — none of its
// arguments are character data, so the ODBC spec declares only one entry
// point (sql.h has SQLSetDescRec, not SQLSetDescRecW/SQLSetDescRecA).
//
// Verifies:
//   1. NullHandle                    - null descriptor handle -> SQL_INVALID_HANDLE
//   2. CannotModifyIrd               - any write on the IRD -> SQL_ERROR / HY016
//   3. RecordNumberZeroReturnsError  - RecNumber 0 -> SQL_ERROR / 07009
//   4. GrowsRecordCount              - RecNumber > SQL_DESC_COUNT grows the record plex
//   5. DatetimeSubTypeResolvesConciseType - Type=SQL_DATETIME + SubType=
//      SQL_CODE_TIMESTAMP resolves SQL_DESC_CONCISE_TYPE to SQL_TYPE_TIMESTAMP
//   6. EquivalentToSetDescFieldSequence - one SQLSetDescRec call and the
//      equivalent sequence of SQLSetDescFieldW calls produce the same record
//   7. BindsAParameterUsableForExecute - a parameter bound purely through
//      SQLSetDescRec (never SQLBindParameter) executes correctly — the
//      strongest form of AB#47437's "descriptor-field and convenience bind
//      APIs must be equivalent" requirement

#include "odbc_test_fixture.h"

#include <cstring>

TEST(SetDescRecTest, NullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLSetDescRec(SQL_NULL_HANDLE, 1, SQL_INTEGER, 0, 0, 0, 0, nullptr, nullptr,
                             nullptr));
}

class SetDescRecLiveTest : public ODBCTest {
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

    SQLHDESC ImpParamDesc() {
        SQLHDESC hdesc = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_IMP_PARAM_DESC, &hdesc, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return hdesc;
    }

    SQLSMALLINT GetSmallInt(SQLHDESC hdesc, SQLSMALLINT record, SQLSMALLINT field) {
        SQLSMALLINT value = -1;
        SQLRETURN rc = SQLGetDescFieldW(hdesc, record, field, &value, sizeof(value), nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
        return value;
    }

    SQLLEN GetLen(SQLHDESC hdesc, SQLSMALLINT record, SQLSMALLINT field) {
        SQLLEN value = -1;
        SQLRETURN rc = SQLGetDescFieldW(hdesc, record, field, &value, sizeof(value), nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_DESC, hdesc);
        return value;
    }
};

TEST_F(SetDescRecLiveTest, CannotModifyIrd) {
    SQLHDESC hdesc = ImpRowDesc();
    ASSERT_SQL_ERROR(
        SQLSetDescRec(hdesc, 1, SQL_INTEGER, 0, 0, 0, 0, nullptr, nullptr, nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "HY016");
}

TEST_F(SetDescRecLiveTest, RecordNumberZeroReturnsError) {
    SQLHDESC hdesc = AppParamDesc();
    ASSERT_SQL_ERROR(
        SQLSetDescRec(hdesc, 0, SQL_INTEGER, 0, 0, 0, 0, nullptr, nullptr, nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, hdesc, "07009");
}

TEST_F(SetDescRecLiveTest, GrowsRecordCount) {
    // msodbcsql's SQLSetDescRec does not call AllocPlex the way
    // SQLSetDescField/SQLBindParameter do (confirmed by reading sqlcdesc.cpp:
    // no AllocPlex call in SQLSetDescRec's own body), so it does not reliably
    // grow SQL_DESC_COUNT for a RecNumber past the current count. Growing
    // eagerly here is a deliberate, spec-compliant design choice for this
    // driver (matching SQLSetDescFieldW's own per-record growth), not shared
    // by msodbcsql for this specific API.
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLHDESC hdesc = AppParamDesc();
    EXPECT_EQ(0, GetSmallInt(hdesc, 0, SQL_DESC_COUNT));
    ASSERT_SQL_OK(
        SQLSetDescRec(hdesc, 3, SQL_C_LONG, 0, 0, 0, 0, nullptr, nullptr, nullptr),
        SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(3, GetSmallInt(hdesc, 0, SQL_DESC_COUNT));
}

TEST_F(SetDescRecLiveTest, DatetimeSubTypeResolvesConciseType) {
    // SQL_DATETIME + SubType resolution is IPD-only: SQL_DESC_TYPE on an
    // application descriptor (APD) means the application's C type, not a SQL
    // type, and SQL_DATETIME(9) aliases SQL_C_DATE/SQL_DATE(9) in that space
    // - using the APD here would resolve as a C-type alias instead of the SQL
    // datetime family this test means to exercise.
    SQLHDESC hdesc = ImpParamDesc();
    ASSERT_SQL_OK(SQLSetDescRec(hdesc, 1, SQL_DATETIME, SQL_CODE_TIMESTAMP, 0, 0, 0, nullptr,
                                 nullptr, nullptr),
                  SQL_HANDLE_DESC, hdesc);
    EXPECT_EQ(SQL_TYPE_TIMESTAMP, GetSmallInt(hdesc, 1, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_CODE_TIMESTAMP, GetSmallInt(hdesc, 1, SQL_DESC_DATETIME_INTERVAL_CODE));
}

TEST_F(SetDescRecLiveTest, EquivalentToSetDescFieldSequence) {
    // SQL_DESC_OCTET_LENGTH is a SQLLEN field, so it must be read through
    // GetLen: SQLGetDescField ignores BufferLength for fixed-size attributes
    // and writes the full 8 bytes, which a 2-byte GetSmallInt slot would
    // smash the stack with (AB#47811).

    // Two independent statements, each with its own implicit APD — comparing
    // against the *same* APD twice would let the second write sequence
    // silently overwrite the first's, rather than proving the two APIs agree.
    SQLHSTMT stmt2 = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt2), SQL_HANDLE_DBC, dbc_);
    SQLHDESC via_rec = AppParamDesc();
    SQLHDESC via_field = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttrW(stmt2, SQL_ATTR_APP_PARAM_DESC, &via_field, 0, nullptr),
                  SQL_HANDLE_STMT, stmt2);

    SQLINTEGER buf_a = 0;
    SQLLEN ind_a = 0;
    ASSERT_SQL_OK(
        SQLSetDescRec(via_rec, 1, SQL_C_LONG, 0, 0, 0, 0, &buf_a, nullptr, &ind_a),
        SQL_HANDLE_DESC, via_rec);

    SQLINTEGER buf_b = 0;
    SQLLEN ind_b = 0;
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_DATETIME_INTERVAL_CODE,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(0)), 0),
                  SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(
        SQLSetDescFieldW(via_field, 1, SQL_DESC_CONCISE_TYPE,
                         reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(SQL_C_LONG)), 0),
        SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_OCTET_LENGTH,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(0)), 0),
                  SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_PRECISION,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(0)), 0),
                  SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_SCALE,
                                   reinterpret_cast<SQLPOINTER>(static_cast<SQLLEN>(0)), 0),
                  SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_OCTET_LENGTH_PTR, nullptr, 0),
                  SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_INDICATOR_PTR, &ind_b, 0),
                  SQL_HANDLE_DESC, via_field);
    ASSERT_SQL_OK(SQLSetDescFieldW(via_field, 1, SQL_DESC_DATA_PTR, &buf_b, 0),
                  SQL_HANDLE_DESC, via_field);

    EXPECT_EQ(GetSmallInt(via_rec, 1, SQL_DESC_CONCISE_TYPE),
              GetSmallInt(via_field, 1, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(GetLen(via_rec, 1, SQL_DESC_OCTET_LENGTH),
              GetLen(via_field, 1, SQL_DESC_OCTET_LENGTH));

    EXPECT_SQL_OK(SQLFreeHandle(SQL_HANDLE_STMT, stmt2), SQL_HANDLE_STMT, stmt2);
}

// The strongest form of AB#47437's equivalence requirement: a parameter bound
// purely through SQLSetDescRec (SQLBindParameter is never called) must be
// just as usable for a real execute as one bound the conventional way. The
// APD carries the C type/buffer, the IPD carries the SQL type — exactly the
// two descriptors SQLBindParameter itself writes in one call
// (bind_param.rs); SQL_DESC_PARAMETER_TYPE needs no explicit write, since a
// freshly-grown IPD record already defaults to SQL_PARAM_INPUT.
TEST_F(SetDescRecLiveTest, BindsAParameterUsableForExecute) {
    SQLHDESC apd = AppParamDesc();
    SQLINTEGER value = 42;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLSetDescRec(apd, 1, SQL_C_LONG, 0, 0, 0, 0, &value, nullptr, &ind),
                  SQL_HANDLE_DESC, apd);

    SQLHDESC ipd = ImpParamDesc();
    ASSERT_SQL_OK(
        SQLSetDescRec(ipd, 1, SQL_INTEGER, 0, 0, 0, 0, nullptr, nullptr, nullptr),
        SQL_HANDLE_DESC, ipd);

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT CAST(? AS INT)");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER result = 0;
    SQLLEN result_ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_LONG, &result, sizeof(result), &result_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, result);
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

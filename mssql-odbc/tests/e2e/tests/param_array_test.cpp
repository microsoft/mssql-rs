// Copyright (c) Microsoft Corporation. All rights reserved.
// param_array_test.cpp  -  E2E tests for column-wise and row-wise parameter
//                          arrays (SQL_ATTR_PARAMSET_SIZE), the path
//                          mssql-python's cursor.executemany() drives.
//
// Every assertion here is written to hold on BOTH drivers so the suite doubles
// as the parity contract: run it with ODBC_TEST_DRIVER="ODBC Driver 18 for SQL
// Server" to measure msodbcsql, and with the "(Rust)" registration to measure
// mssql-odbc. Cases that assert a deliberate mssql-odbc divergence are marked
// with SKIP_IF_COMPARING_MSODBCSQL() and say why.
//
// Behaviour that was measured out of the msodbcsql sources rather than guessed
// (see mssql-odbc/src/api/param_array.rs for file/line citations):
//   - a failing parameter set does NOT stop the ones after it
//   - SQL_ATTR_PARAM_STATUS_PTR downgrades the overall return code to
//     SQL_SUCCESS_WITH_INFO; without it the same batch is SQL_ERROR
//   - SQL_ATTR_PARAMS_PROCESSED_PTR ends at the number of sets attempted
//   - SQL_PARAM_IGNORE sets are skipped and reported SQL_PARAM_UNUSED
//   - SQLRowCount reports the SUM of the sets' affected rows
//
//   1.  ArrayInsertWritesEveryParameterSet        - N sets, N rows
//   2.  ArrayInsertAggregatesRowCount             - SQLRowCount is the sum
//   3.  ArrayHandlesMixedNullAndNonNull           - per-row indicators
//   4.  ArrayHandlesVariableWidthBuffers          - char/binary use BufferLength
//   5.  ArrayHandlesMultipleTypesTogether         - int/float/char/date/guid
//   6.  ScalarExecuteAfterArrayUsesOneRow         - no stale array size
//   7.  ArraySizeOneMatchesScalarExecute          - degenerate array
//   7b. ScalarExecuteWritesParamsProcessed        - pointer is live at size 1
//   8.  MiddleRowFailureStillRunsLaterRows        - measured msodbcsql rule
//   9.  StatusArrayDowngradesReturnCode           - measured msodbcsql rule
//  10.  ParamsProcessedCountsEverySet             - including skipped sets
//  11.  IgnoredParameterSetsAreSkipped            - SQL_PARAM_IGNORE
//  11b. IgnoredSetStatusesAreReportedPerSet       - divergence: msodbcsql shifts
//  12.  BindOffsetShiftsTheWholeArray             - offset + stride together
//  13.  RowWiseBindingWalksStructures             - SQL_ATTR_PARAM_BIND_TYPE
//  14.  ExecDirectSupportsParameterArrays         - the sp_executesql path
//  15.  ArrayRollsBackWithTheTransaction          - manual-commit semantics
//  16.  ArrayCommitsEveryRowUnderAutocommit       - partial-failure durability
//  17.  RowReturningArrayIsRefused                - documented divergence
//  17b. RowReturningArrayRefusalLeavesTheFirstSetWritten - partial write
//  18.  ArrayWithZeroBufferLengthAliasesOneValue  - inherited msodbcsql quirk
//  19.  DataAtExecutionWithArrayIsRefused         - documented divergence
//  20.  QueryTimeoutBoundsTheWholeArray           - one budget for all sets
//  21.  ColumnWiseArrayWithNullIndicatorPointers  - msodbcsql BindColumns shape
//  22.  RowWiseArrayWalksMixedTypeStructures      - msodbcsql RowStruct shape
//  23.  ParamsProcessedCountsTheFailingSet        - msodbcsql Variation_76

#include "odbc_test_fixture.h"

#include <cstring>
#include <limits>
#include <string>
#include <vector>

namespace {

// SQL Server-specific: a CHECK constraint gives us a per-row server error that
// is fatal to its own statement but leaves the connection usable, which is what
// the continue-after-error rule needs to be observable.
constexpr const char* kGuardTable =
    "CREATE TABLE #pa (id int NOT NULL, v int NULL CHECK (v IS NULL OR v < 100))";

} // namespace

class ParamArrayTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured";
        }
        Connect();
    }

    SQLRETURN SetStmtULen(SQLINTEGER attribute, SQLULEN value) {
        return SQLSetStmtAttr(stmt_, attribute,
                              reinterpret_cast<SQLPOINTER>(value), 0);
    }

    SQLRETURN SetStmtPtr(SQLINTEGER attribute, void* value) {
        return SQLSetStmtAttr(stmt_, attribute,
                              reinterpret_cast<SQLPOINTER>(value), 0);
    }

    SQLINTEGER ScalarInt(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        SQLHSTMT probe = AllocStmt();
        EXPECT_SQL_OK(
            SQLExecDirect(probe, const_cast<SQLTCHAR*>(wide.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, probe);
        EXPECT_SQL_OK(SQLFetch(probe), SQL_HANDLE_STMT, probe);
        SQLINTEGER value = -1;
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(probe, 1, SQL_C_SLONG, &value, sizeof(value), &ind),
                      SQL_HANDLE_STMT, probe);
        FreeStmt(probe);
        return value;
    }

    std::string ScalarString(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        SQLHSTMT probe = AllocStmt();
        EXPECT_SQL_OK(
            SQLExecDirect(probe, const_cast<SQLTCHAR*>(wide.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, probe);
        EXPECT_SQL_OK(SQLFetch(probe), SQL_HANDLE_STMT, probe);
        SQLTCHAR buf[512] = {};
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(probe, 1, SQL_C_TCHAR, buf, sizeof(buf), &ind),
                      SQL_HANDLE_STMT, probe);
        FreeStmt(probe);
        return ODBCTestUtils::ToNarrow(SqlTString(buf));
    }

    void Prepare(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(wide.c_str()), SQL_NTS),
                      SQL_HANDLE_STMT, stmt_);
    }

    /// Runs `sql` on a throwaway statement so the statement under test keeps its
    /// bindings, array attributes, and prepared plan untouched.
    void ExecOnProbe(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        SQLHSTMT probe = AllocStmt();
        EXPECT_SQL_OK(
            SQLExecDirect(probe, const_cast<SQLTCHAR*>(wide.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, probe);
        FreeStmt(probe);
    }
};

// -------------------------------------------------------------------
// 1. The whole point: N parameter sets must produce N rows, in order.
// A driver that executed only the first set would pass a COUNT check
// against 1 but fail this one.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayInsertWritesEveryParameterSet) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[4] = {1, 2, 3, 4};
    SQLINTEGER vals[4] = {10, 20, 30, 40};
    SQLLEN ind[4] = {0, 0, 0, 0};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 4));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(4, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ(100, ScalarInt("SELECT SUM(v) FROM #pa"));
    EXPECT_EQ("1,2,3,4",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), id), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa"));
}

// -------------------------------------------------------------------
// 2. SQLRowCount reports the SUM across parameter sets, not the last
// set's count. msodbcsql accumulates into `rowsaffected` for the whole
// bulk operation (sqlctokn.cpp:2246).
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayInsertAggregatesRowCount) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLINTEGER vals[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLLEN affected = -12345;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, affected) << "each set inserts one row, so the batch is 3";
}

// -------------------------------------------------------------------
// 3. The indicator array strides by sizeof(SQLLEN) independently of the
// value array, so NULLs must land on exactly the rows that asked for
// them.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayHandlesMixedNullAndNonNull) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[4] = {1, 2, 3, 4};
    SQLINTEGER vals[4] = {7, 0, 9, 0};
    SQLLEN id_ind[4] = {0, 0, 0, 0};
    SQLLEN v_ind[4] = {0, SQL_NULL_DATA, 0, SQL_NULL_DATA};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, id_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, v_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 4));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(4, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #pa WHERE v IS NULL"));
    EXPECT_EQ("2,4",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), id), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa WHERE v IS NULL"));
}

// -------------------------------------------------------------------
// 4. Character and binary values stride by BufferLength, not by the C
// type's width. A driver using a fixed width here would read every row
// from the wrong offset.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayHandlesVariableWidthBuffers) {
    ExecDirect("CREATE TABLE #pa_v (id int, s varchar(20), b varbinary(8))");
    Prepare("INSERT INTO #pa_v (id, s, b) VALUES (?, ?, ?)");

    constexpr SQLLEN kTextStride = 12;
    constexpr SQLLEN kBinStride = 4;
    SQLINTEGER ids[3] = {1, 2, 3};
    char text[3 * kTextStride] = {};
    std::strcpy(&text[0 * kTextStride], "alpha");
    std::strcpy(&text[1 * kTextStride], "beta");
    std::strcpy(&text[2 * kTextStride], "gamma");
    unsigned char bin[3 * kBinStride] = {
        0xDE, 0xAD, 0x00, 0x00,
        0xBE, 0xEF, 0xCA, 0x00,
        0x01, 0x02, 0x03, 0x04,
    };
    SQLLEN id_ind[3] = {0, 0, 0};
    SQLLEN text_ind[3] = {SQL_NTS, SQL_NTS, SQL_NTS};
    SQLLEN bin_ind[3] = {2, 3, 4};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, id_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 20, 0, text, kTextStride, text_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 3, SQL_PARAM_INPUT, SQL_C_BINARY,
                                   SQL_VARBINARY, 8, 0, bin, kBinStride, bin_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("alpha,beta,gamma",
              ScalarString("SELECT STRING_AGG(s, ',') WITHIN GROUP (ORDER BY id) "
                           "FROM #pa_v"));
    EXPECT_EQ("DEAD,BEEFCA,01020304",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(20), b, 2), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa_v"));
}

// -------------------------------------------------------------------
// 5. A realistic executemany shape: several C types striding side by
// side, each with its own width. Getting one stride wrong corrupts only
// that column, which a single-type test would miss.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayHandlesMultipleTypesTogether) {
    ExecDirect("CREATE TABLE #pa_m (id int, big bigint, d float, s nvarchar(20), "
               "ts datetime2(3), g uniqueidentifier)");
    Prepare("INSERT INTO #pa_m (id, big, d, s, ts, g) VALUES (?, ?, ?, ?, ?, ?)");

    constexpr int kRows = 3;
    SQLINTEGER ids[kRows] = {1, 2, 3};
    SQLBIGINT bigs[kRows] = {1234567890123LL, -42LL, 0LL};
    SQLDOUBLE ds[kRows] = {1.5, -2.25, 0.0};
    SQLWCHAR names[kRows][10] = {};
    std::memcpy(names[0], L"one", 4 * sizeof(wchar_t));
    std::memcpy(names[1], L"two", 4 * sizeof(wchar_t));
    std::memcpy(names[2], L"three", 6 * sizeof(wchar_t));
    SQL_TIMESTAMP_STRUCT ts[kRows] = {};
    for (int i = 0; i < kRows; ++i) {
        ts[i].year = static_cast<SQLSMALLINT>(2020 + i);
        ts[i].month = 1;
        ts[i].day = static_cast<SQLUSMALLINT>(i + 1);
        ts[i].hour = 12;
        ts[i].minute = 30;
        ts[i].second = 15;
        ts[i].fraction = 0;
    }
    SQLGUID guids[kRows] = {};
    for (int i = 0; i < kRows; ++i) {
        guids[i].Data1 = static_cast<unsigned long>(0x11111111 * (i + 1));
        guids[i].Data2 = 0x2222;
        guids[i].Data3 = 0x3333;
        for (int b = 0; b < 8; ++b) {
            guids[i].Data4[b] = static_cast<unsigned char>(b + i);
        }
    }
    SQLLEN ind[kRows] = {0, 0, 0};
    SQLLEN name_ind[kRows] = {SQL_NTS, SQL_NTS, SQL_NTS};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SBIGINT,
                                   SQL_BIGINT, 19, 0, bigs, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 3, SQL_PARAM_INPUT, SQL_C_DOUBLE,
                                   SQL_DOUBLE, 15, 0, ds, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 4, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 20, 0, names, sizeof(names[0]),
                                   name_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 5, SQL_PARAM_INPUT, SQL_C_TYPE_TIMESTAMP,
                                   SQL_TYPE_TIMESTAMP, 23, 3, ts, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 6, SQL_PARAM_INPUT, SQL_C_GUID,
                                   SQL_GUID, 36, 0, guids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, kRows));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(kRows, ScalarInt("SELECT COUNT(*) FROM #pa_m"));
    EXPECT_EQ("one,two,three",
              ScalarString("SELECT STRING_AGG(s, ',') WITHIN GROUP (ORDER BY id) "
                           "FROM #pa_m"));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa_m WHERE big = 1234567890123"));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa_m WHERE d = -2.25"));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa_m WHERE ts = "
                           "'2022-01-03T12:30:15'"));
    EXPECT_EQ(kRows, ScalarInt("SELECT COUNT(DISTINCT g) FROM #pa_m"));
}

// -------------------------------------------------------------------
// 6. mssql-python resets PARAMSET_SIZE to 1 and reuses the statement.
// A driver that kept the previous array size would read past the scalar
// buffers on the next execute. This is the regression that guards it.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ScalarExecuteAfterArrayUsesOneRow) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLINTEGER vals[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa"));

    // Back to scalar, exactly as SQLResetStmt_wrap leaves the statement.
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 1));
    SQLINTEGER one_id = 99;
    SQLINTEGER one_val = 42;
    SQLLEN one_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &one_id, 0, &one_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &one_val, 0, &one_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(4, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa WHERE id = 99 AND v = 42"));
    SQLLEN affected = -1;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, affected);
}

// -------------------------------------------------------------------
// 7. PARAMSET_SIZE = 1 is the default and must stay byte-for-byte the
// scalar path — the array machinery must not engage.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArraySizeOneMatchesScalarExecute) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER id = 5;
    SQLINTEGER val = 6;
    SQLLEN ind = 0;
    SQLULEN processed = 0xDEAD;
    SQLUSMALLINT status = 0xFFFF;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &id, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &val, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 1));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, &status));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa"));
    SQLLEN affected = -1;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, affected);
    // The pointer is live regardless of paramset size: an application that
    // binds it and runs a one-row executemany reads this back.
    EXPECT_EQ(1u, processed);
    // msodbcsql writes the processed pointer for a scalar execute but never the
    // status array: every cmdp.rgfArrayStatus write in sqlctokn.cpp sits behind
    // BULK_OP_IN_PROGRESS, which one parameter set does not enter.
    EXPECT_EQ(SQLUSMALLINT{0xFFFF}, status)
        << "status array untouched by a scalar execute";
}

// -------------------------------------------------------------------
// 7b. The processed pointer is written for a plain scalar execute too,
// including through SQLExecDirect and for a statement with no parameter
// markers at all - msodbcsql writes 1 at sqlccmd.cpp:3493 even then.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ScalarExecuteWritesParamsProcessed) {
    ExecDirect(kGuardTable);

    SQLULEN processed = 0xDEAD;
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    SQLINTEGER id = 1;
    SQLINTEGER val = 2;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &id, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &val, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    SqlTString sql =
        ODBCTestUtils::ToSqlTStr("INSERT INTO #pa (id, v) VALUES (?, ?)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, sql.data(), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(1u, processed) << "parameterized scalar execute";

    // A statement with no markers still reports one processed set.
    processed = 0xDEAD;
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    SqlTString plain =
        ODBCTestUtils::ToSqlTStr("INSERT INTO #pa (id, v) VALUES (99, 1)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, plain.data(), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(1u, processed) << "parameterless statement";
}

// -------------------------------------------------------------------
// 8. Measured msodbcsql rule: parameter-array execution does NOT stop at
// a failing set (sqlctokn.cpp:2341 - "we don't stop on ERROR token in
// parameter array execution"). Row 2 violates the CHECK constraint; rows
// 1 and 3 must still be written, and the failure must be reported.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, MiddleRowFailureStillRunsLaterRows) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLINTEGER vals[3] = {10, 500, 30}; // 500 violates CHECK (v < 100)
    SQLLEN ind[3] = {0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    // No status array is bound, so the failure surfaces as SQL_ERROR.
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "23000");

    EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #pa"))
        << "the set after the failing one must still execute";
    EXPECT_EQ("1,3",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), id), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa"));
}

// -------------------------------------------------------------------
// 9. Measured msodbcsql rule: a bound SQL_ATTR_PARAM_STATUS_PTR makes a
// row failure visible to the caller, so the overall return is downgraded
// from SQL_ERROR to SQL_SUCCESS_WITH_INFO. The identical batch without
// the array stays SQL_ERROR (Variation 8).
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, StatusArrayDowngradesReturnCode) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLINTEGER vals[3] = {10, 500, 30};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLExecute(stmt_));

    EXPECT_EQ(SQL_PARAM_SUCCESS, status[0]);
    EXPECT_EQ(SQL_PARAM_ERROR, status[1]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[2]);
    EXPECT_EQ(3u, processed);
    EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #pa"));
}

// -------------------------------------------------------------------
// 10. SQL_ATTR_PARAMS_PROCESSED_PTR ends at the number of sets the
// driver walked, which is the full array on a clean run.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ParamsProcessedCountsEverySet) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[5] = {1, 2, 3, 4, 5};
    SQLINTEGER vals[5] = {1, 2, 3, 4, 5};
    SQLLEN ind[5] = {0, 0, 0, 0, 0};
    SQLULEN processed = 0xDEAD;
    SQLUSMALLINT status[5] = {0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 5));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(5u, processed);
    for (int i = 0; i < 5; ++i) {
        EXPECT_EQ(SQL_PARAM_SUCCESS, status[i]) << "set " << i;
    }
}

// -------------------------------------------------------------------
// 11. SQL_PARAM_IGNORE skips a set without sending it (sqlccmd.cpp:3218).
// Which rows reach the server is identical on both drivers; the status
// array reporting is asserted separately in Variation 11b because
// msodbcsql mis-indexes it.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, IgnoredParameterSetsAreSkipped) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[4] = {1, 2, 3, 4};
    SQLINTEGER vals[4] = {1, 2, 3, 4};
    SQLLEN ind[4] = {0, 0, 0, 0};
    SQLUSMALLINT operation[4] = {SQL_PARAM_PROCEED, SQL_PARAM_IGNORE,
                                 SQL_PARAM_PROCEED, SQL_PARAM_IGNORE};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 4));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, operation));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("1,3",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), id), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa"));

    SQLLEN affected = -1;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, affected) << "ignored sets contribute no rows";
}

// -------------------------------------------------------------------
// 11b. status[i] must describe parameter set i, and the processed count
// must reach the paramset size, even when the FIRST set is ignored.
//
// This is what msodbcsql's own TestRowWiseParamArraysPaspAfterIgnore
// (MplatNativeTests/gql/paramarray.cpp) asserts. Retail 18.6.2.1
// predates the fixes that made it pass - msodbcsql PR 6629 ("Fix row
// increment logic in OnDone to properly handle SQL_PARAM_IGNORE") and
// PR 6882 ("Fix RowProcessed increment logic") - and instead reports
// status[0] = SQL_PARAM_SUCCESS with processed = 3, shifting every later
// status down one slot. Measured against the installed driver, hence the
// skip on the comparison leg: this asserts the *fixed* contract.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, IgnoredSetStatusesAreReportedPerSet) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[4] = {1, 2, 3, 4};
    SQLINTEGER vals[4] = {1, 2, 3, 4};
    SQLLEN ind[4] = {0, 0, 0, 0};
    SQLUSMALLINT status[4] = {0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 4));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    // Trailing ignores: both drivers agree here.
    SQLUSMALLINT trailing[4] = {SQL_PARAM_PROCEED, SQL_PARAM_IGNORE,
                                SQL_PARAM_PROCEED, SQL_PARAM_IGNORE};
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, trailing));
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[0]);
    EXPECT_EQ(SQL_PARAM_UNUSED, status[1]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[2]);
    EXPECT_EQ(SQL_PARAM_UNUSED, status[3]);
    EXPECT_EQ(4u, processed);

    // Leading ignore: this is where msodbcsql shifts.
    ExecOnProbe("DELETE FROM #pa");
    SQLUSMALLINT leading[4] = {SQL_PARAM_IGNORE, SQL_PARAM_PROCEED,
                               SQL_PARAM_PROCEED, SQL_PARAM_PROCEED};
    for (int i = 0; i < 4; ++i) {
        status[i] = 0xFFFF;
    }
    processed = 0xDEAD;
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, leading));
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_PARAM_UNUSED, status[0]) << "set 0 was the ignored one";
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[1]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[2]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[3]);
    EXPECT_EQ(4u, processed);
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa"));

    // Every set ignored: nothing is sent, every slot is UNUSED.
    ExecOnProbe("DELETE FROM #pa");
    SQLUSMALLINT all_ignored[4] = {SQL_PARAM_IGNORE, SQL_PARAM_IGNORE,
                                   SQL_PARAM_IGNORE, SQL_PARAM_IGNORE};
    for (int i = 0; i < 4; ++i) {
        status[i] = 0xFFFF;
    }
    processed = 0xDEAD;
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, all_ignored));
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa"));
    for (int i = 0; i < 4; ++i) {
        EXPECT_EQ(SQL_PARAM_UNUSED, status[i]) << "set " << i;
    }
    EXPECT_EQ(4u, processed);

    SQLLEN affected = -1;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, affected) << "no set ran, so there is no count at all";
}

// -------------------------------------------------------------------
// 12. SQL_ATTR_PARAM_BIND_OFFSET_PTR displaces the whole array before
// the per-row stride applies, so the batch starts at element 1 rather
// than 0. Both offset and stride must be honoured, and the offset is
// read at execute time.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, BindOffsetShiftsTheWholeArray) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[4] = {10, 20, 30, 40};
    SQLINTEGER vals[4] = {1, 2, 3, 4};
    SQLLEN ind[4] = {0, 0, 0, 0};
    SQLLEN offset = sizeof(SQLINTEGER); // skip element 0 of the value arrays

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 2));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_BIND_OFFSET_PTR, &offset));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    // The offset also shifts the indicator array, which is all zeroes here, so
    // only the values move: sets 0 and 1 read ids[1], ids[2].
    EXPECT_EQ("20,30",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), id), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa"));
}

// -------------------------------------------------------------------
// 13. Row-wise binding: SQL_ATTR_PARAM_BIND_TYPE is the size of one
// application structure, and every pointer - value and indicator alike -
// strides by it. msodbcsql implements this as
// `dwOffset = pADesc->dwBindType` (sqlcfunc.cpp BindOffset).
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, RowWiseBindingWalksStructures) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    struct Row {
        SQLINTEGER id;
        SQLLEN id_ind;
        SQLINTEGER v;
        SQLLEN v_ind;
    };
    Row rows[3] = {};
    for (int i = 0; i < 3; ++i) {
        rows[i].id = i + 1;
        rows[i].id_ind = 0;
        rows[i].v = (i + 1) * 11;
        rows[i].v_ind = 0;
    }

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &rows[0].id, 0,
                                   &rows[0].id_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &rows[0].v, 0,
                                   &rows[0].v_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAM_BIND_TYPE, sizeof(Row)));
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ("11,22,33",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), v), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa"));
}

// -------------------------------------------------------------------
// 14. SQLExecDirect takes the same parameter arrays as SQLExecute; the
// statement is never prepared by the application.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ExecDirectSupportsParameterArrays) {
    ExecDirect(kGuardTable);

    SQLINTEGER ids[3] = {7, 8, 9};
    SQLINTEGER vals[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    SqlTString sql =
        ODBCTestUtils::ToSqlTStr("INSERT INTO #pa (id, v) VALUES (?, ?)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, sql.data(), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa"));
    SQLLEN affected = -1;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, affected);
}

// -------------------------------------------------------------------
// 15. Under manual commit every parameter set joins the same
// transaction, so a rollback discards the whole batch.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayRollsBackWithTheTransaction) {
    ExecDirect("CREATE TABLE #pa_txn (id int)");
    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_OFF),
                                    0),
                  SQL_HANDLE_DBC, dbc_);

    SqlTString sql = ODBCTestUtils::ToSqlTStr("INSERT INTO #pa_txn (id) VALUES (?)");
    ASSERT_SQL_OK(SQLPrepare(stmt_, sql.data(), SQL_NTS), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER ids[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC,
                  dbc_);

    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa_txn"))
        << "every set must be inside the rolled-back transaction";

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_ON),
                                    0),
                  SQL_HANDLE_DBC, dbc_);
}

// -------------------------------------------------------------------
// 16. Under autocommit each set commits on its own, so the sets that ran
// before a failing one are durable. This is the flip side of Variation
// 8 and is what makes partial failure observable to the application.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayCommitsEveryRowUnderAutocommit) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLINTEGER vals[3] = {10, 500, 30};
    SQLLEN ind[3] = {0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));

    // A fresh statement sees the committed effects of the successful sets.
    EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #pa"));
}

// -------------------------------------------------------------------
// 17. DIVERGENCE (mssql-odbc only): msodbcsql sends the whole array as
// one batch, so a row-returning statement yields one result set per set,
// walked with SQLMoreResults. mssql-odbc issues one RPC per set and
// cannot hold N cursors open, so it refuses with HYC00 rather than
// silently discarding the rows. Tracked in docs/attributes_plan.md.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, RowReturningArrayIsRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLINTEGER ids[2] = {1, 2};
    SQLLEN ind[2] = {0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 2));

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    EXPECT_EQ(SQL_ERROR, SQLExecDirect(stmt_, sql.data(), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// -------------------------------------------------------------------
// 17b. DIVERGENCE (mssql-odbc only), and the sharp edge of it: a
// row-returning statement is only detectable after its first set has
// run, because there is no column metadata before the prepare. So
// INSERT ... OUTPUT under autocommit durably writes set 0 and only then
// reports HYC00. This pins how many sets survive so the partial write is
// a recorded decision rather than a surprise. Contrast Variation 19,
// where data-at-execution is caught during conversion and writes nothing.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, RowReturningArrayRefusalLeavesTheFirstSetWritten) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ExecDirect("CREATE TABLE #pa_out (id int)");

    SQLINTEGER ids[4] = {1, 2, 3, 4};
    SQLLEN ind[4] = {0, 0, 0, 0};
    SQLUSMALLINT status[4] = {0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 4));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "INSERT INTO #pa_out (id) OUTPUT inserted.id VALUES (?)");
    EXPECT_EQ(SQL_ERROR, SQLExecDirect(stmt_, sql.data(), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa_out"))
        << "set 0 ran before the shape could be detected";
    EXPECT_EQ(1u, processed);
    EXPECT_EQ(SQL_PARAM_ERROR, status[0]);
}

// -------------------------------------------------------------------
// 19. DIVERGENCE (mssql-odbc only): data-at-execution needs
// SQLParamData/SQLPutData to drive one set at a time, which cannot be
// reconciled with running the whole array inside a single ODBC call
// here. Measured on msodbcsql 18.6: the same binding returns
// SQL_NEED_DATA and parks the connection in the Need Data state (every
// later call on it answers HY010 until the sequence is driven or
// cancelled), so msodbcsql genuinely supports the combination.
// mssql-odbc refuses with HYC00 rather than sending a placeholder with
// no data behind it, and leaves the statement immediately reusable.
// mssql-python already falls back to a scalar row loop for its DAE path
// (SQLExecuteMany_wrap in ddbc_bindings.cpp), so this combination is
// unreachable from the Python driver.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, DataAtExecutionWithArrayIsRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ExecDirect("CREATE TABLE #pa_dae (s varchar(20))");
    Prepare("INSERT INTO #pa_dae (s) VALUES (?)");

    SQLLEN dae_ind[2] = {SQL_LEN_DATA_AT_EXEC(3), SQL_LEN_DATA_AT_EXEC(3)};
    SQLCHAR token[2] = {1, 2};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 20, 0, token, 1, dae_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 2));

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa_dae"));
}

// -------------------------------------------------------------------
// 20. SQL_ATTR_QUERY_TIMEOUT bounds the WHOLE call, not each parameter
// set. Five sets of roughly one second each against a two-second budget
// must stop partway with HYT00; a driver that restarted the budget per
// set would run all five and report success.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, QueryTimeoutBoundsTheWholeArray) {
    ExecDirect("CREATE TABLE #pa_slow (v int)");
    Prepare("INSERT INTO #pa_slow (v) VALUES (?); WAITFOR DELAY '00:00:01'");

    SQLINTEGER vals[5] = {1, 2, 3, 4, 5};
    SQLLEN ind[5] = {0, 0, 0, 0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 5));
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_QUERY_TIMEOUT, 2));

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYT00");

    const SQLINTEGER written = ScalarInt("SELECT COUNT(*) FROM #pa_slow");
    EXPECT_LT(written, 5) << "the budget must cut the array short";
}

// -------------------------------------------------------------------
// 21. msodbcsql's own BindColumns (MplatNativeTests/gql/paramarray.cpp)
// binds every column-wise array with a NULL StrLen_or_IndPtr. There is
// then no indicator array to stride at all, and the character parameter
// falls back to "NUL-terminated". A driver that unconditionally advanced
// the indicator pointer by sizeof(SQLLEN) per set would turn that null
// into a wild pointer.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ColumnWiseArrayWithNullIndicatorPointers) {
    ExecDirect("CREATE TABLE #pa_ni (id int, s char(1))");
    Prepare("INSERT INTO #pa_ni (id, s) VALUES (?, ?)");

    SQLINTEGER ids[3] = {10, 20, 30};
    // One character plus a NUL each, exactly as msodbcsql's rgcCharActual.
    SQLCHAR chars[3][2] = {{'a', 0}, {'b', 0}, {'c', 0}};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_CHAR, 1, 0, chars, sizeof(chars[0]),
                                   nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_ni"));
    EXPECT_EQ("a,b,c",
              ScalarString("SELECT STRING_AGG(s, ',') WITHIN GROUP (ORDER BY id) "
                           "FROM #pa_ni"));
    EXPECT_EQ("10,20,30",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), id), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa_ni"));
}

// -------------------------------------------------------------------
// 22. msodbcsql's row-wise RowStruct interleaves five different C types
// with a per-member SQLLEN indicator. Every member sits at a different
// offset inside the structure, so a wrong row stride corrupts one column
// while leaving the others plausible - which the single-type row-wise
// case (Variation 13) cannot catch.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, RowWiseArrayWalksMixedTypeStructures) {
    ExecDirect("CREATE TABLE #pa_rw (c1 int, c2 char(1), c3 date, c4 real, "
               "c5 time(0))");
    Prepare("INSERT INTO #pa_rw (c1, c2, c3, c4, c5) VALUES (?, ?, ?, ?, ?)");

    // Mirrors msodbcsql's RowStruct: value/indicator pairs, mixed widths.
    struct RowStruct {
        SQLINTEGER c1;
        SQLLEN c1Ind;
        SQLCHAR c2[2];
        SQLLEN c2Ind;
        SQL_DATE_STRUCT c3;
        SQLLEN c3Ind;
        SQLREAL c4;
        SQLLEN c4Ind;
        SQL_TIME_STRUCT c5;
        SQLLEN c5Ind;
    };

    constexpr int kRows = 3;
    RowStruct rows[kRows] = {};
    for (int i = 0; i < kRows; ++i) {
        rows[i].c1 = (i + 1) * 100;
        rows[i].c1Ind = 0;
        rows[i].c2[0] = static_cast<SQLCHAR>('x' + i);
        rows[i].c2[1] = 0;
        rows[i].c2Ind = SQL_NTS;
        rows[i].c3.year = static_cast<SQLSMALLINT>(2021 + i);
        rows[i].c3.month = static_cast<SQLUSMALLINT>(i + 1);
        rows[i].c3.day = static_cast<SQLUSMALLINT>(10 + i);
        rows[i].c3Ind = 0;
        rows[i].c4 = static_cast<SQLREAL>(i) + 0.5f;
        rows[i].c4Ind = 0;
        rows[i].c5.hour = static_cast<SQLUSMALLINT>(1 + i);
        rows[i].c5.minute = static_cast<SQLUSMALLINT>(2 + i);
        rows[i].c5.second = static_cast<SQLUSMALLINT>(3 + i);
        rows[i].c5Ind = 0;
    }

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &rows[0].c1, 0,
                                   &rows[0].c1Ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_CHAR, 1, 0, &rows[0].c2,
                                   sizeof(rows[0].c2), &rows[0].c2Ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 3, SQL_PARAM_INPUT, SQL_C_TYPE_DATE,
                                   SQL_TYPE_DATE, 10, 0, &rows[0].c3, 0,
                                   &rows[0].c3Ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 4, SQL_PARAM_INPUT, SQL_C_FLOAT,
                                   SQL_REAL, 7, 0, &rows[0].c4, 0,
                                   &rows[0].c4Ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 5, SQL_PARAM_INPUT, SQL_C_TYPE_TIME,
                                   SQL_TYPE_TIME, 8, 0, &rows[0].c5, 0,
                                   &rows[0].c5Ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS,
              SetStmtULen(SQL_ATTR_PARAM_BIND_TYPE, sizeof(RowStruct)));
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, kRows));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(kRows, ScalarInt("SELECT COUNT(*) FROM #pa_rw"));
    EXPECT_EQ("100,200,300",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), c1), ',') "
                           "WITHIN GROUP (ORDER BY c1) FROM #pa_rw"));
    EXPECT_EQ("x,y,z",
              ScalarString("SELECT STRING_AGG(c2, ',') WITHIN GROUP "
                           "(ORDER BY c1) FROM #pa_rw"));
    EXPECT_EQ("2021-01-10,2022-02-11,2023-03-12",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(10), c3, 23), ',') "
                           "WITHIN GROUP (ORDER BY c1) FROM #pa_rw"));
    EXPECT_EQ("01:02:03,02:03:04,03:04:05",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(8), c5), ',') "
                           "WITHIN GROUP (ORDER BY c1) FROM #pa_rw"));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #pa_rw WHERE c4 = 2.5"));
}

// -------------------------------------------------------------------
// 23. Ported from msodbcsql's RegressionsODBC Variation_76
// ("SQL_ATTR_PARAMS_PROCESSED_PTR on server-side error with row-wise
// parameter arrays and NaN rejected by CHECK constraint"). Six row-wise
// sets, the fourth carrying NaN.
//
// Shared contract: the set that failed is marked SQL_PARAM_ERROR and is
// itself counted as processed, and a bound status array downgrades the
// return to SQL_SUCCESS_WITH_INFO.
//
// Measured on both drivers: NaN raises SQLSTATE 42000, a *batch-aborting*
// float domain error. msodbcsql serialises all six sets into one TDS
// batch, so the abort kills the remainder and its counter stops at 4.
// mssql-odbc sends one RPC per set, so the abort only kills set 4 and the
// remaining sets still run, giving 6. Both report "sets attempted"; the
// number differs purely because of the wire shape recorded in
// docs/attributes_plan.md.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ParamsProcessedCountsTheFailingSet) {
    ExecDirect("CREATE TABLE #pa_nan (val real NOT NULL, "
               "CONSTRAINT CK_NoNaN CHECK (val = val))");

    struct RowData {
        SQLREAL val;
        SQLLEN valInd;
    };

    constexpr SQLULEN kParamSetSize = 6;
    constexpr SQLUSMALLINT kSentinel = 0xFFFF;
    const float nan_value = std::numeric_limits<float>::quiet_NaN();
    const float values[kParamSetSize] = {1.0f, 1.0f, 3.0f, nan_value, 4.0f, 5.0f};

    RowData rows[kParamSetSize] = {};
    SQLUSMALLINT paramOps[kParamSetSize] = {};
    SQLUSMALLINT paramStatus[kParamSetSize] = {};
    SQLULEN paramsProcessed = 0;
    for (SQLULEN i = 0; i < kParamSetSize; ++i) {
        rows[i].val = values[i];
        rows[i].valInd = 0;
        paramOps[i] = SQL_PARAM_PROCEED;
        paramStatus[i] = kSentinel;
    }

    ASSERT_EQ(SQL_SUCCESS,
              SetStmtULen(SQL_ATTR_PARAM_BIND_TYPE, sizeof(RowData)));
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, kParamSetSize));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, paramOps));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, paramStatus));
    ASSERT_EQ(SQL_SUCCESS,
              SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &paramsProcessed));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_FLOAT,
                                   SQL_REAL, 0, 0, &rows[0].val, 0,
                                   &rows[0].valInd),
                  SQL_HANDLE_STMT, stmt_);

    SqlTString sql =
        ODBCTestUtils::ToSqlTStr("INSERT INTO #pa_nan (val) VALUES (?)");
    const SQLRETURN rc = SQLExecDirect(stmt_, sql.data(), SQL_NTS);
    EXPECT_TRUE(rc == SQL_ERROR || rc == SQL_SUCCESS_WITH_INFO)
        << "NaN must be rejected, rc=" << rc;

    EXPECT_EQ(SQL_PARAM_ERROR, paramStatus[3]);
    EXPECT_GE(paramsProcessed, 4u)
        << "the counter must reach at least the failing set's ordinal";

    // The exact tail is wire-shape dependent - see the comment above.
    SKIP_IF_COMPARING_MSODBCSQL();
    EXPECT_EQ(kParamSetSize, paramsProcessed);
    EXPECT_EQ(SQL_PARAM_SUCCESS, paramStatus[4]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, paramStatus[5]);
    EXPECT_EQ(5, ScalarInt("SELECT COUNT(*) FROM #pa_nan"));
}

// -------------------------------------------------------------------
// 18. A zero BufferLength gives every set the same address, so an array
// insert writes the first value N times. That is msodbcsql's behaviour
// (BindOffset uses cbValueMax verbatim) and is asserted here so the
// inherited quirk is a decision on record rather than an accident.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayWithZeroBufferLengthAliasesOneValue) {
    ExecDirect("CREATE TABLE #pa_z (s varchar(10))");

    char text[3][8] = {"aaa", "bbb", "ccc"};
    SQLLEN ind[3] = {SQL_NTS, SQL_NTS, SQL_NTS};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 10, 0, text, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    SqlTString sql = ODBCTestUtils::ToSqlTStr("INSERT INTO #pa_z (s) VALUES (?)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, sql.data(), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_z"));
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_z WHERE s = 'aaa'"))
        << "a zero stride makes every set read the same buffer";
}

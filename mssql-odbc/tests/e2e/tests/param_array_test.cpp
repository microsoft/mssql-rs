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
// (file/line citations are given on each case below):
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
//   9b. EverySetFailingServerSideStillDowngradesWithAStatusArray
//  10.  ParamsProcessedCountsEverySet             - including skipped sets
//  11.  IgnoredParameterSetsAreSkipped            - SQL_PARAM_IGNORE
//  11b. IgnoredSetStatusesAreReportedPerSet       - divergence: msodbcsql shifts
//  11c. UnknownOperationValueProceedsLikeProceed  - only IGNORE skips
//  12.  BindOffsetShiftsTheWholeArray             - offset + stride together
//  13.  RowWiseBindingWalksStructures             - SQL_ATTR_PARAM_BIND_TYPE
//  14.  ExecDirectParameterArrayIsRefused         - divergence: AB#47939
//  14b. ExecDirectArrayWithNoMarkersIsRefused     - same refusal, no markers
//  15.  ArrayRollsBackWithTheTransaction          - manual-commit semantics
//  16.  ArrayCommitsEveryRowUnderAutocommit       - partial-failure durability
//       (17 was removed: it asserted nothing the other cases did not)
//  18.  ArrayWithZeroBufferLengthAliasesOneValue  - inherited msodbcsql quirk
//  19.  DataAtExecutionWithArrayIsRefused         - documented divergence
//  20.  QueryTimeoutBoundsTheWholeArray           - one budget for all sets
//  21.  ColumnWiseArrayWithNullIndicatorPointers  - msodbcsql BindColumns shape
//  22.  RowWiseArrayWalksMixedTypeStructures      - msodbcsql RowStruct shape
//  23.  ParamsProcessedCountsTheFailingSet        - msodbcsql Variation_76
//  24.  RowWiseArrayHonorsIgnoreForSkippedSets    - row-wise x SQL_PARAM_IGNORE
//  25.  RowReturningArrayOnThePreparedPath        - divergence: AB#47944
//  26.  ConversionFailureIsReportedPerSetWhereverItSits - divergence: AB#47945
//  26b. EverySetFailingToConvertIsError          - total client-side failure
//  27.  AnInfoMessageDegradesTheBatchOnBothDrivers - info-token parity
//  28.  ArrayOfAThousandSetsWritesEveryRowInOrder  - depth: packing, drain, order
//  29.  ThousandSetArrayCorrelatesFailuresToTheirOwnSets - boundary sets 0/499/999
//  30.  CursorApiAfterARowReturningArrayExecute    - AB#47944 as the app sees it
//  31.  PreparedStatementReExecutesAtDifferentArraySizes - 4 -> 7 -> 2

#include "odbc_test_fixture.h"

#include <cstdlib>
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

// Lets a case keep its portable assertions on the msodbcsql leg while dropping
// only the ones that depend on a fix retail has not shipped.
bool ComparingMsodbcsql() {
    const char* target = std::getenv("ODBC_TEST_TARGET");
    return target && std::string(target) == "msodbcsql";
}

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
    // u"" not L"": SQLWCHAR is UTF-16 everywhere, but wchar_t is 4 bytes on
    // Linux, so memcpy of a L"" literal lands UTF-32 in a UTF-16 buffer and the
    // value terminates after its first character. Silent wrong data, so the
    // width these memcpys assume is checked rather than left to the comment.
    static_assert(sizeof(SQLWCHAR) == sizeof(char16_t),
                  "SQLWCHAR must be 2 bytes for these u\"\" literals to copy");
    std::memcpy(names[0], u"one", 4 * sizeof(char16_t));
    std::memcpy(names[1], u"two", 4 * sizeof(char16_t));
    std::memcpy(names[2], u"three", 6 * sizeof(char16_t));
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
// 9b. The downgrade above is not limited to *partial* failure.
// Measured on msodbcsql 18.6.2.1: a 3-set batch where every set
// violates a CHECK constraint still returns SQL_SUCCESS_WITH_INFO when
// a status array is bound - its rule (sqlctokn.cpp:2341-2360) has no
// all-failed branch, because the sets did run and the status array
// describes each one. Runs on both legs: this is parity.
//
// Total *client-side* failure is different and is SQL_ERROR on both
// drivers, because nothing reaches the wire and no set runs. The status
// array is still written - see case 26b.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, EverySetFailingServerSideStillDowngradesWithAStatusArray) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    // Every value violates the CHECK constraint, so no set can commit.
    SQLINTEGER vals[3] = {500, 600, 700};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLExecute(stmt_))
        << "a bound status array downgrades even when no set survived";
    SQLFreeStmt(stmt_, SQL_CLOSE);

    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(SQL_PARAM_ERROR, status[i]) << "set " << i;
    }
    EXPECT_EQ(3u, processed);
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa"));
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
// 11c. Only SQL_PARAM_IGNORE skips a set. msodbcsql tests for that one
// value and lets every other bit pattern through (sqlccmd.cpp:3218,
// :6605, sqlctokn.cpp:2396) rather than validating the operation array,
// so a garbage entry proceeds instead of failing the set. Measured on
// both drivers, hence no skip: this is parity, not a deviation.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, UnknownOperationValueProceedsLikeProceed) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLINTEGER vals[3] = {10, 20, 30};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;
    // 7 is neither SQL_PARAM_PROCEED (0) nor SQL_PARAM_IGNORE (1).
    SQLUSMALLINT operations[3] = {SQL_PARAM_PROCEED, 7, SQL_PARAM_PROCEED};

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, operations));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLFreeStmt(stmt_, SQL_CLOSE);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa"))
        << "an unrecognised operation value must not skip or fail its set";
    EXPECT_EQ(6, ScalarInt("SELECT SUM(id) FROM #pa"));
    EXPECT_EQ(3u, processed);
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(SQL_PARAM_SUCCESS, status[i]) << "set " << i;
    }
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
// 14. Divergence: msodbcsql takes parameter arrays on SQLExecDirect too,
// batching one sp_executesql per set (sqlccmd.cpp:3310). mssql-odbc
// refuses with HYC00 until AB#47939 wires that up - nothing shipped
// drives it (mssql-python's executemany always prepares first), and
// running only the first set would lose the rest silently.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ExecDirectParameterArrayIsRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();
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
    EXPECT_EQ(SQL_ERROR, SQLExecDirect(stmt_, sql.data(), SQL_NTS));
    EXPECT_EQ("HYC00", ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_));
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa"))
        << "a refused array must not execute its first set";
}

// -------------------------------------------------------------------
// 14b. The same refusal with no parameter markers at all. msodbcsql
// sizes the loop from dwArraySize alone (sqlccmd.cpp:3192-3199), so the
// statement runs PARAMSET_SIZE times there. Executing it once here would
// drop the other sets with no diagnostic - refuse instead.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ExecDirectArrayWithNoMarkersIsRefused) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect(kGuardTable);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    SqlTString sql =
        ODBCTestUtils::ToSqlTStr("INSERT INTO #pa (id, v) VALUES (1, 1)");
    EXPECT_EQ(SQL_ERROR, SQLExecDirect(stmt_, sql.data(), SQL_NTS));
    EXPECT_EQ("HYC00", ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_));
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa"))
        << "refusing must not leave one set executed";
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
// NaN raises SQLSTATE 42000, a *batch-aborting* float domain error, and
// both drivers pack every set into one TDS batch - so the abort kills the
// remainder: the counter stops at the failing set, the sets after it are
// left SQL_PARAM_UNUSED, and only the sets before it are written.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ParamsProcessedCountsTheFailingSet) {
    ExecDirect("CREATE TABLE #pa_nan (val real NOT NULL, "
               "CONSTRAINT CK_NoNaN CHECK (val = val))");
    // Prepared, not SQLExecDirect: arrays are refused on that path (case 14),
    // which would mask the batch-abort behaviour this case exists to pin down.
    Prepare("INSERT INTO #pa_nan (val) VALUES (?)");

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

    const SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_TRUE(rc == SQL_ERROR || rc == SQL_SUCCESS_WITH_INFO)
        << "NaN must be rejected, rc=" << rc;

    EXPECT_EQ(SQL_PARAM_SUCCESS, paramStatus[0]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, paramStatus[1]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, paramStatus[2]);
    EXPECT_EQ(SQL_PARAM_ERROR, paramStatus[3]);
    EXPECT_EQ(4u, paramsProcessed)
        << "the counter stops at the set whose error aborted the batch";
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_nan"))
        << "only the sets before the aborting one are written";

    // mssql-odbc pre-fills the whole status array with SQL_PARAM_UNUSED before
    // executing, so the sets the abort skipped say so. msodbcsql only writes
    // the slots it reached and leaves the rest as the caller left them.
    if (ComparingMsodbcsql()) {
        return;
    }
    EXPECT_EQ(SQL_PARAM_UNUSED, paramStatus[4]);
    EXPECT_EQ(SQL_PARAM_UNUSED, paramStatus[5]);
}

// -------------------------------------------------------------------
// 18. A zero BufferLength gives every set the same address, so an array
// insert writes the first value N times. That is msodbcsql's behaviour
// (BindOffset uses cbValueMax verbatim) and is asserted here so the
// inherited quirk is a decision on record rather than an accident.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayWithZeroBufferLengthAliasesOneValue) {
    ExecDirect("CREATE TABLE #pa_z (s varchar(10))");
    Prepare("INSERT INTO #pa_z (s) VALUES (?)");

    char text[3][8] = {"aaa", "bbb", "ccc"};
    SQLLEN ind[3] = {SQL_NTS, SQL_NTS, SQL_NTS};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 10, 0, text, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_z"));
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_z WHERE s = 'aaa'"))
        << "a zero stride makes every set read the same buffer";
}

// -------------------------------------------------------------------
// 24. Row-wise binding and SQL_PARAM_IGNORE together. 11 and 13 cover the
// two separately; only their combination pins down that a skipped set
// still advances the struct stride, so the sets after it read their own
// row instead of sliding into the gap. Ported from PR #512's
// ParameterArrayRowWiseHonorsIgnore.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, RowWiseArrayHonorsIgnoreForSkippedSets) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    struct Row {
        SQLINTEGER id;
        SQLLEN id_ind;
        SQLINTEGER v;
        SQLLEN v_ind;
    };
    constexpr SQLULEN kSets = 4;
    Row rows[kSets] = {};
    for (SQLULEN i = 0; i < kSets; ++i) {
        rows[i].id = static_cast<SQLINTEGER>(i + 1);
        rows[i].id_ind = 0;
        rows[i].v = static_cast<SQLINTEGER>((i + 1) * 11);
        rows[i].v_ind = 0;
    }

    SQLUSMALLINT ops[kSets] = {SQL_PARAM_PROCEED, SQL_PARAM_IGNORE,
                               SQL_PARAM_PROCEED, SQL_PARAM_PROCEED};
    constexpr SQLUSMALLINT kSentinel = 0xFFFF;
    SQLUSMALLINT status[kSets] = {kSentinel, kSentinel, kSentinel, kSentinel};
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &rows[0].id, 0,
                                   &rows[0].id_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, &rows[0].v, 0,
                                   &rows[0].v_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAM_BIND_TYPE, sizeof(Row)));
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, kSets));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_OPERATION_PTR, ops));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS,
              SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa WHERE id = 2"));
    EXPECT_EQ("11,33,44",
              ScalarString("SELECT STRING_AGG(CONVERT(varchar(11), v), ',') "
                           "WITHIN GROUP (ORDER BY id) FROM #pa"))
        << "the skipped set must advance the stride, not shift later sets";

    SQLLEN affected = -1;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, affected);

    // Retail 18.6.2.1 predates msodbcsql 63a70fb07, so its ignored-set
    // bookkeeping still shifts; 11b records that divergence in full.
    if (ComparingMsodbcsql()) {
        return;
    }
    EXPECT_EQ(kSets, processed);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[0]);
    EXPECT_EQ(SQL_PARAM_UNUSED, status[1]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[2]);
    EXPECT_EQ(SQL_PARAM_SUCCESS, status[3]);
}

// -------------------------------------------------------------------
// 25. A row-returning statement executed once per parameter set.
//
// A "parameter set" is one row of the bound arrays: PARAMSET_SIZE = 3
// over ids = {1, 2, 3} is three sets, and the statement runs three
// times, once per set. "Row-returning" means each of those runs produces
// a result set of its own - here INSERT ... OUTPUT inserted.id hands
// back the id it just wrote - so three runs produce three result sets,
// while ODBC exposes only one current result set per statement handle.
//
// Both drivers run every set, so no set is silently skipped. mssql-odbc
// discards the OUTPUT rows and reports each set SQL_PARAM_SUCCESS_WITH_INFO
// with a 01000 warning; delivering the rows is AB#47944. Measured on
// msodbcsql 18.6.2.1: SQL_SUCCESS, the status array never written, and
// *PARAMS_PROCESSED_PTR left at the scalar value 1 despite three sets
// running - hence the skip rather than a shared assertion.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, RowReturningArrayOnThePreparedPath) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("CREATE TABLE #pa_out2 (id int)");
    Prepare("INSERT INTO #pa_out2 (id) OUTPUT inserted.id VALUES (?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    const SQLRETURN rc = SQLExecute(stmt_);
    // Any later call on this handle clears its diagnostics, so read them first.
    const std::string diag = ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
    SQLFreeStmt(stmt_, SQL_CLOSE);

    // Every set ran, and the connection survives the discarded result sets.
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_out2"));
    EXPECT_EQ(1, ScalarInt("SELECT 1"));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_EQ(3u, processed);
    // The set ran - its row is in the table asserted above - so it is a success
    // carrying a 01000 warning that the OUTPUT rows were discarded. Reporting
    // SQL_PARAM_ERROR would invite a retry that double-inserts.
    EXPECT_EQ(SQL_PARAM_SUCCESS_WITH_INFO, status[0]);
    EXPECT_EQ(SQL_PARAM_SUCCESS_WITH_INFO, status[1]);
    EXPECT_EQ(SQL_PARAM_SUCCESS_WITH_INFO, status[2]);
    EXPECT_EQ("01000", diag);
}

// -------------------------------------------------------------------
// 26. DIVERGENCE (mssql-odbc only): a set the driver itself cannot build
// reports like any other failing set and does not stop the sets around
// it, wherever it sits. Case 8 covers only a server-side CHECK violation,
// which never exercises the client-side build path; set 0 additionally
// used to abort the whole batch because it also carried the parameter
// declaration.
//
// Measured on msodbcsql 18.6.2.1: a client-side conversion failure stops
// the array - SQL_ERROR, the status array untouched, *processed = the
// 1-based index of the failing set, and nothing written. Its
// continue-after-error rule (sqlctokn.cpp:2341) governs server ERROR
// tokens, not sets it could not convert.
//
// This driver has already streamed the earlier sets onto the wire by the
// time the failing one is converted, so they commit. That is partial
// success, so the return code stays SQL_SUCCESS_WITH_INFO with the detail
// in the status array - the same answer this suite asserts for a
// server-side partial failure in case 9. msodbcsql's SQL_ERROR is a
// total-failure code and does not transfer to a batch that committed
// rows: a caller that reads it as "nothing ran" and retries double-inserts.
// Recorded in AB#47945.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ConversionFailureIsReportedPerSetWhereverItSits) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("CREATE TABLE #pa_cv (v int)");

    constexpr SQLLEN kStride = 8;
    for (int bad_row = 0; bad_row < 3; ++bad_row) {
        ExecOnProbe("DELETE FROM #pa_cv");
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
        Prepare("INSERT INTO #pa_cv (v) VALUES (?)");

        char text[3 * kStride] = {};
        for (int i = 0; i < 3; ++i) {
            std::strcpy(&text[i * kStride], i == bad_row ? "abc" : (i == 0   ? "10"
                                                                   : i == 1 ? "11"
                                                                            : "12"));
        }
        SQLLEN ind[3] = {SQL_NTS, SQL_NTS, SQL_NTS};
        SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
        SQLULEN processed = 0xDEAD;

        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       SQL_INTEGER, 10, 0, text, kStride, ind),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
        ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
        ASSERT_EQ(SQL_SUCCESS,
                  SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

        const SQLRETURN rc = SQLExecute(stmt_);
        const std::string state = StmtDiagState();
        SQLFreeStmt(stmt_, SQL_CLOSE);

        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc)
            << "sets committed ahead of the failure make this partial success, "
               "which the status array describes; SQL_ERROR would invite a "
               "retry that double-inserts them, bad set "
            << bad_row;
        EXPECT_EQ("22018", state)
            << "the failing set has to say why, bad set " << bad_row;
        EXPECT_EQ(SQL_PARAM_ERROR, status[bad_row]) << "bad set " << bad_row;
        EXPECT_EQ(3u, processed) << "bad set " << bad_row;
        EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #pa_cv"))
            << "a set the driver cannot build must not stop the others, bad set "
            << bad_row;
        for (int i = 0; i < 3; ++i) {
            if (i != bad_row) {
                EXPECT_EQ(SQL_PARAM_SUCCESS, status[i])
                    << "set " << i << ", bad set " << bad_row;
            }
        }
    }
}

// -------------------------------------------------------------------
// 26b. When *every* set fails to convert, no set reaches the wire, so
// nothing ran and the call is SQL_ERROR - not the SQL_SUCCESS_WITH_INFO
// a bound status array buys a partly-committed batch (case 9b).
//
// The return code and the empty table are shared: msodbcsql sends
// nothing on any client-side failure, and here neither do we. The status
// array is not shared - measured, msodbcsql leaves it untouched (0xFFFF
// sentinel) because it stops before writing, while this driver marks
// every set SQL_PARAM_ERROR. That write is why the return code cannot be
// justified by "no per-set detail to report"; it is justified by nothing
// having executed.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, EverySetFailingToConvertIsError) {
    ExecDirect("CREATE TABLE #pa_cv_all (v int)");
    Prepare("INSERT INTO #pa_cv_all (v) VALUES (?)");

    constexpr SQLLEN kStride = 8;
    char text[3 * kStride] = {};
    for (int i = 0; i < 3; ++i) {
        std::strcpy(&text[i * kStride], "abc");
    }
    SQLLEN ind[3] = {SQL_NTS, SQL_NTS, SQL_NTS};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_INTEGER, 10, 0, text, kStride, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_))
        << "no set reached the wire, so the batch did not partly succeed";
    EXPECT_EQ("22018", StmtDiagState())
        << "the caller has to be able to find out why, not just that it failed";
    SQLFreeStmt(stmt_, SQL_CLOSE);

    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa_cv_all"));

    if (ComparingMsodbcsql()) {
        // Divergence, AB#47945: msodbcsql stops before writing any status.
        for (int i = 0; i < 3; ++i) {
            EXPECT_EQ(0xFFFF, status[i]) << "set " << i << " left untouched";
        }
        return;
    }
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(SQL_PARAM_ERROR, status[i]) << "set " << i;
    }
}

// -------------------------------------------------------------------
// 27. A set that raises a server info message degrades the batch,
// identically on both drivers: SQL_SUCCESS_WITH_INFO with every set
// reported SQL_PARAM_SUCCESS_WITH_INFO, even though all three rows
// committed. Success-with-info is the ODBC answer whenever a diagnostic
// exists, so this is parity, not degradation.
//
// Settles the question AB#47945 left open. Two earlier attempts measured
// "no diagnostic at all" because they raised the message from a trigger
// on a temp table, which SQL Server refuses; the CREATE was failing
// unnoticed. An aggregate over a NULL needs no trigger, so the case also
// keeps every object session-scoped like the rest of the suite.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, AnInfoMessageDegradesTheBatchOnBothDrivers) {
    ExecDirect("CREATE TABLE #pa_info (v int)");
    // SUM over a NULL raises 01003 "Null value is eliminated by an aggregate"
    // once per execution. A trigger would do too, but SQL Server refuses
    // triggers on temp tables and this suite keeps every object session-scoped.
    Prepare("INSERT INTO #pa_info (v) "
            "SELECT SUM(n) FROM (VALUES (?), (CAST(NULL AS int))) AS t(n)");

    SQLINTEGER vals[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    const SQLRETURN rc = SQLExecute(stmt_);
    SQLFreeStmt(stmt_, SQL_CLOSE);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc)
        << "the aggregate warning is a 01003 diagnostic, so the call is not clean";
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_info"))
        << "every set still committed";
    EXPECT_EQ(3u, processed);
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(SQL_PARAM_SUCCESS_WITH_INFO, status[i]) << "set " << i;
    }
}

// -------------------------------------------------------------------
// 28. Depth. Every other case in this file runs at PARAMSET_SIZE <= 7,
// which fits one TDS packet and one pass of the drain loop. The whole
// mechanism this PR introduces is packing N sp_execute RPCs into a
// single request, so the interesting failures - packet fragmentation in
// the batch serializer, the drain loop running past the first packet,
// and row-count accumulation across many DONEs - only appear at depth.
//
// The IDENTITY column is the point: seq is assigned in insertion order,
// so `seq <> id` finds any set executed out of order or duplicated,
// which a COUNT or a SUM would both miss.
//
// Every assertion here holds on both drivers; measured on msodbcsql 18.6.2.1.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ArrayOfAThousandSetsWritesEveryRowInOrder) {
    constexpr int kSets = 1000;
    ExecDirect("CREATE TABLE #pa_big (seq int IDENTITY(1,1) NOT NULL, id int NOT NULL)");
    Prepare("INSERT INTO #pa_big (id) VALUES (?)");

    std::vector<SQLINTEGER> ids(kSets);
    std::vector<SQLLEN> ind(kSets, 0);
    std::vector<SQLUSMALLINT> status(kSets, 0xFFFF);
    for (int i = 0; i < kSets; ++i) {
        ids[static_cast<size_t>(i)] = i + 1;
    }
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids.data(), 0, ind.data()),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, kSets));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status.data()));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLLEN affected = -12345;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(kSets, affected) << "one row per set, summed across the batch";

    EXPECT_EQ(static_cast<SQLULEN>(kSets), processed);
    int bad_status = -1;
    for (int i = 0; i < kSets; ++i) {
        if (status[static_cast<size_t>(i)] != SQL_PARAM_SUCCESS) {
            bad_status = i;
            break;
        }
    }
    EXPECT_EQ(-1, bad_status) << "first set not reported SQL_PARAM_SUCCESS";

    EXPECT_EQ(kSets, ScalarInt("SELECT COUNT(*) FROM #pa_big"));
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa_big WHERE seq <> id"))
        << "sets must execute in order, exactly once each";
}

// -------------------------------------------------------------------
// 29. Failure-to-set correlation at depth, on the boundaries.
//
// Mapping a server error back to the set that caused it is index
// arithmetic this PR introduced. At three sets an off-by-one is
// invisible - it lands on a neighbour that is also being asserted - so
// this drives 1000 sets and fails the first, a middle, and the last.
// Set i carries id i+1, so the failing ids are 1, 500 and 1000: the
// table check catches a shift that the status array alone would not.
//
// Every assertion here holds on both drivers; measured on msodbcsql 18.6.2.1.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, ThousandSetArrayCorrelatesFailuresToTheirOwnSets) {
    constexpr int kSets = 1000;
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    std::vector<SQLINTEGER> ids(kSets);
    std::vector<SQLINTEGER> vals(kSets, 50); // under the CHECK (v < 100)
    std::vector<SQLLEN> ind(kSets, 0);
    std::vector<SQLUSMALLINT> status(kSets, 0xFFFF);
    for (int i = 0; i < kSets; ++i) {
        ids[static_cast<size_t>(i)] = i + 1;
    }
    const int failing[3] = {0, 499, 999};
    for (int index : failing) {
        vals[static_cast<size_t>(index)] = 500; // violates CHECK (v < 100)
    }
    SQLULEN processed = 0xDEAD;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids.data(), 0, ind.data()),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals.data(), 0, ind.data()),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, kSets));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status.data()));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    // A bound status array downgrades the failure (case 9), on both drivers.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLExecute(stmt_));
    SQLFreeStmt(stmt_, SQL_CLOSE);

    EXPECT_EQ(static_cast<SQLULEN>(kSets), processed);
    for (int index : failing) {
        EXPECT_EQ(SQL_PARAM_ERROR, status[static_cast<size_t>(index)])
            << "set " << index << " violates the CHECK";
    }
    int wrongly_marked = -1;
    for (int i = 0; i < kSets; ++i) {
        const bool expected_to_fail =
            (i == failing[0] || i == failing[1] || i == failing[2]);
        if (!expected_to_fail && status[static_cast<size_t>(i)] != SQL_PARAM_SUCCESS) {
            wrongly_marked = i;
            break;
        }
    }
    EXPECT_EQ(-1, wrongly_marked) << "first healthy set not reported SQL_PARAM_SUCCESS";

    EXPECT_EQ(kSets - 3, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #pa WHERE id IN (1, 500, 1000)"))
        << "the rows missing must be the ones whose sets were marked failed";
}

// -------------------------------------------------------------------
// 30. What an application actually sees on the cursor API after a
// row-returning array execute. Case 25 asserts the statuses and that the
// connection survives, but nothing in this suite touches
// SQLNumResultCols / SQLFetch / SQLMoreResults afterwards - which is the
// app-visible shape of the AB#47944 divergence.
//
// Pins measured behaviour rather than a desired contract: mssql-odbc
// discards the OUTPUT rows, so the handle carries no result set and the
// cursor calls report exactly that.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, CursorApiAfterARowReturningArrayExecute) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("CREATE TABLE #pa_out3 (id int)");
    Prepare("INSERT INTO #pa_out3 (id) OUTPUT inserted.id VALUES (?)");

    SQLINTEGER ids[3] = {1, 2, 3};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN processed = 0xDEAD;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_PARAMSET_SIZE, 3));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, SQLExecute(stmt_));

    SQLSMALLINT columns = -1;
    const SQLRETURN cols_rc = SQLNumResultCols(stmt_, &columns);
    const std::string cols_diag =
        ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);

    const SQLRETURN fetch_rc = SQLFetch(stmt_);
    const std::string fetch_diag =
        ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);

    const SQLRETURN more_rc = SQLMoreResults(stmt_);
    const std::string more_diag =
        ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);

    SQLFreeStmt(stmt_, SQL_CLOSE);

    // The OUTPUT rows were discarded, so the handle has no result set left.
    EXPECT_EQ(SQL_SUCCESS, cols_rc);
    EXPECT_EQ(0, columns) << "no result set is current after the array execute";
    EXPECT_TRUE(cols_diag.empty()) << "unexpected diagnostic: " << cols_diag;

    EXPECT_EQ(SQL_ERROR, fetch_rc) << "there is no cursor to fetch from";
    EXPECT_EQ("24000", fetch_diag);

    EXPECT_EQ(SQL_NO_DATA, more_rc) << "no further result sets are queued";
    EXPECT_TRUE(more_diag.empty()) << "unexpected diagnostic: " << more_diag;

    // The statement is still usable and every set really ran.
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #pa_out3"));
}

// -------------------------------------------------------------------
// 31. One prepared statement re-executed at three different array sizes.
//
// Case 11b re-executes three times but always at size 4 with the same
// bindings, so the sp_prepare handle is only ever reused at its original
// width. Growing then shrinking is this PR's surface: the batch is built
// from a declaration cloned off the first buildable set, and a stale
// declaration or a stale size would show up as the wrong number of rows.
//
// Each phase writes its own id range and nothing is deleted between
// them, so a set leaking from the previous declaration lands in a range
// that is counted separately.
//
// Every assertion here holds on both drivers; measured on msodbcsql 18.6.2.1.
// -------------------------------------------------------------------
TEST_F(ParamArrayTest, PreparedStatementReExecutesAtDifferentArraySizes) {
    ExecDirect(kGuardTable);
    Prepare("INSERT INTO #pa (id, v) VALUES (?, ?)");

    constexpr int kMax = 7;
    SQLINTEGER ids[kMax] = {};
    SQLINTEGER vals[kMax] = {};
    SQLLEN ind[kMax] = {};
    SQLUSMALLINT status[kMax] = {};
    SQLULEN processed = 0;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, ids, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 10, 0, vals, 0, ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAM_STATUS_PTR, status));
    ASSERT_EQ(SQL_SUCCESS, SetStmtPtr(SQL_ATTR_PARAMS_PROCESSED_PTR, &processed));

    const int sizes[3] = {4, 7, 2};
    const int bases[3] = {100, 200, 300};
    for (int phase = 0; phase < 3; ++phase) {
        const int size = sizes[phase];
        for (int i = 0; i < kMax; ++i) {
            ids[i] = bases[phase] + i + 1;
            vals[i] = 1;
            ind[i] = 0;
            status[i] = 0xFFFF;
        }
        processed = 0xDEAD;

        ASSERT_EQ(SQL_SUCCESS,
                  SetStmtULen(SQL_ATTR_PARAMSET_SIZE, static_cast<SQLULEN>(size)));
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLFreeStmt(stmt_, SQL_CLOSE);

        EXPECT_EQ(static_cast<SQLULEN>(size), processed) << "phase " << phase;
        for (int i = 0; i < size; ++i) {
            EXPECT_EQ(SQL_PARAM_SUCCESS, status[i])
                << "phase " << phase << " set " << i;
        }
        for (int i = size; i < kMax; ++i) {
            EXPECT_EQ(0xFFFF, status[i])
                << "phase " << phase << " wrote past PARAMSET_SIZE at slot " << i;
        }
    }

    // 4 + 7 + 2, each in its own range: a row from a stale declaration would
    // land in the wrong bucket rather than just changing the total.
    EXPECT_EQ(13, ScalarInt("SELECT COUNT(*) FROM #pa"));
    EXPECT_EQ(4, ScalarInt("SELECT COUNT(*) FROM #pa WHERE id BETWEEN 101 AND 199"));
    EXPECT_EQ(7, ScalarInt("SELECT COUNT(*) FROM #pa WHERE id BETWEEN 201 AND 299"));
    EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #pa WHERE id BETWEEN 301 AND 399"));
}

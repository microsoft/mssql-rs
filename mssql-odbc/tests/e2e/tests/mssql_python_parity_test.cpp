// Copyright (c) Microsoft Corporation. All rights reserved.
// mssql_python_parity_test.cpp
//
// Single-file GoogleTest suite covering the ODBC surface that mssql-python
// drives directly. mssql-python does not go through a Driver Manager: it
// LoadLibrary's the driver and calls the exported entrypoints itself, so the
// contract exercised here is exactly the set of calls its pybind layer makes
// (ddbc_bindings.cpp). Every case below maps to a behaviour the Python suite
// depends on, which makes this file the fast regression gate for the
// mssql-python parity work.
//
// The suite is deliberately self-contained: it binds the driver by path
// (MSSQL_ODBC_DLL, defaulting to msodbcsql18.dll next to the binary) instead of
// linking odbc32.lib, so it exercises the same load path as mssql-python and
// needs no Driver Manager registration.
//
// Configure with MSSQL_ODBC_DLL and MSSQL_ODBC_CONNSTR.

#include <gtest/gtest.h>

#include <windows.h>

#include <sql.h>
#include <sqlext.h>
#include <sqltypes.h>

#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

/// mssql-python asks for SQL_CA_SS_VARIANT_TYPE on every column of a result
/// set to detect sql_variant; the value is a SQL Server driver-specific field.
constexpr SQLUSMALLINT kSqlCaSsVariantType = 1215;

using SqlTString = std::wstring;

std::wstring ToWide(const std::string& s) {
    return std::wstring(s.begin(), s.end());
}

std::string GetEnvOr(const char* name, const char* fallback) {
    char* buf = nullptr;
    size_t len = 0;
    if (_dupenv_s(&buf, &len, name) == 0 && buf != nullptr) {
        std::string value(buf);
        free(buf);
        if (!value.empty()) {
            return value;
        }
    }
    return fallback;
}

// ---------------------------------------------------------------------------
// Driver binding
//
// mssql-python resolves each entrypoint by name from the loaded module. The
// table below mirrors that, so a missing export shows up here as a load
// failure rather than as an opaque Python traceback.
// ---------------------------------------------------------------------------

struct DriverApi {
    HMODULE module = nullptr;

    SQLRETURN(SQL_API* AllocHandle)(SQLSMALLINT, SQLHANDLE, SQLHANDLE*) = nullptr;
    SQLRETURN(SQL_API* FreeHandle)(SQLSMALLINT, SQLHANDLE) = nullptr;
    SQLRETURN(SQL_API* SetEnvAttr)(SQLHENV, SQLINTEGER, SQLPOINTER, SQLINTEGER) = nullptr;
    SQLRETURN(SQL_API* DriverConnect)(SQLHDBC, SQLHWND, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*,
                                      SQLSMALLINT, SQLSMALLINT*, SQLUSMALLINT) = nullptr;
    SQLRETURN(SQL_API* Disconnect)(SQLHDBC) = nullptr;
    SQLRETURN(SQL_API* GetDiagRec)(SQLSMALLINT, SQLHANDLE, SQLSMALLINT, SQLWCHAR*, SQLINTEGER*,
                                   SQLWCHAR*, SQLSMALLINT, SQLSMALLINT*) = nullptr;

    SQLRETURN(SQL_API* SetConnectAttr)(SQLHDBC, SQLINTEGER, SQLPOINTER, SQLINTEGER) = nullptr;
    SQLRETURN(SQL_API* GetConnectAttr)(SQLHDBC, SQLINTEGER, SQLPOINTER, SQLINTEGER,
                                       SQLINTEGER*) = nullptr;
    SQLRETURN(SQL_API* EndTran)(SQLSMALLINT, SQLHANDLE, SQLSMALLINT) = nullptr;

    SQLRETURN(SQL_API* ExecDirect)(SQLHSTMT, SQLWCHAR*, SQLINTEGER) = nullptr;
    SQLRETURN(SQL_API* Prepare)(SQLHSTMT, SQLWCHAR*, SQLINTEGER) = nullptr;
    SQLRETURN(SQL_API* Execute)(SQLHSTMT) = nullptr;
    SQLRETURN(SQL_API* Fetch)(SQLHSTMT) = nullptr;
    SQLRETURN(SQL_API* FetchScroll)(SQLHSTMT, SQLSMALLINT, SQLLEN) = nullptr;
    SQLRETURN(SQL_API* GetData)(SQLHSTMT, SQLUSMALLINT, SQLSMALLINT, SQLPOINTER, SQLLEN,
                                SQLLEN*) = nullptr;
    SQLRETURN(SQL_API* BindCol)(SQLHSTMT, SQLUSMALLINT, SQLSMALLINT, SQLPOINTER, SQLLEN,
                                SQLLEN*) = nullptr;
    SQLRETURN(SQL_API* BindParameter)(SQLHSTMT, SQLUSMALLINT, SQLSMALLINT, SQLSMALLINT, SQLSMALLINT,
                                      SQLULEN, SQLSMALLINT, SQLPOINTER, SQLLEN, SQLLEN*) = nullptr;
    SQLRETURN(SQL_API* NumResultCols)(SQLHSTMT, SQLSMALLINT*) = nullptr;
    SQLRETURN(SQL_API* RowCount)(SQLHSTMT, SQLLEN*) = nullptr;
    SQLRETURN(SQL_API* MoreResults)(SQLHSTMT) = nullptr;
    SQLRETURN(SQL_API* FreeStmt)(SQLHSTMT, SQLUSMALLINT) = nullptr;
    SQLRETURN(SQL_API* SetStmtAttr)(SQLHSTMT, SQLINTEGER, SQLPOINTER, SQLINTEGER) = nullptr;
    SQLRETURN(SQL_API* GetStmtAttr)(SQLHSTMT, SQLINTEGER, SQLPOINTER, SQLINTEGER,
                                    SQLINTEGER*) = nullptr;
    SQLRETURN(SQL_API* SetDescField)(SQLHDESC, SQLSMALLINT, SQLSMALLINT, SQLPOINTER,
                                     SQLINTEGER) = nullptr;
    SQLRETURN(SQL_API* ColAttribute)(SQLHSTMT, SQLUSMALLINT, SQLUSMALLINT, SQLPOINTER, SQLSMALLINT,
                                     SQLSMALLINT*, SQLLEN*) = nullptr;

    SQLRETURN(SQL_API* Tables)(SQLHSTMT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*,
                               SQLSMALLINT, SQLWCHAR*, SQLSMALLINT) = nullptr;
    SQLRETURN(SQL_API* Columns)(SQLHSTMT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*,
                                SQLSMALLINT, SQLWCHAR*, SQLSMALLINT) = nullptr;
    SQLRETURN(SQL_API* PrimaryKeys)(SQLHSTMT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*, SQLSMALLINT,
                                    SQLWCHAR*, SQLSMALLINT) = nullptr;
    SQLRETURN(SQL_API* Procedures)(SQLHSTMT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*, SQLSMALLINT,
                                   SQLWCHAR*, SQLSMALLINT) = nullptr;
    SQLRETURN(SQL_API* Statistics)(SQLHSTMT, SQLWCHAR*, SQLSMALLINT, SQLWCHAR*, SQLSMALLINT,
                                   SQLWCHAR*, SQLSMALLINT, SQLUSMALLINT, SQLUSMALLINT) = nullptr;

    std::string load_error;
};

DriverApi& Api() {
    static DriverApi api;
    return api;
}

template <typename Fn>
bool Bind(HMODULE module, const char* name, Fn& slot, std::string& error) {
    auto proc = GetProcAddress(module, name);
    if (proc == nullptr) {
        if (error.empty()) {
            error = "missing export: ";
            error += name;
        }
        return false;
    }
    slot = reinterpret_cast<Fn>(proc);
    return true;
}

bool LoadDriver(DriverApi& api) {
    const std::string path = GetEnvOr("MSSQL_ODBC_DLL", "msodbcsql18.dll");
    api.module = LoadLibraryA(path.c_str());
    if (api.module == nullptr) {
        api.load_error = "LoadLibrary failed for " + path + " (error " +
                         std::to_string(static_cast<unsigned long>(GetLastError())) + ")";
        return false;
    }

    std::string& err = api.load_error;
    bool ok = true;
    ok &= Bind(api.module, "SQLAllocHandle", api.AllocHandle, err);
    ok &= Bind(api.module, "SQLFreeHandle", api.FreeHandle, err);
    ok &= Bind(api.module, "SQLSetEnvAttr", api.SetEnvAttr, err);
    ok &= Bind(api.module, "SQLDriverConnectW", api.DriverConnect, err);
    ok &= Bind(api.module, "SQLDisconnect", api.Disconnect, err);
    ok &= Bind(api.module, "SQLGetDiagRecW", api.GetDiagRec, err);
    ok &= Bind(api.module, "SQLSetConnectAttrW", api.SetConnectAttr, err);
    ok &= Bind(api.module, "SQLGetConnectAttrW", api.GetConnectAttr, err);
    ok &= Bind(api.module, "SQLEndTran", api.EndTran, err);
    ok &= Bind(api.module, "SQLExecDirectW", api.ExecDirect, err);
    ok &= Bind(api.module, "SQLPrepareW", api.Prepare, err);
    ok &= Bind(api.module, "SQLExecute", api.Execute, err);
    ok &= Bind(api.module, "SQLFetch", api.Fetch, err);
    ok &= Bind(api.module, "SQLFetchScroll", api.FetchScroll, err);
    ok &= Bind(api.module, "SQLGetData", api.GetData, err);
    ok &= Bind(api.module, "SQLBindCol", api.BindCol, err);
    ok &= Bind(api.module, "SQLBindParameter", api.BindParameter, err);
    ok &= Bind(api.module, "SQLNumResultCols", api.NumResultCols, err);
    ok &= Bind(api.module, "SQLRowCount", api.RowCount, err);
    ok &= Bind(api.module, "SQLMoreResults", api.MoreResults, err);
    ok &= Bind(api.module, "SQLFreeStmt", api.FreeStmt, err);
    ok &= Bind(api.module, "SQLSetStmtAttrW", api.SetStmtAttr, err);
    ok &= Bind(api.module, "SQLGetStmtAttrW", api.GetStmtAttr, err);
    ok &= Bind(api.module, "SQLSetDescFieldW", api.SetDescField, err);
    ok &= Bind(api.module, "SQLColAttributeW", api.ColAttribute, err);
    ok &= Bind(api.module, "SQLTablesW", api.Tables, err);
    ok &= Bind(api.module, "SQLColumnsW", api.Columns, err);
    ok &= Bind(api.module, "SQLPrimaryKeysW", api.PrimaryKeys, err);
    ok &= Bind(api.module, "SQLProceduresW", api.Procedures, err);
    ok &= Bind(api.module, "SQLStatisticsW", api.Statistics, err);
    return ok;
}

// The tests below are written against the plain ODBC names; route them at the
// loaded module so the file stays readable while bypassing the Driver Manager.
#define SQLAllocHandle       Api().AllocHandle
#define SQLFreeHandle        Api().FreeHandle
#define SQLSetEnvAttr        Api().SetEnvAttr
#define SQLDriverConnectW    Api().DriverConnect
#define SQLDisconnect        Api().Disconnect
#define SQLGetDiagRecW       Api().GetDiagRec
#undef SQLSetConnectAttr
#define SQLSetConnectAttr    Api().SetConnectAttr
#define SQLGetConnectAttrW   Api().GetConnectAttr
#define SQLEndTran           Api().EndTran
#define SQLExecDirectW       Api().ExecDirect
#define SQLPrepareW          Api().Prepare
#define SQLExecute           Api().Execute
#define SQLFetch             Api().Fetch
#define SQLFetchScroll       Api().FetchScroll
#define SQLGetData           Api().GetData
#define SQLBindCol           Api().BindCol
#define SQLBindParameter     Api().BindParameter
#define SQLNumResultCols     Api().NumResultCols
#define SQLRowCount          Api().RowCount
#define SQLMoreResults       Api().MoreResults
#define SQLFreeStmt          Api().FreeStmt
#undef SQLSetStmtAttr
#define SQLSetStmtAttr       Api().SetStmtAttr
#undef SQLGetStmtAttr
#define SQLGetStmtAttr       Api().GetStmtAttr
#define SQLSetDescFieldW     Api().SetDescField
#define SQLColAttributeW     Api().ColAttribute
#define SQLTablesW           Api().Tables
#define SQLColumnsW          Api().Columns
#define SQLPrimaryKeysW      Api().PrimaryKeys
#define SQLProceduresW       Api().Procedures
#define SQLStatisticsW       Api().Statistics

std::wstring DiagField(SQLSMALLINT handle_type, SQLHANDLE handle, bool want_state) {
    SQLWCHAR state[6] = {};
    SQLWCHAR message[1024] = {};
    SQLINTEGER native = 0;
    SQLSMALLINT length = 0;
    if (!SQL_SUCCEEDED(SQLGetDiagRecW(handle_type, handle, 1, state, &native, message,
                                      static_cast<SQLSMALLINT>(std::size(message)), &length))) {
        return L"";
    }
    return want_state ? std::wstring(state) : std::wstring(message);
}

std::string Narrow(const std::wstring& value) {
    return std::string(value.begin(), value.end());
}

std::string DiagMessage(SQLSMALLINT handle_type, SQLHANDLE handle) {
    return "[" + Narrow(DiagField(handle_type, handle, true)) + "] " +
           Narrow(DiagField(handle_type, handle, false));
}

std::string DiagState(SQLSMALLINT handle_type, SQLHANDLE handle) {
    return Narrow(DiagField(handle_type, handle, true));
}

#define ASSERT_SQL_OK(rc, handle_type, handle)                                                     \
    do {                                                                                           \
        SQLRETURN _rc = (rc);                                                                      \
        ASSERT_TRUE(SQL_SUCCEEDED(_rc)) << "rc=" << _rc << " " << DiagMessage(handle_type, handle); \
    } while (0)

#define EXPECT_SQL_OK(rc, handle_type, handle)                                                     \
    do {                                                                                           \
        SQLRETURN _rc = (rc);                                                                      \
        EXPECT_TRUE(SQL_SUCCEEDED(_rc)) << "rc=" << _rc << " " << DiagMessage(handle_type, handle); \
    } while (0)

#define EXPECT_SQLSTATE(handle_type, handle, expected_state)                                       \
    EXPECT_EQ(std::string(expected_state), DiagState(handle_type, handle))

/// Compatibility shim so the test bodies keep the shared-fixture spelling.
struct ODBCTestUtils {
    static SqlTString ToSqlTStr(const std::string& s) { return ToWide(s); }
    static std::string ToNarrow(const SqlTString& s) { return Narrow(s); }
};

/// Fixture that connects once per test and exposes helpers for the
/// mssql-python call patterns.
class PythonParityTest : public ::testing::Test {
protected:
    SQLHENV env_ = nullptr;
    SQLHDBC dbc_ = nullptr;
    SQLHSTMT stmt_ = nullptr;

    void SetUp() override {
        if (Api().module == nullptr) {
            GTEST_SKIP() << "driver not loaded: " << Api().load_error;
        }
        const std::string conn = GetEnvOr("MSSQL_ODBC_CONNSTR", "");
        if (conn.empty()) {
            GTEST_SKIP() << "set MSSQL_ODBC_CONNSTR to run parity tests";
        }

        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_ENV, nullptr, &env_), SQL_HANDLE_ENV, env_);
        ASSERT_SQL_OK(SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                    reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3), 0),
                      SQL_HANDLE_ENV, env_);
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_), SQL_HANDLE_ENV, env_);

        std::wstring wide = ToWide(conn);
        ASSERT_SQL_OK(SQLDriverConnectW(dbc_, nullptr, wide.data(),
                                        static_cast<SQLSMALLINT>(wide.size()), nullptr, 0, nullptr,
                                        SQL_DRIVER_NOPROMPT),
                      SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_), SQL_HANDLE_DBC, dbc_);
    }

    void TearDown() override {
        if (stmt_ != nullptr) {
            SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
        }
        if (dbc_ != nullptr) {
            SQLDisconnect(dbc_);
            SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
        }
        if (env_ != nullptr) {
            SQLFreeHandle(SQL_HANDLE_ENV, env_);
        }
    }

    /// Runs |sql| on |hstmt| and asserts it succeeded.
    void Exec(SQLHSTMT hstmt, const std::string& sql) {
        std::wstring text = ToWide(sql);
        ASSERT_SQL_OK(SQLExecDirectW(hstmt, text.data(), SQL_NTS), SQL_HANDLE_STMT, hstmt);
    }

    /// Runs |sql| and swallows failures; used for best-effort cleanup.
    void ExecDirectIgnoreError(const std::string& sql) {
        std::wstring text = ToWide(sql);
        SQLExecDirectW(stmt_, text.data(), SQL_NTS);
        SQLFreeStmt(stmt_, SQL_CLOSE);
    }

    /// Allocates an extra statement on the same connection.
    SQLHSTMT AllocStmt() {
        SQLHSTMT handle = nullptr;
        EXPECT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &handle), SQL_HANDLE_DBC, dbc_);
        return handle;
    }

    void FreeStmt(SQLHSTMT handle) { SQLFreeHandle(SQL_HANDLE_STMT, handle); }

    /// Fetches a single SQL_C_SLONG column from a one-row query.
    SQLINTEGER ScalarLong(const std::string& sql) {
        Exec(stmt_, sql);
        EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
        SQLINTEGER value = 0;
        SQLLEN indicator = 0;
        EXPECT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        SQLFreeStmt(stmt_, SQL_CLOSE);
        return value;
    }
};


// ---------------------------------------------------------------------------
// Connection attributes and transactions
//
// mssql-python calls SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT) immediately after
// connecting and raises if it fails, then drives commit/rollback exclusively
// through SQLEndTran. A failure in any of these aborts every Python test.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, AutocommitRoundTrips) {
    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_OFF), 0),
                  SQL_HANDLE_DBC, dbc_);

    SQLUINTEGER value = 0xFFFF;
    SQLINTEGER length = 0;
    ASSERT_SQL_OK(SQLGetConnectAttrW(dbc_, SQL_ATTR_AUTOCOMMIT, &value, sizeof(value), &length),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_OFF), value);

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_ON), 0),
                  SQL_HANDLE_DBC, dbc_);
    ASSERT_SQL_OK(SQLGetConnectAttrW(dbc_, SQL_ATTR_AUTOCOMMIT, &value, sizeof(value), &length),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_ON), value);
}

TEST_F(PythonParityTest, ManualCommitPersistsRows) {
    ExecDirectIgnoreError("DROP TABLE IF EXISTS #parity_commit");
    Exec(stmt_, "CREATE TABLE #parity_commit (id INT)");

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_OFF), 0),
                  SQL_HANDLE_DBC, dbc_);
    Exec(stmt_, "INSERT INTO #parity_commit VALUES (1)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    EXPECT_EQ(1, ScalarLong("SELECT COUNT(*) FROM #parity_commit"));
    SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT, reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_ON), 0);
}

TEST_F(PythonParityTest, ManualRollbackDiscardsRows) {
    ExecDirectIgnoreError("DROP TABLE IF EXISTS #parity_rollback");
    Exec(stmt_, "CREATE TABLE #parity_rollback (id INT)");

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_OFF), 0),
                  SQL_HANDLE_DBC, dbc_);
    Exec(stmt_, "INSERT INTO #parity_rollback VALUES (1)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);

    EXPECT_EQ(0, ScalarLong("SELECT COUNT(*) FROM #parity_rollback"));
    SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT, reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_ON), 0);
}

TEST_F(PythonParityTest, EndTranInAutocommitIsANoOp) {
    // Python's Connection.commit() is unconditional, so committing while
    // autocommit is on must succeed instead of raising 25000.
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
}

// ---------------------------------------------------------------------------
// Block fetch — the fetchmany()/fetchall() hot path
//
// FetchBatchData() unbinds, binds every column column-wise with an array of
// |fetchSize| elements, then calls SQLFetchScroll(SQL_FETCH_NEXT, 0) and reads
// SQL_ATTR_ROWS_FETCHED_PTR. Column-wise offsets and the indicator array are
// the parts most likely to regress.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, BlockFetchFillsColumnWiseArrays) {
    constexpr SQLULEN kRowsetSize = 4;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(kRowsetSize), 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLULEN rows_fetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rows_fetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    Exec(stmt_,
         "SELECT v, CAST(v AS VARCHAR(16)) AS t FROM (VALUES (10),(20),(30)) AS s(v) ORDER BY v");

    SQLINTEGER ints[kRowsetSize] = {};
    SQLLEN int_ind[kRowsetSize] = {};
    SQLWCHAR text[kRowsetSize][32] = {};
    SQLLEN text_ind[kRowsetSize] = {};

    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, ints, sizeof(SQLINTEGER), int_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_WCHAR, text, sizeof(text[0]), text_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_TRUE(SQL_SUCCEEDED(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0)));
    ASSERT_EQ(3u, rows_fetched);
    EXPECT_EQ(10, ints[0]);
    EXPECT_EQ(20, ints[1]);
    EXPECT_EQ(30, ints[2]);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), int_ind[0]);
    EXPECT_EQ(std::string("10"), ODBCTestUtils::ToNarrow(SqlTString(
                                     reinterpret_cast<const SQLTCHAR*>(text[0]))));
    EXPECT_EQ(std::string("30"), ODBCTestUtils::ToNarrow(SqlTString(
                                     reinterpret_cast<const SQLTCHAR*>(text[2]))));

    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
}

TEST_F(PythonParityTest, BlockFetchReportsNullIndicators) {
    constexpr SQLULEN kRowsetSize = 2;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(kRowsetSize), 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLULEN rows_fetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rows_fetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    Exec(stmt_, "SELECT CAST(NULL AS INT) UNION ALL SELECT 7");

    SQLINTEGER values[kRowsetSize] = {};
    SQLLEN indicators[kRowsetSize] = {};
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), indicators),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_TRUE(SQL_SUCCEEDED(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0)));
    ASSERT_EQ(2u, rows_fetched);
    EXPECT_EQ(SQL_NULL_DATA, indicators[0]);
    EXPECT_EQ(7, values[1]);
}

TEST_F(PythonParityTest, FetchScrollRejectsNonForwardOrientations) {
    Exec(stmt_, "SELECT 1");
    EXPECT_EQ(SQL_ERROR, SQLFetchScroll(stmt_, SQL_FETCH_PRIOR, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY106");
}

// ---------------------------------------------------------------------------
// Interleaved cursors on one connection
//
// Connection.cursor() allocates another HSTMT on the same HDBC. Without MARS
// the driver must still serve the second statement once the first result set
// is buffered, and the first cursor's remaining rows must survive.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, SecondCursorRunsWhileFirstIsOpen) {
    Exec(stmt_, "SELECT 1 AS n UNION ALL SELECT 2 ORDER BY n");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLINTEGER first = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &first, sizeof(first), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(1, first);

    SQLHSTMT other = AllocStmt();
    Exec(other, "SELECT 42");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(other));
    SQLINTEGER answer = 0;
    ASSERT_SQL_OK(SQLGetData(other, 1, SQL_C_SLONG, &answer, sizeof(answer), &ind), SQL_HANDLE_STMT,
                  other);
    EXPECT_EQ(42, answer);
    FreeStmt(other);

    // The first cursor keeps its position across the interleaved statement.
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLINTEGER second = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &second, sizeof(second), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(2, second);
}

// ---------------------------------------------------------------------------
// SQLGetData conversions
//
// Python materializes every cell through SQLGetData with the C type chosen
// from the column's SQL type, so the conversion matrix is load-bearing for the
// whole data-type test module.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, GetDataConvertsCommonTypes) {
    Exec(stmt_,
         "SELECT CAST('abc' AS NVARCHAR(10)), CAST(1.5 AS FLOAT), CAST(3 AS BIGINT), "
         "CAST('2024-02-29' AS DATE), CAST(1 AS BIT)");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLWCHAR text[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, text, sizeof(text), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(std::string("abc"),
              ODBCTestUtils::ToNarrow(SqlTString(reinterpret_cast<const SQLTCHAR*>(text))));

    double real = 0.0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_DOUBLE, &real, sizeof(real), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_DOUBLE_EQ(1.5, real);

    SQLBIGINT big = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 3, SQL_C_SBIGINT, &big, sizeof(big), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(3, big);

    SQL_DATE_STRUCT date{};
    ASSERT_SQL_OK(SQLGetData(stmt_, 4, SQL_C_TYPE_DATE, &date, sizeof(date), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(2024, date.year);
    EXPECT_EQ(2, date.month);
    EXPECT_EQ(29, date.day);

    unsigned char bit = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 5, SQL_C_BIT, &bit, sizeof(bit), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, bit);
}

TEST_F(PythonParityTest, GetDataReportsNullAndTruncation) {
    Exec(stmt_, "SELECT CAST(NULL AS INT), CAST('abcdef' AS VARCHAR(10))");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLINTEGER value = 123;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_NULL_DATA, ind);

    SQLCHAR tiny_buf[4] = {};
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 2, SQL_C_CHAR, tiny_buf, sizeof(tiny_buf), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
}

// ---------------------------------------------------------------------------
// SQLColAttributeW
//
// Python queries SQL_CA_SS_VARIANT_TYPE per column and falls back to None when
// it fails; it also relies on the standard descriptor fields for cursor
// metadata.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, ColAttributeReportsNameTypeAndCount) {
    Exec(stmt_, "SELECT CAST(1 AS INT) AS answer");

    SQLLEN numeric = 0;
    ASSERT_SQL_OK(SQLColAttributeW(stmt_, 0, SQL_DESC_COUNT, nullptr, 0, nullptr, &numeric),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, numeric);

    SQLWCHAR name[64] = {};
    SQLSMALLINT name_len = 0;
    ASSERT_SQL_OK(
        SQLColAttributeW(stmt_, 1, SQL_DESC_NAME, name, sizeof(name), &name_len, nullptr),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(std::string("answer"),
              ODBCTestUtils::ToNarrow(SqlTString(reinterpret_cast<const SQLTCHAR*>(name))));

    ASSERT_SQL_OK(SQLColAttributeW(stmt_, 1, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &numeric),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_INTEGER, numeric);
}

TEST_F(PythonParityTest, ColAttributeVariantTypeDoesNotCrash) {
    Exec(stmt_, "SELECT CAST(1 AS INT)");
    SQLLEN numeric = 0;
    // Either answer is acceptable — Python treats a failure as "not a variant" —
    // but the call must not fault or leave the statement unusable.
    SQLColAttributeW(stmt_, 1, kSqlCaSsVariantType, nullptr, 0, nullptr, &numeric);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
}

// ---------------------------------------------------------------------------
// Catalog functions
//
// Cursor.tables()/columns()/primaryKeys()/... map one-to-one onto these calls
// and assert on the ODBC-defined column layout.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, TablesReturnsOdbcShapedResultSet) {
    ExecDirectIgnoreError("DROP TABLE dbo.parity_catalog");
    Exec(stmt_, "CREATE TABLE dbo.parity_catalog (id INT NOT NULL PRIMARY KEY, label NVARCHAR(20))");
    SQLFreeStmt(stmt_, SQL_CLOSE);

    SqlTString table = ODBCTestUtils::ToSqlTStr("parity_catalog");
    ASSERT_SQL_OK(SQLTablesW(stmt_, nullptr, 0, nullptr, 0,
                             reinterpret_cast<SQLWCHAR*>(table.data()), SQL_NTS, nullptr, 0),
                  SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT columns = 0;
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &columns), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, columns);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_CLOSE);

    ExecDirectIgnoreError("DROP TABLE dbo.parity_catalog");
}

TEST_F(PythonParityTest, ColumnsAndPrimaryKeysSucceed) {
    ExecDirectIgnoreError("DROP TABLE dbo.parity_keys");
    Exec(stmt_, "CREATE TABLE dbo.parity_keys (id INT NOT NULL PRIMARY KEY, label NVARCHAR(20))");
    SQLFreeStmt(stmt_, SQL_CLOSE);

    SqlTString table = ODBCTestUtils::ToSqlTStr("parity_keys");
    ASSERT_SQL_OK(SQLColumnsW(stmt_, nullptr, 0, nullptr, 0,
                              reinterpret_cast<SQLWCHAR*>(table.data()), SQL_NTS, nullptr, 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_CLOSE);

    table = ODBCTestUtils::ToSqlTStr("parity_keys");
    ASSERT_SQL_OK(SQLPrimaryKeysW(stmt_, nullptr, 0, nullptr, 0,
                                  reinterpret_cast<SQLWCHAR*>(table.data()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_CLOSE);

    ExecDirectIgnoreError("DROP TABLE dbo.parity_keys");
}

TEST_F(PythonParityTest, ProceduresAndStatisticsSucceed) {
    ASSERT_SQL_OK(SQLProceduresW(stmt_, nullptr, 0, nullptr, 0, nullptr, 0), SQL_HANDLE_STMT,
                  stmt_);
    SQLFreeStmt(stmt_, SQL_CLOSE);

    ExecDirectIgnoreError("DROP TABLE dbo.parity_stats");
    Exec(stmt_, "CREATE TABLE dbo.parity_stats (id INT NOT NULL PRIMARY KEY)");
    SQLFreeStmt(stmt_, SQL_CLOSE);

    SqlTString table = ODBCTestUtils::ToSqlTStr("parity_stats");
    ASSERT_SQL_OK(SQLStatisticsW(stmt_, nullptr, 0, nullptr, 0,
                                 reinterpret_cast<SQLWCHAR*>(table.data()), SQL_NTS,
                                 SQL_INDEX_ALL, SQL_QUICK),
                  SQL_HANDLE_STMT, stmt_);
    SQLFreeStmt(stmt_, SQL_CLOSE);

    ExecDirectIgnoreError("DROP TABLE dbo.parity_stats");
}

// ---------------------------------------------------------------------------
// Parameter binding
//
// BindParameters() feeds SQLBindParameter for every Python argument, and the
// SQL_C_NUMERIC path additionally sets precision/scale on the APD through
// SQLSetDescFieldW — a failure there makes every decimal parameter raise.
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, BoundParametersRoundTrip) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? + ?, ?");
    ASSERT_SQL_OK(SQLPrepareW(stmt_, reinterpret_cast<SQLWCHAR*>(sql.data()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    SQLINTEGER left = 40;
    SQLINTEGER right = 2;
    SQLWCHAR text[] = {L'h', L'i', 0};
    SQLLEN int_len = 0;
    SQLLEN text_len = SQL_NTS;

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0, &left,
                                   0, &int_len),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   &right, 0, &int_len),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 3, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR, 2, 0, text,
                                   sizeof(text), &text_len),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLINTEGER sum = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &sum, sizeof(sum), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(42, sum);
}

TEST_F(PythonParityTest, SetDescFieldOnApdSucceedsForNumeric) {
    SQLHDESC apd = nullptr;
    SQLINTEGER length = 0;
    if (!SQL_SUCCEEDED(SQLGetStmtAttr(stmt_, SQL_ATTR_APP_PARAM_DESC, &apd, 0, &length))) {
        GTEST_SKIP() << "APD handle unavailable";
    }
    // Python sets these three fields, in this order, for every decimal argument.
    EXPECT_SQL_OK(SQLSetDescFieldW(apd, 1, SQL_DESC_TYPE, reinterpret_cast<SQLPOINTER>(SQL_C_NUMERIC),
                                   0),
                  SQL_HANDLE_DESC, apd);
    EXPECT_SQL_OK(SQLSetDescFieldW(apd, 1, SQL_DESC_PRECISION, reinterpret_cast<SQLPOINTER>(18), 0),
                  SQL_HANDLE_DESC, apd);
    EXPECT_SQL_OK(SQLSetDescFieldW(apd, 1, SQL_DESC_SCALE, reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_DESC, apd);
}

// ---------------------------------------------------------------------------
// Statement lifecycle
// ---------------------------------------------------------------------------

TEST_F(PythonParityTest, UnbindClearsPreviousBindings) {
    Exec(stmt_, "SELECT 1, 2");
    SQLINTEGER first = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &first, sizeof(first), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(0, first) << "SQL_UNBIND must detach the buffer";
}

TEST_F(PythonParityTest, RowCountReportsAffectedRows) {
    ExecDirectIgnoreError("DROP TABLE IF EXISTS #parity_rowcount");
    Exec(stmt_, "CREATE TABLE #parity_rowcount (id INT)");
    SQLFreeStmt(stmt_, SQL_CLOSE);

    Exec(stmt_, "INSERT INTO #parity_rowcount VALUES (1),(2),(3)");
    SQLLEN affected = 0;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &affected), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, affected);
}

TEST_F(PythonParityTest, MoreResultsWalksMultiStatementBatch) {
    Exec(stmt_, "SELECT 1; SELECT 2");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLINTEGER value = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(1, value);

    ASSERT_TRUE(SQL_SUCCEEDED(SQLMoreResults(stmt_)));
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(2, value);

    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
}


}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    if (!LoadDriver(Api())) {
        fprintf(stderr, "warning: %s\n", Api().load_error.c_str());
    }
    return RUN_ALL_TESTS();
}
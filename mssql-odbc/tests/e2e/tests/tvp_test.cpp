// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// End-to-end coverage for table-valued parameters: describe identity, a
// populated round-trip, and an empty table. Mirrors the ODBC sequence a
// consumer (mssql-python) issues: prepare, describe, bind the SQL_SS_TABLE
// parameter, focus on it, bind its columns as arrays, restore focus, execute.

#include "odbc_test_fixture.h"

#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <deque>
#include <random>
#include <string>
#include <vector>

#ifndef SQL_SS_TABLE
#define SQL_SS_TABLE (-153)
#endif
#ifndef SQL_SOPT_SS_PARAM_FOCUS
#define SQL_SOPT_SS_PARAM_FOCUS 1236
#endif
#ifndef SQL_CA_SS_SCHEMA_NAME
#define SQL_CA_SS_SCHEMA_NAME 1226
#endif
#ifndef SQL_CA_SS_TYPE_NAME
#define SQL_CA_SS_TYPE_NAME 1227
#endif

namespace {

const std::string& UniqueSuffix() {
    static const std::string suffix = [] {
        std::random_device rd;
        std::mt19937_64 gen(rd());
        std::uniform_int_distribution<uint64_t> dist;
        char buf[17] = {};
        std::snprintf(buf, sizeof(buf), "%016llx", static_cast<unsigned long long>(dist(gen)));
        return std::string(buf);
    }();
    return suffix;
}

std::vector<SQLWCHAR> ToSqlW(const std::string& text) {
    std::vector<SQLWCHAR> result;
    result.reserve(text.size() + 1);
    for (unsigned char ch : text) result.push_back(static_cast<SQLWCHAR>(ch));
    result.push_back(0);
    return result;
}

std::string SqlWToAscii(const SQLWCHAR* text, size_t count) {
    std::string result;
    for (size_t i = 0; i < count && text[i] != 0; ++i) {
        result.push_back(static_cast<char>(text[i]));
    }
    return result;
}

} // namespace

class TvpTest : public ODBCTest {
protected:
    // TEST_F is intentional: each test creates permanent dbo objects with the
    // same process-wide suffix, so parallel fixtures would collide.
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection());
        Connect();
        suffix_ = UniqueSuffix();
        type_name_ = "pytvp_type_" + suffix_;
        proc_name_ = "pytvp_proc_" + suffix_;
        Direct("DROP PROCEDURE IF EXISTS dbo.[" + proc_name_ + "]");
        Direct("DROP TYPE IF EXISTS dbo.[" + type_name_ + "]");
        ASSERT_SQL_OK(
            DirectRc("CREATE TYPE dbo.[" + type_name_ +
                     "] AS TABLE (id INT, label NVARCHAR(50))"),
            SQL_HANDLE_STMT, stmt_);
        // The proc returns the rows it received, so a fetch verifies what the
        // server actually materialized from the TVP on the wire.
        ASSERT_SQL_OK(
            DirectRc("CREATE PROCEDURE dbo.[" + proc_name_ +
                     "] @t dbo.[" + type_name_ +
                     "] READONLY AS SELECT id, label FROM @t ORDER BY id"),
            SQL_HANDLE_STMT, stmt_);
    }

    void TearDown() override {
        if (stmt_ != SQL_NULL_HSTMT) {
            if (impersonating_) Direct("REVERT");
            for (const auto& sql : cleanup_sql_) Direct(sql);
            Direct("DROP PROCEDURE IF EXISTS dbo.[" + proc_name_ + "]");
            Direct("DROP TYPE IF EXISTS dbo.[" + type_name_ + "]");
        }
        ODBCTest::TearDown();
    }

    SQLRETURN DirectRc(const std::string& sql) {
        SQLRETURN close_rc = SQLFreeStmt(stmt_, SQL_CLOSE);
        if (!SQL_SUCCEEDED(close_rc)) return close_rc;
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }
    void Direct(const std::string& sql) {
        EXPECT_SQL_OK(DirectRc(sql), SQL_HANDLE_STMT, stmt_);
    }

    SQLRETURN Prepare(const std::string& sql) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    SQLHDESC Ipd() {
        SQLHDESC ipd = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_IMP_PARAM_DESC, &ipd, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return ipd;
    }

    // Binds the top-level TVP at `ordinal`: the type name in the value buffer,
    // the schema through SQL_CA_SS_SCHEMA_NAME, and `rows` (or SQL_DEFAULT_PARAM)
    // as the indicator. The type-name buffer is retained in `type_bufs_` so the
    // pointer stays valid until execute.
    void BindTvp(SQLUSMALLINT ordinal, const std::string& type_name, SQLLEN rows,
                 const std::string& schema = "dbo") {
        type_bufs_.push_back(ToSqlW(type_name));
        std::vector<SQLWCHAR>& tw = type_bufs_.back();
        row_counts_.push_back(rows);
        SQLLEN& ind = row_counts_.back();
        ASSERT_SQL_OK(
            SQLBindParameter(stmt_, ordinal, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_SS_TABLE,
                             rows == SQL_DEFAULT_PARAM ? 0 : static_cast<SQLULEN>(rows), 0,
                             tw.data(),
                             static_cast<SQLLEN>((tw.size() - 1) * sizeof(SQLWCHAR)), &ind),
            SQL_HANDLE_STMT, stmt_);
        schema_bufs_.push_back(ToSqlW(schema));
        std::vector<SQLWCHAR>& sw = schema_bufs_.back();
        ASSERT_SQL_OK(SQLSetDescFieldW(Ipd(), ordinal, SQL_CA_SS_SCHEMA_NAME,
                                       sw.data(),
                                       static_cast<SQLINTEGER>((sw.size() - 1) * sizeof(SQLWCHAR))),
                      SQL_HANDLE_DESC, Ipd());
    }

    void Focus(SQLINTEGER ordinal) {
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_SOPT_SS_PARAM_FOCUS,
                                     reinterpret_cast<SQLPOINTER>(
                                         static_cast<SQLLEN>(ordinal)),
                                     SQL_IS_INTEGER),
                      SQL_HANDLE_STMT, stmt_);
    }
    void RestoreFocus() { Focus(0); }

    std::string suffix_;
    std::string type_name_;
    std::string proc_name_;
    // Buffers retained for the lifetime of a bind so their pointers stay valid
    // until execute. `deque`, not `vector`: binding a second TVP must not
    // reallocate and invalidate the first TVP's retained name pointers.
    std::deque<std::vector<SQLWCHAR>> type_bufs_;
    std::deque<std::vector<SQLWCHAR>> schema_bufs_;
    std::deque<SQLLEN> row_counts_;
    std::vector<std::string> cleanup_sql_;
    bool impersonating_ = false;
};

// SQLDescribeParam reports the TVP as SQL_SS_TABLE and the IPD exposes the
// table type's name through SQL_CA_SS_TYPE_NAME.
TEST_F(TvpTest, DescribeReportsTableTypeAndName) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT data_type = 0;
    SQLULEN param_size = 0;
    SQLSMALLINT decimal_digits = 0;
    SQLSMALLINT nullable = 0;
    ASSERT_SQL_OK(SQLDescribeParam(stmt_, 1, &data_type, &param_size, &decimal_digits, &nullable),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(data_type, SQL_SS_TABLE);

    SQLWCHAR name[128] = {0};
    SQLINTEGER name_len = 0;
    ASSERT_SQL_OK(SQLGetDescFieldW(Ipd(), 1, SQL_CA_SS_TYPE_NAME, name,
                                   sizeof(name), &name_len),
                  SQL_HANDLE_DESC, Ipd());
    EXPECT_EQ(SqlWToAscii(name, static_cast<size_t>(name_len) / sizeof(SQLWCHAR)), type_name_);
}

// A populated TVP round-trips: two rows bound column-wise under param focus are
// returned verbatim by the procedure.
TEST_F(TvpTest, PopulatedTableRoundTrips) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);

    // The top-level TVP: value pointer is the type name, indicator the row count.
    std::vector<SQLWCHAR> type_w = ToSqlW(type_name_);
    SQLLEN row_count = 2;
    ASSERT_SQL_OK(
        SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_SS_TABLE,
                         static_cast<SQLULEN>(row_count), 0,
                         type_w.data(),
                         static_cast<SQLLEN>((type_w.size() - 1) * sizeof(SQLWCHAR)), &row_count),
        SQL_HANDLE_STMT, stmt_);
    std::vector<SQLWCHAR> schema_w = ToSqlW("dbo");
    ASSERT_SQL_OK(SQLSetDescFieldW(Ipd(), 1, SQL_CA_SS_SCHEMA_NAME,
                                   schema_w.data(),
                                   static_cast<SQLINTEGER>((schema_w.size() - 1) * sizeof(SQLWCHAR))),
                  SQL_HANDLE_DESC, Ipd());

    // Focus on the TVP and bind its two columns as arrays of row_count rows.
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_SOPT_SS_PARAM_FOCUS,
                                 reinterpret_cast<SQLPOINTER>(1), SQL_IS_INTEGER),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAMSET_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);

    std::array<SQLINTEGER, 2> ids = {1, 2};
    std::array<SQLLEN, 2> id_ind = {0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER,
                                   0, 0, ids.data(), 0, id_ind.data()),
                  SQL_HANDLE_STMT, stmt_);

    // Two fixed-width wide-char rows, each padded to 50 chars + NUL.
    const SQLULEN label_cap = 50;
    std::vector<SQLWCHAR> labels((label_cap + 1) * 2, 0);
    std::array<SQLLEN, 2> label_ind = {0, 0};
    const char* texts[2] = {"Keyboard", "Mouse"};
    for (size_t r = 0; r < 2; ++r) {
        std::string t(texts[r]);
        for (size_t c = 0; c < t.size(); ++c) {
            labels[r * (label_cap + 1) + c] = static_cast<SQLWCHAR>(t[c]);
        }
        label_ind[r] = static_cast<SQLLEN>(t.size() * sizeof(SQLWCHAR));
    }
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR,
                                   label_cap, 0, labels.data(),
                                   static_cast<SQLLEN>((label_cap + 1) * sizeof(SQLWCHAR)),
                                   label_ind.data()),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_SOPT_SS_PARAM_FOCUS,
                                 reinterpret_cast<SQLPOINTER>(0), SQL_IS_INTEGER),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    // Row 1.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER id = 0;
    SQLLEN id_len = 0;
    SQLWCHAR label[64] = {0};
    SQLLEN label_len = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &id, 0, &id_len), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_WCHAR, label, sizeof(label), &label_len),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(id, 1);
    EXPECT_EQ(SqlWToAscii(label, sizeof(label) / sizeof(SQLWCHAR)), "Keyboard");

    // Row 2.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &id, 0, &id_len), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_WCHAR, label, sizeof(label), &label_len),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(id, 2);
    EXPECT_EQ(SqlWToAscii(label, sizeof(label) / sizeof(SQLWCHAR)), "Mouse");

    EXPECT_EQ(SQLFetch(stmt_), SQL_NO_DATA);
}

// An empty TVP (indicator SQL_DEFAULT_PARAM, no column binds) yields zero rows.
TEST_F(TvpTest, EmptyTableYieldsNoRows) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLWCHAR> type_w = ToSqlW(type_name_);
    SQLLEN indicator = SQL_DEFAULT_PARAM;
    ASSERT_SQL_OK(
        SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_SS_TABLE, 0, 0,
                         type_w.data(),
                         static_cast<SQLLEN>((type_w.size() - 1) * sizeof(SQLWCHAR)), &indicator),
        SQL_HANDLE_STMT, stmt_);
    std::vector<SQLWCHAR> schema_w = ToSqlW("dbo");
    ASSERT_SQL_OK(SQLSetDescFieldW(Ipd(), 1, SQL_CA_SS_SCHEMA_NAME,
                                   schema_w.data(),
                                   static_cast<SQLINTEGER>((schema_w.size() - 1) * sizeof(SQLWCHAR))),
                  SQL_HANDLE_DESC, Ipd());

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQLFetch(stmt_), SQL_NO_DATA);
}

// Rows with NULL and all-NULL cells round-trip; the column's declared type
// still drives the wire metadata even when every value is NULL.
TEST_F(TvpTest, NullAndAllNullCells) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);
    BindTvp(1, type_name_, 2);
    Focus(1);

    std::array<SQLINTEGER, 2> ids = {0, 0};         // values ignored for NULLs
    std::array<SQLLEN, 2> id_ind = {SQL_NULL_DATA, SQL_NULL_DATA};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER,
                                   0, 0, ids.data(), 0, id_ind.data()),
                  SQL_HANDLE_STMT, stmt_);

    const SQLULEN cap = 50;
    std::vector<SQLWCHAR> labels((cap + 1) * 2, 0);
    // Row 0 has a label, row 1 is NULL.
    const std::string first = "only";
    for (size_t c = 0; c < first.size(); ++c) labels[c] = static_cast<SQLWCHAR>(first[c]);
    std::array<SQLLEN, 2> label_ind = {
        static_cast<SQLLEN>(first.size() * sizeof(SQLWCHAR)), SQL_NULL_DATA};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR,
                                   cap, 0, labels.data(),
                                   static_cast<SQLLEN>((cap + 1) * sizeof(SQLWCHAR)),
                                   label_ind.data()),
                  SQL_HANDLE_STMT, stmt_);
    RestoreFocus();
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    // Both id cells are NULL; exactly one label is "only" and one is NULL.
    int rows = 0;
    int only_labels = 0;
    int null_labels = 0;
    SQLINTEGER id = 0;
    SQLLEN id_len = 0;
    SQLWCHAR label[64] = {0};
    SQLLEN label_len = 0;
    while (SQLFetch(stmt_) == SQL_SUCCESS) {
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &id, 0, &id_len), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_WCHAR, label, sizeof(label), &label_len),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(id_len, SQL_NULL_DATA) << "both id cells were bound NULL";
        if (label_len == SQL_NULL_DATA) {
            ++null_labels;
        } else {
            EXPECT_EQ(label_len, static_cast<SQLLEN>(first.size() * sizeof(SQLWCHAR)));
            EXPECT_EQ(SqlWToAscii(label, sizeof(label) / sizeof(SQLWCHAR)), first);
            ++only_labels;
        }
        ++rows;
    }
    EXPECT_EQ(rows, 2);
    EXPECT_EQ(only_labels, 1);
    EXPECT_EQ(null_labels, 1);
}

// Binding a TVP with any direction other than SQL_PARAM_INPUT is HY105.
TEST_F(TvpTest, NonInputDirectionIsRejected) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLWCHAR> type_w = ToSqlW(type_name_);
    SQLLEN ind = 0;
    EXPECT_EQ(
        SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_BINARY, SQL_SS_TABLE, 0, 0,
                         type_w.data(),
                         static_cast<SQLLEN>((type_w.size() - 1) * sizeof(SQLWCHAR)), &ind),
        SQL_ERROR);
    EXPECT_EQ(ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_), "HY105");
}

// A TVP cannot be a column of another TVP: binding SQL_SS_TABLE while focus is
// set is HY004.
TEST_F(TvpTest, NestedTvpIsRejected) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);
    BindTvp(1, type_name_, 1);
    Focus(1);
    std::vector<SQLWCHAR> type_w = ToSqlW(type_name_);
    SQLLEN ind = 0;
    EXPECT_EQ(
        SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_SS_TABLE, 0, 0,
                         type_w.data(),
                         static_cast<SQLLEN>((type_w.size() - 1) * sizeof(SQLWCHAR)), &ind),
        SQL_ERROR);
    EXPECT_EQ(ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_), "HY004");
    RestoreFocus();
}

// Parameter focus must be reset to 0 before execute, else HY024.
TEST_F(TvpTest, FocusMustBeZeroAtExecute) {
    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc_name_ + "] ?"), SQL_HANDLE_STMT, stmt_);
    BindTvp(1, type_name_, 1);
    Focus(1);
    // Deliberately not restored.
    EXPECT_EQ(SQLExecute(stmt_), SQL_ERROR);
    EXPECT_EQ(ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_), "HY024");
    RestoreFocus();
}

// Benefits-from-mock-tds: pins default-schema resolution; a mock could also
// assert that the prepared declaration leaves the type name unqualified.
TEST_F(TvpTest, UnqualifiedTypeUsesTheCallersDefaultSchemaWithoutDescribe) {
    const std::string schema = "pytvp_default_" + suffix_;
    const std::string user = "pytvp_user_" + suffix_;
    const std::string type = "DefaultOrders";
    const std::string proc = "ReadOrders";
    cleanup_sql_ = {
        "DROP PROCEDURE IF EXISTS [" + schema + "].[" + proc + "]",
        "DROP TYPE IF EXISTS [" + schema + "].[" + type + "]",
        "DROP USER IF EXISTS [" + user + "]",
        "DROP SCHEMA IF EXISTS [" + schema + "]",
    };
    ASSERT_SQL_OK(DirectRc("CREATE SCHEMA [" + schema + "]"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(DirectRc("CREATE TYPE [" + schema + "].[" + type + "] AS TABLE (id INT)"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(DirectRc("CREATE PROCEDURE [" + schema + "].[" + proc + "] @t [" +
                           schema + "].[" + type + "] READONLY AS SELECT id FROM @t"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(DirectRc("CREATE USER [" + user + "] WITHOUT LOGIN WITH DEFAULT_SCHEMA = [" +
                           schema + "]"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(DirectRc("GRANT EXECUTE, REFERENCES ON SCHEMA::[" + schema + "] TO [" + user + "]"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(DirectRc("EXECUTE AS USER = '" + user + "'"), SQL_HANDLE_STMT, stmt_);
    impersonating_ = true;
    ASSERT_SQL_OK(Prepare("EXEC [" + schema + "].[" + proc + "] ?"), SQL_HANDLE_STMT, stmt_);
    auto type_w = ToSqlW(type);
    SQLLEN count = 1;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_SS_TABLE,
                                   1, 0, type_w.data(),
                                   static_cast<SQLLEN>((type_w.size() - 1) * sizeof(SQLWCHAR)), &count),
                  SQL_HANDLE_STMT, stmt_);
    Focus(1);
    SQLINTEGER value = 42;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER,
                                   0, 0, &value, 0, &indicator), SQL_HANDLE_STMT, stmt_);
    RestoreFocus();
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER actual = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &actual, 0, &indicator), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(actual, value);
    EXPECT_EQ(SQLFetch(stmt_), SQL_NO_DATA);
}

// A table type in a non-dbo schema round-trips when the schema is set through
// SQL_CA_SS_SCHEMA_NAME.
TEST_F(TvpTest, SchemaQualifiedType) {
    const std::string schema = "pytvp_s_" + suffix_;
    const std::string type = "qtype_" + suffix_;
    const std::string proc = "qproc_" + suffix_;
    Direct("DROP PROCEDURE IF EXISTS [" + schema + "].[" + proc + "]");
    Direct("DROP TYPE IF EXISTS [" + schema + "].[" + type + "]");
    Direct("DROP SCHEMA IF EXISTS [" + schema + "]");
    ASSERT_SQL_OK(DirectRc("CREATE SCHEMA [" + schema + "]"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(
        DirectRc("CREATE TYPE [" + schema + "].[" + type + "] AS TABLE (id INT)"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(
        DirectRc("CREATE PROCEDURE [" + schema + "].[" + proc + "] @t [" + schema + "].[" +
                 type + "] READONLY AS SELECT id FROM @t ORDER BY id"),
        SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("EXEC [" + schema + "].[" + proc + "] ?"), SQL_HANDLE_STMT, stmt_);
    BindTvp(1, type, 2, schema);
    Focus(1);
    std::array<SQLINTEGER, 2> ids = {7, 9};
    std::array<SQLLEN, 2> id_ind = {0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER,
                                   0, 0, ids.data(), 0, id_ind.data()),
                  SQL_HANDLE_STMT, stmt_);
    RestoreFocus();
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER id = 0;
    SQLLEN id_len = 0;
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &id, 0, &id_len), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(id, 7);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &id, 0, &id_len), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(id, 9);
    EXPECT_EQ(SQLFetch(stmt_), SQL_NO_DATA);

    Direct("DROP PROCEDURE IF EXISTS [" + schema + "].[" + proc + "]");
    Direct("DROP TYPE IF EXISTS [" + schema + "].[" + type + "]");
    Direct("DROP SCHEMA IF EXISTS [" + schema + "]");
}

// A TVP with a mix of column types (bit, bigint, decimal, varbinary) round-trips
// each value through the server.
TEST_F(TvpTest, MixedColumnTypes) {
    const std::string type = "mtype_" + suffix_;
    const std::string proc = "mproc_" + suffix_;
    Direct("DROP PROCEDURE IF EXISTS dbo.[" + proc + "]");
    Direct("DROP TYPE IF EXISTS dbo.[" + type + "]");
    ASSERT_SQL_OK(
        DirectRc("CREATE TYPE dbo.[" + type +
                 "] AS TABLE (flag BIT, big BIGINT, amount DECIMAL(10,2), blob VARBINARY(16))"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(
        DirectRc("CREATE PROCEDURE dbo.[" + proc + "] @t dbo.[" + type +
                 "] READONLY AS SELECT flag, big, amount, blob FROM @t"),
        SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc + "] ?"), SQL_HANDLE_STMT, stmt_);
    BindTvp(1, type, 1);
    Focus(1);

    unsigned char flag = 1;
    SQLLEN flag_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BIT, SQL_BIT, 0, 0,
                                   &flag, 0, &flag_ind),
                  SQL_HANDLE_STMT, stmt_);
    SQLBIGINT big = 9000000000LL;
    SQLLEN big_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SBIGINT, SQL_BIGINT, 0, 0,
                                   &big, 0, &big_ind),
                  SQL_HANDLE_STMT, stmt_);
    // Decimal carried as its character form (the driver resolves it on the wire).
    std::string amount = "1234.56";
    SQLLEN amount_ind = SQL_NTS;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 3, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_DECIMAL, 10, 2,
                                   const_cast<char*>(amount.c_str()),
                                   static_cast<SQLLEN>(amount.size() + 1), &amount_ind),
                  SQL_HANDLE_STMT, stmt_);
    unsigned char blob[4] = {0xDE, 0xAD, 0xBE, 0xEF};
    SQLLEN blob_ind = sizeof(blob);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 4, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_VARBINARY, 16, 0,
                                   blob, sizeof(blob), &blob_ind),
                  SQL_HANDLE_STMT, stmt_);
    RestoreFocus();
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    unsigned char got_flag = 0;
    SQLBIGINT got_big = 0;
    unsigned char got_blob[8] = {0};
    char got_amount[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BIT, &got_flag, 0, &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SBIGINT, &got_big, 0, &ind), SQL_HANDLE_STMT, stmt_);
    SQLLEN amount_len = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 3, SQL_C_CHAR, got_amount, sizeof(got_amount), &amount_len),
                  SQL_HANDLE_STMT, stmt_);
    SQLLEN blob_len = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 4, SQL_C_BINARY, got_blob, sizeof(got_blob), &blob_len),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(got_flag, 1);
    EXPECT_EQ(got_big, 9000000000LL);
    EXPECT_EQ(std::string(got_amount), "1234.56");
    EXPECT_EQ(amount_len, static_cast<SQLLEN>(std::strlen("1234.56")));
    ASSERT_EQ(blob_len, 4);
    EXPECT_EQ(std::memcmp(got_blob, blob, 4), 0);
    EXPECT_EQ(SQLFetch(stmt_), SQL_NO_DATA);

    Direct("DROP PROCEDURE IF EXISTS dbo.[" + proc + "]");
    Direct("DROP TYPE IF EXISTS dbo.[" + type + "]");
}

// Two TVPs with a scalar parameter between them bind and execute together; each
// TVP's columns are captured under its own focus.
TEST_F(TvpTest, MultipleTvpsWithInterleavedScalar) {
    const std::string proc = "m2proc_" + suffix_;
    Direct("DROP PROCEDURE IF EXISTS dbo.[" + proc + "]");
    // Reuses the fixture's (id, label) type for both TVPs.
    ASSERT_SQL_OK(
        DirectRc("CREATE PROCEDURE dbo.[" + proc + "] @a dbo.[" + type_name_ +
                 "] READONLY, @n INT, @b dbo.[" + type_name_ +
                 "] READONLY AS SELECT COUNT(*) + @n AS c FROM (SELECT id FROM @a "
                 "UNION ALL SELECT id FROM @b) u"),
        SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("EXEC dbo.[" + proc + "] ?, ?, ?"), SQL_HANDLE_STMT, stmt_);

    BindTvp(1, type_name_, 2);
    BindTvp(3, type_name_, 1);
    SQLINTEGER n = 100;
    SQLLEN n_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   &n, 0, &n_ind),
                  SQL_HANDLE_STMT, stmt_);

    // TVP @a: two rows.
    Focus(1);
    std::array<SQLINTEGER, 2> a_ids = {1, 2};
    std::array<SQLLEN, 2> a_ind = {0, 0};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   a_ids.data(), 0, a_ind.data()),
                  SQL_HANDLE_STMT, stmt_);
    const SQLULEN cap = 50;
    std::vector<SQLWCHAR> a_labels((cap + 1) * 2, 0);
    std::array<SQLLEN, 2> a_lind = {SQL_NULL_DATA, SQL_NULL_DATA};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR, cap, 0,
                                   a_labels.data(),
                                   static_cast<SQLLEN>((cap + 1) * sizeof(SQLWCHAR)),
                                   a_lind.data()),
                  SQL_HANDLE_STMT, stmt_);
    RestoreFocus();

    // TVP @b: one row.
    Focus(3);
    SQLINTEGER b_id = 9;
    SQLLEN b_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   &b_id, 0, &b_ind),
                  SQL_HANDLE_STMT, stmt_);
    std::vector<SQLWCHAR> b_labels(cap + 1, 0);
    SQLLEN b_lind = SQL_NULL_DATA;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR, cap, 0,
                                   b_labels.data(),
                                   static_cast<SQLLEN>((cap + 1) * sizeof(SQLWCHAR)), &b_lind),
                  SQL_HANDLE_STMT, stmt_);
    RestoreFocus();

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER total = 0;
    SQLLEN total_ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &total, 0, &total_ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(total, 103) << "2 rows + 1 row + @n(100)";

    Direct("DROP PROCEDURE IF EXISTS dbo.[" + proc + "]");
}

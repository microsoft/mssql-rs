// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"
#include "odbc_concurrent_test.h"

#include <algorithm>
#include <optional>
#include <random>
#include <string>
#include <vector>

namespace {

using Cell = std::optional<std::u16string>;
using Row = std::vector<Cell>;
using Rows = std::vector<Row>;
constexpr SQLSMALLINT kSqlSsJson = -157;
// The fixture must negotiate VECTOR on its own connection, not a lent DBC.
constexpr unsigned kCatalogModes = odbc_concurrency::kSingle |
    odbc_concurrency::kCrossThread | odbc_concurrency::kSharedEnv;

std::basic_string<SQLWCHAR> Wide(const std::string& text) {
    return std::basic_string<SQLWCHAR>(text.begin(), text.end());
}

void ReadRows(SQLHSTMT stmt, Rows& rows) {
    SQLSMALLINT count = 0;
    ASSERT_SQL_OK(SQLNumResultCols(stmt, &count), SQL_HANDLE_STMT, stmt);
    ASSERT_GT(count, 0);
    SQLRETURN rc;
    while (SQL_SUCCEEDED(rc = SQLFetch(stmt))) {
        Row row;
        for (SQLUSMALLINT column = 1; column <= count; ++column) {
            SQLWCHAR buffer[4096] = {};
            SQLLEN indicator = 0;
            ASSERT_EQ(SQL_SUCCESS, SQLGetData(stmt, column, SQL_C_WCHAR, buffer,
                                              sizeof(buffer), &indicator))
                << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt);
            if (indicator == SQL_NULL_DATA) {
                row.push_back(std::nullopt);
            } else {
                ASSERT_GE(indicator, 0);
                ASSERT_LT(static_cast<size_t>(indicator), sizeof(buffer));
                std::u16string value;
                for (SQLLEN i = 0; i < indicator / sizeof(SQLWCHAR); ++i)
                    value.push_back(static_cast<char16_t>(buffer[i]));
                row.push_back(std::move(value));
            }
        }
        rows.push_back(std::move(row));
    }
    ASSERT_EQ(SQL_NO_DATA, rc) << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt);
    ASSERT_SQL_OK(SQLFreeStmt(stmt, SQL_CLOSE), SQL_HANDLE_STMT, stmt);
}

class JsonCatalogLiveTest : public ODBCTest {
protected:
    std::string table_;
    bool created_ = false;

    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection())
            GTEST_SKIP() << "No SQL Server connection configured";
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_), SQL_HANDLE_ENV, env_);
        const SqlTString connection = ODBCTestUtils::BuildConnectionString() +
            ODBCTestUtils::ToSqlTStr(";VectorTypeSupport=v1;");
        ASSERT_SQL_OK(SQLDriverConnect(dbc_, nullptr, const_cast<SQLTCHAR*>(connection.c_str()),
            SQL_NTS, nullptr, 0, nullptr, SQL_DRIVER_NOPROMPT), SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_), SQL_HANDLE_DBC, dbc_);
        SQLCHAR version[128] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        std::cout << "JSON catalog reference: SQL_DRIVER_VER="
                  << reinterpret_cast<const char*>(version) << std::endl;
        ASSERT_SQL_OK(SQLExecDirectA(stmt_, reinterpret_cast<SQLCHAR*>(
            const_cast<char*>("SELECT CASE WHEN TYPE_ID(N'json') IS NULL THEN 0 ELSE 1 END")),
            SQL_NTS), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER supported = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &supported, sizeof(supported), nullptr),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        if (!supported)
            GTEST_SKIP() << "Server has no native JSON type";
        std::random_device random;
        table_ = "odbc_json_catalog_" + std::to_string(random()) + "_" + std::to_string(random());
        ASSERT_NO_FATAL_FAILURE(ExecDirect("CREATE TABLE dbo.[" + table_ +
            "] (id int NOT NULL, j json NULL, jr json NOT NULL, n nvarchar(max) NULL,"
            " v varchar(max) NULL, x xml NULL)"));
        created_ = true;
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }

    void TearDown() override {
        if (created_) {
            SQLFreeStmt(stmt_, SQL_CLOSE);
            const std::string sql = "DROP TABLE dbo.[" + table_ + "]";
            EXPECT_TRUE(SQL_SUCCEEDED(SQLExecDirectA(
                stmt_, reinterpret_cast<SQLCHAR*>(const_cast<char*>(sql.c_str())), SQL_NTS)))
                << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        }
        ODBCTest::TearDown();
    }

    void ServerRows(const std::string& sql, Rows& rows) {
        ASSERT_SQL_OK(SQLExecDirectA(stmt_, reinterpret_cast<SQLCHAR*>(
            const_cast<char*>(sql.c_str())), SQL_NTS), SQL_HANDLE_STMT, stmt_);
        ASSERT_NO_FATAL_FAILURE(ReadRows(stmt_, rows));
    }
};

// Benefits-from-mock-tds: pins _170 dispatch and missing-procedure cache on the wire;
// api::catalog_procedure tests cover those branches without requiring this server.
ODBC_TEST_F_MODES(JsonCatalogLiveTest, ColumnsEqualServerRowsThroughAnsiUnicodeAndQualifiedCalls,
                  kCatalogModes) {
    Rows expected;
    ASSERT_NO_FATAL_FAILURE(ServerRows("EXEC sys.sp_columns_170 @table_name=N'" + table_ +
        "', @table_owner=N'dbo', @ODBCVer=3", expected));
    ASSERT_EQ(6u, expected.size());
    for (bool wide : {false, true}) {
        for (bool qualified : {false, true}) {
            SCOPED_TRACE(wide ? "Unicode" : "ANSI");
            SCOPED_TRACE(qualified ? "qualified" : "current database");
            const std::string database = ODBCTestConfig::Instance().Database();
            SQLRETURN rc;
            if (wide) {
                auto table = Wide(table_);
                auto schema = Wide("dbo");
                auto catalog = Wide(database);
                rc = SQLColumnsW(stmt_, qualified ? const_cast<SQLWCHAR*>(catalog.c_str()) : nullptr,
                    qualified ? SQL_NTS : 0, const_cast<SQLWCHAR*>(schema.c_str()), SQL_NTS,
                    const_cast<SQLWCHAR*>(table.c_str()), SQL_NTS, nullptr, 0);
            } else {
                rc = SQLColumnsA(stmt_, qualified ? reinterpret_cast<SQLCHAR*>(
                    const_cast<char*>(database.c_str())) : nullptr, qualified ? SQL_NTS : 0,
                    reinterpret_cast<SQLCHAR*>(const_cast<char*>("dbo")), SQL_NTS,
                    reinterpret_cast<SQLCHAR*>(table_.data()), SQL_NTS, nullptr, 0);
            }
            ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
            Rows actual;
            ASSERT_NO_FATAL_FAILURE(ReadRows(stmt_, actual));
            EXPECT_EQ(expected, actual);
            const auto json = std::find_if(actual.begin(), actual.end(), [](const Row& row) {
                return row.size() > 17 && row[3] == Cell(u"j");
            });
            ASSERT_NE(actual.end(), json);
            EXPECT_EQ(Cell(u"-157"), (*json)[4]);
            EXPECT_EQ(Cell(u"json"), (*json)[5]);
            EXPECT_EQ(Cell(u"-157"), (*json)[13]);
            const auto required = std::find_if(actual.begin(), actual.end(), [](const Row& row) {
                return row.size() > 17 && row[3] == Cell(u"jr");
            });
            ASSERT_NE(actual.end(), required);
            EXPECT_EQ(Cell(u"0"), (*required)[10]);
            EXPECT_EQ(Cell(u"NO"), (*required)[17]);
        }
    }
}

// Benefits-from-mock-tds: api::catalog_procedure tests pin the selected RPC and fallback;
// this checks every cell from the real server's procedure and ODBC metadata fixups.
ODBC_TEST_F_MODES(JsonCatalogLiveTest, TypeInfoEqualsServerRowsForJsonAllTypesAndTextFilters,
                  kCatalogModes) {
    for (SQLSMALLINT type : {static_cast<SQLSMALLINT>(SQL_ALL_TYPES), kSqlSsJson,
                            static_cast<SQLSMALLINT>(SQL_WVARCHAR),
                            static_cast<SQLSMALLINT>(SQL_WLONGVARCHAR),
                            static_cast<SQLSMALLINT>(SQL_VARCHAR)}) {
        SCOPED_TRACE(type);
        Rows expected;
        ASSERT_NO_FATAL_FAILURE(ServerRows("EXEC sys.sp_datatype_info_170 @data_type=" +
            std::to_string(type) + ", @ODBCVer=4", expected));
        for (bool wide : {false, true}) {
            ASSERT_SQL_OK(wide ? SQLGetTypeInfoW(stmt_, type) : SQLGetTypeInfoA(stmt_, type),
                          SQL_HANDLE_STMT, stmt_);
            SQLWCHAR name[128] = {};
            SQLSMALLINT length = 0, dataType = 0, digits = 0, nullable = 0;
            SQLULEN size = 0;
            ASSERT_SQL_OK(SQLDescribeColW(stmt_, 3, name, 128, &length, &dataType, &size,
                                          &digits, &nullable), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(Wide("COLUMN_SIZE"), std::basic_string<SQLWCHAR>(name));
            ASSERT_SQL_OK(SQLDescribeColW(stmt_, 1, name, 128, &length, &dataType, &size,
                                          &digits, &nullable), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(SQL_NO_NULLS, nullable);
            Rows actual;
            ASSERT_NO_FATAL_FAILURE(ReadRows(stmt_, actual));
            EXPECT_EQ(expected, actual);
            const bool listsJson = type == SQL_ALL_TYPES || type == kSqlSsJson;
            EXPECT_EQ(listsJson, std::any_of(actual.begin(), actual.end(), [](const Row& row) {
                return row.size() > 1 && row[1] == Cell(u"-157");
            }));
            if (type == kSqlSsJson) {
                ASSERT_EQ(1u, actual.size());
                EXPECT_EQ(Cell(u"json"), actual[0][0]);
                EXPECT_EQ(Cell(u"2147483647"), actual[0][2]);
                EXPECT_EQ(Cell(u"1"), actual[0][7]);
            }
        }
    }
}

} // namespace

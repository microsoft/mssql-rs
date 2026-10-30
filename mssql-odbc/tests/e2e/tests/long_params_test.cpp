// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"
#include "odbc_concurrent_test.h"
#include "odbc_sql_server_types.h"

#include <algorithm>
#include <array>
#include <cstdint>
#include <optional>
#include <string>
#include <vector>

namespace {
struct LongType {
    SQLSMALLINT sql_type;
    SQLSMALLINT c_type;
    const char* legacy;
    const char* max;
};

constexpr LongType kTypes[] = {
    {SQL_LONGVARCHAR, SQL_C_CHAR, "text", "varchar"},
    {SQL_WLONGVARCHAR, SQL_C_WCHAR, "ntext", "nvarchar"},
    {SQL_LONGVARBINARY, SQL_C_BINARY, "image", "varbinary"},
};

enum class ValueKind { Value, Empty, Null };

std::string ProbeSql(const LongType& type) {
    return "SET NOCOUNT ON; SELECT ? AS v INTO #odbc_long_probe;"
        "SELECT t.name,c.max_length,DATALENGTH(p.v),"
        "CONVERT(varchar(max),CONVERT(varbinary(max),CONVERT("
        + std::string(type.max) + "(max),p.v)),2)"
        " FROM tempdb.sys.columns c"
        " JOIN tempdb.sys.types t ON c.user_type_id=t.user_type_id"
        " CROSS JOIN #odbc_long_probe p"
        " WHERE c.object_id=OBJECT_ID('tempdb..#odbc_long_probe');"
        "DROP TABLE #odbc_long_probe;";
}

std::vector<SQLCHAR> Bytes(const LongType& type, const std::string& text) {
    std::vector<SQLCHAR> result;
    for (unsigned char ch : text) {
        result.push_back(ch);
        if (type.c_type == SQL_C_WCHAR) result.push_back(0);
    }
    return result;
}

std::string Hex(const std::vector<SQLCHAR>& bytes) {
    constexpr char digits[] = "0123456789ABCDEF";
    std::string result;
    for (SQLCHAR byte : bytes) {
        result.push_back(digits[byte >> 4]);
        result.push_back(digits[byte & 15]);
    }
    return result;
}
}

class LongParameterTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection());
        Connect();
        SQLCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("SQL_DRIVER_VER", reinterpret_cast<const char*>(version));
    }

    SQLRETURN Prepare(const std::string& sql = ProbeSql(kTypes[0])) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    SQLRETURN Direct(const std::string& sql) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    void SetLongAsMax(SQLULEN value) {
        ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_LONGASMAX,
                                       reinterpret_cast<SQLPOINTER>(value), 0),
                      SQL_HANDLE_DBC, dbc_);
        SQLUINTEGER actual = 0;
        SQLINTEGER length = -1;
        ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_COPT_SS_LONGASMAX, &actual, 0, &length),
                      SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(static_cast<SQLUINTEGER>(value), actual);
        EXPECT_EQ(sizeof(actual), static_cast<size_t>(length));
    }

    void ReadProbe(const LongType& type, bool as_max,
                   const std::vector<SQLCHAR>& bytes, bool is_null) {
        SQLSMALLINT columns = 0;
        ASSERT_SQL_OK(SQLNumResultCols(stmt_, &columns), SQL_HANDLE_STMT, stmt_);
        while (columns == 0) {
            ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLNumResultCols(stmt_, &columns), SQL_HANDLE_STMT, stmt_);
        }
        ASSERT_EQ(4, columns);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLCHAR name[32] = {};
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, name, sizeof(name), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(std::string(as_max ? type.max : type.legacy),
                  reinterpret_cast<const char*>(name));
        SQLINTEGER maximum = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &maximum, 0, &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(as_max ? -1 : 16, maximum);
        SQLINTEGER size = -1;
        ASSERT_SQL_OK(SQLGetData(stmt_, 3, SQL_C_SLONG, &size, 0, &indicator),
                      SQL_HANDLE_STMT, stmt_);
        if (is_null) {
            EXPECT_EQ(SQL_NULL_DATA, indicator);
        } else {
            EXPECT_EQ(bytes.size(), static_cast<size_t>(size));
        }
        SQLCHAR hex[2048] = {};
        ASSERT_SQL_OK(SQLGetData(stmt_, 4, SQL_C_CHAR, hex, sizeof(hex), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        if (is_null) {
            EXPECT_EQ(SQL_NULL_DATA, indicator);
        } else {
            EXPECT_EQ(Hex(bytes), reinterpret_cast<const char*>(hex));
        }
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    }

    void Drain() {
        SQLRETURN rc;
        while ((rc = SQLMoreResults(stmt_)) != SQL_NO_DATA) {
            ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        }
    }

    void Probe(const LongType& type, bool as_max, bool prepared, bool streamed,
               ValueKind kind, const std::string& text, SQLULEN column_size = 4096) {
        SCOPED_TRACE(::testing::Message() << type.legacy << " max=" << as_max
                     << " prepared=" << prepared << " DAE=" << streamed
                     << " kind=" << static_cast<int>(kind));
        if (prepared) ASSERT_SQL_OK(Prepare(ProbeSql(type)), SQL_HANDLE_STMT, stmt_);
        auto bytes = kind == ValueKind::Value ? Bytes(type, text) : std::vector<SQLCHAR>{};
        SQLCHAR empty = 0, token = 0;
        SQLPOINTER data = bytes.empty() ? &empty : bytes.data();
        SQLLEN indicator = streamed ? SQL_DATA_AT_EXEC
            : kind == ValueKind::Null ? SQL_NULL_DATA : static_cast<SQLLEN>(bytes.size());
        ASSERT_SQL_OK(SQLBindParameter(
            stmt_, 1, SQL_PARAM_INPUT,
            kind == ValueKind::Null && !streamed ? SQL_C_DEFAULT : type.c_type,
            type.sql_type, column_size, 0, streamed ? &token : data,
            streamed ? 0 : static_cast<SQLLEN>(bytes.size()), &indicator),
            SQL_HANDLE_STMT, stmt_);
        SQLRETURN rc = prepared ? SQLExecute(stmt_) : Direct(ProbeSql(type));
        if (streamed) {
            ASSERT_EQ(SQL_NEED_DATA, rc);
            SQLPOINTER returned = nullptr;
            ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &returned));
            ASSERT_EQ(&token, returned);
            if (kind == ValueKind::Null) {
                ASSERT_SQL_OK(SQLPutData(stmt_, nullptr, SQL_NULL_DATA), SQL_HANDLE_STMT, stmt_);
            } else if (bytes.empty()) {
                ASSERT_SQL_OK(SQLPutData(stmt_, &empty, 0), SQL_HANDLE_STMT, stmt_);
            } else {
                const size_t split = type.c_type == SQL_C_WCHAR ? 2 : 1;
                ASSERT_SQL_OK(SQLPutData(stmt_, bytes.data(), static_cast<SQLLEN>(split)),
                              SQL_HANDLE_STMT, stmt_);
                ASSERT_SQL_OK(SQLPutData(stmt_, bytes.data() + split,
                                        static_cast<SQLLEN>(bytes.size() - split)),
                              SQL_HANDLE_STMT, stmt_);
            }
            rc = SQLParamData(stmt_, &returned);
        }
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        ReadProbe(type, as_max, bytes, kind == ValueKind::Null);
        Drain();
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }
};

ODBC_TEST_F(LongParameterTest, LongTypesUseLegacyDeclarationsByDefault) {
    SQLUINTEGER setting = 99;
    ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_COPT_SS_LONGASMAX, &setting, 0, nullptr),
                  SQL_HANDLE_DBC, dbc_);
    ASSERT_EQ(SQL_XL_OFF, setting);
    for (const auto& type : kTypes) {
        for (bool prepared : {false, true}) {
            for (ValueKind kind : {ValueKind::Value, ValueKind::Empty, ValueKind::Null}) {
                Probe(type, false, prepared, false, kind, WorkerToken("legacy"));
            }
        }
    }
}

ODBC_TEST_F(LongParameterTest, LongAsMaxChangesAllThreeDeclarations) {
    for (SQLULEN setting : {SQLULEN(0), SQLULEN(1), SQLULEN(2)}) {
        SetLongAsMax(setting);
        for (const auto& type : kTypes) {
            for (bool prepared : {false, true}) {
                for (ValueKind kind : {ValueKind::Value, ValueKind::Empty, ValueKind::Null}) {
                    Probe(type, setting == SQL_XL_ON, prepared, false, kind, WorkerToken("option"));
                }
            }
        }
    }
}

// Benefits-from-mock-tds: the server observes the declaration and value, not
// the PLP header. streamed_legacy_declaration_keeps_a_max_wire_header pins it.
ODBC_TEST_F(LongParameterTest, DataAtExecutionKeepsLegacyDeclarationsUnlessOptedIn) {
    for (SQLULEN setting : {SQLULEN(0), SQLULEN(1)}) {
        SetLongAsMax(setting);
        for (const auto& type : kTypes) {
            for (bool prepared : {false, true}) {
                for (ValueKind kind : {ValueKind::Value, ValueKind::Empty, ValueKind::Null}) {
                    Probe(type, setting == SQL_XL_ON, prepared, true, kind, WorkerToken("stream"));
                }
            }
        }
    }
}

ODBC_TEST_F(LongParameterTest, ExplicitMaxTypesAreUnaffected) {
    const LongType types[] = {
        {SQL_VARCHAR, SQL_C_CHAR, "text", "varchar"},
        {SQL_WVARCHAR, SQL_C_WCHAR, "ntext", "nvarchar"},
        {SQL_VARBINARY, SQL_C_BINARY, "image", "varbinary"},
    };
    for (SQLULEN setting : {SQLULEN(0), SQLULEN(1)}) {
        SetLongAsMax(setting);
        for (const auto& type : types) {
            Probe(type, true, true, false, ValueKind::Value, WorkerToken("max"), 0);
            Probe(type, true, true, true, ValueKind::Value, WorkerToken("max_stream"), 0);
        }
    }
}

ODBC_TEST_F(LongParameterTest, DescribedLongNullsExecuteTheDescribedInsert) {
    SetLongAsMax(SQL_XL_OFF);
    // Global temp tables are server-wide; the session id keeps concurrent runs
    // apart, as in transaction_test.cpp.
    ASSERT_SQL_OK(Direct("SELECT @@SPID"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER spid = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &spid, 0, nullptr), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    for (const auto& type : kTypes) {
        const std::string table =
            "[##" + WorkerToken(type.legacy) + "-" + std::to_string(spid) + "]";
        ASSERT_SQL_OK(Direct("CREATE TABLE " + table + " (v " + type.legacy + ")"),
                      SQL_HANDLE_STMT, stmt_);
        Drain();
        ASSERT_SQL_OK(Prepare("INSERT INTO " + table + " (v) VALUES (?)"),
                      SQL_HANDLE_STMT, stmt_);
        SQLSMALLINT sql_type = 0, digits = -1, nullable = -1;
        SQLULEN size = 0;
        ASSERT_SQL_OK(SQLDescribeParam(stmt_, 1, &sql_type, &size, &digits, &nullable),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(type.sql_type, sql_type);
        ASSERT_GT(size, SQLULEN(0));
        SQLLEN indicator = SQL_NULL_DATA;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                                      sql_type, size, digits, nullptr, 0, &indicator),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        Drain();
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(Direct("SELECT v FROM " + table), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLCHAR value[8] = {};
        SQLLEN length = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, value, sizeof(value), &length),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_NULL_DATA, length);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        Drain();
        ASSERT_SQL_OK(Direct("DROP TABLE " + table), SQL_HANDLE_STMT, stmt_);
        Drain();
    }
}

// Benefits-from-mock-tds: pins the reused server declaration; a mock would also
// assert that changing the connection option did not issue sp_unprepare.
ODBC_TEST_F(LongParameterTest, AttributeChangesDoNotInvalidateCachedDeclarations) {
    const auto& type = kTypes[0];
    SetLongAsMax(SQL_XL_OFF);
    ASSERT_SQL_OK(Prepare(), SQL_HANDLE_STMT, stmt_);
    auto bytes = Bytes(type, WorkerToken("cached"));
    SQLLEN indicator = static_cast<SQLLEN>(bytes.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, type.c_type,
                                  type.sql_type, 4096, 0, bytes.data(), indicator, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    for (SQLULEN setting : {SQLULEN(0), SQLULEN(0), SQLULEN(1), SQLULEN(1)}) {
        SetLongAsMax(setting);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ReadProbe(type, false, bytes, false);
        Drain();
    }
    ASSERT_SQL_OK(Prepare(), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ReadProbe(type, true, bytes, false);
    Drain();
}

ODBC_TEST_F(LongParameterTest, LongAsMaxStillEnforcesColumnSize) {
    for (SQLULEN setting : {SQLULEN(0), SQLULEN(1)}) {
        SetLongAsMax(setting);
        for (const auto& type : kTypes) {
            ASSERT_SQL_OK(Prepare("SELECT ?"), SQL_HANDLE_STMT, stmt_);
            auto bytes = Bytes(type, "abcd");
            SQLLEN indicator = static_cast<SQLLEN>(bytes.size());
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, type.c_type,
                                          type.sql_type, 3, 0, bytes.data(), indicator, &indicator),
                          SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(SQL_ERROR, SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, type.c_type,
                                                  type.sql_type, 0, 0, bytes.data(),
                                                  indicator, &indicator));
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY104");
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
        }
    }
}

ODBC_TEST_F(LongParameterTest, ParameterArraysUseTheExecutionOptionForEveryRow) {
    for (SQLULEN setting : {SQLULEN(0), SQLULEN(1)}) {
        SetLongAsMax(setting);
        for (const auto& type : kTypes) {
            const std::array<std::vector<SQLCHAR>, 3> values = {
                Bytes(type, WorkerToken("array_a")),
                Bytes(type, WorkerToken("array_b")), std::vector<SQLCHAR>{}
            };
            const size_t stride = (std::max)(values[0].size(), values[1].size()) + 2;
            std::vector<SQLCHAR> buffer(stride * values.size());
            for (size_t row = 0; row < values.size(); ++row) {
                std::copy(values[row].begin(), values[row].end(), buffer.begin() + row * stride);
            }
            SQLLEN indicators[] = {static_cast<SQLLEN>(values[0].size()),
                                   static_cast<SQLLEN>(values[1].size()), SQL_NULL_DATA};
            SQLUSMALLINT statuses[] = {SQL_PARAM_UNUSED, SQL_PARAM_UNUSED, SQL_PARAM_UNUSED};
            SQLULEN processed = 0;
            ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAMSET_SIZE,
                                         reinterpret_cast<SQLPOINTER>(values.size()), 0),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAM_STATUS_PTR, statuses, 0),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAMS_PROCESSED_PTR, &processed, 0),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(Prepare(ProbeSql(type)), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, type.c_type,
                                          type.sql_type, 4096, 0, buffer.data(),
                                          static_cast<SQLLEN>(stride), indicators),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
            for (size_t row = 0; row < values.size(); ++row) {
                if (row != 0) ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
                ReadProbe(type, setting == SQL_XL_ON, values[row], row == 2);
            }
            Drain();
            EXPECT_EQ(values.size(), processed);
            for (auto status : statuses) EXPECT_EQ(SQL_PARAM_SUCCESS, status);
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAMSET_SIZE,
                                         reinterpret_cast<SQLPOINTER>(SQLULEN(1)), 0),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAM_STATUS_PTR, nullptr, 0),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAMS_PROCESSED_PTR, nullptr, 0),
                          SQL_HANDLE_STMT, stmt_);
        }
    }
}

class LongParameterConnectTest : public LongParameterTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection());
    }
};

TEST_F(LongParameterConnectTest, KeywordsAndPreconnectAttributesHaveNativePrecedence) {
    const std::optional<SQLULEN> overrides[] = {std::nullopt, SQLULEN(0), SQLULEN(1), SQLULEN(2)};
    for (const auto& override : overrides) {
        for (const std::string keyword : {"No", "Yes", "bogus"}) {
            ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_), SQL_HANDLE_ENV, env_);
            if (override) {
                ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_LONGASMAX,
                                               reinterpret_cast<SQLPOINTER>(*override), 0),
                              SQL_HANDLE_DBC, dbc_);
            }
            auto connection = ODBCTestUtils::ToSqlTStr("LongAsMax=" + keyword + ";")
                + ODBCTestUtils::BuildConnectionString();
            ASSERT_SQL_OK(SQLDriverConnect(dbc_, nullptr, const_cast<SQLTCHAR*>(connection.c_str()),
                                          SQL_NTS, nullptr, 0, nullptr, SQL_DRIVER_NOPROMPT),
                          SQL_HANDLE_DBC, dbc_);
            const SQLULEN expected = override.value_or(keyword == "Yes" ? SQL_XL_ON : SQL_XL_OFF);
            SQLUINTEGER actual = 99;
            SQLINTEGER length = -1;
            ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_COPT_SS_LONGASMAX, &actual, 0, &length),
                          SQL_HANDLE_DBC, dbc_);
            ASSERT_EQ(expected, actual);
            ASSERT_EQ(4, length);
            ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_), SQL_HANDLE_DBC, dbc_);
            Probe(kTypes[0], expected == SQL_XL_ON, true, false, ValueKind::Value, "keyword");
            FreeStmt(stmt_);
            ASSERT_SQL_OK(SQLDisconnect(dbc_), SQL_HANDLE_DBC, dbc_);
            ASSERT_SQL_OK(SQLFreeHandle(SQL_HANDLE_DBC, dbc_), SQL_HANDLE_DBC, dbc_);
            dbc_ = SQL_NULL_HDBC;
        }
    }
}

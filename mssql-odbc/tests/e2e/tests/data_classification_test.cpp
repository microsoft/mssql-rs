// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// data_classification_test.cpp - SQL_COPT_SS_DATACLASSIFICATION_VERSION and the
// IRD header fields SQL_CA_SS_DATA_CLASSIFICATION(_VERSION).
//
// msodbcsql requests version 1 on every connection (sqlcfunc.cpp:3487) and
// returns the DATACLASSIFICATION payload, every byte after the 0xA3 token
// type, as the server sent it (Parse.cpp ProcessDataClassification,
// sqlctokn.cpp OnDataClassification). The payload is parsed here with the
// [MS-TDS] 2.2.7.5 layout and must be consumed exactly, so both drivers are
// held to the wire format rather than to each other's output.

#include "odbc_test_fixture.h"

#include <cstdint>
#include <cstring>
#include <optional>
#include <random>
#include <string>
#include <vector>

namespace {

constexpr SQLINTEGER kDataClassificationVersion = 1400;
constexpr SQLSMALLINT kCaSsDataClassification = 1237;
constexpr SQLSMALLINT kCaSsDataClassificationVersion = 1238;

struct Classification {
    const char* label;
    const char* label_id;
    const char* information_type;
    const char* information_type_id;
    const char* rank_name;
    int32_t rank;
};

constexpr Classification kSsn = {"Highly Confidential", "3302ae7f-b8ac-46bc-97f8-378828781efd",
                                 "National ID", "6f5a11a7-08b1-19c3-59e5-8c89cf4f8444",
                                 "CRITICAL", 40};
constexpr Classification kEmail = {"Confidential", "331f0b13-76b5-2f1b-a77b-def5a73c73c2",
                                   "Contact Info", "5c503e21-22c6-81fa-620b-f369b8ec38d1",
                                   "MEDIUM", 20};

struct Property {
    std::string label;
    std::string label_id;
    std::string information_type;
    std::string information_type_id;
    std::optional<int32_t> rank;
};

struct Payload {
    std::optional<int32_t> rank;
    std::vector<std::vector<Property>> columns;
};

class Reader {
public:
    explicit Reader(const std::vector<unsigned char>& bytes) : bytes_(bytes) {}

    bool U16(uint16_t& value) {
        if (pos_ + 2 > bytes_.size()) return false;
        value = static_cast<uint16_t>(bytes_[pos_] | (bytes_[pos_ + 1] << 8));
        pos_ += 2;
        return true;
    }

    bool I32(int32_t& value) {
        if (pos_ + 4 > bytes_.size()) return false;
        uint32_t raw = 0;
        for (int i = 3; i >= 0; --i) raw = (raw << 8) | bytes_[pos_ + i];
        std::memcpy(&value, &raw, sizeof(value));
        pos_ += 4;
        return true;
    }

    // B_VARCHAR of ASCII text, which is all these tests classify with.
    bool BVarchar(std::string& value) {
        if (pos_ >= bytes_.size()) return false;
        size_t units = bytes_[pos_++];
        if (pos_ + units * 2 > bytes_.size()) return false;
        value.clear();
        for (size_t i = 0; i < units; ++i, pos_ += 2) {
            if (bytes_[pos_ + 1] != 0 || bytes_[pos_] > 0x7F) return false;
            value.push_back(static_cast<char>(bytes_[pos_]));
        }
        return true;
    }

    bool AtEnd() const { return pos_ == bytes_.size(); }

private:
    const std::vector<unsigned char>& bytes_;
    size_t pos_ = 0;
};

// Parses a DATACLASSIFICATION payload, resolving dictionary references.
// Returns nullopt unless the bytes follow the layout for `v2` exactly.
std::optional<Payload> Parse(const std::vector<unsigned char>& bytes, bool v2) {
    Reader r(bytes);
    using Pair = std::pair<std::string, std::string>;
    std::vector<Pair> dictionaries[2];
    for (auto& dictionary : dictionaries) {
        uint16_t count = 0;
        if (!r.U16(count)) return std::nullopt;
        for (uint16_t i = 0; i < count; ++i) {
            Pair entry;
            if (!r.BVarchar(entry.first) || !r.BVarchar(entry.second)) return std::nullopt;
            dictionary.push_back(entry);
        }
    }
    Payload payload;
    if (v2) {
        int32_t rank = 0;
        if (!r.I32(rank)) return std::nullopt;
        payload.rank = rank;
    }
    uint16_t columns = 0;
    if (!r.U16(columns)) return std::nullopt;
    for (uint16_t c = 0; c < columns; ++c) {
        uint16_t count = 0;
        if (!r.U16(count)) return std::nullopt;
        std::vector<Property> properties;
        for (uint16_t p = 0; p < count; ++p) {
            uint16_t label = 0;
            uint16_t information_type = 0;
            if (!r.U16(label) || !r.U16(information_type)) return std::nullopt;
            Property property;
            if (label != 0xFFFF) {
                if (label >= dictionaries[0].size()) return std::nullopt;
                property.label = dictionaries[0][label].first;
                property.label_id = dictionaries[0][label].second;
            }
            if (information_type != 0xFFFF) {
                if (information_type >= dictionaries[1].size()) return std::nullopt;
                property.information_type = dictionaries[1][information_type].first;
                property.information_type_id = dictionaries[1][information_type].second;
            }
            if (v2) {
                int32_t rank = 0;
                if (!r.I32(rank)) return std::nullopt;
                property.rank = rank;
            }
            properties.push_back(property);
        }
        payload.columns.push_back(properties);
    }
    if (!r.AtEnd()) return std::nullopt;
    return payload;
}

void ExpectClassified(const std::vector<Property>& column, const Classification& expected,
                      bool v2) {
    ASSERT_EQ(1u, column.size());
    EXPECT_EQ(expected.label, column[0].label);
    EXPECT_EQ(expected.label_id, column[0].label_id);
    EXPECT_EQ(expected.information_type, column[0].information_type);
    EXPECT_EQ(expected.information_type_id, column[0].information_type_id);
    if (v2) {
        EXPECT_EQ(std::optional<int32_t>(expected.rank), column[0].rank);
    } else {
        EXPECT_FALSE(column[0].rank.has_value());
    }
}

}  // namespace

// Not ODBC_TEST_F: every case sets connection attributes or connects again,
// and creates a classified table.
class DataClassificationTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_), SQL_HANDLE_ENV, env_);
    }

    // Drops the table on a fresh statement: stmt_ may still carry a bound
    // parameter array that would turn the DROP into an array execution.
    void TearDown() override {
        if (!table_.empty() && stmt_ != SQL_NULL_HSTMT) {
            SQLFreeStmt(stmt_, SQL_CLOSE);
            SQLHSTMT cleanup = SQL_NULL_HSTMT;
            if (SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &cleanup))) {
                auto drop = ODBCTestUtils::ToSqlTStr("DROP TABLE IF EXISTS dbo." + table_);
                SQLExecDirect(cleanup, const_cast<SQLTCHAR*>(drop.c_str()), SQL_NTS);
                SQLFreeHandle(SQL_HANDLE_STMT, cleanup);
            }
        }
        ODBCTest::TearDown();
    }

    void ConnectDbc() {
        SqlTString connstr = ODBCTestUtils::BuildConnectionString();
        ASSERT_SQL_OK(SQLDriverConnect(dbc_, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                       static_cast<SQLSMALLINT>(connstr.size()), nullptr, 0,
                                       nullptr, SQL_DRIVER_NOPROMPT),
                      SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_), SQL_HANDLE_DBC, dbc_);
        SQLCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("SQL_DRIVER_VER", reinterpret_cast<const char*>(version));
    }

    void Reconnect() {
        if (stmt_ != SQL_NULL_HSTMT) {
            SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
            stmt_ = SQL_NULL_HSTMT;
        }
        ASSERT_SQL_OK(SQLDisconnect(dbc_), SQL_HANDLE_DBC, dbc_);
        ConnectDbc();
    }

    SQLRETURN SetRequestedVersion(SQLULEN version) {
        return SQLSetConnectAttrW(dbc_, kDataClassificationVersion,
                                  reinterpret_cast<SQLPOINTER>(version), 0);
    }

    SQLUINTEGER RequestedVersion() {
        SQLUINTEGER value = 0xAAAAAAAA;
        SQLINTEGER length = -1;
        EXPECT_SQL_OK(SQLGetConnectAttrW(dbc_, kDataClassificationVersion, &value,
                                         sizeof(value), &length),
                      SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(static_cast<SQLINTEGER>(sizeof(value)), length);
        return value;
    }

    // `id, ssn, email, note`, with ssn and email classified. Skips on a
    // server without ADD SENSITIVITY CLASSIFICATION (before SQL Server 2019).
    void CreateClassifiedTable() {
        SQLINTEGER major = 0;
        SQLLEN indicator = 0;
        ExecDirect("SELECT CAST(SERVERPROPERTY('ProductMajorVersion') AS INT)");
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &major, 0, &indicator),
                      SQL_HANDLE_STMT, stmt_);
        SQLFreeStmt(stmt_, SQL_CLOSE);
        if (major < 15) GTEST_SKIP() << "ADD SENSITIVITY CLASSIFICATION needs SQL Server 2019";

        std::mt19937_64 rng(std::random_device{}());
        char suffix[17] = {};
        std::snprintf(suffix, sizeof(suffix), "%016llx",
                      static_cast<unsigned long long>(rng()));
        table_ = std::string("odbc_dc_") + suffix;
        auto classify = [&](const char* column, const Classification& c) {
            return "ADD SENSITIVITY CLASSIFICATION TO dbo." + table_ + "." + column +
                   " WITH (LABEL = '" + c.label + "', LABEL_ID = '" + c.label_id +
                   "', INFORMATION_TYPE = '" + c.information_type +
                   "', INFORMATION_TYPE_ID = '" + c.information_type_id + "', RANK = " +
                   c.rank_name + ");";
        };
        ExecDirect("CREATE TABLE dbo." + table_ +
                   " (id INT NOT NULL, ssn NVARCHAR(11) NOT NULL, email NVARCHAR(50) NOT NULL,"
                   " note NVARCHAR(20) NOT NULL);"
                   " INSERT INTO dbo." + table_ +
                   " VALUES (1, N'123-45-6789', N'ann@contoso.com', N'first');" +
                   classify("ssn", kSsn) + classify("email", kEmail));
        // Read every statement's result, so closing cannot cancel a pending
        // classification or hide its error.
        SQLRETURN rc;
        while (SQL_SUCCEEDED(rc = SQLMoreResults(stmt_))) {
        }
        ASSERT_EQ(SQL_NO_DATA, rc) << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        SQLFreeStmt(stmt_, SQL_CLOSE);
    }

    std::string ClassifiedSelect() const {
        return "SELECT id, ssn, email, note FROM dbo." + table_;
    }

    SQLHDESC Ird() {
        SQLHDESC ird = SQL_NULL_HDESC;
        EXPECT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_IMP_ROW_DESC, &ird, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        return ird;
    }

    SQLINTEGER IrdVersion() {
        SQLHDESC ird = Ird();
        SQLINTEGER version = -1;
        SQLINTEGER length = -1;
        EXPECT_SQL_OK(SQLGetDescFieldW(ird, 0, kCaSsDataClassificationVersion, &version, 0,
                                       &length),
                      SQL_HANDLE_DESC, ird);
        EXPECT_EQ(static_cast<SQLINTEGER>(sizeof(version)), length);
        return version;
    }

    // The payload, or nullopt after asserting HY091 when there is none.
    std::optional<std::vector<unsigned char>> IrdPayload() {
        SQLHDESC ird = Ird();
        std::vector<unsigned char> buffer(8192);
        SQLINTEGER length = -1;
        SQLRETURN rc = SQLGetDescFieldW(ird, 0, kCaSsDataClassification, buffer.data(),
                                        static_cast<SQLINTEGER>(buffer.size()), &length);
        if (rc == SQL_ERROR) {
            EXPECT_SQLSTATE(SQL_HANDLE_DESC, ird, "HY091");
            return std::nullopt;
        }
        EXPECT_EQ(SQL_SUCCESS, rc);
        EXPECT_GE(length, 0);
        buffer.resize(static_cast<size_t>(length));
        return buffer;
    }

    void ExpectClassifiedPayload(bool v2) {
        auto bytes = IrdPayload();
        ASSERT_TRUE(bytes.has_value());
        auto payload = Parse(*bytes, v2);
        ASSERT_TRUE(payload.has_value()) << "payload does not follow the v" << (v2 ? 2 : 1)
                                         << " layout (" << bytes->size() << " bytes)";
        EXPECT_EQ(v2, payload->rank.has_value());
        ASSERT_EQ(4u, payload->columns.size());
        EXPECT_TRUE(payload->columns[0].empty());
        ExpectClassified(payload->columns[1], kSsn, v2);
        ExpectClassified(payload->columns[2], kEmail, v2);
        EXPECT_TRUE(payload->columns[3].empty());
    }

    std::string table_;
};

// Every attribute call below runs while connected: before connecting, the
// Driver Manager caches SQLSetConnectAttr and the Windows one answers
// SQLGetConnectAttr itself (08003, or the cached value). The IRD is read only
// while the statement has a result or is prepared, since unixODBC rejects an
// IRD read in state S1 with HY007.

TEST_F(DataClassificationTest, DefaultConnectionNegotiatesVersion1) {
    ConnectDbc();
    EXPECT_EQ(1u, RequestedVersion());
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    ExecDirect(ClassifiedSelect());
    EXPECT_EQ(1, IrdVersion());
    ExpectClassifiedPayload(false);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR ssn[16] = {};
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_WCHAR, ssn, sizeof(ssn), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(static_cast<SQLLEN>(11 * sizeof(SQLWCHAR)), indicator);
}

TEST_F(DataClassificationTest, Version2AddsRanks) {
    ASSERT_SQL_OK(SetRequestedVersion(2), SQL_HANDLE_DBC, dbc_);
    ConnectDbc();
    EXPECT_EQ(2u, RequestedVersion());
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    ExecDirect(ClassifiedSelect());
    EXPECT_EQ(2, IrdVersion());
    ExpectClassifiedPayload(true);
}

// The Driver Manager replays a cached attribute at connect and the driver
// rejects it, so the connection keeps the default. The Windows Driver Manager
// reports the rejection as IM006; unixODBC ignores it (SQLConnect.c).
TEST_F(DataClassificationTest, InvalidPreConnectVersionKeepsTheDefault) {
    ASSERT_SQL_OK(SetRequestedVersion(0), SQL_HANDLE_DBC, dbc_);
    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLRETURN rc = SQLDriverConnect(dbc_, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                    static_cast<SQLSMALLINT>(connstr.size()), nullptr, 0,
                                    nullptr, SQL_DRIVER_NOPROMPT);
    ASSERT_TRUE(SQL_SUCCEEDED(rc)) << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_DBC, dbc_);
#ifdef _WIN32
    EXPECT_TRUE(ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, dbc_, "IM006"));
#endif
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(1u, RequestedVersion());
    ExecDirect("SELECT 1 AS x");
    EXPECT_EQ(1, IrdVersion());
}

TEST_F(DataClassificationTest, OutOfRangeVersionsAreRejected) {
    ConnectDbc();
    for (SQLULEN invalid : {SQLULEN{0}, SQLULEN{3}}) {
        EXPECT_EQ(SQL_ERROR, SetRequestedVersion(invalid)) << invalid;
        EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
        EXPECT_EQ(1u, RequestedVersion());
    }
}

TEST_F(DataClassificationTest, VersionSetWhileConnectedAppliesAtTheNextLogin) {
    ConnectDbc();
    EXPECT_EQ(SQL_SUCCESS, SetRequestedVersion(2));
    EXPECT_EQ(2u, RequestedVersion());
    ExecDirect("SELECT 1 AS x");
    EXPECT_EQ(1, IrdVersion());
    Reconnect();
    ExecDirect("SELECT 1 AS x");
#ifdef _WIN32
    EXPECT_EQ(2, IrdVersion());
#else
    // unixODBC frees the driver's connection on SQLDisconnect (SQLConnect.c,
    // __disconnect_part_three) and replays only
    // attributes set before connecting, so the value set while connected is
    // gone; the login still requests what the attribute now reports.
    EXPECT_EQ(static_cast<SQLINTEGER>(RequestedVersion()), IrdVersion());
#endif
}

// A prepared statement keeps its classification across a cursor close; the
// end of the batch and a failed execution drop it (measured on msodbcsql
// 18.07.0001). A prepared statement's IRD stays readable under unixODBC.
TEST_F(DataClassificationTest, PreparedStatementClassificationLifecycle) {
    ConnectDbc();
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    auto sql = ODBCTestUtils::ToSqlTStr(ClassifiedSelect());
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_FALSE(IrdPayload().has_value());
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ExpectClassifiedPayload(false);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    ExpectClassifiedPayload(false);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    while (SQLFetch(stmt_) == SQL_SUCCESS) {
    }
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_FALSE(IrdPayload().has_value());

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ExpectClassifiedPayload(false);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLHSTMT other = AllocStmt();
    auto drop = ODBCTestUtils::ToSqlTStr("DROP TABLE dbo." + table_);
    ASSERT_SQL_OK(SQLExecDirect(other, const_cast<SQLTCHAR*>(drop.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, other);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_FALSE(IrdPayload().has_value());
}

TEST_F(DataClassificationTest, ParameterArrayReportsEachSetsClassification) {
    ConnectDbc();
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    auto sql = ODBCTestUtils::ToSqlTStr(ClassifiedSelect() + " WHERE id = ?");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    SQLINTEGER ids[2] = {1, 2};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   ids, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAMSET_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ExpectClassifiedPayload(false);
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ExpectClassifiedPayload(false);
}

TEST_F(DataClassificationTest, UnclassifiedResultHasNoPayload) {
    ConnectDbc();
    ExecDirect("SELECT 1 AS x");
    EXPECT_FALSE(IrdPayload().has_value());
}

TEST_F(DataClassificationTest, EachResultSetReportsOnlyItsOwnClassification) {
    ConnectDbc();
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    ExecDirect(ClassifiedSelect() + "; SELECT 1 AS x; " + ClassifiedSelect());
    ExpectClassifiedPayload(false);
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_FALSE(IrdPayload().has_value());
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ExpectClassifiedPayload(false);
}

// Header fields of the IRD only: SQLColAttribute and other descriptors do not
// answer them, the IRD ignores the record number, and the IRD is read-only.
TEST_F(DataClassificationTest, FieldsAreReadOnlyIrdHeaderFields) {
    ConnectDbc();
    ExecDirect("SELECT 1 AS x");

    for (SQLUSMALLINT field : {SQLUSMALLINT{1237}, SQLUSMALLINT{1238}}) {
        SQLLEN numeric = 0;
        EXPECT_EQ(SQL_ERROR, SQLColAttributeW(stmt_, 1, field, nullptr, 0, nullptr, &numeric))
            << field;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY091");
    }

    SQLHDESC ard = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, &ard, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetDescFieldW(ard, 0, kCaSsDataClassificationVersion, &value,
                                          sizeof(value), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, ard, "HY091");

    SQLHDESC ird = Ird();
    value = -1;
    EXPECT_SQL_OK(SQLGetDescFieldW(ird, 1, kCaSsDataClassificationVersion, &value,
                                   sizeof(value), nullptr),
                  SQL_HANDLE_DESC, ird);
    EXPECT_EQ(1, value);
    EXPECT_EQ(SQL_ERROR, SQLSetDescFieldW(ird, 0, kCaSsDataClassification, &value,
                                          sizeof(value)));
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, ird, "HY016");
}

TEST_F(DataClassificationTest, ShortBufferReturnsThePrefixAndFullLength) {
    ConnectDbc();
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    ExecDirect(ClassifiedSelect());
    auto full = IrdPayload();
    ASSERT_TRUE(full.has_value());

    SQLHDESC ird = Ird();
    unsigned char prefix[4] = {};
    SQLINTEGER length = -1;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetDescFieldW(ird, 0, kCaSsDataClassification, prefix,
                                                      sizeof(prefix), &length));
    EXPECT_EQ(static_cast<SQLINTEGER>(full->size()), length);
    EXPECT_EQ(0, std::memcmp(prefix, full->data(), sizeof(prefix)));

    length = -1;
    EXPECT_EQ(SQL_SUCCESS,
              SQLGetDescFieldW(ird, 0, kCaSsDataClassification, nullptr, 0, &length));
    EXPECT_EQ(static_cast<SQLINTEGER>(full->size()), length);
}

// Pins parity-deviations entry 40: msodbcsql posts IDS_S1_004 (HY004, "Invalid
// SQL data type") for a truncated payload (sqlcdesc.cpp:5240-5256); this driver
// reports the ODBC truncation state.
TEST_F(DataClassificationTest, ShortBufferSqlState) {
    ConnectDbc();
    ASSERT_NO_FATAL_FAILURE(CreateClassifiedTable());
    if (IsSkipped()) return;

    ExecDirect(ClassifiedSelect());
    SQLHDESC ird = Ird();
    unsigned char prefix[4] = {};
    SQLINTEGER length = -1;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetDescFieldW(ird, 0, kCaSsDataClassification, prefix,
                                                      sizeof(prefix), &length));
    const char* target = std::getenv("ODBC_TEST_TARGET");
    bool reference = target != nullptr && std::string(target) == "msodbcsql";
    EXPECT_SQLSTATE(SQL_HANDLE_DESC, ird, reference ? "HY004" : "01004");
}
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "perf_fixture.h"

#include <cstdlib>
#include <sstream>

namespace perf {

SqlTString ToSqlTStr(const std::string& s) {
    return SqlTString(s.begin(), s.end());
}

std::string ToNarrow(const SqlTString& s) {
    return std::string(s.begin(), s.end());
}

std::string DiagText(SQLSMALLINT handle_type, SQLHANDLE handle) {
    SQLTCHAR state[8] = {};
    SQLINTEGER native = 0;
    SQLTCHAR msg[1024] = {};
    SQLSMALLINT msg_len = 0;
    std::ostringstream oss;
    bool found = false;

    for (SQLSMALLINT rec = 1;; rec++) {
        SQLRETURN rc = SQLGetDiagRec(
            handle_type, handle, rec, state, &native, msg,
            static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)), &msg_len);
        if (!SQL_SUCCEEDED(rc)) {
            break;
        }
        if (found) {
            oss << " | ";
        }
        oss << "[" << ToNarrow(SqlTString(state)) << "] " << ToNarrow(SqlTString(msg));
        found = true;
    }
    return found ? oss.str() : "(no diagnostic)";
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const Config& Config::Instance() {
    static Config cfg;
    return cfg;
}

Config::Config()
    : dsn_(GetEnv("ODBC_TEST_DSN")),
      server_(GetEnv("ODBC_TEST_SERVER")),
      database_(GetEnv("ODBC_TEST_DATABASE", "tempdb")),
      uid_(GetEnv("ODBC_TEST_UID")),
      pwd_(GetEnv("ODBC_TEST_PWD")),
      driver_(GetEnv("ODBC_TEST_DRIVER", "ODBC Driver 18 for SQL Server")),
      connstr_(GetEnv("ODBC_TEST_CONNSTR")),
      trust_cert_(GetEnv("ODBC_TEST_TRUST_CERT", "Yes")),
      encrypt_(GetEnv("ODBC_TEST_ENCRYPT")) {}

std::string Config::GetEnv(const char* name, const char* fallback) {
#ifdef _WIN32
    char* buf = nullptr;
    size_t len = 0;
    if (_dupenv_s(&buf, &len, name) == 0 && buf != nullptr) {
        std::string val(buf);
        free(buf);
        if (!val.empty()) {
            return val;
        }
    }
    return fallback ? fallback : "";
#else
    const char* val = std::getenv(name);
    return (val && val[0]) ? std::string(val)
                           : (fallback ? std::string(fallback) : std::string());
#endif
}

SqlTString Config::ConnectionString() const {
    if (!connstr_.empty()) {
        return ToSqlTStr(connstr_);
    }

    std::ostringstream cs;
    if (!dsn_.empty()) {
        cs << "DSN=" << dsn_ << ";";
    } else {
        cs << "Driver={" << driver_ << "};Server=" << server_ << ";";
    }
    cs << "Database=" << database_ << ";TrustServerCertificate=" << trust_cert_ << ";";
    if (!encrypt_.empty()) {
        cs << "Encrypt=" << encrypt_ << ";";
    }
    if (!uid_.empty()) {
        cs << "Uid=" << uid_ << ";Pwd=" << pwd_ << ";";
    } else {
        cs << "Trusted_Connection=Yes;";
    }
    return ToSqlTStr(cs.str());
}

// ---------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------

Env::Env() {
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env_))) {
        env_ = SQL_NULL_HENV;
        return;
    }
    SQLRETURN rc = SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                 reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
    if (!SQL_SUCCEEDED(rc)) {
        SQLFreeHandle(SQL_HANDLE_ENV, env_);
        env_ = SQL_NULL_HENV;
    }
}

Env::~Env() {
    if (env_ != SQL_NULL_HENV) {
        SQLFreeHandle(SQL_HANDLE_ENV, env_);
    }
}

// ---------------------------------------------------------------------------
// Conn
// ---------------------------------------------------------------------------

Conn::Conn(const Env& env) {
    if (!env.ok()) {
        error_ = "environment handle allocation failed";
        return;
    }
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_DBC, env.get(), &dbc_))) {
        error_ = "SQLAllocHandle(DBC) failed: " + DiagText(SQL_HANDLE_ENV, env.get());
        dbc_ = SQL_NULL_HDBC;
        return;
    }

    SqlTString conn_str = Config::Instance().ConnectionString();
    SQLRETURN rc = SQLDriverConnect(dbc_, nullptr, conn_str.data(),
                                    static_cast<SQLSMALLINT>(conn_str.size()), nullptr, 0,
                                    nullptr, SQL_DRIVER_NOPROMPT);
    if (!SQL_SUCCEEDED(rc)) {
        error_ = "SQLDriverConnect failed: " + DiagText(SQL_HANDLE_DBC, dbc_);
        return;
    }
    connected_ = true;

    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_))) {
        error_ = "SQLAllocHandle(STMT) failed: " + DiagText(SQL_HANDLE_DBC, dbc_);
        stmt_ = SQL_NULL_HSTMT;
    }
}

Conn::~Conn() {
    if (stmt_ != SQL_NULL_HSTMT) {
        SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
    }
    if (connected_) {
        SQLDisconnect(dbc_);
    }
    if (dbc_ != SQL_NULL_HDBC) {
        SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
    }
}

bool Conn::Exec(const std::string& sql) {
    if (stmt_ == SQL_NULL_HSTMT) {
        error_ = "no statement handle";
        return false;
    }
    SqlTString text = ToSqlTStr(sql);
    SQLRETURN rc = SQLExecDirect(stmt_, text.data(), SQL_NTS);
    if (!SQL_SUCCEEDED(rc)) {
        error_ = "SQLExecDirect(" + sql + ") failed: " + DiagText(SQL_HANDLE_STMT, stmt_);
        CloseCursor(stmt_);
        return false;
    }
    do {
        std::string drain_err;
        if (DrainRows(stmt_, &drain_err) < 0) {
            error_ = drain_err;
            CloseCursor(stmt_);
            return false;
        }
    } while (SQLMoreResults(stmt_) == SQL_SUCCESS);
    CloseCursor(stmt_);
    return true;
}

// ---------------------------------------------------------------------------
// Row draining
// ---------------------------------------------------------------------------

int64_t DrainRows(SQLHSTMT stmt, std::string* error) {
    // Upper bound on SQLGetData calls for a single column, so a driver that never
    // terminates a truncation sequence fails the run instead of hanging it.
    constexpr int kMaxChunksPerColumn = 100000;

    SQLSMALLINT col_count = 0;
    if (!SQL_SUCCEEDED(SQLNumResultCols(stmt, &col_count))) {
        if (error) {
            *error = "SQLNumResultCols failed: " + DiagText(SQL_HANDLE_STMT, stmt);
        }
        return -1;
    }
    if (col_count == 0) {
        return 0;
    }

    // Sized to hold a full inline (non-LOB) column in one SQLGetData call; larger
    // values loop below, which is exactly what a real application does.
    char buf[8192];
    int64_t rows = 0;

    for (;;) {
        SQLRETURN rc = SQLFetch(stmt);
        if (rc == SQL_NO_DATA) {
            break;
        }
        if (!SQL_SUCCEEDED(rc)) {
            if (error) {
                *error = "SQLFetch failed: " + DiagText(SQL_HANDLE_STMT, stmt);
            }
            return -1;
        }
        for (SQLSMALLINT col = 1; col <= col_count; col++) {
            SQLLEN ind = 0;
            SQLRETURN grc = SQL_SUCCESS;
            // Standard truncation idiom: a long value comes back as repeated
            // SQL_SUCCESS_WITH_INFO chunks terminated by SQL_SUCCESS. The cap
            // guards against a driver that never terminates the sequence.
            for (int chunk = 0; chunk < kMaxChunksPerColumn; chunk++) {
                grc = SQLGetData(stmt, static_cast<SQLUSMALLINT>(col), SQL_C_CHAR, buf,
                                 static_cast<SQLLEN>(sizeof(buf)), &ind);
                if (grc == SQL_NO_DATA) {
                    break;
                }
                if (!SQL_SUCCEEDED(grc)) {
                    if (error) {
                        *error = "SQLGetData failed: " + DiagText(SQL_HANDLE_STMT, stmt);
                    }
                    return -1;
                }
                benchmark::DoNotOptimize(buf);
                if (grc != SQL_SUCCESS_WITH_INFO) {
                    break;
                }
            }
        }
        rows++;
    }
    return rows;
}

void CloseCursor(SQLHSTMT stmt) {
    // SQLCloseCursor returns 24000 when no cursor is open (e.g. after a DDL
    // statement); that is expected, so the return code is deliberately ignored.
    SQLCloseCursor(stmt);
}

}  // namespace perf

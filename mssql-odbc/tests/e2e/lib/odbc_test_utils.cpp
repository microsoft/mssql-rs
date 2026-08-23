// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_test_utils.cpp  –  Diagnostic and connection-string helpers.

#include "odbc_test_fixture.h"
#include <sstream>

// ---------------------------------------------------------------------------
// ODBCTestUtils
// ---------------------------------------------------------------------------

std::string ODBCTestUtils::GetDiagState(SQLSMALLINT handleType,
                                        SQLHANDLE handle) {
    SQLTCHAR state[8] = {};
    SQLINTEGER nativeErr = 0;
    SQLTCHAR msg[512] = {};
    SQLSMALLINT msgLen = 0;

    SQLRETURN rc = SQLGetDiagRec(handleType, handle, 1, state, &nativeErr,
                                 msg,
                                 static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)),
                                 &msgLen);
    if (SQL_SUCCEEDED(rc)) {
        return ToNarrow(SqlTString(state));
    }
    return "";
}

bool ODBCTestUtils::HasDiagState(SQLSMALLINT handleType, SQLHANDLE handle,
                                 const std::string& target) {
    SQLTCHAR state[8] = {};
    SQLINTEGER nativeErr = 0;
    SQLTCHAR msg[512] = {};
    SQLSMALLINT msgLen = 0;

    for (SQLSMALLINT recNum = 1; ; recNum++) {
        SQLRETURN rc = SQLGetDiagRec(handleType, handle, recNum, state, &nativeErr,
                                     msg,
                                     static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)),
                                     &msgLen);
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        if (ToNarrow(SqlTString(state)) == target) {
            return true;
        }
    }
    return false;
}

std::string ODBCTestUtils::GetDiagMessage(SQLSMALLINT handleType,
                                          SQLHANDLE handle) {
    SQLTCHAR state[8] = {};
    SQLINTEGER nativeErr = 0;
    SQLTCHAR msg[1024] = {};
    SQLSMALLINT msgLen = 0;
    std::ostringstream oss;
    bool found = false;

    for (SQLSMALLINT recNum = 1; ; recNum++) {
        SQLRETURN rc = SQLGetDiagRec(handleType, handle, recNum, state, &nativeErr,
                                     msg,
                                     static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)),
                                     &msgLen);
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        if (found) {
            oss << " | ";
        }
        oss << "[" << ToNarrow(SqlTString(state)) << "] "
            << ToNarrow(SqlTString(msg))
            << " (native=" << nativeErr << ")";
        found = true;
    }
    return found ? oss.str() : "(no diagnostic)";
}

SqlTString ODBCTestUtils::BuildConnectionString() {
    auto& cfg = ODBCTestConfig::Instance();

    // If a full connection string override is provided, use it directly.
    if (cfg.HasConnStr()) {
        return ToSqlTStr(cfg.ConnStr());
    }

    std::ostringstream cs;

    // DSN-based connection  (like LTM tests)
    if (cfg.HasDSN()) {
        cs << "DSN=" << cfg.DSN() << ";";
    } else {
        // DSN-less: specify driver + server
        cs << "Driver={" << cfg.Driver() << "};";
        cs << "Server=" << cfg.Server() << ";";
    }

    cs << "Database=" << cfg.Database() << ";";
    cs << "TrustServerCertificate=" << cfg.TrustCert() << ";";

    if (!cfg.Encrypt().empty()) {
        cs << "Encrypt=" << cfg.Encrypt() << ";";
    }

    if (cfg.HasCredentials()) {
        cs << "Uid=" << cfg.Uid() << ";";
        cs << "Pwd=" << cfg.Pwd() << ";";
    } else {
        // Windows integrated auth
        cs << "Trusted_Connection=Yes;";
    }

    return ToSqlTStr(cs.str());
}

SqlTString ODBCTestUtils::ToSqlTStr(const std::string& s) {
    return SqlTString(s.begin(), s.end());
}

std::string ODBCTestUtils::ToNarrow(const SqlTString& s) {
    return std::string(s.begin(), s.end());
}

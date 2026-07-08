// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_test_config.cpp  –  Read test connection info from environment.

#include "odbc_test_fixture.h"
#include <cstdlib>

// ---------------------------------------------------------------------------
// ODBCTestConfig
// ---------------------------------------------------------------------------
ODBCTestConfig& ODBCTestConfig::Instance() {
    static ODBCTestConfig cfg;
    return cfg;
}

ODBCTestConfig::ODBCTestConfig()
    : dsn_       (GetEnv("ODBC_TEST_DSN"))
    , server_    (GetEnv("ODBC_TEST_SERVER"))
    , database_  (GetEnv("ODBC_TEST_DATABASE",   "tempdb"))
    , uid_       (GetEnv("ODBC_TEST_UID"))
    , pwd_       (GetEnv("ODBC_TEST_PWD"))
    , driver_    (GetEnv("ODBC_TEST_DRIVER",      "ODBC Driver 18 for SQL Server"))
    , connstr_   (GetEnv("ODBC_TEST_CONNSTR"))
    , trust_cert_(GetEnv("ODBC_TEST_TRUST_CERT",  "Yes"))
    , encrypt_   (GetEnv("ODBC_TEST_ENCRYPT"))
{}

std::string ODBCTestConfig::GetEnv(const char* name, const char* fallback) {
#ifdef _WIN32
    // Use _dupenv_s to avoid MSVC deprecation warning for getenv.
    char* buf = nullptr;
    size_t len = 0;
    if (_dupenv_s(&buf, &len, name) == 0 && buf != nullptr) {
        std::string val(buf);
        free(buf);
        return val;
    }
    return fallback ? fallback : "";
#else
    const char* val = std::getenv(name);
    return (val && val[0]) ? std::string(val)
                           : (fallback ? std::string(fallback) : std::string());
#endif
}

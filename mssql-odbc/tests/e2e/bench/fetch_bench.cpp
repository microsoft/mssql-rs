// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// fetch_bench.cpp  –  Fetch/row-throughput A/B benchmark for ODBC drivers.
//
// Times the SQLFetch drain over a large, wide result set for one or more ODBC
// drivers selected by name, then prints raw per-rep numbers and an A/B ratio.
// This benchmarks the FETCH / decode path ONLY (execute+fetch drain) — it is
// deliberately NOT an ExecDirect micro-benchmark (that path is I/O bound, see
// PR #186). The query returns many rows so row decode dominates the timing.
//
// Reuses the e2e harness plumbing (ODBCTestConfig for server/uid/pwd/database,
// ODBCTestUtils for diagnostics) but builds its own per-driver connection
// string so the SAME binary can drive both the native "ODBC Driver 18 for SQL
// Server" and the Rust mssql-odbc dev driver back-to-back in one invocation.
//
// Usage:
//   fetch_bench [--rows N] [--reps R] [--warmup W]
//               [--driver LABEL=DRIVER_NAME | LABEL=dll:PATH] ...
//
//   --rows   N   rows the query returns              (default 200000)
//   --reps   R   timed reps per driver, incl warmup  (default 9)
//   --warmup W   leading reps discarded per driver   (default 1)
//   --driver LABEL=TARGET   register a driver leg; may be repeated. TARGET is
//            either a DM-registered driver name (routed via odbc32.dll) or
//            `dll:<path>` to load an unregistered driver DLL directly (no admin,
//            no registry). If omitted, defaults to two legs:
//              native = env ODBC_BENCH_NATIVE_DLL (direct) or
//                       ODBC_BENCH_NATIVE_DRIVER (default "ODBC Driver 18 for SQL Server")
//              rust   = env ODBC_BENCH_RUST_DLL (direct) or
//                       ODBC_BENCH_RUST_DRIVER (default "mssql-odbc")

#include "odbc_test_fixture.h"

#include <algorithm>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iomanip>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif

namespace {

// ---------------------------------------------------------------------------
// High-resolution wall-clock timer.
// Windows: QueryPerformanceCounter (per the benchmark spec).
// POSIX:   steady_clock, so the harness still builds/runs cross-platform.
// ---------------------------------------------------------------------------
class Stopwatch {
public:
    void Start() { start_ = Now(); }
    double ElapsedMs() const { return (Now() - start_) * 1000.0; }

private:
#ifdef _WIN32
    static double Now() {
        LARGE_INTEGER c, f;
        QueryPerformanceCounter(&c);
        QueryPerformanceFrequency(&f);
        return static_cast<double>(c.QuadPart) / static_cast<double>(f.QuadPart);
    }
#else
    static double Now() {
        return std::chrono::duration_cast<std::chrono::duration<double>>(
                   std::chrono::steady_clock::now().time_since_epoch())
            .count();
    }
#endif
    double start_ = 0.0;
};

struct DriverLeg {
    std::string label;
    std::string target;  // registered driver name (DM leg) or DLL path (direct leg)
    bool        direct;  // true: LoadLibrary(target); false: route via odbc32 DM
};

struct RepResult {
    double ms = 0.0;
    long long rows = 0;
};

// ---------------------------------------------------------------------------
// Direct-load ODBC entry points (bypass the Driver Manager).
//
// The Windows DM only loads drivers registered in HKLM\...\ODBCINST.INI, which
// requires admin. To exercise an *unregistered* dev driver — and to keep both
// A/B legs on an identical, DM-free code path — every leg resolves the handful
// of ODBC entry points it needs from an explicit module:
//   * a `dll:<path>` leg loads the driver DLL itself and calls it directly;
//   * a registered-name leg loads odbc32.dll (the DM), which then resolves the
//     driver from the registry as usual.
// Wide (…W) entry points are used throughout to match the UNICODE harness build.
// ---------------------------------------------------------------------------
using Fn_AllocHandle = SQLRETURN(SQL_API*)(SQLSMALLINT, SQLHANDLE, SQLHANDLE*);
using Fn_SetEnvAttr = SQLRETURN(SQL_API*)(SQLHENV, SQLINTEGER, SQLPOINTER, SQLINTEGER);
using Fn_DriverConnect = SQLRETURN(SQL_API*)(SQLHDBC, SQLHWND, SQLWCHAR*, SQLSMALLINT,
                                             SQLWCHAR*, SQLSMALLINT, SQLSMALLINT*,
                                             SQLUSMALLINT);
using Fn_ExecDirect = SQLRETURN(SQL_API*)(SQLHSTMT, SQLWCHAR*, SQLINTEGER);
using Fn_GetData = SQLRETURN(SQL_API*)(SQLHSTMT, SQLUSMALLINT, SQLSMALLINT, SQLPOINTER,
                                       SQLLEN, SQLLEN*);
using Fn_Fetch = SQLRETURN(SQL_API*)(SQLHSTMT);
using Fn_FreeStmt = SQLRETURN(SQL_API*)(SQLHSTMT, SQLUSMALLINT);
using Fn_Disconnect = SQLRETURN(SQL_API*)(SQLHDBC);
using Fn_FreeHandle = SQLRETURN(SQL_API*)(SQLSMALLINT, SQLHANDLE);
using Fn_GetDiagRec = SQLRETURN(SQL_API*)(SQLSMALLINT, SQLHANDLE, SQLSMALLINT, SQLWCHAR*,
                                          SQLINTEGER*, SQLWCHAR*, SQLSMALLINT,
                                          SQLSMALLINT*);

struct OdbcApi {
#ifdef _WIN32
    HMODULE mod = nullptr;
#else
    void* mod = nullptr;
#endif
    Fn_AllocHandle AllocHandle = nullptr;
    Fn_SetEnvAttr SetEnvAttr = nullptr;
    Fn_DriverConnect DriverConnect = nullptr;
    Fn_ExecDirect ExecDirect = nullptr;
    Fn_GetData GetData = nullptr;
    Fn_Fetch Fetch = nullptr;
    Fn_FreeStmt FreeStmt = nullptr;
    Fn_Disconnect Disconnect = nullptr;
    Fn_FreeHandle FreeHandle = nullptr;
    Fn_GetDiagRec GetDiagRec = nullptr;

    bool Complete() const {
        return AllocHandle && SetEnvAttr && DriverConnect && ExecDirect && GetData &&
               Fetch && FreeStmt && Disconnect && FreeHandle && GetDiagRec;
    }
};

void* ResolveSym(const OdbcApi& a, const char* name) {
#ifdef _WIN32
    return reinterpret_cast<void*>(GetProcAddress(a.mod, name));
#else
    return dlsym(a.mod, name);
#endif
}

// Load |modulePath| and bind the ODBC entry points. Returns false (with a
// message on stderr) if the module or any required symbol is missing.
bool LoadOdbcApi(OdbcApi& a, const std::string& modulePath) {
#ifdef _WIN32
    SqlTString w = ODBCTestUtils::ToSqlTStr(modulePath);
    a.mod = LoadLibraryW(reinterpret_cast<const wchar_t*>(w.c_str()));
#else
    a.mod = dlopen(modulePath.c_str(), RTLD_NOW | RTLD_LOCAL);
#endif
    if (!a.mod) {
        std::cerr << "ERROR: cannot load module '" << modulePath << "'\n";
        return false;
    }
    a.AllocHandle = reinterpret_cast<Fn_AllocHandle>(ResolveSym(a, "SQLAllocHandle"));
    a.SetEnvAttr = reinterpret_cast<Fn_SetEnvAttr>(ResolveSym(a, "SQLSetEnvAttr"));
    a.DriverConnect =
        reinterpret_cast<Fn_DriverConnect>(ResolveSym(a, "SQLDriverConnectW"));
    a.ExecDirect = reinterpret_cast<Fn_ExecDirect>(ResolveSym(a, "SQLExecDirectW"));
    a.GetData = reinterpret_cast<Fn_GetData>(ResolveSym(a, "SQLGetData"));
    a.Fetch = reinterpret_cast<Fn_Fetch>(ResolveSym(a, "SQLFetch"));
    a.FreeStmt = reinterpret_cast<Fn_FreeStmt>(ResolveSym(a, "SQLFreeStmt"));
    a.Disconnect = reinterpret_cast<Fn_Disconnect>(ResolveSym(a, "SQLDisconnect"));
    a.FreeHandle = reinterpret_cast<Fn_FreeHandle>(ResolveSym(a, "SQLFreeHandle"));
    a.GetDiagRec = reinterpret_cast<Fn_GetDiagRec>(ResolveSym(a, "SQLGetDiagRecW"));
    if (!a.Complete()) {
        std::cerr << "ERROR: module '" << modulePath
                  << "' is missing required ODBC entry points\n";
        return false;
    }
    return true;
}

// Walk the diagnostic records for |handle| and join them into one string.
std::string DiagMessage(const OdbcApi& a, SQLSMALLINT ht, SQLHANDLE h) {
    std::string out;
    for (SQLSMALLINT rec = 1; rec <= 8; ++rec) {
        SQLWCHAR state[6] = {};
        SQLWCHAR msg[1024] = {};
        SQLINTEGER native = 0;
        SQLSMALLINT len = 0;
        SQLRETURN rc = a.GetDiagRec(ht, h, rec, state, &native, msg,
                                    static_cast<SQLSMALLINT>(1024), &len);
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) break;
        SqlTString ss(reinterpret_cast<SQLTCHAR*>(state));
        SqlTString ms(reinterpret_cast<SQLTCHAR*>(msg));
        if (!out.empty()) out += " | ";
        out += "[" + ODBCTestUtils::ToNarrow(ss) + "] " + ODBCTestUtils::ToNarrow(ms);
    }
    return out.empty() ? "(no diagnostic)" : out;
}

// Build a DSN-less connection string, pulling server/uid/pwd/etc from the shared
// e2e config so both legs hit the same live server. Direct-load legs omit the
// Driver= keyword (the driver DLL is already selected by module path).
SqlTString BuildConnStr(bool includeDriver, const std::string& driver) {
    auto& cfg = ODBCTestConfig::Instance();
    std::ostringstream cs;
    if (includeDriver) {
        cs << "Driver={" << driver << "};";
    }
    cs << "Server=" << cfg.Server() << ";";
    cs << "Database=" << cfg.Database() << ";";
    cs << "TrustServerCertificate=" << cfg.TrustCert() << ";";
    if (!cfg.Encrypt().empty()) {
        cs << "Encrypt=" << cfg.Encrypt() << ";";
    }
    if (cfg.HasCredentials()) {
        // Brace uid/pwd so values with ODBC connection-string metacharacters
        // (spaces, ';', '=') survive DM parsing.
        cs << "Uid={" << cfg.Uid() << "};";
        cs << "Pwd={" << cfg.Pwd() << "};";
    } else {
        cs << "Trusted_Connection=Yes;";
    }
    return ODBCTestUtils::ToSqlTStr(cs.str());
}

// A wide, representative row set: int / bigint / varchar / nvarchar / float.
// TOP (N) over a self cross-join of sys.all_objects generates N rows entirely
// server-side, so the client spends its time decoding rows (the path we time).
// Every column is retrieved as character data (SQLGetData -> SQL_C_CHAR/WCHAR),
// the one column-retrieval path both drivers share. datetime2 is intentionally
// avoided: the Rust dev driver's Phase-1 SQLGetData cannot yet convert it to
// text, so including it would make the A/B legs diverge.
std::string BuildQuery(long long rows) {
    std::ostringstream q;
    q << "SELECT TOP (" << rows << ") "
         "CAST(ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS int) AS n, "
         "CAST(CAST(ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS bigint) * 2654435761 AS bigint) AS big, "
         "CONVERT(varchar(50), 'row-' + CAST(ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS varchar(20))) AS vc, "
         "CONVERT(nvarchar(50), N'row-' + CAST(ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS nvarchar(20))) AS nvc, "
         "CAST(ROW_NUMBER() OVER (ORDER BY (SELECT 1)) * 1.5 AS float) AS f "
         "FROM sys.all_objects a CROSS JOIN sys.all_objects b";
    return q.str();
}

[[noreturn]] void Fail(const OdbcApi& a, const std::string& what, SQLSMALLINT ht,
                       SQLHANDLE h) {
    std::cerr << "ERROR: " << what << " -> " << DiagMessage(a, ht, h) << "\n";
    std::exit(2);
}

// Execute the query and drain every row/column, returning rows fetched.
// |checksum| is accumulated across reps so the compiler/driver can't elide the
// decode work. Timing is the caller's responsibility (wraps this call).
// Execute the query and drain every row/column via SQLFetch + SQLGetData,
// returning rows fetched. SQLGetData (rather than bound columns) is used so the
// same path exercises both drivers — the Rust dev driver retrieves columns via
// SQLGetData and does not export SQLBindCol. |checksum| is accumulated across
// reps so the compiler/driver can't elide the decode work. Timing is the
// caller's responsibility (wraps this call).
long long ExecuteAndDrain(const OdbcApi& a, SQLHSTMT stmt, const std::string& sql,
                          uint64_t& checksum) {
    SqlTString tsql = ODBCTestUtils::ToSqlTStr(sql);
    if (!SQL_SUCCEEDED(a.ExecDirect(
            stmt, reinterpret_cast<SQLWCHAR*>(const_cast<SQLTCHAR*>(tsql.c_str())),
            SQL_NTS)))
        Fail(a, "SQLExecDirect", SQL_HANDLE_STMT, stmt);

    long long rows = 0;
    uint64_t sum = checksum;
    for (;;) {
        SQLRETURN rc = a.Fetch(stmt);
        if (rc == SQL_NO_DATA) break;
        if (!SQL_SUCCEEDED(rc)) Fail(a, "SQLFetch", SQL_HANDLE_STMT, stmt);

        SQLCHAR  n[32] = {};
        SQLCHAR  big[32] = {};
        SQLCHAR  vc[128] = {};
        SQLWCHAR nvc[128] = {};
        SQLCHAR  f[64] = {};
        SQLLEN nInd = 0, bigInd = 0, vcInd = 0, nvcInd = 0, fInd = 0;

        if (!SQL_SUCCEEDED(a.GetData(stmt, 1, SQL_C_CHAR, n, sizeof(n), &nInd)))
            Fail(a, "SQLGetData(n)", SQL_HANDLE_STMT, stmt);
        if (!SQL_SUCCEEDED(a.GetData(stmt, 2, SQL_C_CHAR, big, sizeof(big), &bigInd)))
            Fail(a, "SQLGetData(big)", SQL_HANDLE_STMT, stmt);
        if (!SQL_SUCCEEDED(a.GetData(stmt, 3, SQL_C_CHAR, vc, sizeof(vc), &vcInd)))
            Fail(a, "SQLGetData(vc)", SQL_HANDLE_STMT, stmt);
        if (!SQL_SUCCEEDED(a.GetData(stmt, 4, SQL_C_WCHAR, nvc, sizeof(nvc), &nvcInd)))
            Fail(a, "SQLGetData(nvc)", SQL_HANDLE_STMT, stmt);
        if (!SQL_SUCCEEDED(a.GetData(stmt, 5, SQL_C_CHAR, f, sizeof(f), &fInd)))
            Fail(a, "SQLGetData(f)", SQL_HANDLE_STMT, stmt);

        ++rows;
        sum += (nInd == SQL_NULL_DATA) ? 0u : static_cast<uint64_t>(n[0]);
        sum += (bigInd == SQL_NULL_DATA) ? 0u : static_cast<uint64_t>(big[0]);
        sum += (vcInd == SQL_NULL_DATA) ? 0u : static_cast<uint64_t>(vc[0]);
        sum += (nvcInd == SQL_NULL_DATA) ? 0u : static_cast<uint64_t>(nvc[0]);
        sum += (fInd == SQL_NULL_DATA) ? 0u : static_cast<uint64_t>(f[0]);
    }
    checksum = sum;

    a.FreeStmt(stmt, SQL_CLOSE);
    return rows;
}

double Median(std::vector<double> v) {
    if (v.empty()) return 0.0;
    std::sort(v.begin(), v.end());
    size_t m = v.size() / 2;
    return (v.size() % 2) ? v[m] : (v[m - 1] + v[m]) / 2.0;
}

// Run warmup+timed reps for one driver leg; prints each retained rep.
std::vector<RepResult> RunLeg(const DriverLeg& leg, const std::string& sql,
                              int reps, int warmup) {
    std::string module = leg.direct ? leg.target : std::string("odbc32.dll");
    OdbcApi api;
    if (!LoadOdbcApi(api, module)) {
        std::cerr << "ERROR: leg '" << leg.label << "' could not load '" << module
                  << "'\n";
        std::exit(2);
    }

    SQLHENV env = SQL_NULL_HENV;
    SQLHDBC dbc = SQL_NULL_HDBC;

    if (!SQL_SUCCEEDED(api.AllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env)))
        Fail(api, "SQLAllocHandle(ENV)", SQL_HANDLE_ENV, env);
    api.SetEnvAttr(env, SQL_ATTR_ODBC_VERSION,
                   reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
    if (!SQL_SUCCEEDED(api.AllocHandle(SQL_HANDLE_DBC, env, &dbc)))
        Fail(api, "SQLAllocHandle(DBC)", SQL_HANDLE_ENV, env);

    SqlTString connstr = BuildConnStr(!leg.direct, leg.target);
    SQLWCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    SQLRETURN rc = api.DriverConnect(
        dbc, nullptr,
        reinterpret_cast<SQLWCHAR*>(const_cast<SQLTCHAR*>(connstr.c_str())),
        static_cast<SQLSMALLINT>(connstr.size()), outStr,
        static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLWCHAR)), &outLen,
        SQL_DRIVER_NOPROMPT);
    if (!SQL_SUCCEEDED(rc))
        Fail(api, "SQLDriverConnect [" + leg.label + " / " + leg.target + "]",
             SQL_HANDLE_DBC, dbc);

    SQLHSTMT stmt = SQL_NULL_HSTMT;
    if (!SQL_SUCCEEDED(api.AllocHandle(SQL_HANDLE_STMT, dbc, &stmt)))
        Fail(api, "SQLAllocHandle(STMT)", SQL_HANDLE_DBC, dbc);

    std::cout << "\n=== Leg: " << leg.label << "  ("
              << (leg.direct ? "direct-load " : "DM driver=") << "\"" << leg.target
              << "\") ===\n";

    std::vector<RepResult> kept;
    uint64_t checksum = 0;
    for (int i = 0; i < reps; ++i) {
        Stopwatch sw;
        sw.Start();
        long long rows = ExecuteAndDrain(api, stmt, sql, checksum);
        double ms = sw.ElapsedMs();
        bool isWarmup = i < warmup;
        double rps = (ms > 0.0) ? (rows / (ms / 1000.0)) : 0.0;
        std::cout << "  rep " << std::setw(2) << (i + 1)
                  << (isWarmup ? " [warmup]" : "        ") << "  "
                  << std::fixed << std::setprecision(2) << std::setw(10) << ms
                  << " ms  " << std::setw(12)
                  << static_cast<long long>(rps) << " rows/s  (" << rows
                  << " rows)\n";
        if (!isWarmup) kept.push_back({ms, rows});
    }
    std::cout << "  checksum=" << checksum << "\n";

    api.FreeHandle(SQL_HANDLE_STMT, stmt);
    api.Disconnect(dbc);
    api.FreeHandle(SQL_HANDLE_DBC, dbc);
    api.FreeHandle(SQL_HANDLE_ENV, env);
    return kept;
}

std::string GetEnvOr(const char* name, const char* fallback) {
#ifdef _WIN32
    char* buf = nullptr;
    size_t len = 0;
    if (_dupenv_s(&buf, &len, name) == 0 && buf) {
        std::string v(buf);
        free(buf);
        return v;
    }
    return fallback;
#else
    const char* v = std::getenv(name);
    return (v && v[0]) ? std::string(v) : std::string(fallback);
#endif
}

}  // namespace

int main(int argc, char** argv) {
    long long rows = 200000;
    int reps = 9;
    int warmup = 1;
    std::vector<DriverLeg> legs;

    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        auto next = [&](const char* what) -> std::string {
            if (i + 1 >= argc) {
                std::cerr << "ERROR: " << what << " needs a value\n";
                std::exit(1);
            }
            return argv[++i];
        };
        if (a == "--rows") {
            rows = std::stoll(next("--rows"));
        } else if (a == "--reps") {
            reps = std::stoi(next("--reps"));
        } else if (a == "--warmup") {
            warmup = std::stoi(next("--warmup"));
        } else if (a == "--driver") {
            std::string spec = next("--driver");
            auto eq = spec.find('=');
            std::string label = (eq == std::string::npos) ? spec : spec.substr(0, eq);
            std::string value = (eq == std::string::npos) ? spec : spec.substr(eq + 1);
            bool direct = false;
            if (value.rfind("dll:", 0) == 0) {
                direct = true;
                value = value.substr(4);
            }
            legs.push_back({label, value, direct});
        } else {
            std::cerr << "ERROR: unknown arg '" << a << "'\n";
            return 1;
        }
    }

    if (warmup >= reps) {
        std::cerr << "ERROR: --warmup (" << warmup << ") must be < --reps ("
                  << reps << ")\n";
        return 1;
    }

    if (legs.empty()) {
        // Prefer direct-load legs when DLL paths are supplied (no DM registration
        // needed); otherwise fall back to DM-registered driver names.
        std::string ndll = GetEnvOr("ODBC_BENCH_NATIVE_DLL", "");
        std::string rdll = GetEnvOr("ODBC_BENCH_RUST_DLL", "");
        if (!ndll.empty() && !rdll.empty()) {
            legs.push_back({"native", ndll, true});
            legs.push_back({"rust", rdll, true});
        } else {
            legs.push_back({"native",
                            GetEnvOr("ODBC_BENCH_NATIVE_DRIVER",
                                     "ODBC Driver 18 for SQL Server"),
                            false});
            legs.push_back(
                {"rust", GetEnvOr("ODBC_BENCH_RUST_DRIVER", "mssql-odbc"), false});
        }
    }

    auto& cfg = ODBCTestConfig::Instance();
    if (!cfg.HasConnection()) {
        std::cerr << "ERROR: no connection configured — set ODBC_TEST_SERVER "
                     "(and ODBC_TEST_UID / ODBC_TEST_PWD for SQL auth)\n";
        return 1;
    }

    std::string sql = BuildQuery(rows);
    std::cout << "Fetch-throughput A/B benchmark\n"
              << "  server   = " << cfg.Server() << "\n"
              << "  database = " << cfg.Database() << "\n"
              << "  rows     = " << rows << "\n"
              << "  reps     = " << reps << " (warmup " << warmup
              << " discarded)\n"
              << "  query    = " << sql << "\n";

    std::vector<std::pair<std::string, double>> medians;
    for (const auto& leg : legs) {
        auto kept = RunLeg(leg, sql, reps, warmup);
        std::vector<double> ms;
        for (const auto& r : kept) ms.push_back(r.ms);
        double med = Median(ms);
        double medRps = (kept.empty() || med <= 0.0)
                            ? 0.0
                            : (kept.front().rows / (med / 1000.0));
        medians.push_back({leg.label, med});
        std::cout << "  --> median " << std::fixed << std::setprecision(2)
                  << med << " ms  (" << static_cast<long long>(medRps)
                  << " rows/s)\n";
    }

    std::cout << "\n=== Summary (median ms, lower is faster) ===\n";
    for (const auto& m : medians) {
        std::cout << "  " << std::setw(10) << std::left << m.first << "  "
                  << std::fixed << std::setprecision(2) << m.second << " ms\n";
    }

    // A/B ratio: rust median / native median when both labels are present.
    auto find = [&](const std::string& lbl) -> double {
        for (const auto& m : medians)
            if (m.first == lbl) return m.second;
        return -1.0;
    };
    double nativeMed = find("native");
    double rustMed = find("rust");
    if (nativeMed > 0.0 && rustMed > 0.0) {
        std::cout << "\n=== A/B: Rust vs native ===\n"
                  << "  ratio (rust median / native median) = " << std::fixed
                  << std::setprecision(3) << (rustMed / nativeMed) << "x\n"
                  << "  (1.00x = parity; >1 = Rust slower; deltas <15% are "
                     "noise)\n";
    }
    return 0;
}

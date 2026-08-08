// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Statement-execution cost on an already-established connection: round-trip
// latency, the prepare/execute split, and parameter marshalling.

#include "perf_fixture.h"

#include <string>

namespace {

/// Shared connection across all iterations so the measurement excludes login.
struct Session {
    perf::Env env;
    perf::Conn conn{env};
};

/// Minimal round-trip — measures per-statement overhead with almost no server
/// work and no result-set decoding.
void BM_ExecDirect_SelectOne(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());

    SqlTString sql = perf::ToSqlTStr("SELECT 1");
    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLExecDirect(s.conn.stmt(), sql.data(), SQL_NTS))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        std::string err;
        if (perf::DrainRows(s.conn.stmt(), &err) < 0) {
            state.SkipWithError(err.c_str());
            return;
        }
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(state.iterations());
}

/// SQLPrepare once, SQLExecute repeatedly — the path a well-written application
/// uses for a hot statement. Isolates execute cost from statement compilation.
void BM_PreparedExecute(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());

    SqlTString sql = perf::ToSqlTStr("SELECT 1");
    PERF_REQUIRE(SQL_SUCCEEDED(SQLPrepare(s.conn.stmt(), sql.data(), SQL_NTS)), state,
                 perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());

    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLExecute(s.conn.stmt()))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        std::string err;
        if (perf::DrainRows(s.conn.stmt(), &err) < 0) {
            state.SkipWithError(err.c_str());
            return;
        }
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(state.iterations());
}

/// Prepare + execute every iteration — shows the cost a driver pays when an
/// application does not cache prepared handles.
void BM_PrepareExecute_EachTime(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());

    SqlTString sql = perf::ToSqlTStr("SELECT 1");
    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLPrepare(s.conn.stmt(), sql.data(), SQL_NTS)) ||
            !SQL_SUCCEEDED(SQLExecute(s.conn.stmt()))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        std::string err;
        if (perf::DrainRows(s.conn.stmt(), &err) < 0) {
            state.SkipWithError(err.c_str());
            return;
        }
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(state.iterations());
}

/// Parameterised execute — adds SQLBindParameter marshalling to the round trip.
/// Bound as SQL_C_CHAR → SQL_VARCHAR because that is the conversion pair
/// mssql-odbc currently implements; msodbcsql accepts it too, so both drivers
/// are measured on identical work.
void BM_ParameterizedExecute(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());

    char value[32] = "perf-parameter-value";
    SQLLEN ind = SQL_NTS;
    PERF_REQUIRE(SQL_SUCCEEDED(SQLBindParameter(s.conn.stmt(), 1, SQL_PARAM_INPUT,
                                                SQL_C_CHAR, SQL_VARCHAR, sizeof(value) - 1,
                                                0, value, sizeof(value), &ind)),
                 state, perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());

    SqlTString sql = perf::ToSqlTStr("SELECT ?");
    PERF_REQUIRE(SQL_SUCCEEDED(SQLPrepare(s.conn.stmt(), sql.data(), SQL_NTS)), state,
                 perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());

    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLExecute(s.conn.stmt()))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        std::string err;
        if (perf::DrainRows(s.conn.stmt(), &err) < 0) {
            state.SkipWithError(err.c_str());
            return;
        }
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(state.iterations());
}

/// SQLExecDirect without draining the result set — isolates the request/response
/// round trip from result-set consumption. Paired with BM_ExecDirect_SelectOne,
/// the difference is the fetch-and-close cost for a single trivial row.
void BM_ExecDirect_NoDrain(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());

    SqlTString sql = perf::ToSqlTStr("SELECT 1");
    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLExecDirect(s.conn.stmt(), sql.data(), SQL_NTS))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(state.iterations());
}

BENCHMARK(BM_ExecDirect_SelectOne)->Unit(benchmark::kMicrosecond);
BENCHMARK(BM_ExecDirect_NoDrain)->Unit(benchmark::kMicrosecond);
BENCHMARK(BM_PreparedExecute)->Unit(benchmark::kMicrosecond);
BENCHMARK(BM_PrepareExecute_EachTime)->Unit(benchmark::kMicrosecond);
BENCHMARK(BM_ParameterizedExecute)->Unit(benchmark::kMicrosecond);

}  // namespace

BENCHMARK_MAIN();

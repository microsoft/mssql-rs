// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Connection-establishment cost: handle allocation, login handshake, TLS.

#include "perf_fixture.h"

namespace {

/// Full connect/disconnect cycle — the dominant cost for short-lived processes
/// and the scenario where TLS negotiation and login round-trips show up.
void BM_Connect_Disconnect(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");

    perf::Env env;
    PERF_REQUIRE(env.ok(), state, "SQLAllocHandle(ENV) failed");

    {
        perf::Conn probe(env);
        PERF_REQUIRE(probe.ok(), state, probe.error().c_str());
    }

    for (auto _ : state) {
        perf::Conn conn(env);
        if (!conn.ok()) {
            state.SkipWithError(conn.error().c_str());
            return;
        }
        benchmark::DoNotOptimize(conn.dbc());
    }
    state.SetItemsProcessed(state.iterations());
}

/// Handle allocation only — isolates driver-side bookkeeping from network cost.
void BM_AllocFree_Stmt(benchmark::State& state) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");

    perf::Env env;
    PERF_REQUIRE(env.ok(), state, "SQLAllocHandle(ENV) failed");
    perf::Conn conn(env);
    PERF_REQUIRE(conn.ok(), state, conn.error().c_str());

    for (auto _ : state) {
        SQLHSTMT stmt = SQL_NULL_HSTMT;
        if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, conn.dbc(), &stmt))) {
            state.SkipWithError("SQLAllocHandle(STMT) failed");
            return;
        }
        SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    }
    state.SetItemsProcessed(state.iterations());
}

BENCHMARK(BM_Connect_Disconnect)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_AllocFree_Stmt)->Unit(benchmark::kMicrosecond);

}  // namespace

BENCHMARK_MAIN();

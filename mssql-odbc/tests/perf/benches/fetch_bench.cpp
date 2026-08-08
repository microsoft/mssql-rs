// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Result-set retrieval throughput: token-stream decoding, row materialisation,
// and SQLGetData conversion cost across row counts and row widths.

#include "perf_fixture.h"

#include <string>

namespace {

/// Rows are served from a session temp table populated once, so the measurement
/// reflects fetch cost rather than the server's row-generation cost.
constexpr int kSourceRows = 20000;

struct Session {
    perf::Env env;
    perf::Conn conn{env};

    bool Prepare() {
        return conn.ok() &&
               conn.Exec("SELECT TOP (" + std::to_string(kSourceRows) +
                         ") ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS id, "
                         "CAST('row payload text for fetch benchmarking' AS VARCHAR(64)) "
                         "AS payload "
                         "INTO #perf_rows "
                         "FROM sys.all_objects a CROSS JOIN sys.all_objects b");
    }
};

void FetchRange(benchmark::State& state, const std::string& columns,
                bool get_data = true) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());
    PERF_REQUIRE(s.Prepare(), state, s.conn.error().c_str());

    const int64_t rows = state.range(0);
    SqlTString sql = perf::ToSqlTStr("SELECT TOP (" + std::to_string(rows) + ") " +
                                     columns + " FROM #perf_rows");

    int64_t fetched = 0;
    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLExecDirect(s.conn.stmt(), sql.data(), SQL_NTS))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        std::string err;
        int64_t n = get_data ? perf::DrainRows(s.conn.stmt(), &err)
                             : perf::DrainRowsNoGetData(s.conn.stmt(), &err);
        if (n < 0) {
            state.SkipWithError(err.c_str());
            return;
        }
        fetched += n;
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(fetched);
}

/// Narrow rows — dominated by per-row protocol and cursor overhead.
void BM_Fetch_NarrowRows(benchmark::State& state) {
    FetchRange(state, "id");
}

/// Wide rows — adds per-column SQLGetData conversion to the per-row cost.
void BM_Fetch_WideRows(benchmark::State& state) {
    FetchRange(state,
               "id, payload, payload AS p2, payload AS p3, payload AS p4, "
               "payload AS p5, payload AS p6, payload AS p7");
}

/// Cursor-only advance — isolates SQLFetch cost from SQLGetData conversion.
void BM_Fetch_RowsOnly(benchmark::State& state) {
    FetchRange(state, "id", /*get_data=*/false);
}

BENCHMARK(BM_Fetch_NarrowRows)
    ->Arg(100)
    ->Arg(1000)
    ->Arg(10000)
    ->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Fetch_WideRows)
    ->Arg(100)
    ->Arg(1000)
    ->Arg(10000)
    ->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Fetch_RowsOnly)->Arg(10000)->Unit(benchmark::kMillisecond);

}  // namespace

BENCHMARK_MAIN();

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Per-type decode cost. Each case fetches the same number of rows so the delta
// between cases is attributable to the column type's conversion path.

#include "perf_fixture.h"

#include <string>

namespace {

constexpr int kRows = 5000;

struct Session {
    perf::Env env;
    perf::Conn conn{env};
};

/// Fetch |rows| rows of a single expression.
void TypeBench(benchmark::State& state, const std::string& expr, int rows = kRows) {
    PERF_REQUIRE(perf::Config::Instance().HasConnection(), state,
                 "no connection configured (set ODBC_TEST_SERVER)");
    Session s;
    PERF_REQUIRE(s.conn.ok(), state, s.conn.error().c_str());

    SqlTString sql = perf::ToSqlTStr("SELECT TOP (" + std::to_string(rows) + ") " + expr +
                                     " AS v FROM sys.all_objects a CROSS JOIN "
                                     "sys.all_objects b");

    int64_t fetched = 0;
    for (auto _ : state) {
        if (!SQL_SUCCEEDED(SQLExecDirect(s.conn.stmt(), sql.data(), SQL_NTS))) {
            state.SkipWithError(
                perf::DiagText(SQL_HANDLE_STMT, s.conn.stmt()).c_str());
            return;
        }
        std::string err;
        int64_t n = perf::DrainRows(s.conn.stmt(), &err);
        if (n < 0) {
            state.SkipWithError(err.c_str());
            return;
        }
        fetched += n;
        perf::CloseCursor(s.conn.stmt());
    }
    state.SetItemsProcessed(fetched);
}

void BM_Type_Int(benchmark::State& state) {
    TypeBench(state, "CAST(a.object_id AS INT)");
}

void BM_Type_BigInt(benchmark::State& state) {
    TypeBench(state, "CAST(a.object_id AS BIGINT)");
}

void BM_Type_Decimal(benchmark::State& state) {
    TypeBench(state, "CAST(a.object_id AS DECIMAL(18,4))");
}

void BM_Type_Float(benchmark::State& state) {
    TypeBench(state, "CAST(a.object_id AS FLOAT)");
}

void BM_Type_Varchar(benchmark::State& state) {
    TypeBench(state, "CAST(REPLICATE('x', 100) AS VARCHAR(100))");
}

void BM_Type_NVarchar(benchmark::State& state) {
    TypeBench(state, "CAST(REPLICATE(N'\u00e9', 100) AS NVARCHAR(100))");
}

void BM_Type_DateTime2(benchmark::State& state) {
    TypeBench(state, "CAST('2026-01-02 03:04:05.1234567' AS DATETIME2)");
}

void BM_Type_Guid(benchmark::State& state) {
    TypeBench(state, "CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS UNIQUEIDENTIFIER)");
}

/// A value larger than the harness read buffer, so this measures the chunked
/// SQLGetData path rather than a single-shot copy. Row count is deliberately
/// small: each row moves ~20 KB, and a driver with a quadratic chunked-read path
/// makes this case orders of magnitude slower than the others.
void BM_Type_VarcharMax(benchmark::State& state) {
    TypeBench(state, "CAST(REPLICATE(CAST('y' AS VARCHAR(MAX)), 20000) AS VARCHAR(MAX))",
              100);
}

BENCHMARK(BM_Type_Int)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_BigInt)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_Decimal)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_Float)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_Varchar)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_NVarchar)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_DateTime2)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_Guid)->Unit(benchmark::kMillisecond);
BENCHMARK(BM_Type_VarcharMax)->Unit(benchmark::kMillisecond);

}  // namespace

BENCHMARK_MAIN();

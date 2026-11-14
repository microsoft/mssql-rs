// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_bench.hpp"

#include <benchmark/benchmark.h>

#include <exception>
#include <iostream>
#include <memory>
#include <string>
#include <vector>

namespace {

using mssql::odbc::bench::RetrievalMetrics;
using mssql::odbc::bench::WorkloadRunner;

// Publish wall-time-derived throughput alongside phase timings so the report can
// compare end-to-end cost while retaining enough context to diagnose a shift.
void set_counters(benchmark::State& state, const RetrievalMetrics& metrics) {
    state.counters["rows"] = static_cast<double>(metrics.rows);
    state.counters["cells"] = static_cast<double>(metrics.cells);
    state.counters["logical_bytes"] = static_cast<double>(metrics.logical_bytes);
    state.counters["rows_per_second"] =
        static_cast<double>(metrics.rows) / metrics.total_seconds;
    state.counters["cells_per_second"] =
        static_cast<double>(metrics.cells) / metrics.total_seconds;
    state.counters["logical_bytes_per_second"] =
        static_cast<double>(metrics.logical_bytes) / metrics.total_seconds;
    state.counters["execute_ms"] = metrics.execute_seconds * 1000.0;
    if (metrics.metadata_bind_seconds >= 0.0) {
        state.counters["metadata_bind_ms"] = metrics.metadata_bind_seconds * 1000.0;
    }
    state.counters["fetch_ms"] = metrics.fetch_seconds * 1000.0;
    if (metrics.execute_calls != 0) {
        state.counters["execute_calls"] = static_cast<double>(metrics.execute_calls);
    }
    // Zero for the bound modes. On the row-at-a-time workloads it is the number of
    // driver round trips the consumer's access pattern forced, which is the thing
    // that actually differs between drivers there.
    state.counters["get_data_calls"] = static_cast<double>(metrics.get_data_calls);
    state.SetItemsProcessed(static_cast<std::int64_t>(metrics.rows));
    state.SetBytesProcessed(static_cast<std::int64_t>(metrics.logical_bytes));
}

// Register a write runner that owns its own timing; `execute()` is one sample.
template <typename Runner>
void register_write(Runner* runner, std::string& benchmark_error) {
    benchmark::RegisterBenchmark(
        runner->name(),
        [runner, &benchmark_error](benchmark::State& state) {
            if (!benchmark_error.empty()) {
                state.SkipWithError(benchmark_error.c_str());
                return;
            }
            try {
                for (auto _ : state) {
                    (void)_;
                    const auto metrics = runner->execute();
                    state.SetIterationTime(metrics.total_seconds);
                    set_counters(state, metrics);
                }
            } catch (const std::exception& error) {
                benchmark_error = error.what();
                state.SkipWithError(benchmark_error.c_str());
            }
        })
        ->Iterations(1)
        ->UseManualTime()
        ->Unit(benchmark::kMillisecond);
}

}  // namespace

// Register only the selected scenario after an untimed correctness preflight, then
// let Google Benchmark own repetition and raw JSON emission.
int main(int argc, char** argv) {
    benchmark::Initialize(&argc, argv);
    if (benchmark::ReportUnrecognizedArguments(argc, argv)) {
        return 2;
    }

    try {
        const auto config = mssql::odbc::bench::Config::from_environment();
        mssql::odbc::bench::OdbcSession session(config);
        std::vector<std::unique_ptr<WorkloadRunner>> runners;
        runners.reserve(mssql::odbc::bench::workloads().size());

        for (const auto& spec : mssql::odbc::bench::workloads()) {
            if (!config.scenario.empty() && config.scenario != spec.scenario) {
                continue;
            }
            auto runner = std::make_unique<WorkloadRunner>(session, spec);
            runner->preflight();
            runners.push_back(std::move(runner));
        }

        std::string benchmark_error;
        for (auto& runner : runners) {
            WorkloadRunner* const current = runner.get();
            benchmark::RegisterBenchmark(
                current->spec().benchmark_name,
                [current, &benchmark_error](benchmark::State& state) {
                    if (!benchmark_error.empty()) {
                        state.SkipWithError(benchmark_error.c_str());
                        return;
                    }
                    try {
                        for (auto _ : state) {
                            (void)_;
                            const auto metrics = current->retrieve();
                            state.SetIterationTime(metrics.total_seconds);
                            set_counters(state, metrics);
                        }
                    } catch (const std::exception& error) {
                        benchmark_error = error.what();
                        state.SkipWithError(benchmark_error.c_str());
                    }
                })
                ->Iterations(1)
                ->UseManualTime()
                ->Unit(benchmark::kMillisecond);
        }

        std::unique_ptr<mssql::odbc::bench::ParameterArrayWriteRunner> write_runner;
        if (config.scenario.empty() || config.scenario == "write") {
            write_runner =
                std::make_unique<mssql::odbc::bench::ParameterArrayWriteRunner>(
                    session, config.write_mode);
            write_runner->preflight();
            register_write(write_runner.get(), benchmark_error);
        }

        std::vector<std::unique_ptr<mssql::odbc::bench::ExecuteManyWriteRunner>>
            execute_many_runners;
        if (config.scenario.empty() ||
            config.scenario == mssql::odbc::bench::kExecuteManyScenario) {
            for (const auto shape : {mssql::odbc::bench::ExecuteManyShape::c3,
                                     mssql::odbc::bench::ExecuteManyShape::c10}) {
                auto runner = std::make_unique<mssql::odbc::bench::ExecuteManyWriteRunner>(
                    session, shape);
                runner->preflight();
                register_write(runner.get(), benchmark_error);
                execute_many_runners.push_back(std::move(runner));
            }
        }

        benchmark::RunSpecifiedBenchmarks();
        benchmark::Shutdown();
        if (!benchmark_error.empty()) {
            std::cerr << "ODBC benchmark failed: " << benchmark_error << '\n';
            return 1;
        }
    } catch (const std::exception& error) {
        benchmark::Shutdown();
        std::cerr << "ODBC benchmark failed: " << error.what() << '\n';
        return 1;
    }

    return 0;
}

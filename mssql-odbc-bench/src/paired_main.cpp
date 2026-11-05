// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_bench.hpp"

#include <exception>
#include <fstream>
#include <iostream>
#include <string>
#include <string_view>

// Paired latency is not a Google Benchmark family: one process measures every
// driver leg, so a sample has no meaning without its partner from the same round.
int main(int argc, char** argv) {
    constexpr std::string_view kOutPrefix = "--out=";
    std::string output_path;
    for (int index = 1; index < argc; ++index) {
        const std::string_view argument(argv[index]);
        if (argument.substr(0, kOutPrefix.size()) == kOutPrefix) {
            output_path = std::string(argument.substr(kOutPrefix.size()));
        } else {
            output_path.clear();
            break;
        }
    }
    if (output_path.empty()) {
        std::cerr << "Usage: mssql_odbc_bench_paired --out=<paired-latency.json>\n";
        return 2;
    }

    try {
        const auto config = mssql::odbc::bench::PairedConfig::from_environment();
        const auto json = mssql::odbc::bench::run_paired_latency(config);
        // Written only after every round succeeded, so a failed run never leaves
        // a partial document for the comparator.
        std::ofstream file(output_path, std::ios::binary | std::ios::trunc);
        file << json;
        file.close();
        if (!file) {
            std::cerr << "Paired ODBC latency benchmark could not write " << output_path
                      << '\n';
            return 1;
        }
    } catch (const std::exception& error) {
        std::cerr << "Paired ODBC latency benchmark failed: " << error.what() << '\n';
        return 1;
    }
    return 0;
}

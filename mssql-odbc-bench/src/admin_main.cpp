// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_bench.hpp"

#include <exception>
#include <iostream>
#include <string>

// Keep table lifecycle in a separate executable so setup and cleanup cannot enter
// Google Benchmark's measurement process or timing boundary.
int main(int argc, char** argv) {
    if (argc != 2) {
        std::cerr << "Usage: mssql_odbc_bench_admin <setup|cleanup|print-sql>\n";
        return 2;
    }

    try {
        const std::string command(argv[1]);
        // print-sql is deliberately handled before any connection is made: it is
        // the offline way to review or replay the generated schema and data.
        if (command == "print-sql") {
            mssql::odbc::bench::print_benchmark_sql(std::cout);
            return 0;
        }

        const auto config = mssql::odbc::bench::Config::from_environment();
        mssql::odbc::bench::OdbcSession session(config);
        if (command == "setup") {
            mssql::odbc::bench::setup_benchmark_tables(session);
        } else if (command == "cleanup") {
            mssql::odbc::bench::cleanup_benchmark_tables(session);
        } else {
            std::cerr << "Unknown command '" << command
                      << "'; expected setup, cleanup, or print-sql\n";
            return 2;
        }
    } catch (const std::exception& error) {
        std::cerr << "ODBC benchmark administration failed: " << error.what() << '\n';
        return 1;
    }

    return 0;
}

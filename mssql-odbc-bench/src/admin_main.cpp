// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_bench.hpp"

#include <exception>
#include <iostream>
#include <string>

int main(int argc, char** argv) {
    if (argc != 2) {
        std::cerr << "Usage: mssql_odbc_bench_admin <setup|cleanup>\n";
        return 2;
    }

    try {
        const auto config = mssql::odbc::bench::Config::from_environment();
        mssql::odbc::bench::OdbcSession session(config);
        const std::string command(argv[1]);
        if (command == "setup") {
            mssql::odbc::bench::setup_benchmark_tables(session);
        } else if (command == "cleanup") {
            mssql::odbc::bench::cleanup_benchmark_tables(session);
        } else {
            std::cerr << "Unknown command '" << command << "'; expected setup or cleanup\n";
            return 2;
        }
    } catch (const std::exception& error) {
        std::cerr << "ODBC benchmark administration failed: " << error.what() << '\n';
        return 1;
    }

    return 0;
}

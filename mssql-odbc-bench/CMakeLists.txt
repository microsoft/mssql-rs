# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

cmake_minimum_required(VERSION 3.20)
project(mssql_odbc_bench LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

include(FetchContent)

set(BENCHMARK_ENABLE_TESTING OFF CACHE BOOL "" FORCE)
set(BENCHMARK_ENABLE_INSTALL OFF CACHE BOOL "" FORCE)
set(BENCHMARK_ENABLE_GTEST_TESTS OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
    googlebenchmark
    GIT_REPOSITORY https://github.com/google/benchmark.git
    GIT_TAG c58e6d0710581e3a08d65c349664128a8d9a2461 # v1.9.1
)
FetchContent_MakeAvailable(googlebenchmark)

if(WIN32)
    set(ODBC_DM_LIBRARIES odbc32)
else()
    find_path(ODBC_INCLUDE_DIR sql.h)
    find_library(ODBC_DM_LIBRARY NAMES odbc REQUIRED)
    if(NOT ODBC_INCLUDE_DIR)
        message(FATAL_ERROR "Could not find sql.h; install the unixODBC development package")
    endif()
    set(ODBC_DM_LIBRARIES ${ODBC_DM_LIBRARY})
endif()

add_library(mssql_odbc_bench_common STATIC
    src/odbc_bench.cpp
)
target_include_directories(mssql_odbc_bench_common
    PUBLIC
        ${CMAKE_CURRENT_SOURCE_DIR}/include
        $<$<BOOL:${ODBC_INCLUDE_DIR}>:${ODBC_INCLUDE_DIR}>
)
target_compile_definitions(mssql_odbc_bench_common PUBLIC ODBCVER=0x0380)
if(WIN32)
    target_compile_definitions(mssql_odbc_bench_common PUBLIC UNICODE _UNICODE NOMINMAX)
endif()
target_link_libraries(mssql_odbc_bench_common PUBLIC ${ODBC_DM_LIBRARIES})

if(MSVC)
    target_compile_options(mssql_odbc_bench_common PRIVATE /W4 /permissive-)
else()
    target_compile_options(mssql_odbc_bench_common PRIVATE -Wall -Wextra -Wpedantic)
endif()

add_executable(mssql_odbc_bench
    src/benchmark_main.cpp
)
target_link_libraries(mssql_odbc_bench
    PRIVATE
        mssql_odbc_bench_common
        benchmark::benchmark
)

add_executable(mssql_odbc_bench_admin
    src/admin_main.cpp
)
target_link_libraries(mssql_odbc_bench_admin PRIVATE mssql_odbc_bench_common)

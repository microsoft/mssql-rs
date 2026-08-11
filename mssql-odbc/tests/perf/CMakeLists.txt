cmake_minimum_required(VERSION 3.15)
project(odbc_perf LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Perf numbers from a debug build are meaningless — default to Release when the
# caller did not pick a configuration for a single-config generator.
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

find_package(Threads QUIET)
if(NOT Threads_FOUND)
    set(CMAKE_THREAD_LIBS_INIT "-lpthread")
    set(THREADS_PREFER_PTHREAD_FLAG ON)
endif()

# ---------------------------------------------------------------------------
# Google Benchmark via FetchContent (mirrors how tests/e2e fetches GoogleTest)
# ---------------------------------------------------------------------------
include(FetchContent)
set(BENCHMARK_ENABLE_TESTING OFF CACHE BOOL "" FORCE)
set(BENCHMARK_ENABLE_GTEST_TESTS OFF CACHE BOOL "" FORCE)
set(BENCHMARK_ENABLE_INSTALL OFF CACHE BOOL "" FORCE)
set(BENCHMARK_USE_BUNDLED_GTEST OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
    benchmark
    GIT_REPOSITORY https://github.com/google/benchmark.git
    GIT_TAG v1.9.1
)
FetchContent_MakeAvailable(benchmark)

# ---------------------------------------------------------------------------
# Platform-specific ODBC libraries
# ---------------------------------------------------------------------------
if(WIN32)
    set(ODBC_LIBRARIES odbc32 odbccp32)
else()
    find_library(ODBC_LIB NAMES odbc odbc2 iodbc)
    if(NOT ODBC_LIB)
        message(FATAL_ERROR "Could not find an ODBC library (unixODBC or iODBC). "
                            "Install unixODBC-dev / libiodbc2-dev or equivalent.")
    endif()
    find_library(ODBCINST_LIB NAMES odbcinst iodbcinst)
    set(ODBC_LIBRARIES ${ODBC_LIB})
    if(ODBCINST_LIB)
        list(APPEND ODBC_LIBRARIES ${ODBCINST_LIB})
    endif()

    find_path(ODBC_INCLUDE_DIR sql.h
        PATHS /opt/homebrew/include /usr/local/include /usr/include
    )
    if(NOT ODBC_INCLUDE_DIR)
        message(FATAL_ERROR "Could not find sql.h. Ensure unixODBC headers are installed.")
    endif()
endif()

# ---------------------------------------------------------------------------
# Unicode build mode (after FetchContent so the vendored benchmark build is not
# compiled with UNICODE/_UNICODE). Windows defaults to ON because mssql-odbc
# exports only wide (W) entry points.
# ---------------------------------------------------------------------------
if(WIN32)
    set(_perf_unicode_default ON)
else()
    set(_perf_unicode_default OFF)
endif()
option(ODBC_PERF_FORCE_UNICODE "Build perf benchmarks with UNICODE/_UNICODE" ${_perf_unicode_default})
if(ODBC_PERF_FORCE_UNICODE)
    add_compile_definitions(UNICODE _UNICODE)
endif()

# ---------------------------------------------------------------------------
# perf_lib  –  shared config + connection helpers
# ---------------------------------------------------------------------------
add_library(perf_lib STATIC lib/perf_fixture.cpp)
target_include_directories(perf_lib PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
if(ODBC_INCLUDE_DIR)
    target_include_directories(perf_lib PUBLIC ${ODBC_INCLUDE_DIR})
endif()
target_link_libraries(perf_lib PUBLIC benchmark::benchmark ${ODBC_LIBRARIES})

function(add_odbc_bench BENCH_NAME)
    add_executable(${BENCH_NAME} ${ARGN})
    target_link_libraries(${BENCH_NAME} PRIVATE perf_lib)
endfunction()

add_odbc_bench(connect_bench   benches/connect_bench.cpp)
add_odbc_bench(exec_bench      benches/exec_bench.cpp)
add_odbc_bench(fetch_bench     benches/fetch_bench.cpp)
add_odbc_bench(datatype_bench  benches/datatype_bench.cpp)

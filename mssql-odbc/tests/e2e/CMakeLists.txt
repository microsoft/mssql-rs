cmake_minimum_required(VERSION 3.15)
project(odbc_gtests LANGUAGES C CXX)

# C++17 is the minimum for GoogleTest (v1.15+).
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# threads (required by gtest on older glibc where pthread is separate)
find_package(Threads QUIET)
if(NOT Threads_FOUND)
    message(STATUS "FindThreads failed — falling back to -lpthread")
    set(CMAKE_THREAD_LIBS_INIT "-lpthread")
    set(THREADS_PREFER_PTHREAD_FLAG ON)
endif()

# ---------------------------------------------------------------------------
# Google Test via FetchContent
# ---------------------------------------------------------------------------
# msodbcsql uses a git submodule in third_party/googletest/.
# We use FetchContent for a self-contained build without submodule setup.
include(FetchContent)
FetchContent_Declare(
    googletest
    GIT_REPOSITORY https://github.com/google/googletest.git
    GIT_TAG 52eb8108c5bdec04579160ae17225d66034bd723 # v1.17.0
)
set(gtest_force_shared_crt ON CACHE BOOL "" FORCE)
FetchContent_MakeAvailable(googletest)

# ---------------------------------------------------------------------------
# Platform-specific ODBC libraries
# ---------------------------------------------------------------------------
if(WIN32)
    set(ODBC_LIBRARIES odbc32 odbccp32)
else()
    # On Linux / macOS, prefer unixODBC; fall back to iODBC on macOS.
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

    # Find ODBC headers (sql.h)
    find_path(ODBC_INCLUDE_DIR sql.h
        PATHS /opt/homebrew/include /usr/local/include /usr/include
    )
    if(NOT ODBC_INCLUDE_DIR)
        message(FATAL_ERROR "Could not find sql.h. "
                            "Ensure unixODBC headers are installed.")
    endif()
    message(STATUS "ODBC headers found at: ${ODBC_INCLUDE_DIR}")
endif()

# ---------------------------------------------------------------------------
# Unicode build mode (must come after googletest so the vendored gtest build
# is not compiled with UNICODE/_UNICODE).
# Windows defaults to ON because mssql-odbc exports wide (W) entrypoints. Unix
# stays narrow to exercise Driver Manager ANSI routing, including pre-connect
# attribute replay. Applies to odbc_test_lib and every test target below.
# ---------------------------------------------------------------------------
if(WIN32)
    set(_odbc_e2e_unicode_default ON)
else()
    set(_odbc_e2e_unicode_default OFF)
endif()
option(ODBC_E2E_FORCE_UNICODE "Build e2e tests with UNICODE/_UNICODE" ${_odbc_e2e_unicode_default})
if(ODBC_E2E_FORCE_UNICODE)
    add_compile_definitions(UNICODE _UNICODE)
    message(STATUS "ODBC e2e tests: UNICODE/_UNICODE enabled")
else()
    message(STATUS "ODBC e2e tests: UNICODE/_UNICODE disabled (platform default TCHAR mode)")
endif()

# ---------------------------------------------------------------------------
# odbc_test_lib  –  shared fixture + helpers (static library)
# ---------------------------------------------------------------------------
add_library(odbc_test_lib STATIC
    lib/odbc_test_fixture.cpp
    lib/odbc_test_utils.cpp
    lib/odbc_test_config.cpp
)
target_include_directories(odbc_test_lib PUBLIC
    ${CMAKE_CURRENT_SOURCE_DIR}/include
)
if(ODBC_INCLUDE_DIR)
    target_include_directories(odbc_test_lib PUBLIC ${ODBC_INCLUDE_DIR})
endif()
target_link_libraries(odbc_test_lib PUBLIC
    gtest
    ${ODBC_LIBRARIES}
)
if(NOT WIN32)
    find_package(Iconv REQUIRED)
    target_link_libraries(odbc_test_lib PUBLIC Iconv::Iconv)
endif()
# odbc_concurrent_test.h runs test bodies on std::thread workers, and on Windows
# writes hang and crash minidumps through dbghelp.
if(TARGET Threads::Threads)
    target_link_libraries(odbc_test_lib PUBLIC Threads::Threads)
endif()
if(WIN32)
    target_link_libraries(odbc_test_lib PUBLIC dbghelp)
endif()
# Each ODBC_TEST_F instantiates every mode runner for its fixture, so a large
# converted file outgrows MSVC's default object section limit (C1128).
if(MSVC)
    target_compile_options(odbc_test_lib PUBLIC /bigobj)
endif()

# ---------------------------------------------------------------------------
# include/odbc_sql_server_types.h copies values and layouts from msodbcsql.h
# rather than depending on it. Where msodbcsql.h is installed, compile a check
# that the two still agree; a mismatch fails the build. Nothing is linked or
# shipped. The header comes with the msodbcsql18 SDK feature on Windows (part of
# a default MSI install) and with the msodbcsql18 package on Linux.
# Override the search with -DMSODBCSQL_INCLUDE_DIR=<dir>.
# ---------------------------------------------------------------------------
set(_msodbcsql_hints)
if(WIN32)
    file(TO_CMAKE_PATH "$ENV{ProgramFiles}" _program_files)
    list(APPEND _msodbcsql_hints
         "${_program_files}/Microsoft SQL Server/Client SDK/ODBC/180/SDK/Include")
elseif(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    list(APPEND _msodbcsql_hints "/opt/microsoft/msodbcsql18/include")
endif()
find_path(MSODBCSQL_INCLUDE_DIR msodbcsql.h HINTS ${_msodbcsql_hints})
if(MSODBCSQL_INCLUDE_DIR)
    message(STATUS "Checking odbc_sql_server_types.h against ${MSODBCSQL_INCLUDE_DIR}/msodbcsql.h")
    add_library(msodbcsql_types_check OBJECT check/msodbcsql_types_check.cpp)
    target_include_directories(msodbcsql_types_check PRIVATE
        ${CMAKE_CURRENT_SOURCE_DIR}/include ${MSODBCSQL_INCLUDE_DIR})
    if(ODBC_INCLUDE_DIR)
        target_include_directories(msodbcsql_types_check PRIVATE ${ODBC_INCLUDE_DIR})
    endif()
else()
    message(STATUS "msodbcsql.h not found; skipping the odbc_sql_server_types.h check")
endif()

# ---------------------------------------------------------------------------
# Test registration
#
# An executable whose sources invoke ODBC_TEST_F or ODBC_TEST_F_MODES at the
# start of a line (odbc_concurrent_test.h) is registered twice, so a concurrency
# failure cannot hide the single-threaded verdict in the parity report:
#   <name>     single-threaded tests only
#   <name>_mt  the concurrent variants only, labelled "mt" (ctest -L mt)
# Both filter on the mode suffixes rather than "*/*", because parameterized
# TEST_P names contain "/" too. A source converted after the last configure
# still runs every variant in <name> until CMake re-runs, so nothing is lost.
# On Unix <name>_mt runs through run_mt_test.sh, which can run it under gdb so
# a crash or watchdog abort prints every thread's stack (MSSQL_ODBC_MT_GDB=1).
# Such executables link lib/odbc_skip_aware_main.cpp: MSSQL_ODBC_MT_SKIP_MODES
# can skip every variant an entry selects, which must read as skipped (77), not
# as a pass.
# ---------------------------------------------------------------------------
set(ODBC_MT_MODE_FILTER "*/CrossThread:*/SharedEnv:*/SharedDbc:*/SharedStmt")

# Sets ${OUT_VAR} to ON when any of the sources uses ODBC_TEST_F(_MODES).
function(odbc_uses_concurrent_tests OUT_VAR)
    set(concurrent OFF)
    foreach(source IN LISTS ARGN)
        get_filename_component(path "${source}" ABSOLUTE BASE_DIR "${CMAKE_CURRENT_SOURCE_DIR}")
        if(EXISTS "${path}")
            file(STRINGS "${path}" invocations REGEX "^ODBC_TEST_F(_MODES)?\\(")
            if(invocations)
                set(concurrent ON)
            endif()
        endif()
    endforeach()
    set(${OUT_VAR} ${concurrent} PARENT_SCOPE)
endfunction()

function(odbc_register_ctest TEST_NAME)
    odbc_uses_concurrent_tests(concurrent ${ARGN})
    if(concurrent)
        add_test(NAME ${TEST_NAME} COMMAND ${TEST_NAME} "--gtest_filter=-${ODBC_MT_MODE_FILTER}")
        if(UNIX)
            add_test(NAME ${TEST_NAME}_mt
                     COMMAND sh "${CMAKE_CURRENT_SOURCE_DIR}/run_mt_test.sh"
                             $<TARGET_FILE:${TEST_NAME}> "--gtest_filter=${ODBC_MT_MODE_FILTER}")
        else()
            add_test(NAME ${TEST_NAME}_mt
                     COMMAND ${TEST_NAME} "--gtest_filter=${ODBC_MT_MODE_FILTER}")
        endif()
        set_tests_properties(${TEST_NAME}_mt PROPERTIES SKIP_RETURN_CODE 77 LABELS mt)
    else()
        add_test(NAME ${TEST_NAME} COMMAND ${TEST_NAME})
    endif()
    set_tests_properties(${TEST_NAME} PROPERTIES SKIP_RETURN_CODE 77)
endfunction()

# ---------------------------------------------------------------------------
# Helper function: add a test executable under tests/
# Usage:  add_odbc_test(smoke_test tests/smoke_test.cpp)
# ---------------------------------------------------------------------------
function(add_odbc_test TEST_NAME)
    odbc_uses_concurrent_tests(concurrent ${ARGN})
    if(concurrent)
        add_executable(${TEST_NAME} ${ARGN} lib/odbc_skip_aware_main.cpp)
        target_link_libraries(${TEST_NAME} PRIVATE odbc_test_lib)
    else()
        add_executable(${TEST_NAME} ${ARGN})
        target_link_libraries(${TEST_NAME} PRIVATE odbc_test_lib gtest_main)
    endif()
    odbc_register_ctest(${TEST_NAME} ${ARGN})
endfunction()

# ---------------------------------------------------------------------------
# Helper function: a test that loads the driver directly by path and so skips
# wholesale on a leg where MSSQL_ODBC_DLL is not set.
#
# Links lib/odbc_skip_aware_main.cpp instead of gtest_main so a fully-skipped
# run exits 77; see that file for why ctest needs the distinct exit code.
# ---------------------------------------------------------------------------
function(add_odbc_direct_load_test TEST_NAME)
    add_executable(${TEST_NAME} ${ARGN} lib/odbc_skip_aware_main.cpp)
    target_link_libraries(${TEST_NAME} PRIVATE odbc_test_lib)
    odbc_register_ctest(${TEST_NAME} ${ARGN})
endfunction()

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------
enable_testing()

add_odbc_test(smoke_test         tests/smoke_test.cpp)
add_odbc_test(alloc_env_test     tests/alloc_env_test.cpp)
add_odbc_test(alloc_dbc_test     tests/alloc_dbc_test.cpp)
add_odbc_test(alloc_stmt_test    tests/alloc_stmt_test.cpp)
add_odbc_test(free_handle_test   tests/free_handle_test.cpp)
add_odbc_test(set_env_attr_test  tests/set_env_attr_test.cpp)
add_odbc_test(get_diag_rec_test  tests/get_diag_rec_test.cpp)
add_odbc_test(describe_col_test  tests/describe_col_test.cpp)
add_odbc_test(col_attribute_test tests/col_attribute_test.cpp)
add_odbc_test(fetch_scroll_test tests/fetch_scroll_test.cpp)
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    add_executable(get_data_iso88591_test
        tests/get_data_iso88591_test.cpp
        lib/odbc_iso88591_main.cpp
    )
    target_link_libraries(get_data_iso88591_test PRIVATE odbc_test_lib)
    odbc_register_ctest(get_data_iso88591_test tests/get_data_iso88591_test.cpp)
endif()
add_odbc_test(num_result_cols_test tests/num_result_cols_test.cpp)
add_odbc_test(get_diag_field_test tests/get_diag_field_test.cpp)
add_odbc_test(get_desc_field_test tests/get_desc_field_test.cpp)
add_odbc_test(set_desc_field_test tests/set_desc_field_test.cpp)
add_odbc_test(get_desc_rec_test tests/get_desc_rec_test.cpp)
add_odbc_test(set_desc_rec_test tests/set_desc_rec_test.cpp)
add_odbc_test(alloc_free_desc_test tests/alloc_free_desc_test.cpp)
add_odbc_test(driver_connect_test tests/driver_connect_test.cpp)
if(UNIX)
    option(ODBC_E2E_RELEASE_DRIVER "Use release-driver stack budgets" OFF)
    add_odbc_test(small_stack_connect_test tests/multithreaded_tests/small_stack_connect_test.cpp)
    if(ODBC_E2E_RELEASE_DRIVER)
        target_compile_definitions(small_stack_connect_test PRIVATE ODBC_E2E_RELEASE_DRIVER)
    endif()
    set_tests_properties(small_stack_connect_test PROPERTIES TIMEOUT 60)
endif()
add_odbc_test(exec_direct_test   tests/exec_direct_test.cpp)
add_odbc_test(cancel_test        tests/multithreaded_tests/cancel_test.cpp)
if(WIN32)
    target_link_libraries(cancel_test PRIVATE ws2_32)
endif()
set_tests_properties(cancel_test PROPERTIES TIMEOUT 30)
add_odbc_test(shared_connection_busy_test tests/multithreaded_tests/shared_connection_busy_test.cpp)
add_odbc_test(more_results_test  tests/more_results_test.cpp)
add_odbc_test(output_params_test tests/output_params_test.cpp)
add_odbc_test(dml_output_test    tests/dml_output_test.cpp)
add_odbc_test(call_routing_test tests/call_routing_test.cpp)
add_odbc_test(execute_test       tests/execute_test.cpp)
add_odbc_test(param_char_conversions_test tests/param_char_conversions_test.cpp)
add_odbc_test(param_conversions_test tests/param_conversions_test.cpp)
add_odbc_test(param_binary_conversions_test tests/param_binary_conversions_test.cpp)
add_odbc_test(describe_param_test tests/describe_param_test.cpp)
add_odbc_test(tvp_test tests/tvp_test.cpp)
add_odbc_test(escape_sequence_test tests/escape_sequence_test.cpp)
add_odbc_test(api_contract_test tests/api_contract_test.cpp)
add_odbc_test(get_data_test      tests/get_data_test.cpp)
add_odbc_test(binary_fetch_test tests/binary_fetch_test.cpp)
add_odbc_test(bin_to_char_test tests/bin_to_char_test.cpp)
add_odbc_test(get_type_info_test tests/get_type_info_test.cpp)
add_odbc_test(get_info_test      tests/get_info_test.cpp)
add_odbc_test(catalog_test       tests/catalog_test.cpp)
add_odbc_test(row_count_test     tests/row_count_test.cpp)
add_odbc_test(transaction_test   tests/transaction_test.cpp)
add_odbc_test(session_recovery_test tests/session_recovery_test.cpp)
add_odbc_test(connection_pool_test tests/connection_pool_test.cpp)
add_odbc_test(attributes_test    tests/attributes_test.cpp)
add_odbc_test(param_array_test   tests/param_array_test.cpp)
add_odbc_test(connection_busy_test tests/connection_busy_test.cpp)
add_odbc_test(datetime_types_test tests/datetime_types_test.cpp)
add_odbc_test(time_to_timestamp_test tests/time_to_timestamp_test.cpp)
add_odbc_test(numeric_types_test  tests/numeric_types_test.cpp)
add_odbc_test(text_money_types_test tests/text_money_types_test.cpp)
add_odbc_test(fetch_error_semantics_test tests/fetch_error_semantics_test.cpp)
add_odbc_direct_load_test(dll_unload_stress_test tests/dll_unload_stress_test.cpp)
add_odbc_direct_load_test(trace_rotation_test tests/trace_rotation_test.cpp)

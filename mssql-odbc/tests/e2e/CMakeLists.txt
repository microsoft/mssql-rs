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
    GIT_TAG v1.17.0
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

# ---------------------------------------------------------------------------
# Helper function: add a test executable under tests/
# Usage:  add_odbc_test(smoke_test tests/smoke_test.cpp)
# ---------------------------------------------------------------------------
function(add_odbc_test TEST_NAME)
    add_executable(${TEST_NAME} ${ARGN})
    target_link_libraries(${TEST_NAME} PRIVATE odbc_test_lib gtest_main)
    add_test(NAME ${TEST_NAME} COMMAND ${TEST_NAME})
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
add_odbc_test(driver_connect_test tests/driver_connect_test.cpp)

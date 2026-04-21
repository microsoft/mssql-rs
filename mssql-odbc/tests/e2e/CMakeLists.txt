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
# Driver path for self-contained test registration
# ---------------------------------------------------------------------------
# When MSODBCSQL18_DRIVER_PATH is set, ODBCTest::SetUpTestSuite() will
# create a temporary odbcinst.ini so tests can run without run_e2e.sh.
if(NOT DEFINED MSODBCSQL18_DRIVER_PATH)
    if(WIN32)
        set(MSODBCSQL18_DRIVER_PATH "${CMAKE_CURRENT_SOURCE_DIR}/../../target/debug/msodbcsql18.dll")
    elseif(APPLE)
        set(MSODBCSQL18_DRIVER_PATH "${CMAKE_CURRENT_SOURCE_DIR}/../../target/debug/libmsodbcsql18.dylib")
    else()
        set(MSODBCSQL18_DRIVER_PATH "${CMAKE_CURRENT_SOURCE_DIR}/../../target/debug/libmsodbcsql18.so")
    endif()
endif()
# Resolve to absolute so the path works regardless of cwd at runtime.
get_filename_component(MSODBCSQL18_DRIVER_PATH "${MSODBCSQL18_DRIVER_PATH}" ABSOLUTE)
message(STATUS "MSODBCSQL18_DRIVER_PATH=${MSODBCSQL18_DRIVER_PATH}")
target_compile_definitions(odbc_test_lib PUBLIC
    MSODBCSQL18_DRIVER_PATH="${MSODBCSQL18_DRIVER_PATH}"
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

add_odbc_test(smoke_test      tests/smoke_test.cpp)
add_odbc_test(alloc_env_test  tests/alloc_env_test.cpp)

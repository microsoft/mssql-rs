// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// trace_rotation_test  –  Windows-only end-to-end guard for AB#48091.
//
// Covers the file trace sink's bounded-rotation policy against the real driver
// binary rather than against the Rust unit-test seam:
//
//   * an active trace file rolls over once it reaches
//     MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB, moving to numbered successors;
//   * every rolled-over file is retained. The driver has no retention policy,
//     no maximum file count, and no age-based cleanup, so nothing it writes may
//     ever disappear;
//   * a pre-existing trace file belonging to another process is never touched,
//     including one whose last-write time is far in the past.
//
// The last two are the load-bearing assertions. A future "helpful" cleanup pass
// would be invisible to the unit tests that drive `TraceFileWriter` directly,
// because it would live in the initialization path that only runs when the
// driver is actually loaded.
//
// Deliberately bypasses the Driver Manager and the shared fixture, matching
// dll_unload_stress_test: tracing is configured once per driver load from the
// environment, so the test needs to own the load itself. No SQL Server is
// required — handle allocation alone emits plenty of trace volume.
//
// Env:
//   MSSQL_ODBC_DLL   Path to the driver under test. Set by run_e2e.ps1 for the
//                    mssql-odbc leg only, so this skips on the reference leg
//                    (parity-neutral). msodbcsql does rotate — BIDTraceFileSize
//                    in odbcinst.ini — but it is configured through the ini
//                    file rather than the environment, so this harness cannot
//                    drive it. See docs/parity-deviations.md.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <sql.h>
#include <sqlext.h>

#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <string>
#include <vector>

namespace {

using SQLAllocHandleFn = SQLRETURN(SQL_API*)(SQLSMALLINT, SQLHANDLE, SQLHANDLE*);
using SQLSetEnvAttrFn = SQLRETURN(SQL_API*)(SQLHENV, SQLINTEGER, SQLPOINTER, SQLINTEGER);
using SQLFreeHandleFn = SQLRETURN(SQL_API*)(SQLSMALLINT, SQLHANDLE);

class ScopedEnvironmentVariable {
   public:
    ScopedEnvironmentVariable(const char* name, const char* value) : name_(name) {
        const DWORD length = GetEnvironmentVariableA(name, nullptr, 0);
        if (length != 0) {
            previous_.resize(length);
            GetEnvironmentVariableA(name, previous_.data(), length);
            previous_.resize(length - 1);
            existed_ = true;
        }
        SetEnvironmentVariableA(name, value);
    }

    ~ScopedEnvironmentVariable() {
        SetEnvironmentVariableA(name_.c_str(), existed_ ? previous_.c_str() : nullptr);
    }

   private:
    std::string name_;
    std::string previous_;
    bool existed_ = false;
};

std::string GetEnvOr(const char* name, const char* fallback) {
    char* buf = nullptr;
    size_t len = 0;
    if (_dupenv_s(&buf, &len, name) == 0 && buf != nullptr) {
        std::string value(buf);
        free(buf);
        if (!value.empty()) {
            return value;
        }
    }
    return std::string(fallback);
}

std::filesystem::path MakeTraceDirectory(const char* label) {
    const std::filesystem::path dir =
        std::filesystem::temp_directory_path() /
        ("mssqlodbc-" + std::string(label) + "-" + std::to_string(GetCurrentProcessId()));
    std::filesystem::remove_all(dir);
    std::filesystem::create_directory(dir);
    return dir;
}

std::vector<std::filesystem::path> TraceFilesIn(const std::filesystem::path& dir) {
    std::vector<std::filesystem::path> files;
    for (const auto& entry : std::filesystem::directory_iterator(dir)) {
        if (entry.is_regular_file()) {
            files.push_back(entry.path());
        }
    }
    std::sort(files.begin(), files.end());
    return files;
}

// Emits trace volume without needing a server. Every exported entry point logs
// on entry and on return, and ENV allocation adds its own records, so a few
// thousand cycles comfortably exceeds a 1 MiB rollover threshold.
//
// The outer ENV is held open for the duration: with no live environment the sink
// reopens the file per event, which is correct but far slower.
void GenerateTraceVolume(HMODULE driver, int cycles) {
    auto alloc_handle = reinterpret_cast<SQLAllocHandleFn>(GetProcAddress(driver, "SQLAllocHandle"));
    auto set_env_attr = reinterpret_cast<SQLSetEnvAttrFn>(GetProcAddress(driver, "SQLSetEnvAttr"));
    auto free_handle = reinterpret_cast<SQLFreeHandleFn>(GetProcAddress(driver, "SQLFreeHandle"));
    ASSERT_TRUE(alloc_handle && set_env_attr && free_handle)
        << "driver is missing a required export";

    SQLHANDLE env = SQL_NULL_HANDLE;
    ASSERT_TRUE(SQL_SUCCEEDED(alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env)));
    ASSERT_TRUE(SQL_SUCCEEDED(set_env_attr(
        env, SQL_ATTR_ODBC_VERSION, reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0)));

    for (int i = 0; i < cycles; ++i) {
        SQLHANDLE dbc = SQL_NULL_HANDLE;
        if (SQL_SUCCEEDED(alloc_handle(SQL_HANDLE_DBC, env, &dbc))) {
            free_handle(SQL_HANDLE_DBC, dbc);
        }
        // Rejected on purpose: the error path is logged too, and it costs no
        // connection state.
        alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, nullptr);
    }

    ASSERT_TRUE(SQL_SUCCEEDED(free_handle(SQL_HANDLE_ENV, env)));
}

TEST(TraceRotation, RollsOverBySizeAndRetainsEveryFile) {
    const std::string dll_path = GetEnvOr("MSSQL_ODBC_DLL", "");
    if (dll_path.empty()) {
        GTEST_SKIP() << "MSSQL_ODBC_DLL is not set; skipping the trace rotation test";
    }

    const std::filesystem::path trace_dir = MakeTraceDirectory("trace-rotation");

    ScopedEnvironmentVariable trace_enabled("MSSQL_TDS_TRACE", "true");
    ScopedEnvironmentVariable trace_level("MSSQL_TDS_TRACE_LEVEL", "trace");
    ScopedEnvironmentVariable trace_directory("MSSQL_TDS_TRACE_DIR", trace_dir.string().c_str());
    ScopedEnvironmentVariable trace_size("MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB", "1");

    HMODULE driver = LoadLibraryA(dll_path.c_str());
    ASSERT_NE(driver, nullptr) << "LoadLibraryA(" << dll_path << ") failed with " << GetLastError();
    ASSERT_NO_FATAL_FAILURE(GenerateTraceVolume(driver, 8000));
    ASSERT_NE(FreeLibrary(driver), 0) << "FreeLibrary failed with " << GetLastError();

    const std::vector<std::filesystem::path> files = TraceFilesIn(trace_dir);

    // More than the five files the superseded count-capped policy would have
    // kept, so a reintroduced cap fails here rather than passing silently.
    ASSERT_GE(files.size(), 6u)
        << "expected repeated rollover at a 1 MiB limit; got " << files.size() << " file(s)";

    // Writing starts in `<stem>.log` and each rollover moves to `<stem>.<n>.log`,
    // so the base is the shortest name. It is NOT the lexicographic first: '1'
    // sorts below 'l', which puts `<stem>.1.log` ahead of `<stem>.log`.
    const std::filesystem::path base = *std::min_element(
        files.begin(), files.end(), [](const auto& left, const auto& right) {
            return left.filename().string().size() < right.filename().string().size();
        });
    const std::string base_stem = base.stem().string();

    std::vector<std::string> actual_names;
    for (const auto& file : files) {
        actual_names.push_back(file.filename().string());
    }
    std::sort(actual_names.begin(), actual_names.end());

    std::vector<std::string> expected_names{base.filename().string()};
    for (size_t index = 1; index < files.size(); ++index) {
        expected_names.push_back(base_stem + "." + std::to_string(index) + ".log");
    }
    std::sort(expected_names.begin(), expected_names.end());

    EXPECT_EQ(actual_names, expected_names)
        << "rollovers must form a contiguous numbered sequence with nothing removed";

    // One event may carry a file past the threshold, but only by that event. The
    // most recent file is still filling and simply sits under the limit.
    constexpr uintmax_t kLimit = 1ull * 1024 * 1024;
    for (const auto& file : files) {
        EXPECT_LE(std::filesystem::file_size(file), kLimit + 64ull * 1024)
            << file.string() << " grew well past the configured rollover size";
    }

    std::filesystem::remove_all(trace_dir);
}

TEST(TraceRotation, LeavesPreExistingTraceFilesUntouched) {
    const std::string dll_path = GetEnvOr("MSSQL_ODBC_DLL", "");
    if (dll_path.empty()) {
        GTEST_SKIP() << "MSSQL_ODBC_DLL is not set; skipping the trace retention test";
    }

    const std::filesystem::path trace_dir = MakeTraceDirectory("trace-retention");

    // Names that a naive cleanup pass would match: the driver's own prefix, and
    // a rollover-shaped sibling. Both are stamped far enough in the past to trip
    // any age-based policy.
    const std::filesystem::path bystander =
        trace_dir / "mssql_tds_trace_19700101000000000_4242.log";
    const std::filesystem::path bystander_rollover =
        trace_dir / "mssql_tds_trace_19700101000000000_4242.1.log";
    std::filesystem::create_directories(trace_dir);
    for (const auto& path : {bystander, bystander_rollover}) {
        std::ofstream(path) << "written by another process";
        ASSERT_TRUE(std::filesystem::exists(path));
        std::filesystem::last_write_time(
            path, std::filesystem::file_time_type::clock::now() - std::chrono::hours(24 * 3650));
    }

    ScopedEnvironmentVariable trace_enabled("MSSQL_TDS_TRACE", "true");
    ScopedEnvironmentVariable trace_level("MSSQL_TDS_TRACE_LEVEL", "trace");
    ScopedEnvironmentVariable trace_directory("MSSQL_TDS_TRACE_DIR", trace_dir.string().c_str());

    HMODULE driver = LoadLibraryA(dll_path.c_str());
    ASSERT_NE(driver, nullptr) << "LoadLibraryA(" << dll_path << ") failed with " << GetLastError();
    ASSERT_NO_FATAL_FAILURE(GenerateTraceVolume(driver, 64));
    ASSERT_NE(FreeLibrary(driver), 0) << "FreeLibrary failed with " << GetLastError();

    EXPECT_TRUE(std::filesystem::exists(bystander))
        << "the driver deleted a pre-existing trace file";
    EXPECT_TRUE(std::filesystem::exists(bystander_rollover))
        << "the driver deleted a pre-existing rolled-over trace file";
    EXPECT_EQ(std::filesystem::file_size(bystander), 26u)
        << "the driver truncated a pre-existing trace file";

    // Its own file is additional, never a replacement.
    EXPECT_GE(TraceFilesIn(trace_dir).size(), 3u);

    std::filesystem::remove_all(trace_dir);
}

}  // namespace

#else

TEST(TraceRotation, SkippedOnNonWindows) {
    GTEST_SKIP() << "trace_rotation_test drives the Windows loader directly";
}

#endif  // _WIN32

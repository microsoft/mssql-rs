// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include <gtest/gtest.h>

#ifdef _WIN32
#include <windows.h>
#include <filesystem>
#else
#include <dlfcn.h>
#endif

#include <sql.h>
#include <sqlext.h>

#include <algorithm>
#include <cstdlib>
#include <iostream>
#include <string>
#include <unordered_set>
#include <vector>

namespace {

#ifdef _WIN32
using DriverPath = std::filesystem::path;
#else
using DriverPath = std::string;
#endif

struct Handle {
    SQLSMALLINT type;
    SQLHANDLE value;
};

// Own the module and its handles together so assertion failures still clean up
// child-first, including joining the final ENV's runtime before unloading.
class DirectDriver {
   public:
    DirectDriver() = default;
    DirectDriver(const DirectDriver&) = delete;
    DirectDriver& operator=(const DirectDriver&) = delete;

    ~DirectDriver() {
        while (!handles_.empty()) {
            const auto handle = handles_.back();
            const auto rc = Free(handle.type, handle.value);
            if (rc != SQL_SUCCESS) {
                ADD_FAILURE() << "Cleanup failed for handle type " << handle.type << ": " << rc;
                // Keep the module mapped if a failed cleanup left runtime workers alive.
                return;
            }
        }
        if (module_) {
#ifdef _WIN32
            EXPECT_NE(FreeLibrary(module_), 0) << LoadError();
#else
            EXPECT_EQ(dlclose(module_), 0) << LoadError();
#endif
        }
    }

    void Open(const DriverPath& path) {
#ifdef _WIN32
        ASSERT_TRUE(path.is_absolute()) << "MSSQL_ODBC_DRIVER_PATH must be absolute";
        module_ = LoadLibraryW(path.c_str());
#else
        ASSERT_TRUE(!path.empty() && path.front() == '/')
            << "MSSQL_ODBC_DRIVER_PATH must be absolute";
        module_ = dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
#endif
        ASSERT_NE(module_, nullptr) << "Cannot load " << path << ": " << LoadError();
        alloc_handle_ = Resolve<decltype(alloc_handle_)>("SQLAllocHandle");
        free_handle_ = Resolve<decltype(free_handle_)>("SQLFreeHandle");
        set_env_attr = Resolve<decltype(set_env_attr)>("SQLSetEnvAttr");
        get_env_attr = Resolve<decltype(get_env_attr)>("SQLGetEnvAttr");
        get_connect_attr = Resolve<decltype(get_connect_attr)>("SQLGetConnectAttrW");
        get_stmt_attr = Resolve<decltype(get_stmt_attr)>("SQLGetStmtAttrW");
        get_desc_field = Resolve<decltype(get_desc_field)>("SQLGetDescFieldW");
        ASSERT_TRUE(alloc_handle_ && free_handle_ && set_env_attr && get_env_attr &&
                    get_connect_attr && get_stmt_attr && get_desc_field);
    }

    SQLRETURN Alloc(SQLSMALLINT type, SQLHANDLE parent, SQLHANDLE* output) {
        const auto rc = alloc_handle_(type, parent, output);
        if (SQL_SUCCEEDED(rc)) {
            handles_.push_back({type, *output});
        }
        return rc;
    }

    SQLRETURN Free(SQLSMALLINT type, SQLHANDLE value) {
        const auto rc = free_handle_(type, value);
        if (rc == SQL_SUCCESS) {
            const auto it = std::find_if(handles_.begin(), handles_.end(), [=](const Handle& h) {
                return h.type == type && h.value == value;
            });
            if (it != handles_.end()) {
                handles_.erase(it);
            }
        }
        return rc;
    }

    decltype(&SQLSetEnvAttr) set_env_attr = nullptr;
    decltype(&SQLGetEnvAttr) get_env_attr = nullptr;
    decltype(&SQLGetConnectAttrW) get_connect_attr = nullptr;
    decltype(&SQLGetStmtAttrW) get_stmt_attr = nullptr;
    decltype(&SQLGetDescFieldW) get_desc_field = nullptr;

   private:
    static std::string LoadError() {
#ifdef _WIN32
        return std::to_string(GetLastError());
#else
        const char* error = dlerror();
        return error ? error : "unknown loader error";
#endif
    }

    template <typename T>
    T Resolve(const char* name) {
#ifdef _WIN32
        const auto symbol = GetProcAddress(module_, name);
#else
        const auto symbol = dlsym(module_, name);
#endif
        EXPECT_NE(symbol, nullptr) << "Missing export " << name << ": " << LoadError();
        return reinterpret_cast<T>(symbol);
    }

#ifdef _WIN32
    HMODULE module_ = nullptr;
#else
    void* module_ = nullptr;
#endif
    decltype(&SQLAllocHandle) alloc_handle_ = nullptr;
    decltype(&SQLFreeHandle) free_handle_ = nullptr;
    std::vector<Handle> handles_;
};

class HandleIdentityTest : public ::testing::Test {
   protected:
    void SetUp() override {
#ifdef _WIN32
        const wchar_t* path = _wgetenv(L"MSSQL_ODBC_DRIVER_PATH");
#else
        const char* path = std::getenv("MSSQL_ODBC_DRIVER_PATH");
#endif
        ASSERT_TRUE(path && *path)
            << "Set MSSQL_ODBC_DRIVER_PATH to the Rust driver, or use run_e2e.sh/ps1";
        ASSERT_NO_FATAL_FAILURE(driver_.Open(DriverPath(path)));
    }

    void AllocateTree() {
        SQLHANDLE env = SQL_NULL_HANDLE;
        SQLHANDLE dbc = SQL_NULL_HANDLE;
        SQLHANDLE stmt = SQL_NULL_HANDLE;
        SQLHANDLE desc = SQL_NULL_HANDLE;
        ASSERT_EQ(driver_.Alloc(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env), SQL_SUCCESS);
        ASSERT_EQ(driver_.set_env_attr(
                      env, SQL_ATTR_ODBC_VERSION,
                      reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0),
                  SQL_SUCCESS);
        ASSERT_EQ(driver_.Alloc(SQL_HANDLE_DBC, env, &dbc), SQL_SUCCESS);
        // The DM normally enforces a connected DBC; direct driver allocation does not.
        ASSERT_EQ(driver_.Alloc(SQL_HANDLE_STMT, dbc, &stmt), SQL_SUCCESS);
        ASSERT_EQ(driver_.Alloc(SQL_HANDLE_DESC, dbc, &desc), SQL_SUCCESS);
        current_ = {{SQL_HANDLE_ENV, env}, {SQL_HANDLE_DBC, dbc},
                    {SQL_HANDLE_STMT, stmt}, {SQL_HANDLE_DESC, desc}};
        for (const auto attr : {SQL_ATTR_APP_ROW_DESC, SQL_ATTR_APP_PARAM_DESC,
                                SQL_ATTR_IMP_ROW_DESC, SQL_ATTR_IMP_PARAM_DESC}) {
            SQLHDESC implicit = SQL_NULL_HDESC;
            ASSERT_EQ(driver_.get_stmt_attr(stmt, attr, &implicit, sizeof(implicit), nullptr),
                      SQL_SUCCESS);
            current_.push_back({SQL_HANDLE_DESC, implicit});
        }
        for (const auto& handle : current_) {
            ASSERT_NE(handle.value, nullptr);
        }
    }

    void ExpectReadable(const Handle& handle, SQLRETURN expected) {
        SCOPED_TRACE(::testing::Message() << "type " << handle.type << ", ID " << handle.value);
        switch (handle.type) {
            case SQL_HANDLE_ENV: {
                SQLUINTEGER version = 0;
                EXPECT_EQ(driver_.get_env_attr(handle.value, SQL_ATTR_ODBC_VERSION, &version,
                                              sizeof(version), nullptr),
                          expected);
                EXPECT_EQ(version, expected == SQL_SUCCESS ? SQL_OV_ODBC3_80 : 0u);
                break;
            }
            case SQL_HANDLE_DBC: {
                SQLUINTEGER timeout = 0;
                EXPECT_EQ(driver_.get_connect_attr(handle.value, SQL_ATTR_LOGIN_TIMEOUT, &timeout,
                                                  sizeof(timeout), nullptr),
                          expected);
                break;
            }
            case SQL_HANDLE_STMT: {
                SQLULEN rows = 0;
                EXPECT_EQ(driver_.get_stmt_attr(handle.value, SQL_ATTR_ROW_ARRAY_SIZE, &rows,
                                               sizeof(rows), nullptr),
                          expected);
                EXPECT_EQ(rows, expected == SQL_SUCCESS ? 1u : 0u);
                break;
            }
            case SQL_HANDLE_DESC: {
                SQLSMALLINT count = -1;
                EXPECT_EQ(driver_.get_desc_field(handle.value, 0, SQL_DESC_COUNT, &count,
                                                sizeof(count), nullptr),
                          expected);
                EXPECT_EQ(count, expected == SQL_SUCCESS ? 0 : -1);
                break;
            }
            default:
                FAIL() << "Unexpected test handle type";
        }
    }

    DirectDriver driver_;
    std::vector<Handle> current_;
};

TEST_F(HandleIdentityTest, RetiredIdsAreRejectedBeforeNamespaceReuse) {
    std::unordered_set<SQLHANDLE> issued;
    std::vector<Handle> retired;
    for (int generation = 0; generation < 4; ++generation) {
        SCOPED_TRACE(generation);
        ASSERT_NO_FATAL_FAILURE(AllocateTree());
        for (const auto& handle : current_) {
            ASSERT_TRUE(issued.insert(handle.value).second)
                << "Driver reused ID " << handle.value << " for type " << handle.type;
        }
        for (const auto& stale : retired) {
            ExpectReadable(stale, SQL_INVALID_HANDLE);
            // Missing STMT/DESC frees are successful no-ops for DM compatibility.
            const auto expected = stale.type == SQL_HANDLE_STMT || stale.type == SQL_HANDLE_DESC
                                      ? SQL_SUCCESS : SQL_INVALID_HANDLE;
            EXPECT_EQ(driver_.Free(stale.type, stale.value), expected);
        }
        for (const auto& live : current_) {
            ExpectReadable(live, SQL_SUCCESS);
        }

        ASSERT_EQ(driver_.Free(SQL_HANDLE_DESC, current_[3].value), SQL_SUCCESS);
        ASSERT_EQ(driver_.Free(SQL_HANDLE_STMT, current_[2].value), SQL_SUCCESS);
        // All four implicit descriptors retire with the STMT, not with the DBC.
        for (size_t i = 4; i < current_.size(); ++i) {
            ExpectReadable(current_[i], SQL_INVALID_HANDLE);
        }
        ASSERT_EQ(driver_.Free(SQL_HANDLE_DBC, current_[1].value), SQL_SUCCESS);
        ASSERT_EQ(driver_.Free(SQL_HANDLE_ENV, current_[0].value), SQL_SUCCESS);
        retired.insert(retired.end(), current_.begin(), current_.end());
        for (const auto& stale : retired) {
            ExpectReadable(stale, SQL_INVALID_HANDLE);
        }
    }
}

TEST_F(HandleIdentityTest, NullAndWrongTypesDoNotRetireLiveIds) {
    ASSERT_NO_FATAL_FAILURE(AllocateTree());
    for (const SQLSMALLINT type : {SQL_HANDLE_ENV, SQL_HANDLE_DBC, SQL_HANDLE_STMT, SQL_HANDLE_DESC}) {
        ExpectReadable({type, SQL_NULL_HANDLE}, SQL_INVALID_HANDLE);
        EXPECT_EQ(driver_.Free(type, SQL_NULL_HANDLE), SQL_INVALID_HANDLE);
        for (const auto& live : current_) {
            if (live.type != type) {
                ExpectReadable({type, live.value}, SQL_INVALID_HANDLE);
                EXPECT_EQ(driver_.Free(type, live.value), SQL_INVALID_HANDLE);
            }
        }
    }
    for (const auto& live : current_) {
        ExpectReadable(live, SQL_SUCCESS);
    }
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    const char* target = std::getenv("ODBC_TEST_TARGET");
    if (target && std::string(target) == "msodbcsql") {
        std::cout << "Rust driver ID recycling policy is internal; not compared with msodbcsql.\n";
        return 77;
    }
    return RUN_ALL_TESTS();
}

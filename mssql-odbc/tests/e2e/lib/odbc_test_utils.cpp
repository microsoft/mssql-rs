// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_test_utils.cpp  –  Diagnostic and connection-string helpers.

#include "odbc_test_fixture.h"
#include <algorithm>
#include <limits>
#include <sstream>
#include <stdexcept>

#ifndef _WIN32
#include <cerrno>
#include <cctype>
#include <iconv.h>
#include <langinfo.h>

namespace {

std::string NativeCodeset() {
    const char* codeset = nl_langinfo(CODESET);
    std::string name;
    for (const unsigned char ch : std::string(codeset == nullptr ? "" : codeset)) {
        name += static_cast<char>(std::toupper(ch));
    }
    if (name == "UTF8" || name == "UTF-8") {
        return "UTF-8";
    }
    if (name == "UTF-32LE") {
        return "UTF-32LE";
    }
    if (name == "BIG5" || name == "BIG5-HKSCS") {
        return "CP950";
    }
    if (name == "GB2312" || name == "GBK") {
        return "CP936";
    }
    for (const char* supported : {
             "GB18030", "CP437", "CP850", "CP874", "CP932", "CP936", "CP949", "CP950",
             "CP1250", "CP1251", "CP1252", "CP1253", "CP1254", "CP1255", "CP1256", "CP1257", "CP1258"}) {
        if (name == supported) {
            return name;
        }
    }
    for (const int part : {1, 2, 3, 4, 5, 6, 7, 8, 9, 13, 15}) {
        for (const char* prefix : {
                 "ISO-8859-", "8859_", "ISO8859-", "ISO8859", "ISO_8859-", "ISO_8859_"}) {
            if (name == prefix + std::to_string(part)) {
                return "ISO8859-" + std::to_string(part);
            }
        }
    }
    // C/POSIX's ASCII codeset and unsupported locales use the UTF-8 fallback.
    return "UTF-8";
}

std::string ConvertNativeTestText(const std::string& utf8, const std::string& encoding,
                                  bool allowReplacement, bool* usedDefault) {
    if (utf8.size() > (std::string{}.max_size() - 16) / 4) {
        throw std::length_error("UTF-8 test value exceeds conversion buffer limit");
    }
    std::string result(utf8.size() * 4 + 16, '\0');
    struct Converter {
        iconv_t value;
        ~Converter() {
            if (value != reinterpret_cast<iconv_t>(-1)) {
                iconv_close(value);
            }
        }
    } converter{iconv_open(encoding.c_str(), "UTF-8")};
    if (converter.value == reinterpret_cast<iconv_t>(-1)) {
        throw std::runtime_error("iconv_open failed for " + encoding);
    }
    char* input = const_cast<char*>(utf8.data());
    size_t inputLeft = utf8.size();
    char* output = result.data();
    size_t outputLeft = result.size();
    while (inputLeft != 0) {
        const size_t converted = iconv(converter.value, &input, &inputLeft, &output, &outputLeft);
        if (converted != static_cast<size_t>(-1)) {
            if (usedDefault != nullptr && converted != 0) {
                *usedDefault = true;
            }
            break;
        }
        if (errno != EILSEQ || !allowReplacement) {
            throw std::runtime_error("iconv native test conversion failed: " +
                                     std::to_string(errno));
        }
        // Input was validated before native conversion. If transliteration
        // fails, the native converter substitutes once per UTF-16 code unit.
        const unsigned char lead = static_cast<unsigned char>(*input);
        const size_t sourceBytes = lead < 0x80 ? 1 : lead < 0xE0 ? 2 : lead < 0xF0 ? 3 : 4;
        const size_t replacements = sourceBytes == 4 ? 2 : 1;
        if (sourceBytes > inputLeft || replacements > outputLeft) {
            throw std::runtime_error("iconv substitution exceeds test buffer");
        }
        std::fill_n(output, replacements, '?');
        input += sourceBytes;
        inputLeft -= sourceBytes;
        output += replacements;
        outputLeft -= replacements;
        if (usedDefault != nullptr) {
            *usedDefault = true;
        }
    }
    const size_t flushed = iconv(converter.value, nullptr, nullptr, &output, &outputLeft);
    if (flushed == static_cast<size_t>(-1)) {
        throw std::runtime_error("iconv native test flush failed: " + std::to_string(errno));
    }
    if (usedDefault != nullptr && flushed != 0) {
        *usedDefault = true;
    }
    result.resize(result.size() - outputLeft);
    return result;
}

}  // namespace
#endif

// ---------------------------------------------------------------------------
// ODBCTestUtils
// ---------------------------------------------------------------------------

std::string ODBCTestUtils::GetDiagState(SQLSMALLINT handleType,
                                        SQLHANDLE handle) {
    SQLTCHAR state[8] = {};
    SQLINTEGER nativeErr = 0;
    SQLTCHAR msg[512] = {};
    SQLSMALLINT msgLen = 0;

    SQLRETURN rc = SQLGetDiagRec(handleType, handle, 1, state, &nativeErr,
                                 msg,
                                 static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)),
                                 &msgLen);
    if (SQL_SUCCEEDED(rc)) {
        return ToNarrow(SqlTString(state));
    }
    return "";
}

bool ODBCTestUtils::HasDiagState(SQLSMALLINT handleType, SQLHANDLE handle,
                                 const std::string& target) {
    SQLTCHAR state[8] = {};
    SQLINTEGER nativeErr = 0;
    SQLTCHAR msg[512] = {};
    SQLSMALLINT msgLen = 0;

    for (SQLSMALLINT recNum = 1; ; recNum++) {
        SQLRETURN rc = SQLGetDiagRec(handleType, handle, recNum, state, &nativeErr,
                                     msg,
                                     static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)),
                                     &msgLen);
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        if (ToNarrow(SqlTString(state)) == target) {
            return true;
        }
    }
    return false;
}

std::string ODBCTestUtils::GetDiagMessage(SQLSMALLINT handleType,
                                          SQLHANDLE handle) {
    SQLTCHAR state[8] = {};
    SQLINTEGER nativeErr = 0;
    SQLTCHAR msg[1024] = {};
    SQLSMALLINT msgLen = 0;
    std::ostringstream oss;
    bool found = false;

    for (SQLSMALLINT recNum = 1; ; recNum++) {
        SQLRETURN rc = SQLGetDiagRec(handleType, handle, recNum, state, &nativeErr,
                                     msg,
                                     static_cast<SQLSMALLINT>(sizeof(msg) / sizeof(SQLTCHAR)),
                                     &msgLen);
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        if (found) {
            oss << " | ";
        }
        oss << "[" << ToNarrow(SqlTString(state)) << "] "
            << ToNarrow(SqlTString(msg))
            << " (native=" << nativeErr << ")";
        found = true;
    }
    return found ? oss.str() : "(no diagnostic)";
}

SqlTString ODBCTestUtils::BuildConnectionString() {
    auto& cfg = ODBCTestConfig::Instance();

    // If a full connection string override is provided, use it directly.
    if (cfg.HasConnStr()) {
        return ToSqlTStr(cfg.ConnStr());
    }

    std::ostringstream cs;

    // DSN-based connection  (like LTM tests)
    if (cfg.HasDSN()) {
        cs << "DSN=" << cfg.DSN() << ";";
    } else {
        // DSN-less: specify driver + server
        cs << "Driver={" << cfg.Driver() << "};";
        cs << "Server=" << cfg.Server() << ";";
    }

    cs << "Database=" << cfg.Database() << ";";
    cs << "TrustServerCertificate=" << cfg.TrustCert() << ";";

    if (!cfg.Encrypt().empty()) {
        cs << "Encrypt=" << cfg.Encrypt() << ";";
    }

    if (cfg.HasCredentials()) {
        cs << "Uid=" << cfg.Uid() << ";";
        cs << "Pwd=" << cfg.Pwd() << ";";
    } else {
        // Windows integrated auth
        cs << "Trusted_Connection=Yes;";
    }

    return ToSqlTStr(cs.str());
}

SqlTString ODBCTestUtils::ToSqlTStr(const std::string& s) {
    return SqlTString(s.begin(), s.end());
}

std::string ODBCTestUtils::ToNarrow(const SqlTString& s) {
    return std::string(s.begin(), s.end());
}

std::string ODBCTestUtils::Utf8ToNativeClient(const std::string& utf8,
                                             bool* usedDefault) {
    if (usedDefault != nullptr) {
        *usedDefault = false;
    }
    if (utf8.empty()) {
        return {};
    }
#ifdef _WIN32
    if (utf8.size() > static_cast<size_t>((std::numeric_limits<int>::max)())) {
        throw std::length_error("UTF-8 test value exceeds Windows conversion limit");
    }
    const int inputSize = static_cast<int>(utf8.size());
    const int wideSize = MultiByteToWideChar(
        CP_UTF8, MB_ERR_INVALID_CHARS, utf8.data(), inputSize, nullptr, 0);
    if (wideSize == 0) {
        throw std::runtime_error("MultiByteToWideChar sizing failed: " +
                                 std::to_string(GetLastError()));
    }
    std::wstring wide(wideSize, L'\0');
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8.data(),
                           inputSize, wide.data(), wideSize) != wideSize) {
        throw std::runtime_error("MultiByteToWideChar conversion failed: " +
                                 std::to_string(GetLastError()));
    }
    const UINT codepage = GetACP();
    BOOL substituted = FALSE;
    BOOL* loss = codepage == CP_UTF8 ? nullptr : &substituted;
    const int byteSize = WideCharToMultiByte(
        codepage, 0, wide.data(), wideSize, nullptr, 0, nullptr, loss);
    if (byteSize == 0) {
        throw std::runtime_error("WideCharToMultiByte sizing failed: " +
                                 std::to_string(GetLastError()));
    }
    std::string result(byteSize, '\0');
    if (WideCharToMultiByte(codepage, 0, wide.data(), wideSize, result.data(),
                            byteSize, nullptr, loss) != byteSize) {
        throw std::runtime_error("WideCharToMultiByte conversion failed: " +
                                 std::to_string(GetLastError()));
    }
    if (usedDefault != nullptr) {
        *usedDefault = substituted != FALSE;
    }
    return result;
#else
    // No setlocale call: snapshot the active codeset once, like the driver.
    // The test runners normally leave LC_CTYPE=C, which the driver treats as UTF-8.
    const auto validated = ConvertNativeTestText(utf8, "UTF-8", false, nullptr);
    static const auto nativeEncoding = NativeCodeset();
    auto encoding = nativeEncoding;
    if (encoding == "UTF-8") {
        return validated;
    }
#if defined(__GLIBC__) || defined(__APPLE__)
    if (encoding != "UTF-32LE") {
        encoding += "//TRANSLIT";
    }
#endif
    return ConvertNativeTestText(validated, encoding, true, usedDefault);
#endif
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#include "odbc_test_fixture.h"

#include <algorithm>
#include <cstring>
#include <string>
#include <vector>

namespace Cp1252TestData {

struct Value {
    const char* name;
    std::string hex;
    std::vector<SQLWCHAR> units;
    std::vector<unsigned char> bytes;
};

inline constexpr const char* CodePageExpression =
    "CONVERT(int, COLLATIONPROPERTY(CONVERT(nvarchar(128), "
    "SQL_VARIANT_PROPERTY(COALESCE(v, ''), 'Collation')), 'CodePage'))";

inline Value FromBytes(const char* name, const std::vector<unsigned char>& bytes) {
    // Undefined CP1252 bytes retain their C1 code points on both platforms.
    constexpr SQLWCHAR c1[] = {
        0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021,
        0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F,
        0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
        0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
    };
    constexpr char digits[] = "0123456789ABCDEF";
    Value result{name, "0x", {}, bytes};
    for (unsigned char byte : bytes) {
        result.hex += digits[byte >> 4];
        result.hex += digits[byte & 15];
        result.units.push_back(byte >= 0x80 && byte <= 0x9F ? c1[byte - 0x80] : byte);
    }
    return result;
}

inline const std::vector<Value>& Values() {
    static const auto values = [] {
        std::vector<unsigned char> all(256);
        for (size_t i = 0; i < all.size(); ++i) all[i] = static_cast<unsigned char>(i);
        return std::vector<Value>{
            FromBytes("all256", all),
            FromBytes("ascii", {0x41, 0x42, 0x43}),
            FromBytes("high", {0x80, 0x81, 0x8D, 0x91, 0x92, 0x93, 0x94, 0xE9, 0xFF}),
            FromBytes("utf8_bom", {0xEF, 0xBB, 0xBF, 0x41}),
            FromBytes("utf16_bom", {0xFF, 0xFE}),
            FromBytes("reversed_bom", {0xFE, 0xFF}),
            FromBytes("embedded_trailing_nul", {0x41, 0, 0x80, 0, 0}),
            FromBytes("empty", {}),
            {"null", "NULL", {}, {}},
        };
    }();
    return values;
}

inline void CheckBytes(const std::vector<unsigned char>& buffer, size_t offset,
                       size_t capacity, const std::vector<SQLWCHAR>& units) {
    static_assert(sizeof(SQLWCHAR) == 2);
    std::vector<unsigned char> expected(buffer.size(), 0xCC);
    if (capacity >= sizeof(SQLWCHAR)) {
        const size_t count = (std::min)(units.size(), capacity / sizeof(SQLWCHAR) - 1);
        if (count != 0) {
            std::memcpy(expected.data() + offset, units.data(), count * sizeof(SQLWCHAR));
        }
        const SQLWCHAR terminator = 0;
        std::memcpy(expected.data() + offset + count * sizeof(SQLWCHAR),
                    &terminator, sizeof(terminator));
    }
    EXPECT_EQ(expected, buffer);
}

}  // namespace Cp1252TestData

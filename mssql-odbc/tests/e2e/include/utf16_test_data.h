// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#include "odbc_test_fixture.h"

#include <string>
#include <vector>

namespace Utf16TestData {

struct Value {
    const char* name;
    const char* hex;
    std::vector<SQLWCHAR> units;
};

inline const std::vector<Value>& Values() {
    static const std::vector<Value> values = {
        {"high", "0x00D8", {0xD800}},
        {"low", "0x00DC", {0xDC00}},
        {"pair", "0x3DD800DE", {0xD83D, 0xDE00}},
        {"bom", "0xFFFE", {0xFEFF}},
        {"reversed_bom", "0xFEFF", {0xFFFE}},
        {"embedded_nul", "0x410000004200", {0x0041, 0, 0x0042}},
        {"trailing_nul", "0x41000000", {0x0041, 0}},
        {"empty", "0x", {}},
        {"null", "NULL", {}},
        {"ordinary", "0x4100E900", {0x0041, 0x00E9}},
        {"isolated_units", "0x00D8410000DC", {0xD800, 0x0041, 0xDC00}},
    };
    return values;
}

inline std::vector<SQLWCHAR> Expected(const Value& value, bool fixed) {
    auto units = value.units;
    if (fixed && std::string(value.hex) != "NULL") {
        units.resize(8, 0x0020);
    }
    return units;
}

}  // namespace Utf16TestData

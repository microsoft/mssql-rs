// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "lob_chunk.hpp"

#include <algorithm>
#include <iostream>
#include <stdexcept>
#include <vector>

namespace {

using mssql::odbc::bench::lob_chunk_payload_bytes;

void require(bool condition, const char* message) {
    if (!condition) {
        throw std::runtime_error(message);
    }
}

void test_terminators() {
    const unsigned char narrow[] = {'a', 'b', 0, 'x', 'y', 0};
    require(lob_chunk_payload_bytes(narrow, sizeof(narrow), 1) == 2,
            "short narrow chunk included stale bytes");
    const unsigned char wide[] = {0, 1, 'a', 0, 0, 0, 'x', 0};
    require(lob_chunk_payload_bytes(wide, sizeof(wide), 2) == 4,
            "wide chunk did not stop at the first complete NUL unit");
    const unsigned char empty[] = {0, 0, 'x', 'y'};
    require(lob_chunk_payload_bytes(empty, sizeof(empty), 1) == 0,
            "empty narrow chunk included stale bytes");
    require(lob_chunk_payload_bytes(empty, sizeof(empty), 2) == 0,
            "empty wide chunk included stale bytes");

    for (std::size_t unit : {1u, 2u}) {
        std::vector<unsigned char> full(8192, 'a');
        std::fill(full.end() - unit, full.end(), 0);
        require(lob_chunk_payload_bytes(full.data(), full.size(), unit) == 8192 - unit,
                "full chunk lost payload bytes");
        bool rejected = false;
        try {
            lob_chunk_payload_bytes(full.data(), full.size() - unit, unit);
        } catch (const std::runtime_error&) {
            rejected = true;
        }
        require(rejected, "unterminated chunk was accepted");
    }
}

void test_reused_buffer() {
    std::vector<unsigned char> chunk(8192, 0);
    const auto drain = [&](const std::vector<unsigned char>& value, std::size_t unit,
                           std::size_t read_limit, bool clear_buffer) {
        std::vector<unsigned char> delivered;
        for (std::size_t offset = 0; offset < value.size();) {
            const auto count = std::min(read_limit, value.size() - offset);
            if (clear_buffer) {
                std::fill(chunk.begin(), chunk.end(), 0);
            }
            std::copy_n(value.data() + offset, count, chunk.data());
            std::fill_n(chunk.data() + count, unit, 0);
            const auto payload = lob_chunk_payload_bytes(chunk.data(), chunk.size(), unit);
            delivered.insert(delivered.end(), chunk.begin(), chunk.begin() + payload);
            offset += count;
        }
        require(delivered == value, "reused LOB buffer changed the generated value");
    };

    // Row 557 leaves 2734 bytes from NVARCHAR before VARCHAR writes 2730 bytes.
    // Backward trimming counts the old bytes beyond the new terminator.
    for (std::size_t row = 1; row <= 1000; ++row) {
        if ((row + 1) % 7 != 0) {
            std::vector<unsigned char> wide;
            for (std::size_t i = 0; i < 9000 + row % 1000; ++i) {
                wide.push_back(static_cast<unsigned char>('a' + i % 10));
                wide.push_back(0);
            }
            drain(wide, 2, 8190, true);
        }
        if ((row + 2) % 7 != 0) {
            std::vector<unsigned char> narrow;
            for (std::size_t i = 0; i < 20000 + row % 1000; ++i) {
                narrow.push_back(static_cast<unsigned char>('a' + i % 10));
            }
            drain(narrow, 1, 2730, false);
        }
    }
}

}  // namespace

int main() {
    try {
        test_terminators();
        test_reused_buffer();
    } catch (const std::exception& error) {
        std::cerr << error.what() << '\n';
        return 1;
    }
    return 0;
}

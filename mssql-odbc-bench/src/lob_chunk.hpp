// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#include <algorithm>
#include <cstddef>
#include <stdexcept>

namespace mssql::odbc::bench {

// Generated LOB text has no embedded NUL. Ignore stale bytes after the current
// terminator: a transcoding SQLGetData call need not fill the output buffer.
inline std::size_t lob_chunk_payload_bytes(const unsigned char* chunk,
                                          std::size_t capacity,
                                          std::size_t unit_bytes) {
    for (std::size_t offset = 0; offset + unit_bytes <= capacity; offset += unit_bytes) {
        if (std::all_of(chunk + offset, chunk + offset + unit_bytes,
                        [](unsigned char byte) { return byte == 0; })) {
            return offset;
        }
    }
    throw std::runtime_error("SQLGetData returned LOB text without a terminator");
}

}  // namespace mssql::odbc::bench

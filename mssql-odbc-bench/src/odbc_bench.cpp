// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_bench.hpp"

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cctype>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iomanip>
#include <iostream>
#include <iterator>
#include <limits>
#include <sstream>
#include <stdexcept>
#include <string_view>
#include <utility>
#include <vector>

#ifndef SQL_OV_ODBC3_80
#define SQL_OV_ODBC3_80 380UL
#endif

namespace mssql::odbc::bench {
namespace {

constexpr SQLSMALLINT kSqlCSSsTime2 = 0x4000;
constexpr SQLSMALLINT kSqlCSSsTimestampOffset = 0x4001;
// SQL Server ODBC extension descriptor field carrying a sql_variant value's
// underlying C type. mssql-python is the only consumer that asks for it, and it
// is the sole SQLColAttribute field on its fetch path.
constexpr SQLUSMALLINT kSqlCaSsVariantType = 1215;
constexpr std::size_t kPatternSize = 15;
constexpr SQLLEN kIndicatorSentinel = -777;
// The encrypted Linux and Windows labs confirmed that Microsoft ODBC 18.6.2.1,
// the candidate, and the pinned Rust baseline all honor and report 16192 exactly.
constexpr char kDefaultPacketSize[] = "16192";
// One NULL in every seven rows for a nullable column, offset per column so no two
// columns go NULL on the same rows. Realistic sparsity without making the payload
// dominated by NULLs.
constexpr std::uint64_t kNullPeriod = 7;
// Repeating source text for every generated string. A pure function of position,
// so a delivered value can be checked without storing an expected copy.
constexpr std::string_view kTextCycle = "abcdefghij";

// Mirror the Microsoft SQL Server ODBC extension ABI without depending on
// platform-specific copies of sqlncli.h.
struct SqlSsTime2 {
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
};

struct SqlSsTimestampOffset {
    SQLSMALLINT year;
    SQLUSMALLINT month;
    SQLUSMALLINT day;
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
    SQLSMALLINT timezone_hour;
    SQLSMALLINT timezone_minute;
};

struct SqlGuid {
    SQLUINTEGER data1;
    SQLUSMALLINT data2;
    SQLUSMALLINT data3;
    SQLCHAR data4[8];
};

// These sizes are interoperability checks: a mismatch would make bound rows
// incomparable across the Rust and Microsoft drivers.
static_assert(sizeof(SqlSsTime2) == 12);
static_assert(sizeof(SqlSsTimestampOffset) == 20);
static_assert(sizeof(SqlGuid) == 16);

// Selects the semantic validator independently from the ODBC C storage type.
enum class ValueKind {
    bit,
    tinyint,
    smallint,
    integer,
    bigint,
    real,
    double_precision,
    decimal,
    date,
    time,
    datetime2,
    datetimeoffset,
    guid,
    character,
    wide_character,
    // Inline nullable variable-width text. Length and NULL placement are both
    // functions of the row id, so the indicator is checked, not just the bytes.
    var_character,
    var_wide_character,
    // PLP columns, never bound: delivered by repeated 8192-byte SQLGetData calls.
    lob_character,
    lob_wide_character,
    // sql_variant values reached through the probe plus SQLColAttribute.
    variant_integer,
    variant_bigint,
    variant_text,
};

// True for the shapes that must never be bound. mssql-odbc answers a bound PLP
// column with SQL_ROW_ERROR (AB#47361) and mssql-python never binds one either,
// so routing them through SQLGetData is both the supported and the realistic path.
bool is_lob_kind(ValueKind kind) {
    return kind == ValueKind::lob_character || kind == ValueKind::lob_wide_character;
}

// True for the shapes that need the zero-length SQL_C_BINARY probe first.
bool is_variant_kind(ValueKind kind) {
    return kind == ValueKind::variant_integer || kind == ValueKind::variant_bigint ||
           kind == ValueKind::variant_text;
}

// Couples one generated SQL column to everything that must agree about it:
// its DDL, its deterministic value generator, the C type used to read it, the
// buffer stride, and the validator. Keeping them on one record is what stops
// setup and measurement from drifting apart.
struct ColumnSpec {
    std::string name;
    ValueKind kind;
    std::string sql_type;
    std::string sql_expression;
    // C type used by SQLBindCol and SQLGetData alike. SQL_C_DEFAULT is never
    // used: mssql-odbc rejects it at bind time (HY003) and no consumer sends it.
    SQLSMALLINT c_type = SQL_C_CHAR;
    std::size_t slot_size = 0;
    // Constant payload width for the fixed shapes; 0 when length varies per row.
    std::uint64_t logical_bytes = 0;
    // Exact indicator every row must report, or 0 when it varies.
    SQLLEN expected_indicator = 0;
    // Delivered length for row_id is length_base + (row_id % length_span)
    // characters. length_span == 0 means the column has no generated length.
    std::size_t length_base = 0;
    std::size_t length_span = 0;
    // Bytes per delivered character (1 for narrow, sizeof(SQLWCHAR) for wide).
    std::size_t unit_bytes = 1;
    // 0 for NOT NULL; otherwise the row is NULL when (row_id + phase) % 7 == 0.
    std::uint64_t null_phase = 0;
};

// Read environment values without platform-specific ownership leaking to callers.
std::string environment_value(const char* name) {
#ifdef _WIN32
    char* value = nullptr;
    std::size_t length = 0;
    if (_dupenv_s(&value, &length, name) != 0 || value == nullptr) {
        return {};
    }
    std::string result(value);
    std::free(value);
    return result;
#else
    const char* value = std::getenv(name);
    return value == nullptr ? std::string{} : std::string(value);
#endif
}

// Apply defaults only when the variable is absent or empty.
std::string environment_value_or(const char* name, const char* fallback) {
    auto value = environment_value(name);
    return value.empty() ? std::string(fallback) : value;
}

// Accept the benchmark-specific variable first, then the shared pipeline spelling.
std::string first_environment_value(const char* primary, const char* fallback) {
    auto value = environment_value(primary);
    return value.empty() ? environment_value(fallback) : value;
}

// Escape closing braces according to ODBC connection-string grammar.
std::string brace_connection_value(std::string_view value) {
    std::string result;
    result.reserve(value.size() + 2);
    result.push_back('{');
    for (const char character : value) {
        result.push_back(character);
        if (character == '}') {
            result.push_back('}');
        }
    }
    result.push_back('}');
    return result;
}

using SqlString = std::basic_string<SQLTCHAR>;

#ifdef UNICODE
// Append a Unicode scalar using the UTF-16 representation SQLTCHAR expects.
void append_code_point(SqlString& output, std::uint32_t code_point) {
    if (code_point <= 0xFFFF) {
        output.push_back(static_cast<SQLTCHAR>(code_point));
        return;
    }
    code_point -= 0x10000;
    output.push_back(static_cast<SQLTCHAR>(0xD800 + (code_point >> 10)));
    output.push_back(static_cast<SQLTCHAR>(0xDC00 + (code_point & 0x3FF)));
}

// Validate UTF-8 while converting pipeline-provided text to the ODBC wide API.
SqlString to_sql_string(std::string_view input) {
    SqlString output;
    output.reserve(input.size());
    for (std::size_t index = 0; index < input.size();) {
        const auto first = static_cast<unsigned char>(input[index]);
        std::uint32_t code_point = 0;
        std::size_t count = 0;
        if (first <= 0x7F) {
            code_point = first;
            count = 1;
        } else if ((first & 0xE0) == 0xC0) {
            code_point = first & 0x1F;
            count = 2;
        } else if ((first & 0xF0) == 0xE0) {
            code_point = first & 0x0F;
            count = 3;
        } else if ((first & 0xF8) == 0xF0) {
            code_point = first & 0x07;
            count = 4;
        } else {
            throw std::runtime_error("environment contains invalid UTF-8");
        }

        if (index + count > input.size()) {
            throw std::runtime_error("environment contains truncated UTF-8");
        }
        for (std::size_t offset = 1; offset < count; ++offset) {
            const auto next = static_cast<unsigned char>(input[index + offset]);
            if ((next & 0xC0) != 0x80) {
                throw std::runtime_error("environment contains invalid UTF-8");
            }
            code_point = (code_point << 6) | (next & 0x3F);
        }

        const bool overlong = (count == 2 && code_point < 0x80) ||
                              (count == 3 && code_point < 0x800) ||
                              (count == 4 && code_point < 0x10000);
        if (overlong || code_point > 0x10FFFF ||
            (code_point >= 0xD800 && code_point <= 0xDFFF)) {
            throw std::runtime_error("environment contains invalid UTF-8");
        }
        append_code_point(output, code_point);
        index += count;
    }
    return output;
}
#else
// The narrow build intentionally passes the UTF-8 byte sequence unchanged.
SqlString to_sql_string(std::string_view input) {
    return SqlString(reinterpret_cast<const SQLTCHAR*>(input.data()), input.size());
}
#endif

// Reduce diagnostics to log-safe ASCII; benchmark SQLSTATEs and messages are
// expected to be ASCII and no diagnostic text participates in measurement.
std::string from_sql_string(const SQLTCHAR* input, std::size_t length) {
#ifdef UNICODE
    std::string output;
    output.reserve(length);
    for (std::size_t index = 0; index < length; ++index) {
        const auto value = static_cast<std::uint32_t>(input[index]);
        output.push_back(value <= 0x7F ? static_cast<char>(value) : '?');
    }
    return output;
#else
    return std::string(reinterpret_cast<const char*>(input), length);
#endif
}

// Collect the full ODBC diagnostic chain so a failed perf run remains actionable.
std::string diagnostics(SQLSMALLINT handle_type, SQLHANDLE handle) {
    if (handle == SQL_NULL_HANDLE) {
        return "(no diagnostic handle)";
    }

    std::ostringstream output;
    bool found = false;
    for (SQLSMALLINT record = 1; record <= 32; ++record) {
        SQLTCHAR state[8] = {};
        SQLINTEGER native_error = 0;
        SQLTCHAR message[2048] = {};
        SQLSMALLINT message_length = 0;
        const SQLRETURN rc =
            SQLGetDiagRec(handle_type, handle, record, state, &native_error, message,
                          static_cast<SQLSMALLINT>(std::size(message)), &message_length);
        if (rc == SQL_NO_DATA) {
            break;
        }
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            break;
        }
        if (found) {
            output << " | ";
        }
        const auto diagnostic_length = static_cast<std::size_t>(
            std::clamp<int>(message_length, 0,
                            static_cast<int>(std::size(message) - 1)));
        output << '[' << from_sql_string(state, 5) << "] "
               << from_sql_string(message, diagnostic_length)
               << " (native=" << native_error << ')';
        found = true;
    }
    return found ? output.str() : "(no diagnostic)";
}

// Turn an ODBC return code into one exception carrying operation and driver details.
[[noreturn]] void throw_odbc_error(const char* operation, SQLRETURN rc,
                                   SQLSMALLINT handle_type, SQLHANDLE handle) {
    std::ostringstream message;
    message << operation << " returned " << static_cast<long>(rc) << ": "
            << diagnostics(handle_type, handle);
    throw std::runtime_error(message.str());
}

// Timed operations reject SUCCESS_WITH_INFO because truncation or conversion
// warnings would make throughput numbers invalid.
void require_exact_success(SQLRETURN rc, const char* operation,
                           SQLSMALLINT handle_type, SQLHANDLE handle) {
    if (rc != SQL_SUCCESS) {
        throw_odbc_error(operation, rc, handle_type, handle);
    }
}

// Connection warnings are logged but accepted because TLS and server policy can
// produce benign diagnostics before any measured work.
void require_connection_success(SQLRETURN rc, SQLHDBC dbc) {
    if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
        throw_odbc_error("SQLDriverConnect", rc, SQL_HANDLE_DBC, dbc);
    }
    if (rc == SQL_SUCCESS_WITH_INFO) {
        std::cerr << "SQLDriverConnect completed with diagnostics: "
                  << diagnostics(SQL_HANDLE_DBC, dbc) << '\n';
    }
}

// Generate short stable names that can be checked after SQLDescribeCol.
std::string column_name(std::size_t ordinal) {
    char buffer[16] = {};
    const int written = std::snprintf(buffer, sizeof(buffer), "c%03zu", ordinal);
    if (written <= 0 || static_cast<std::size_t>(written) >= sizeof(buffer)) {
        throw std::runtime_error("failed to generate benchmark column name");
    }
    return buffer;
}

// Delivered character count for a generated variable-length value.
std::size_t generated_length(const ColumnSpec& column, std::uint64_t row_id) {
    if (column.length_span == 0) {
        return column.length_base;
    }
    return column.length_base + static_cast<std::size_t>(row_id % column.length_span);
}

// Whether the deterministic generator wrote NULL into this cell.
bool generated_null(const ColumnSpec& column, std::uint64_t row_id) {
    return column.null_phase != 0 && (row_id + column.null_phase) % kNullPeriod == 0;
}

// Wrap a value expression in the column's NULL rule so the schema, the data, and
// the validator share one definition of where the NULLs are.
std::string apply_null_rule(const ColumnSpec& column, const std::string& expression) {
    if (column.null_phase == 0) {
        return expression;
    }
    std::ostringstream sql;
    sql << "CASE WHEN (g.[value] + " << column.null_phase << ") % " << kNullPeriod
        << " = 0 THEN NULL ELSE " << expression << " END";
    return sql.str();
}

// Build the server-side generator for one variable-length text column: take the
// first N characters of a repeated cycle, where N is a function of the row id.
std::string text_generator(const ColumnSpec& column, bool wide, bool max_length) {
    const std::size_t longest = column.length_base + (column.length_span == 0
                                                          ? 0
                                                          : column.length_span - 1);
    const std::size_t repeats = (longest + kTextCycle.size() - 1) / kTextCycle.size();
    std::ostringstream source;
    if (wide) {
        source << "N'" << kTextCycle << '\'';
    } else {
        source << '\'' << kTextCycle << '\'';
    }
    std::ostringstream sql;
    sql << "LEFT(REPLICATE(";
    if (max_length) {
        // REPLICATE truncates at 8000 bytes / 4000 characters unless its input is
        // already a MAX type, which is the only way to generate a value that
        // needs more than one SQLGetData chunk.
        sql << "CAST(" << source.str() << " AS " << (wide ? "NVARCHAR(MAX)" : "VARCHAR(MAX)")
            << ')';
    } else {
        sql << source.str();
    }
    sql << ", " << repeats << "), " << column.length_base;
    if (column.length_span != 0) {
        sql << " + CAST(g.[value] % " << column.length_span << " AS INT)";
    }
    sql << ')';
    return sql.str();
}

// Cover common fixed-width conversions plus narrow and wide character paths that
// stress result-set materialization.
struct FixedType {
    ValueKind kind;
    const char* sql_type;
    const char* sql_expression;
    SQLSMALLINT c_type;
    std::size_t slot_size;
    std::uint64_t logical_bytes;
    SQLLEN expected_indicator;
};

// The fifteen-column pattern every fixed-width table repeats. Its members pin
// the C type, the buffer stride, and the exact indicator each column must
// report, so a driver that silently changed a transfer size is caught in
// preflight rather than showing up as a throughput difference.
const std::array<FixedType, kPatternSize>& type_pattern() {
    static const std::array<FixedType, kPatternSize> pattern = {{
        {ValueKind::bit, "BIT", "CAST(g.[value] % CAST(2 AS BIGINT) AS BIT)",
         SQL_C_BIT, sizeof(SQLCHAR), 1, sizeof(SQLCHAR)},
        {ValueKind::tinyint, "TINYINT",
         "CAST(g.[value] % CAST(251 AS BIGINT) AS TINYINT)", SQL_C_UTINYINT,
         sizeof(SQLCHAR), 1, sizeof(SQLCHAR)},
        {ValueKind::smallint, "SMALLINT",
         "CAST((g.[value] % CAST(60001 AS BIGINT)) - CAST(30000 AS BIGINT) AS SMALLINT)",
         SQL_C_SSHORT, sizeof(SQLSMALLINT), 2, sizeof(SQLSMALLINT)},
        {ValueKind::integer, "INT", "CAST(g.[value] AS INT)", SQL_C_SLONG,
         sizeof(SQLINTEGER), 4, sizeof(SQLINTEGER)},
        {ValueKind::bigint, "BIGINT", "CAST(g.[value] AS BIGINT)", SQL_C_SBIGINT,
         sizeof(SQLBIGINT), 8, sizeof(SQLBIGINT)},
        {ValueKind::real, "REAL",
         "CAST(g.[value] % CAST(10000 AS BIGINT) AS REAL)", SQL_C_FLOAT,
         sizeof(float), 4, sizeof(float)},
        {ValueKind::double_precision, "FLOAT(53)",
         "CAST(g.[value] % CAST(1000000 AS BIGINT) AS FLOAT(53))", SQL_C_DOUBLE,
         sizeof(double), 8, sizeof(double)},
        {ValueKind::decimal, "DECIMAL(18,4)",
         "CAST(g.[value] AS DECIMAL(18,4))", SQL_C_CHAR, 32, 9, 0},
        {ValueKind::date, "DATE", "CAST('2024-02-29' AS DATE)", SQL_C_TYPE_DATE,
         sizeof(SQL_DATE_STRUCT), 3, sizeof(SQL_DATE_STRUCT)},
        {ValueKind::time, "TIME(7)", "CAST('12:34:56.1234567' AS TIME(7))",
         kSqlCSSsTime2, sizeof(SqlSsTime2), 5, sizeof(SqlSsTime2)},
        {ValueKind::datetime2, "DATETIME2(7)",
         "CAST('2024-02-29T12:34:56.1234567' AS DATETIME2(7))",
         SQL_C_TYPE_TIMESTAMP, sizeof(SQL_TIMESTAMP_STRUCT), 8,
         sizeof(SQL_TIMESTAMP_STRUCT)},
        {ValueKind::datetimeoffset, "DATETIMEOFFSET(7)",
         "CAST('2024-02-29T12:34:56.1234567+00:00' AS DATETIMEOFFSET(7))",
         kSqlCSSsTimestampOffset, sizeof(SqlSsTimestampOffset), 10,
         sizeof(SqlSsTimestampOffset)},
        {ValueKind::guid, "UNIQUEIDENTIFIER",
         "CAST('00112233-4455-6677-8899-AABBCCDDEEFF' AS UNIQUEIDENTIFIER)",
         SQL_C_GUID, sizeof(SqlGuid), 16, sizeof(SqlGuid)},
        {ValueKind::character, "CHAR(8)", "CAST('ODBCBEN1' AS CHAR(8))",
         SQL_C_CHAR, 9, 8, 8},
        {ValueKind::wide_character, "NCHAR(8)",
         "CAST(N'ODBCWIDE' AS NCHAR(8))", SQL_C_WCHAR, 9 * sizeof(SQLWCHAR), 16,
         8 * sizeof(SQLWCHAR)},
    }};
    return pattern;
}

// Append one repeat of the fixed pattern, numbering columns from the current width.
void append_fixed_pattern(std::vector<ColumnSpec>& columns, std::size_t repetitions) {
    for (std::size_t repeat = 0; repeat < repetitions; ++repeat) {
        for (const auto& type : type_pattern()) {
            ColumnSpec column;
            column.name = column_name(columns.size() + 1);
            column.kind = type.kind;
            column.sql_type = type.sql_type;
            column.sql_expression = type.sql_expression;
            column.c_type = type.c_type;
            column.slot_size = type.slot_size;
            column.logical_bytes = type.logical_bytes;
            column.expected_indicator = type.expected_indicator;
            columns.push_back(std::move(column));
        }
    }
}

// Build one nullable inline variable-width column: bounded VARCHAR/NVARCHAR only,
// so it stays on the bindable path and never becomes a PLP column by accident.
ColumnSpec make_inline_text(std::vector<ColumnSpec>& columns, bool wide,
                            std::size_t max_length, std::uint64_t null_phase) {
    ColumnSpec column;
    column.name = column_name(columns.size() + 1);
    column.kind = wide ? ValueKind::var_wide_character : ValueKind::var_character;
    column.unit_bytes = wide ? sizeof(SQLWCHAR) : 1;
    column.c_type = wide ? SQL_C_WCHAR : SQL_C_CHAR;
    column.length_base = 1;
    column.length_span = max_length;
    column.null_phase = null_phase;
    // One extra unit for the terminator ODBC always writes.
    column.slot_size = (max_length + 1) * column.unit_bytes;
    std::ostringstream type;
    type << (wide ? "NVARCHAR(" : "VARCHAR(") << max_length << ')';
    column.sql_type = type.str();
    column.sql_expression = apply_null_rule(column, text_generator(column, wide, false));
    return column;
}

// Build one MAX column whose shortest value still needs more than one 8192-byte
// SQLGetData call, which is the continuation loop the review asked to measure.
ColumnSpec make_lob_text(std::vector<ColumnSpec>& columns, bool wide,
                         std::size_t length_base, std::size_t length_span,
                         std::uint64_t null_phase) {
    ColumnSpec column;
    column.name = column_name(columns.size() + 1);
    column.kind = wide ? ValueKind::lob_wide_character : ValueKind::lob_character;
    column.unit_bytes = wide ? sizeof(SQLWCHAR) : 1;
    column.c_type = wide ? SQL_C_WCHAR : SQL_C_CHAR;
    column.length_base = length_base;
    column.length_span = length_span;
    column.null_phase = null_phase;
    // Never bound, so the slot is the continuation chunk rather than a row stride.
    column.slot_size = kLobChunkBytes;
    column.sql_type = wide ? "NVARCHAR(MAX)" : "VARCHAR(MAX)";
    column.sql_expression = apply_null_rule(column, text_generator(column, wide, true));
    return column;
}

// Build one sql_variant column. The exact numerics are no longer excluded for a
// parity reason: since AB#47702 both drivers answer SQL_C_NUMERIC for
// decimal/money variants, which `variant_read_c_type` folds onto SQL_C_CHAR on
// either driver. The column set is left as-is so existing baselines stay
// comparable.
ColumnSpec make_variant(std::vector<ColumnSpec>& columns, ValueKind kind,
                        std::uint64_t null_phase) {
    ColumnSpec column;
    column.name = column_name(columns.size() + 1);
    column.kind = kind;
    column.sql_type = "SQL_VARIANT";
    column.null_phase = null_phase;
    // Widest rendering any accepted variant C type needs, including the character
    // fallback the driver may legitimately choose for the text arm.
    column.slot_size = 64;
    switch (kind) {
        case ValueKind::variant_integer:
            column.sql_expression = "CAST(CAST(g.[value] AS INT) AS SQL_VARIANT)";
            column.logical_bytes = 4;
            break;
        case ValueKind::variant_bigint:
            column.sql_expression = "CAST(CAST(g.[value] AS BIGINT) AS SQL_VARIANT)";
            column.logical_bytes = 8;
            break;
        default:
            column.sql_expression =
                "CAST(CAST(N'ODBCVARIANT' AS NVARCHAR(32)) AS SQL_VARIANT)";
            // Delivered as SQL_C_CHAR, which is what mssql-python requests for a
            // character variant, so the payload is one byte per ASCII character.
            column.logical_bytes = 11;
            break;
    }
    column.sql_expression = apply_null_rule(column, column.sql_expression);
    return column;
}

// The single source of truth for a table's columns: DDL, generator, binding, and
// validation are all derived from this one list.
//
// VARBINARY is deliberately absent. Raw binary delivery is covered by the ODBC
// functional tests; this workload isolates character-width and NULL-handling
// costs. Binary-to-character hex rendering remains a separate conversion.
std::vector<ColumnSpec> columns_for(const TableSpec& table) {
    std::vector<ColumnSpec> columns;
    switch (table.shape) {
        case TableShape::fixed_pattern:
            columns.reserve(table.pattern_repetitions * kPatternSize);
            append_fixed_pattern(columns, table.pattern_repetitions);
            break;
        case TableShape::mixed_lob:
            append_fixed_pattern(columns, table.pattern_repetitions);
            // One short MAX column. The payload is tiny on purpose: what this
            // table measures is that a single PLP column moves the whole result
            // onto the row-at-a-time path, not the cost of the LOB itself.
            columns.push_back(make_lob_text(columns, true, 24, 0, 0));
            break;
        case TableShape::variable_width: {
            ColumnSpec identity;
            identity.name = column_name(1);
            identity.kind = ValueKind::integer;
            identity.sql_type = "INT";
            identity.sql_expression = "CAST(g.[value] AS INT)";
            identity.c_type = SQL_C_SLONG;
            identity.slot_size = sizeof(SQLINTEGER);
            identity.logical_bytes = 4;
            identity.expected_indicator = sizeof(SQLINTEGER);
            columns.push_back(std::move(identity));
            columns.push_back(make_inline_text(columns, false, 64, 1));
            columns.push_back(make_inline_text(columns, false, 256, 2));
            columns.push_back(make_inline_text(columns, false, 1024, 3));
            columns.push_back(make_inline_text(columns, true, 64, 4));
            columns.push_back(make_inline_text(columns, true, 256, 5));
            columns.push_back(make_inline_text(columns, true, 1024, 6));
            break;
        }
        case TableShape::lob_max: {
            ColumnSpec identity;
            identity.name = column_name(1);
            identity.kind = ValueKind::integer;
            identity.sql_type = "INT";
            identity.sql_expression = "CAST(g.[value] AS INT)";
            identity.c_type = SQL_C_SLONG;
            identity.slot_size = sizeof(SQLINTEGER);
            identity.logical_bytes = 4;
            identity.expected_indicator = sizeof(SQLINTEGER);
            columns.push_back(std::move(identity));
            // 9000-9999 UTF-16 characters is 18000-19998 bytes, so SQL_C_WCHAR
            // needs three 8192-byte calls; 20000-20999 narrow bytes needs three
            // as well. Both stay well inside one TDS PLP value.
            columns.push_back(make_lob_text(columns, true, 9000, 1000, 1));
            columns.push_back(make_lob_text(columns, false, 20000, 1000, 2));
            break;
        }
        case TableShape::sql_variant: {
            ColumnSpec identity;
            identity.name = column_name(1);
            identity.kind = ValueKind::integer;
            identity.sql_type = "INT";
            identity.sql_expression = "CAST(g.[value] AS INT)";
            identity.c_type = SQL_C_SLONG;
            identity.slot_size = sizeof(SQLINTEGER);
            identity.logical_bytes = 4;
            identity.expected_indicator = sizeof(SQLINTEGER);
            columns.push_back(std::move(identity));
            columns.push_back(make_variant(columns, ValueKind::variant_integer, 0));
            columns.push_back(make_variant(columns, ValueKind::variant_text, 0));
            // Nullable so the probe's SQL_NULL_DATA arm is exercised too; that is
            // the branch mssql-python takes before it ever asks for the type.
            columns.push_back(make_variant(columns, ValueKind::variant_bigint, 3));
            break;
        }
    }
    return columns;
}

// Keep benchmark tables isolated under deterministic, explicitly quoted names.
std::string qualified_table(const TableSpec& table) {
    return std::string("[dbo].[") + table.table_name + ']';
}

// Materialize the schema the workload catalog promises.
std::string create_table_sql(const TableSpec& table) {
    const auto columns = columns_for(table);
    std::ostringstream sql;
    sql << "CREATE TABLE " << qualified_table(table) << " (";
    for (std::size_t index = 0; index < columns.size(); ++index) {
        if (index != 0) {
            sql << ',';
        }
        sql << '[' << columns[index].name << "] " << columns[index].sql_type
            << (columns[index].null_phase == 0 ? " NOT NULL" : " NULL");
    }
    sql << ')';
    return sql.str();
}

// Generate deterministic values in one server-side statement outside the timed run.
std::string insert_sql(const TableSpec& table) {
    const auto columns = columns_for(table);
    std::ostringstream sql;
    sql << "INSERT INTO " << qualified_table(table) << " (";
    for (std::size_t index = 0; index < columns.size(); ++index) {
        if (index != 0) {
            sql << ',';
        }
        sql << '[' << columns[index].name << ']';
    }
    sql << ") SELECT ";
    for (std::size_t index = 0; index < columns.size(); ++index) {
        if (index != 0) {
            sql << ',';
        }
        sql << columns[index].sql_expression;
    }
    sql << " FROM GENERATE_SERIES(CAST(1 AS BIGINT), CAST(" << table.row_count
        << " AS BIGINT)) AS g OPTION (MAXDOP 1)";
    return sql.str();
}

// Preserve column order so each bound buffer has a stable semantic contract.
std::string select_sql(const TableSpec& table, const std::vector<ColumnSpec>& columns) {
    std::ostringstream sql;
    sql << "SELECT ";
    for (std::size_t index = 0; index < columns.size(); ++index) {
        if (index != 0) {
            sql << ',';
        }
        sql << '[' << columns[index].name << ']';
    }
    sql << " FROM " << qualified_table(table) << " OPTION (MAXDOP 1)";
    return sql.str();
}

// Mix row IDs into an order-independent checksum that detects loss and duplication.
std::uint64_t splitmix64(std::uint64_t value) {
    value += 0x9E3779B97F4A7C15ULL;
    value = (value ^ (value >> 30)) * 0xBF58476D1CE4E5B9ULL;
    value = (value ^ (value >> 27)) * 0x94D049BB133111EBULL;
    return value ^ (value >> 31);
}

// Use monotonic time because wall-clock adjustments must not affect a benchmark.
double seconds_between(std::chrono::steady_clock::time_point start,
                       std::chrono::steady_clock::time_point end) {
    return std::chrono::duration<double>(end - start).count();
}

// Encode integer statement attributes using the pointer-sized ODBC ABI convention.
SQLPOINTER attribute_value(SQLULEN value) {
    return reinterpret_cast<SQLPOINTER>(static_cast<std::uintptr_t>(value));
}

}  // namespace

// Derive width from the shared column model so setup and binding cannot drift.
std::size_t TableSpec::column_count() const {
    return columns_for(*this).size();
}

// Every shape except the fixed pattern leads with its INT identity column; the
// pattern's identity is its fourth entry, and repeats duplicate it.
std::size_t TableSpec::row_id_column() const {
    return (shape == TableShape::fixed_pattern || shape == TableShape::mixed_lob) ? 3 : 0;
}

// Sum the generator's own length rule over every row, so throughput counters
// describe delivered payload instead of a nominal column width.
std::uint64_t TableSpec::logical_bytes_total() const {
    const auto columns = columns_for(*this);
    std::uint64_t total = 0;
    for (const auto& column : columns) {
        const bool constant_width = column.length_span == 0 && column.length_base == 0;
        if (constant_width && column.null_phase == 0) {
            total += column.logical_bytes * row_count;
            continue;
        }
        for (std::uint64_t row_id = 1; row_id <= row_count; ++row_id) {
            if (generated_null(column, row_id)) {
                continue;
            }
            total += constant_width
                         ? column.logical_bytes
                         : static_cast<std::uint64_t>(generated_length(column, row_id)) *
                               column.unit_bytes;
        }
    }
    return total;
}

// The catalog the admin executable creates and every measurement reads.
//
// Row counts are chosen per access shape so one full retrieval stays in the
// hundreds of milliseconds on all three drivers: a rowset-1 workload issues one
// driver round trip per row, so it cannot use the same row count as a
// rowset-1000 one and still finish in comparable time.
const std::array<TableSpec, kTableCount>& tables() {
    static const std::array<TableSpec, kTableCount> catalog = {{
        {"mssql_odbc_bench_fixed_2m_c15", TableShape::fixed_pattern, 2'000'000, 1},
        {"mssql_odbc_bench_fixed_10k_c600", TableShape::fixed_pattern, 10'000, 40},
        {"mssql_odbc_bench_fixed_100k_c15", TableShape::fixed_pattern, 100'000, 1},
        {"mssql_odbc_bench_fixed_20k_c15", TableShape::fixed_pattern, 20'000, 1},
        {"mssql_odbc_bench_varwidth_100k_c7", TableShape::variable_width, 100'000, 0},
        {"mssql_odbc_bench_lobmax_1k_c3", TableShape::lob_max, 1'000, 0},
        {"mssql_odbc_bench_mixedlob_20k_c16", TableShape::mixed_lob, 20'000, 1},
        {"mssql_odbc_bench_variant_20k_c4", TableShape::sql_variant, 20'000, 0},
    }};
    return catalog;
}

// Look up a catalog table by name so the workload catalog stays declarative and a
// typo becomes a startup failure instead of a missing benchmark.
static const TableSpec& table_by_name(const char* name) {
    for (const auto& table : tables()) {
        if (std::strcmp(table.table_name, name) == 0) {
            return table;
        }
    }
    throw std::logic_error("benchmark workload names an unknown table");
}

// The measured catalog. Ids are stable and carry the shape, so a report row still
// means something without the catalog beside it, and the candidate, baseline, and
// Microsoft legs always produce exactly the same set.
const std::array<WorkloadSpec, kWorkloadCount>& workloads() {
    static const std::array<WorkloadSpec, kWorkloadCount> specs = {{
        // Row volume at the fetchall cadence.
        {"fetch/narrow_2m_c15_fixed/bound_rowset_1000", "narrow",
         &table_by_name("mssql_odbc_bench_fixed_2m_c15"), AccessMode::bound_drain,
         kFetchAllRowset},
        // Per-row conversion and binding breadth, still below the 8060-byte row limit.
        {"fetch/wide_10k_c600_fixed/bound_rowset_1000", "wide",
         &table_by_name("mssql_odbc_bench_fixed_10k_c600"), AccessMode::bound_drain,
         kFetchAllRowset},
        // The three rowset sizes over identical data, so only the cadence differs.
        {"fetch/rowset_100k_c15_fixed/bound_rowset_1", "rowset",
         &table_by_name("mssql_odbc_bench_fixed_100k_c15"), AccessMode::bound_drain,
         kFetchManyDefaultRowset},
        {"fetch/rowset_100k_c15_fixed/bound_rowset_64", "rowset",
         &table_by_name("mssql_odbc_bench_fixed_100k_c15"), AccessMode::bound_drain,
         kFetchManyCadenceRowset},
        {"fetch/rowset_100k_c15_fixed/bound_rowset_1000", "rowset",
         &table_by_name("mssql_odbc_bench_fixed_100k_c15"), AccessMode::bound_drain,
         kFetchAllRowset},
        // Same data and cadence as bound_rowset_64, plus the per-call describe,
        // rebind, and unbind that fetchmany() repeats. The pair isolates that cost.
        {"fetch/rowset_100k_c15_fixed/bind_cycle_rowset_64", "rowset",
         &table_by_name("mssql_odbc_bench_fixed_100k_c15"), AccessMode::bound_bind_cycle,
         kFetchManyCadenceRowset},
        // The default arraysize: one bind/fetch/unbind lifecycle per row.
        {"fetch/rowset_20k_c15_fixed/bind_cycle_rowset_1", "rowset",
         &table_by_name("mssql_odbc_bench_fixed_20k_c15"), AccessMode::bound_bind_cycle,
         kFetchManyDefaultRowset},
        // Nullable inline variable width, kept separate from the MAX/PLP path.
        {"fetch/varwidth_100k_c7_nullable/bound_rowset_1000", "varwidth",
         &table_by_name("mssql_odbc_bench_varwidth_100k_c7"), AccessMode::bound_drain,
         kFetchAllRowset},
        {"fetch/varwidth_100k_c7_nullable/bound_rowset_64", "varwidth",
         &table_by_name("mssql_odbc_bench_varwidth_100k_c7"), AccessMode::bound_drain,
         kFetchManyCadenceRowset},
        // Row-at-a-time SQLGetData over ordinary inline values: the same columns
        // as bound_rowset_1, so the pair separates the call shape from the cadence.
        {"getdata/rowwise_20k_c15_fixed/inline_values", "getdata",
         &table_by_name("mssql_odbc_bench_fixed_20k_c15"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
        // MAX text past one chunk, so every value needs repeated 8192-byte calls.
        {"getdata/rowwise_1k_c3_lob_max/chunked_8192", "getdata",
         &table_by_name("mssql_odbc_bench_lobmax_1k_c3"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
        // One small MAX column forcing all 16 columns onto the row-at-a-time path.
        {"getdata/rowwise_20k_c16_mixed_lob/whole_result_rowwise", "getdata",
         &table_by_name("mssql_odbc_bench_mixedlob_20k_c16"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
        // The sql_variant sequence: zero-length SQL_C_BINARY probe, then
        // SQLColAttribute(SQL_CA_SS_VARIANT_TYPE), then the typed read.
        {"getdata/rowwise_20k_c4_variant/probe_colattribute", "getdata",
         &table_by_name("mssql_odbc_bench_variant_20k_c4"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
    }};
    return specs;
}

// Resolve one explicit configuration contract shared by both executables.
Config Config::from_environment() {
    Config config;
    config.driver = environment_value("ODBC_BENCH_DRIVER");
    config.server = first_environment_value("ODBC_BENCH_SERVER", "SQL_SERVER");
    config.database = environment_value_or("ODBC_BENCH_DATABASE", "tempdb");
    config.uid = environment_value_or("ODBC_BENCH_UID", "sa");
    config.pwd = first_environment_value("ODBC_BENCH_PWD", "SQL_PASSWORD");
    config.trust_certificate =
        environment_value_or("ODBC_BENCH_TRUST_CERT", "Yes");
    config.encrypt = environment_value_or("ODBC_BENCH_ENCRYPT", "Mandatory");
    config.packet_size =
        environment_value_or("ODBC_BENCH_PACKET_SIZE", kDefaultPacketSize);
    config.packet_size_keyword =
        environment_value_or("ODBC_BENCH_PACKET_SIZE_KEYWORD", "PacketSize");
    config.scenario = environment_value("ODBC_BENCH_SCENARIO");

    std::vector<std::string> missing;
    if (config.driver.empty()) {
        missing.emplace_back("ODBC_BENCH_DRIVER");
    }
    if (config.server.empty()) {
        missing.emplace_back("ODBC_BENCH_SERVER (or SQL_SERVER)");
    }
    if (config.pwd.empty()) {
        missing.emplace_back("ODBC_BENCH_PWD (or SQL_PASSWORD)");
    }
    if (!missing.empty()) {
        std::ostringstream message;
        message << "missing required connection environment:";
        for (const auto& name : missing) {
            message << ' ' << name;
        }
        throw std::runtime_error(message.str());
    }

    if (!std::all_of(config.packet_size.begin(), config.packet_size.end(),
                     [](unsigned char character) {
                         return std::isdigit(character) != 0;
                     })) {
        throw std::runtime_error("ODBC_BENCH_PACKET_SIZE must be an integer");
    }
    const unsigned long packet_size = std::stoul(config.packet_size);
    if (packet_size < 512 || packet_size > 32767) {
        throw std::runtime_error(
            "ODBC_BENCH_PACKET_SIZE must be between 512 and 32767");
    }
    if (config.packet_size_keyword != "PacketSize" &&
        config.packet_size_keyword != "Packet Size") {
        throw std::runtime_error(
            "ODBC_BENCH_PACKET_SIZE_KEYWORD must be PacketSize or Packet Size");
    }
    if (!config.scenario.empty()) {
        // Reject an unknown scenario instead of silently registering nothing: a
        // leg that measures zero benchmarks would fail the comparator's
        // benchmark-set check much further downstream.
        const bool known = std::any_of(
            workloads().begin(), workloads().end(), [&config](const WorkloadSpec& spec) {
                return config.scenario == spec.scenario;
            });
        if (!known) {
            throw std::runtime_error(
                "ODBC_BENCH_SCENARIO must be one of narrow, wide, rowset, varwidth, "
                "getdata, or unset");
        }
    }
    return config;
}

// Keep all three driver legs identical except for the registered driver name and
// the packet-size keyword spelling each implementation accepts.
std::string Config::connection_string() const {
    std::ostringstream connection;
    connection << "Driver=" << brace_connection_value(driver)
               << ";Server=" << brace_connection_value(server)
               << ";Database=" << brace_connection_value(database)
               << ";UID=" << brace_connection_value(uid)
               << ";PWD=" << brace_connection_value(pwd)
               << ";TrustServerCertificate="
               << brace_connection_value(trust_certificate)
               << ";Encrypt=" << brace_connection_value(encrypt)
               << ';' << packet_size_keyword << '=' << packet_size << ';';
    return connection.str();
}

// Build a complete handle chain once so connection setup is outside measurements.
OdbcSession::OdbcSession(const Config& config) {
    try {
        SQLRETURN rc =
            SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env_);
        require_exact_success(rc, "SQLAllocHandle(SQL_HANDLE_ENV)", SQL_HANDLE_ENV,
                              env_);

        rc = SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                           attribute_value(SQL_OV_ODBC3_80), 0);
        require_exact_success(rc, "SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION)",
                              SQL_HANDLE_ENV, env_);

        rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_);
        require_exact_success(rc, "SQLAllocHandle(SQL_HANDLE_DBC)", SQL_HANDLE_ENV,
                              env_);

        const auto requested_packet_size =
            static_cast<SQLUINTEGER>(std::stoul(config.packet_size));
        rc = SQLSetConnectAttr(dbc_, SQL_ATTR_PACKET_SIZE,
                               attribute_value(requested_packet_size), 0);
        require_exact_success(rc, "SQLSetConnectAttr(SQL_ATTR_PACKET_SIZE)",
                              SQL_HANDLE_DBC, dbc_);

        auto connection = to_sql_string(config.connection_string());
        rc = SQLDriverConnect(dbc_, nullptr, connection.data(), SQL_NTS, nullptr, 0,
                              nullptr, SQL_DRIVER_NOPROMPT);
        require_connection_success(rc, dbc_);

        SQLUINTEGER negotiated_packet_size = 0;
        rc = SQLGetConnectAttr(dbc_, SQL_ATTR_PACKET_SIZE, &negotiated_packet_size,
                               sizeof(negotiated_packet_size), nullptr);
        require_exact_success(rc, "SQLGetConnectAttr(SQL_ATTR_PACKET_SIZE)",
                              SQL_HANDLE_DBC, dbc_);
        if (negotiated_packet_size != requested_packet_size) {
            std::ostringstream message;
            message << "driver reports packet size " << negotiated_packet_size
                    << "; requested " << requested_packet_size;
            throw std::runtime_error(message.str());
        }

        rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_);
        require_exact_success(rc, "SQLAllocHandle(SQL_HANDLE_STMT)", SQL_HANDLE_DBC,
                              dbc_);
    } catch (...) {
        release();
        throw;
    }
}

// Centralize cleanup so both normal and exceptional paths use the same handle order.
OdbcSession::~OdbcSession() {
    release();
}

// ODBC requires child handles to be released before their parents.
void OdbcSession::release() noexcept {
    if (stmt_ != SQL_NULL_HSTMT) {
        SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
        stmt_ = SQL_NULL_HSTMT;
    }
    if (dbc_ != SQL_NULL_HDBC) {
        SQLDisconnect(dbc_);
        SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
        dbc_ = SQL_NULL_HDBC;
    }
    if (env_ != SQL_NULL_HENV) {
        SQLFreeHandle(SQL_HANDLE_ENV, env_);
        env_ = SQL_NULL_HENV;
    }
}

// The runner owns statement state transitions; other code receives no ownership.
SQLHSTMT OdbcSession::statement() const {
    return stmt_;
}

// Drain all results so the shared statement is clean for the next setup operation.
void OdbcSession::execute_non_query(const std::string& sql) {
    auto text = to_sql_string(sql);
    try {
        require_exact_success(SQLExecDirect(stmt_, text.data(), SQL_NTS),
                              "SQLExecDirect", SQL_HANDLE_STMT, stmt_);
        for (;;) {
            const SQLRETURN rc = SQLMoreResults(stmt_);
            if (rc == SQL_NO_DATA) {
                break;
            }
            require_exact_success(rc, "SQLMoreResults", SQL_HANDLE_STMT, stmt_);
        }
    } catch (...) {
        SQLFreeStmt(stmt_, SQL_CLOSE);
        throw;
    }
}

// Check generated table cardinality through bound ODBC fetches, not a side channel.
std::uint64_t OdbcSession::query_count(const std::string& table) {
    const std::string sql =
        "SELECT COUNT_BIG(*) FROM " + table + " OPTION (MAXDOP 1)";
    auto text = to_sql_string(sql);
    SQLBIGINT count = -1;
    SQLLEN indicator = kIndicatorSentinel;
    try {
        require_exact_success(SQLExecDirect(stmt_, text.data(), SQL_NTS),
                              "SQLExecDirect(COUNT_BIG)", SQL_HANDLE_STMT, stmt_);
        SQLSMALLINT columns = 0;
        require_exact_success(SQLNumResultCols(stmt_, &columns), "SQLNumResultCols",
                              SQL_HANDLE_STMT, stmt_);
        if (columns != 1) {
            throw std::runtime_error("COUNT_BIG query returned the wrong column count");
        }
        require_exact_success(
            SQLBindCol(stmt_, 1, SQL_C_SBIGINT, &count, sizeof(count), &indicator),
            "SQLBindCol(COUNT_BIG)", SQL_HANDLE_STMT, stmt_);
        require_exact_success(SQLFetch(stmt_), "SQLFetch(COUNT_BIG)",
                              SQL_HANDLE_STMT, stmt_);
        if (indicator != static_cast<SQLLEN>(sizeof(count)) || count < 0) {
            throw std::runtime_error("COUNT_BIG query returned an invalid value");
        }
        const SQLRETURN final_fetch = SQLFetch(stmt_);
        if (final_fetch != SQL_NO_DATA) {
            throw_odbc_error("SQLFetch(COUNT_BIG final)", final_fetch,
                             SQL_HANDLE_STMT, stmt_);
        }
        require_exact_success(SQLCloseCursor(stmt_), "SQLCloseCursor(COUNT_BIG)",
                              SQL_HANDLE_STMT, stmt_);
        require_exact_success(SQLFreeStmt(stmt_, SQL_UNBIND),
                              "SQLFreeStmt(SQL_UNBIND COUNT_BIG)",
                              SQL_HANDLE_STMT, stmt_);
    } catch (...) {
        SQLFreeStmt(stmt_, SQL_CLOSE);
        SQLFreeStmt(stmt_, SQL_UNBIND);
        throw;
    }
    return static_cast<std::uint64_t>(count);
}

// Drop in reverse catalog order to keep cleanup safe if dependencies are added later.
void cleanup_benchmark_tables(OdbcSession& session) {
    for (auto iterator = tables().rbegin(); iterator != tables().rend(); ++iterator) {
        session.execute_non_query("DROP TABLE IF EXISTS " + qualified_table(*iterator));
    }
    std::cout << "Benchmark tables removed\n";
}

// Recreate and count each table before any timed process can consume it.
void setup_benchmark_tables(OdbcSession& session) {
    cleanup_benchmark_tables(session);
    for (const auto& table : tables()) {
        std::cout << "Creating " << qualified_table(table) << " with " << table.row_count
                  << " rows and " << table.column_count() << " columns\n";
        session.execute_non_query(create_table_sql(table));
        session.execute_non_query(insert_sql(table));
        const auto actual_rows = session.query_count(qualified_table(table));
        if (actual_rows != table.row_count) {
            std::ostringstream message;
            message << qualified_table(table) << " contains " << actual_rows
                    << " rows; expected " << table.row_count;
            throw std::runtime_error(message.str());
        }
    }
    std::cout << "Benchmark setup complete\n";
}

// Emit the whole catalog as replayable T-SQL, batch-separated so it can be piped
// straight into sqlcmd. The statements are byte-identical to what setup sends.
void print_benchmark_sql(std::ostream& output) {
    for (const auto& table : tables()) {
        output << "-- table " << table.table_name << ": " << table.row_count
               << " rows, " << table.column_count() << " columns\n";
        output << "DROP TABLE IF EXISTS " << qualified_table(table) << ";\nGO\n";
        output << create_table_sql(table) << ";\nGO\n";
        output << insert_sql(table) << ";\nGO\n";
    }
    for (const auto& workload : workloads()) {
        output << "-- workload " << workload.benchmark_name << " (scenario "
               << workload.scenario << ", rowset " << workload.rowset_size << ")\n";
        output << select_sql(*workload.table, columns_for(*workload.table))
               << ";\nGO\n";
    }
}

namespace {

// SQLGetData's chunked and probe forms legitimately answer SQL_SUCCESS_WITH_INFO:
// 01004 is how a driver says "more of this value remains", and a zero-length
// probe can only answer that way. The strict rule stays in force everywhere
// else; rejecting it here would reject the protocol these workloads measure.
void require_succeeded(SQLRETURN rc, const char* operation, SQLHSTMT stmt) {
    if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
        throw_odbc_error(operation, rc, SQL_HANDLE_STMT, stmt);
    }
}

// Fold whatever SQL_CA_SS_VARIANT_TYPE reports onto the C type the value is read
// with. Both the signed/unsigned and the concise/verbose spellings are accepted
// because the answer comes from the driver, and a benchmark that only understood
// one spelling would fail on the other driver rather than measure it.
//
// SQL_C_WCHAR folds to SQL_C_CHAR deliberately: mssql-python does the same,
// because requesting SQL_C_WCHAR after the binary probe fails on unixODBC.
//
// The date and timestamp arms accept both the 2.x and 3.x spellings for a
// concrete reason, not for symmetry: measured against msodbcsql18 18.06.0001, a
// date variant answers SQL_C_DATE (9) and the datetime family SQL_C_TIMESTAMP
// (11), while mssql-odbc answers SQL_C_TYPE_DATE (91) / SQL_C_TYPE_TIMESTAMP
// (93). Accepting only the 3.x form would bind a typed fetch on one driver and
// a character fetch on the other, which is precisely the asymmetry this fold
// exists to prevent. Unreachable today - `make_variant` builds no temporal
// column - but the trap is removed rather than left for whoever adds one.
SQLSMALLINT variant_read_c_type(SQLLEN reported) {
    switch (reported) {
        case SQL_C_BIT:
            return SQL_C_BIT;
        case SQL_C_TINYINT:
        case SQL_C_STINYINT:
        case SQL_C_UTINYINT:
            return SQL_C_STINYINT;
        case SQL_C_SHORT:
        case SQL_C_SSHORT:
        case SQL_C_USHORT:
            return SQL_C_SSHORT;
        case SQL_C_LONG:
        case SQL_C_SLONG:
        case SQL_C_ULONG:
            return SQL_C_SLONG;
        case SQL_C_SBIGINT:
        case SQL_C_UBIGINT:
            return SQL_C_SBIGINT;
        case SQL_C_FLOAT:
            return SQL_C_FLOAT;
        case SQL_C_DOUBLE:
            return SQL_C_DOUBLE;
        case SQL_C_GUID:
            return SQL_C_GUID;
        case SQL_C_TYPE_DATE:
        case SQL_C_DATE:
            return SQL_C_TYPE_DATE;
        case SQL_C_TYPE_TIMESTAMP:
        case SQL_C_TIMESTAMP:
            return SQL_C_TYPE_TIMESTAMP;
        default:
            return SQL_C_CHAR;
    }
}

// Owns one column-wise row-array buffer with alignment valid for every bound C type.
class ColumnBuffer {
public:
    ColumnBuffer(const ColumnSpec& column, std::size_t rows)
        : column_(&column),
          rows_(rows),
          storage_((column.slot_size * rows + sizeof(std::max_align_t) - 1) /
                   sizeof(std::max_align_t)),
          indicators_(rows, kIndicatorSentinel) {}

    SQLPOINTER data() {
        return static_cast<SQLPOINTER>(storage_.data());
    }

    SQLLEN* indicators() {
        return indicators_.data();
    }

    SQLLEN indicator(std::size_t row) const {
        return indicators_[row];
    }

    // Row-wise reads write their own indicator because SQLGetData reports it per
    // call rather than into the bound array.
    void set_indicator(std::size_t row, SQLLEN value) {
        indicators_[row] = value;
    }

    void reset_indicators() {
        std::fill(indicators_.begin(), indicators_.end(), kIndicatorSentinel);
    }

    const ColumnSpec& column() const {
        return *column_;
    }

    // Slots allocated, which is the workload's rowset size for the bound modes
    // and 1 for the row-at-a-time one.
    std::size_t rows() const {
        return rows_;
    }

    // Copy rather than reinterpret storage so validation does not assume alignment
    // beyond the ODBC buffer contract.
    template <typename T>
    T read(std::size_t row) const {
        if (sizeof(T) > column_->slot_size || row >= rows_) {
            throw std::logic_error("invalid typed read from column buffer");
        }
        T value{};
        const auto* bytes = reinterpret_cast<const unsigned char*>(storage_.data());
        std::memcpy(&value, bytes + row * column_->slot_size, sizeof(T));
        return value;
    }

    const unsigned char* row_bytes(std::size_t row) const {
        const auto* bytes = reinterpret_cast<const unsigned char*>(storage_.data());
        return bytes + row * column_->slot_size;
    }

private:
    const ColumnSpec* column_;
    std::size_t rows_;
    std::vector<std::max_align_t> storage_;
    std::vector<SQLLEN> indicators_;
};

// Attach the row identity to semantic mismatches found during untimed preflight.
void require_value(bool condition, const char* description, std::uint64_t row_id) {
    if (!condition) {
        std::ostringstream message;
        message << "preflight value mismatch for " << description << " at row id "
                << row_id;
        throw std::runtime_error(message.str());
    }
}

// Confirm the driver wrote a complete value of the length the generator promised,
// and that NULL appears exactly where the generator put it.
void validate_indicator(const ColumnBuffer& buffer, std::size_t row,
                        std::uint64_t row_id) {
    const auto& column = buffer.column();
    const SQLLEN indicator = buffer.indicator(row);
    if (indicator == SQL_NULL_DATA) {
        require_value(generated_null(column, row_id), "unexpected NULL", row_id);
        return;
    }
    require_value(!generated_null(column, row_id), "missing NULL", row_id);
    if (indicator < 0 || indicator == kIndicatorSentinel) {
        throw std::runtime_error("preflight found an invalid or unwritten indicator");
    }

    if (column.expected_indicator != 0 && indicator != column.expected_indicator) {
        std::ostringstream message;
        message << "preflight indicator mismatch: expected " << column.expected_indicator
                << ", got " << indicator;
        throw std::runtime_error(message.str());
    }
    if (column.length_span != 0 || column.length_base != 0) {
        const auto expected = static_cast<SQLLEN>(
            static_cast<std::uint64_t>(generated_length(column, row_id)) *
            column.unit_bytes);
        require_value(indicator == expected, "generated value length", row_id);
    }
    if (column.kind == ValueKind::decimal &&
        (indicator == 0 || static_cast<std::size_t>(indicator) >= column.slot_size)) {
        throw std::runtime_error("preflight decimal indicator is outside its fixed slot");
    }
}

// Validate both text termination and numeric meaning for DECIMAL-to-character binding.
void validate_decimal(const ColumnBuffer& buffer, std::size_t row,
                      std::uint64_t row_id) {
    const auto length = static_cast<std::size_t>(buffer.indicator(row));
    const char* text = reinterpret_cast<const char*>(buffer.row_bytes(row));
    require_value(text[length] == '\0', "DECIMAL terminator", row_id);

    errno = 0;
    char* end = nullptr;
    const long double value = std::strtold(text, &end);
    while (end != nullptr && *end != '\0' &&
           std::isspace(static_cast<unsigned char>(*end)) != 0) {
        ++end;
    }
    require_value(errno == 0 && end != text && end != nullptr && *end == '\0' &&
                      value == static_cast<long double>(row_id),
                  "DECIMAL(18,4)", row_id);
}

// Every generated string is a prefix of the repeating cycle, so the expected byte
// at any position is a pure function of that position.
bool matches_text_cycle_narrow(const unsigned char* bytes, std::size_t characters) {
    for (std::size_t index = 0; index < characters; ++index) {
        if (bytes[index] != static_cast<unsigned char>(kTextCycle[index % kTextCycle.size()])) {
            return false;
        }
    }
    return bytes[characters] == 0;
}

// The wide form copies through an aligned buffer first: a bound SQLWCHAR array
// is only guaranteed to be aligned at the start of the row slot.
bool matches_text_cycle_wide(const unsigned char* bytes, std::size_t characters) {
    std::vector<SQLWCHAR> value(characters + 1);
    std::memcpy(value.data(), bytes, (characters + 1) * sizeof(SQLWCHAR));
    for (std::size_t index = 0; index < characters; ++index) {
        if (value[index] !=
            static_cast<SQLWCHAR>(kTextCycle[index % kTextCycle.size()])) {
            return false;
        }
    }
    return value[characters] == 0;
}

// Check each conversion shape on representative rows without adding work to timing.
void validate_representative_value(const ColumnBuffer& buffer, std::size_t row,
                                   std::uint64_t row_id) {
    const auto& column = buffer.column();
    if (buffer.indicator(row) == SQL_NULL_DATA) {
        return;
    }
    switch (column.kind) {
        case ValueKind::bit:
            require_value(buffer.read<SQLCHAR>(row) == row_id % 2, "BIT", row_id);
            break;
        case ValueKind::tinyint:
            require_value(buffer.read<SQLCHAR>(row) == row_id % 251, "TINYINT",
                          row_id);
            break;
        case ValueKind::smallint:
            require_value(
                buffer.read<SQLSMALLINT>(row) ==
                    static_cast<SQLSMALLINT>(
                        static_cast<std::int64_t>(row_id % 60001) - 30000),
                "SMALLINT", row_id);
            break;
        case ValueKind::integer:
            require_value(buffer.read<SQLINTEGER>(row) ==
                              static_cast<SQLINTEGER>(row_id),
                          "INT", row_id);
            break;
        case ValueKind::bigint:
            require_value(buffer.read<SQLBIGINT>(row) ==
                              static_cast<SQLBIGINT>(row_id),
                          "BIGINT", row_id);
            break;
        case ValueKind::real:
            require_value(buffer.read<float>(row) ==
                              static_cast<float>(row_id % 10000),
                          "REAL", row_id);
            break;
        case ValueKind::double_precision:
            require_value(buffer.read<double>(row) ==
                              static_cast<double>(row_id % 1000000),
                          "FLOAT(53)", row_id);
            break;
        case ValueKind::decimal:
            validate_decimal(buffer, row, row_id);
            break;
        case ValueKind::date: {
            const auto value = buffer.read<SQL_DATE_STRUCT>(row);
            require_value(value.year == 2024 && value.month == 2 && value.day == 29,
                          "DATE", row_id);
            break;
        }
        case ValueKind::time: {
            const auto value = buffer.read<SqlSsTime2>(row);
            require_value(value.hour == 12 && value.minute == 34 &&
                              value.second == 56 && value.fraction == 123456700,
                          "TIME(7)", row_id);
            break;
        }
        case ValueKind::datetime2: {
            const auto value = buffer.read<SQL_TIMESTAMP_STRUCT>(row);
            require_value(
                value.year == 2024 && value.month == 2 && value.day == 29 &&
                    value.hour == 12 && value.minute == 34 && value.second == 56 &&
                    value.fraction == 123456700,
                "DATETIME2(7)", row_id);
            break;
        }
        case ValueKind::datetimeoffset: {
            const auto value = buffer.read<SqlSsTimestampOffset>(row);
            require_value(
                value.year == 2024 && value.month == 2 && value.day == 29 &&
                    value.hour == 12 && value.minute == 34 && value.second == 56 &&
                    value.fraction == 123456700 && value.timezone_hour == 0 &&
                    value.timezone_minute == 0,
                "DATETIMEOFFSET(7)", row_id);
            break;
        }
        case ValueKind::guid: {
            const auto value = buffer.read<SqlGuid>(row);
            const SQLCHAR tail[] = {0x88, 0x99, 0xAA, 0xBB,
                                    0xCC, 0xDD, 0xEE, 0xFF};
            require_value(value.data1 == 0x00112233 && value.data2 == 0x4455 &&
                              value.data3 == 0x6677 &&
                              std::memcmp(value.data4, tail, sizeof(tail)) == 0,
                          "UNIQUEIDENTIFIER", row_id);
            break;
        }
        case ValueKind::character:
            require_value(
                std::memcmp(buffer.row_bytes(row), "ODBCBEN1", 8) == 0 &&
                    buffer.row_bytes(row)[8] == 0,
                "CHAR(8)", row_id);
            break;
        case ValueKind::wide_character: {
            const auto* value =
                reinterpret_cast<const SQLWCHAR*>(buffer.row_bytes(row));
            constexpr char expected[] = "ODBCWIDE";
            bool matches = value[8] == 0;
            for (std::size_t index = 0; index < 8 && matches; ++index) {
                matches =
                    value[index] == static_cast<SQLWCHAR>(expected[index]);
            }
            require_value(matches, "NCHAR(8)", row_id);
            break;
        }
        case ValueKind::var_character:
            require_value(matches_text_cycle_narrow(buffer.row_bytes(row),
                                                    generated_length(column, row_id)),
                          "VARCHAR(n)", row_id);
            break;
        case ValueKind::var_wide_character:
            require_value(matches_text_cycle_wide(buffer.row_bytes(row),
                                                  generated_length(column, row_id)),
                          "NVARCHAR(n)", row_id);
            break;
        case ValueKind::lob_character:
        case ValueKind::lob_wide_character:
        case ValueKind::variant_integer:
        case ValueKind::variant_bigint:
        case ValueKind::variant_text:
            // Validated where they are read: their payload never lands in a row
            // slot, so there is nothing here to inspect.
            break;
    }
}

// Proves that fetching returns every row exactly once and that every conversion
// family produces the expected representation before timing is trusted.
class PreflightValidator {
public:
    PreflightValidator(const TableSpec& table, std::size_t row_id_column)
        : table_(table),
          row_id_column_(row_id_column),
          seen_(static_cast<std::size_t>(table.row_count + 1), false) {
        // Cover the ends, the interior, and both sides of a 1000-row rowset
        // boundary, so an off-by-one at a batch edge cannot hide.
        representatives_ = {1, 2, kFetchAllRowset, kFetchAllRowset + 1,
                            table.row_count / 2, table.row_count};
        std::sort(representatives_.begin(), representatives_.end());
        representatives_.erase(
            std::unique(representatives_.begin(), representatives_.end()),
            representatives_.end());
        representatives_.erase(
            std::remove_if(representatives_.begin(), representatives_.end(),
                           [&table](std::uint64_t row_id) {
                               return row_id == 0 || row_id > table.row_count;
                           }),
            representatives_.end());
        representative_seen_.assign(representatives_.size(), false);
        for (std::uint64_t row_id = 1; row_id <= table_.row_count; ++row_id) {
            expected_checksum_ += splitmix64(row_id);
        }
    }

    // Fold one row's identity into the completeness check and return its row id so
    // the caller can validate anything that never reaches a row slot.
    std::uint64_t accept_row(const std::vector<ColumnBuffer>& buffers,
                             std::size_t row_index) {
        const SQLINTEGER signed_id =
            buffers[row_id_column_].read<SQLINTEGER>(row_index);
        if (signed_id <= 0 ||
            static_cast<std::uint64_t>(signed_id) > table_.row_count) {
            throw std::runtime_error("preflight found an out-of-range row id");
        }
        const auto row_id = static_cast<std::uint64_t>(signed_id);
        if (seen_[static_cast<std::size_t>(row_id)]) {
            throw std::runtime_error("preflight found a duplicate row id");
        }
        seen_[static_cast<std::size_t>(row_id)] = true;
        ++accepted_rows_;
        checksum_ += splitmix64(row_id);

        for (const auto& buffer : buffers) {
            if (is_lob_kind(buffer.column().kind) ||
                is_variant_kind(buffer.column().kind)) {
                continue;
            }
            validate_indicator(buffer, row_index, row_id);
        }

        if (is_representative(row_id)) {
            for (const auto& buffer : buffers) {
                validate_representative_value(buffer, row_index, row_id);
            }
        }
        return row_id;
    }

    // Validate one bound rowset by folding each of its rows in turn.
    void accept_rowset(const std::vector<ColumnBuffer>& buffers, SQLULEN rows_fetched) {
        for (SQLULEN row = 0; row < rows_fetched; ++row) {
            (void)accept_row(buffers, static_cast<std::size_t>(row));
        }
    }

    // Record that a chosen row was seen, so finish() can insist the expensive
    // per-value checks actually ran instead of being skipped by a short result.
    bool is_representative(std::uint64_t row_id) {
        const auto entry = std::lower_bound(representatives_.begin(),
                                            representatives_.end(), row_id);
        if (entry == representatives_.end() || *entry != row_id) {
            return false;
        }
        representative_seen_[static_cast<std::size_t>(entry - representatives_.begin())] =
            true;
        return true;
    }

    // Reject a run unless counts, checksum, and representative values all completed.
    std::uint64_t finish() const {
        if (accepted_rows_ != table_.row_count) {
            throw std::runtime_error("preflight row count did not match the workload");
        }
        if (checksum_ != expected_checksum_) {
            throw std::runtime_error("preflight deterministic row checksum did not match");
        }
        if (std::find(representative_seen_.begin(), representative_seen_.end(),
                      false) != representative_seen_.end()) {
            throw std::runtime_error("preflight did not see every representative row");
        }
        return checksum_;
    }

private:
    const TableSpec& table_;
    std::size_t row_id_column_;
    std::vector<bool> seen_;
    std::vector<std::uint64_t> representatives_;
    std::vector<bool> representative_seen_;
    std::uint64_t accepted_rows_ = 0;
    std::uint64_t checksum_ = 0;
    std::uint64_t expected_checksum_ = 0;
};

}  // namespace

// Holds mutable ODBC fetch state behind the stable public runner interface.
class WorkloadRunner::Impl {
public:
    Impl(OdbcSession& session, const WorkloadSpec& spec)
        : session_(session),
          spec_(spec),
          columns_(columns_for(*spec.table)),
          query_(to_sql_string(select_sql(*spec.table, columns_))),
          logical_bytes_(spec.table->logical_bytes_total()) {
        const bool row_wise = spec_.access == AccessMode::row_wise_get_data;
        const std::size_t slots = row_wise ? 1 : spec_.rowset_size;
        if (spec_.rowset_size == 0 || spec_.rowset_size > kMaxRowsetSize) {
            throw std::logic_error("workload rowset size is outside the supported range");
        }
        buffers_.reserve(columns_.size());
        std::uint64_t fixed_row_bytes = 0;
        for (const auto& column : columns_) {
            if (!row_wise && (is_lob_kind(column.kind) || is_variant_kind(column.kind))) {
                // Binding either shape is not merely slower, it is unsupported:
                // mssql-odbc answers a bound PLP column with SQL_ROW_ERROR
                // (AB#47361), and a variant needs the probe before its type is
                // even known. Both belong to a row-at-a-time workload.
                throw std::logic_error(
                    "LOB and sql_variant columns cannot be bound; use row-wise access");
            }
            buffers_.emplace_back(column, slots);
            fixed_row_bytes += column.logical_bytes;
        }
        variant_types_.assign(columns_.size(), 0);
        lob_bytes_.assign(columns_.size(), 0);
        lob_calls_.assign(columns_.size(), 0);
        chunk_.resize(kLobChunkBytes);
        if (row_wise) {
            // The row-wise reader validates a LOB or variant value as it reads it,
            // which needs the row id, and ODBC only allows SQLGetData to move
            // forward through the columns. A shape that put either kind before its
            // identity column would validate against row id 0 forever.
            const std::size_t identity = spec_.table->row_id_column();
            for (std::size_t index = 0; index <= identity && index < columns_.size();
                 ++index) {
                if (is_lob_kind(columns_[index].kind) ||
                    is_variant_kind(columns_[index].kind)) {
                    throw std::logic_error(
                        "LOB and sql_variant columns must follow the identity column");
                }
            }
        }
        if (spec_.table->shape == TableShape::fixed_pattern && fixed_row_bytes >= 8060) {
            throw std::logic_error(
                "generated fixed-width row is not below SQL Server's 8060-byte limit");
        }
    }

    // Return the catalog entry the benchmark registration names this run after.
    const WorkloadSpec& spec() const {
        return spec_;
    }

    // Run the identical retrieval path with validation enabled and timing ignored.
    void preflight() {
        PreflightValidator validator(*spec_.table, spec_.table->row_id_column());
        const auto metrics = run(&validator);
        const auto checksum = validator.finish();
        std::ostringstream message;
        message << "Preflight passed for " << spec_.benchmark_name << ": "
                << metrics.rows << " rows, " << columns_.size() << " columns, "
                << metrics.get_data_calls << " SQLGetData calls, checksum=0x"
                << std::hex << std::setw(16) << std::setfill('0') << checksum;
        std::cerr << message.str() << '\n';
    }

    // Run the production measurement path without per-cell validation overhead.
    RetrievalMetrics retrieve() {
        return run(nullptr);
    }

private:
    // One statement handle is shared by every workload in the process, so each
    // run() is responsible for leaving it in a clean, known state.
    SQLHSTMT stmt() const {
        return session_.statement();
    }

    // Column-wise binding is the only layout mssql-python uses and the only one
    // mssql-odbc implements, so it is set once and never varied.
    void set_bind_type() {
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_ROW_BIND_TYPE,
                           attribute_value(SQL_BIND_BY_COLUMN), 0),
            "SQLSetStmtAttr(SQL_ATTR_ROW_BIND_TYPE)", SQL_HANDLE_STMT, stmt());
    }

    // Install the rowset shape. `track` mirrors mssql-python, which points
    // SQL_ATTR_ROWS_FETCHED_PTR at its own counter for a batch and clears it
    // again afterwards so no stale pointer survives the call.
    void set_rowset(SQLULEN size, bool track) {
        rows_fetched_ = 0;
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_ROW_ARRAY_SIZE, attribute_value(size), 0),
            "SQLSetStmtAttr(SQL_ATTR_ROW_ARRAY_SIZE)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_ROWS_FETCHED_PTR,
                           track ? &rows_fetched_ : nullptr, 0),
            "SQLSetStmtAttr(SQL_ATTR_ROWS_FETCHED_PTR)", SQL_HANDLE_STMT, stmt());
    }

    // Ask the driver for the result shape. Every access mode issues these calls,
    // but only preflight compares the answers, so measurement and validation
    // perform the same driver work.
    void describe_columns(bool validate) {
        SQLSMALLINT result_columns = 0;
        require_exact_success(SQLNumResultCols(stmt(), &result_columns),
                              "SQLNumResultCols", SQL_HANDLE_STMT, stmt());
        if (validate && result_columns != static_cast<SQLSMALLINT>(columns_.size())) {
            std::ostringstream message;
            message << "result has " << result_columns << " columns; expected "
                    << columns_.size();
            throw std::runtime_error(message.str());
        }
        for (std::size_t index = 0; index < columns_.size(); ++index) {
            describe_column(index, validate);
        }
    }

    // Describe one column, comparing the answer against the catalog only when
    // validating, so measurement and preflight make identical driver calls.
    void describe_column(std::size_t index, bool validate) {
        SQLTCHAR name[32] = {};
        SQLSMALLINT name_length = 0;
        SQLSMALLINT data_type = 0;
        SQLULEN column_size = 0;
        SQLSMALLINT decimal_digits = 0;
        SQLSMALLINT nullable = SQL_NULLABLE_UNKNOWN;
        require_exact_success(
            SQLDescribeCol(stmt(), static_cast<SQLUSMALLINT>(index + 1), name,
                           static_cast<SQLSMALLINT>(std::size(name)), &name_length,
                           &data_type, &column_size, &decimal_digits, &nullable),
            "SQLDescribeCol", SQL_HANDLE_STMT, stmt());
        if (!validate) {
            return;
        }

        const auto& column = columns_[index];
        bool name_matches = name_length == static_cast<SQLSMALLINT>(column.name.size());
        for (std::size_t position = 0; position < column.name.size() && name_matches;
             ++position) {
            name_matches =
                name[position] ==
                static_cast<SQLTCHAR>(static_cast<unsigned char>(column.name[position]));
        }
        const bool nullability_matches =
            column.null_phase == 0 ? nullable == SQL_NO_NULLS : nullable == SQL_NULLABLE;
        if (!name_matches || !nullability_matches || data_type == 0) {
            throw std::runtime_error(
                "SQLDescribeCol returned unexpected workload metadata");
        }
        (void)decimal_digits;
        (void)column_size;
    }

    // Bind every column to its row-array slot. BufferLength is the slot stride,
    // which is what places the next row's value inside the caller's storage.
    void bind_columns() {
        for (std::size_t index = 0; index < columns_.size(); ++index) {
            auto& buffer = buffers_[index];
            require_exact_success(
                SQLBindCol(stmt(), static_cast<SQLUSMALLINT>(index + 1),
                           buffer.column().c_type, buffer.data(),
                           static_cast<SQLLEN>(buffer.column().slot_size),
                           buffer.indicators()),
                "SQLBindCol", SQL_HANDLE_STMT, stmt());
        }
    }

    // Drop every binding. mssql-python calls this after each fetchmany(), and
    // leaving one behind would let a later leg fetch into a stale buffer.
    void unbind_columns() {
        require_exact_success(SQLFreeStmt(stmt(), SQL_UNBIND),
                              "SQLFreeStmt(SQL_UNBIND)", SQL_HANDLE_STMT, stmt());
    }

    // Require successful cleanup because stale bindings would contaminate later legs.
    void cleanup_statement() {
        require_exact_success(SQLCloseCursor(stmt()), "SQLCloseCursor",
                              SQL_HANDLE_STMT, stmt());
        unbind_columns();
    }

    // Best-effort reset preserves the original ODBC error during stack unwinding.
    void cleanup_statement_noexcept() noexcept {
        SQLFreeStmt(stmt(), SQL_CLOSE);
        SQLFreeStmt(stmt(), SQL_UNBIND);
    }

    // Drain one PLP value the way mssql-python's FetchLobColumnData does: repeated
    // fixed 8192-byte SQLGetData calls, each answered with SQL_SUCCESS_WITH_INFO
    // until the final one returns SQL_SUCCESS. The accumulating buffer is reused
    // across values rather than reallocated per value; the consumer allocates a
    // fresh one, and paying that allocator cost on every value would measure the
    // allocator rather than the driver.
    void read_lob_column(std::size_t index, bool validate, std::uint64_t row_id) {
        const auto& column = columns_[index];
        lob_bytes_[index] = 0;
        lob_calls_[index] = 0;
        lob_payload_.clear();
        for (;;) {
            SQLLEN indicator = kIndicatorSentinel;
            const SQLRETURN rc =
                SQLGetData(stmt(), static_cast<SQLUSMALLINT>(index + 1), column.c_type,
                           chunk_.data(), static_cast<SQLLEN>(kLobChunkBytes),
                           &indicator);
            ++lob_calls_[index];
            ++get_data_calls_;
            // A driver may end the value with SQL_NO_DATA instead of a final
            // SQL_SUCCESS. mssql-python would raise there; the benchmark accepts
            // it so a legal ending cannot be reported as a failed measurement.
            if (rc == SQL_NO_DATA) {
                break;
            }
            require_succeeded(rc, "SQLGetData(LOB chunk)", stmt());
            if (indicator == SQL_NULL_DATA) {
                if (validate) {
                    require_value(generated_null(column, row_id),
                                  "unexpected NULL LOB", row_id);
                }
                buffers_[index].set_indicator(0, SQL_NULL_DATA);
                return;
            }

            // A driver that knows the remaining length reports it; one streaming
            // an unbounded PLP value reports SQL_NO_TOTAL. Either way a value
            // longer than the buffer means the call filled it.
            std::size_t payload = kLobChunkBytes;
            if (indicator >= 0 && static_cast<std::size_t>(indicator) < kLobChunkBytes) {
                payload = static_cast<std::size_t>(indicator);
            }
            // The driver writes a terminator inside the buffer, so a filled chunk
            // carries fewer payload bytes than it is long. Trimming trailing NUL
            // units recovers the exact payload, as mssql-python's
            // FetchLobColumnData does; it is exact here because the generated
            // text contains no embedded NUL.
            payload -= payload % column.unit_bytes;
            while (payload >= column.unit_bytes &&
                   chunk_[payload - 1] == 0 &&
                   (column.unit_bytes == 1 || chunk_[payload - 2] == 0)) {
                payload -= column.unit_bytes;
            }
            if (payload > 0) {
                lob_payload_.insert(lob_payload_.end(), chunk_.begin(),
                                    chunk_.begin() + static_cast<std::ptrdiff_t>(payload));
            }
            if (rc == SQL_SUCCESS) {
                break;
            }
            if (payload == 0) {
                throw std::runtime_error(
                    "SQLGetData reported more LOB data but delivered no bytes");
            }
        }

        lob_bytes_[index] = lob_payload_.size();
        buffers_[index].set_indicator(0, static_cast<SQLLEN>(lob_payload_.size()));
        if (validate) {
            validate_lob_value(index, row_id);
        }
    }

    // Check the drained value's length, its content, and that draining it really
    // needed more than one chunk where the workload says it should.
    void validate_lob_value(std::size_t index, std::uint64_t row_id) {
        const auto& column = columns_[index];
        require_value(!generated_null(column, row_id), "missing NULL LOB", row_id);
        const std::size_t characters = generated_length(column, row_id);
        const std::size_t expected_bytes = characters * column.unit_bytes;
        require_value(lob_payload_.size() == expected_bytes, "LOB payload length",
                      row_id);
        if (column.kind == ValueKind::lob_wide_character) {
            std::vector<SQLWCHAR> value(characters);
            std::memcpy(value.data(), lob_payload_.data(), expected_bytes);
            for (std::size_t position = 0; position < characters; ++position) {
                require_value(value[position] == static_cast<SQLWCHAR>(
                                                     kTextCycle[position % kTextCycle.size()]),
                              "NVARCHAR(MAX) content", row_id);
            }
        } else {
            for (std::size_t position = 0; position < characters; ++position) {
                require_value(lob_payload_[position] ==
                                  static_cast<unsigned char>(
                                      kTextCycle[position % kTextCycle.size()]),
                              "VARCHAR(MAX) content", row_id);
            }
        }
        // A single-call value would mean the continuation loop this workload
        // exists to measure never ran, so it is a validation failure, not a
        // faster result.
        const std::size_t expected_calls = expected_bytes / kLobChunkBytes + 1;
        if (expected_calls > 1) {
            require_value(lob_calls_[index] >= expected_calls,
                          "LOB continuation call count", row_id);
        }
    }

    // Reproduce mssql-python's sql_variant sequence exactly: a zero-length
    // SQL_C_BINARY probe (which both detects NULL and makes the driver resolve the
    // value's type), then SQLColAttribute(SQL_CA_SS_VARIANT_TYPE), then the read.
    void read_variant_column(std::size_t index, bool validate, std::uint64_t row_id) {
        const auto ordinal = static_cast<SQLUSMALLINT>(index + 1);
        SQLLEN probe_indicator = kIndicatorSentinel;
        // The probe is zero-length, but the buffer pointer is real. mssql-python
        // passes NULL here and gets away with it because it dlopens the driver and
        // calls its exports directly; this harness goes through the Driver
        // Manager, which rejects a NULL TargetValuePtr with HY009 before the
        // driver ever sees the call. A valid pointer with BufferLength 0 is the
        // same request as far as both drivers are concerned — mssql-odbc gates its
        // probe on `buffer_length == 0` alone, and msodbcsql treats it as an
        // ordinary length probe.
        require_succeeded(
            SQLGetData(stmt(), ordinal, SQL_C_BINARY, probe_sink_.data(), 0,
                       &probe_indicator),
            "SQLGetData(sql_variant probe)", stmt());
        ++get_data_calls_;
        if (probe_indicator == SQL_NULL_DATA) {
            buffers_[index].set_indicator(0, SQL_NULL_DATA);
            variant_types_[index] = 0;
            if (validate) {
                require_value(generated_null(columns_[index], row_id),
                              "unexpected NULL sql_variant", row_id);
            }
            return;
        }

        SQLLEN reported_type = 0;
        require_succeeded(SQLColAttribute(stmt(), ordinal, kSqlCaSsVariantType, nullptr,
                                          0, nullptr, &reported_type),
                          "SQLColAttribute(SQL_CA_SS_VARIANT_TYPE)", stmt());
        const SQLSMALLINT read_type = variant_read_c_type(reported_type);
        variant_types_[index] = read_type;

        auto& buffer = buffers_[index];
        SQLLEN indicator = kIndicatorSentinel;
        require_succeeded(
            SQLGetData(stmt(), ordinal, read_type, buffer.data(),
                       static_cast<SQLLEN>(buffer.column().slot_size), &indicator),
            "SQLGetData(sql_variant value)", stmt());
        ++get_data_calls_;
        buffer.set_indicator(0, indicator);
        if (validate) {
            validate_variant_value(index, row_id);
        }
    }

    // Compare the delivered value against the generator, accepting either the
    // typed or the character rendering. Which one a driver picks is a documented
    // parity difference, but the value it stands for must be the same.
    void validate_variant_value(std::size_t index, std::uint64_t row_id) {
        const auto& buffer = buffers_[index];
        const auto& column = buffer.column();
        require_value(!generated_null(column, row_id), "missing NULL sql_variant",
                      row_id);
        require_value(buffer.indicator(0) > 0, "sql_variant indicator", row_id);
        const SQLSMALLINT read_type = variant_types_[index];
        if (column.kind == ValueKind::variant_text) {
            const auto length = static_cast<std::size_t>(buffer.indicator(0));
            const char* text = reinterpret_cast<const char*>(buffer.row_bytes(0));
            require_value(length == 11 && std::memcmp(text, "ODBCVARIANT", 11) == 0,
                          "sql_variant NVARCHAR value", row_id);
            return;
        }

        std::int64_t value = 0;
        if (read_type == SQL_C_SLONG) {
            value = buffer.read<SQLINTEGER>(0);
        } else if (read_type == SQL_C_SBIGINT) {
            value = static_cast<std::int64_t>(buffer.read<SQLBIGINT>(0));
        } else if (read_type == SQL_C_CHAR) {
            const char* text = reinterpret_cast<const char*>(buffer.row_bytes(0));
            errno = 0;
            char* end = nullptr;
            value = static_cast<std::int64_t>(std::strtoll(text, &end, 10));
            require_value(errno == 0 && end != text && end != nullptr && *end == '\0',
                          "sql_variant character rendering", row_id);
        } else {
            require_value(false, "unexpected sql_variant C type", row_id);
        }
        require_value(value == static_cast<std::int64_t>(row_id),
                      "sql_variant integer value", row_id);
    }

    // Read one column of the current row. Every column of a result that contains a
    // LOB or a sql_variant travels this path, which is why the ordinary inline
    // columns are read here too rather than being bound.
    void read_row_wise_column(std::size_t index, bool validate, std::uint64_t row_id) {
        const auto& column = columns_[index];
        // mssql-python re-describes every column on every row inside
        // SQLGetData_wrap, so the metadata call is part of the consumer-visible
        // cost of this path and belongs inside the measurement.
        describe_column(index, validate);
        if (is_lob_kind(column.kind)) {
            read_lob_column(index, validate, row_id);
            return;
        }
        if (is_variant_kind(column.kind)) {
            read_variant_column(index, validate, row_id);
            return;
        }

        auto& buffer = buffers_[index];
        SQLLEN indicator = kIndicatorSentinel;
        require_succeeded(
            SQLGetData(stmt(), static_cast<SQLUSMALLINT>(index + 1), column.c_type,
                       buffer.data(), static_cast<SQLLEN>(column.slot_size), &indicator),
            "SQLGetData", stmt());
        ++get_data_calls_;
        buffer.set_indicator(0, indicator);
    }

    // Sum the delivered payload for one row without re-walking the data. Used only
    // to confirm the row-wise path actually delivered something for every column;
    // reported throughput uses the generator's own byte total so that all three
    // drivers are credited with identical payload regardless of representation.
    std::uint64_t row_payload_bytes() const {
        std::uint64_t total = 0;
        for (std::size_t index = 0; index < buffers_.size(); ++index) {
            const auto indicator = buffers_[index].indicator(0);
            if (indicator == SQL_NULL_DATA) {
                continue;
            }
            total += is_lob_kind(columns_[index].kind)
                         ? lob_bytes_[index]
                         : static_cast<std::uint64_t>(indicator);
        }
        return total;
    }

    // Measure from SQL execution through terminal SQL_NO_DATA. This is the
    // regression boundary; connection, buffer allocation, setup, untimed
    // preflight, cursor close, and unbind are all deliberately outside it.
    RetrievalMetrics run(PreflightValidator* validator) {
        const bool validate = validator != nullptr;
        get_data_calls_ = 0;
        set_bind_type();
        // Every mode starts from an explicit rowset state so one workload cannot
        // inherit the shape another left on the shared statement handle.
        if (spec_.access == AccessMode::bound_drain) {
            set_rowset(static_cast<SQLULEN>(spec_.rowset_size), true);
        } else {
            // Row-wise fetching and the bind-cycle's between-call state are both a
            // one-row rowset with no fetched-row counter, which is exactly the
            // state mssql-python restores after every fetchmany().
            set_rowset(1, false);
        }

        try {
            const auto start = std::chrono::steady_clock::now();
            require_exact_success(SQLExecDirect(stmt(), query_.data(), SQL_NTS),
                                  "SQLExecDirect", SQL_HANDLE_STMT, stmt());
            const auto after_execute = std::chrono::steady_clock::now();

            std::uint64_t rows = 0;
            std::chrono::steady_clock::time_point after_bind = after_execute;
            switch (spec_.access) {
                case AccessMode::bound_drain:
                    describe_columns(validate);
                    bind_columns();
                    after_bind = std::chrono::steady_clock::now();
                    rows = drain_bound(validator);
                    break;
                case AccessMode::bound_bind_cycle:
                    rows = drain_bind_cycle(validator);
                    break;
                case AccessMode::row_wise_get_data:
                    describe_columns(validate);
                    after_bind = std::chrono::steady_clock::now();
                    rows = drain_row_wise(validator);
                    break;
            }

            if (rows != spec_.table->row_count) {
                std::ostringstream message;
                message << "retrieval fetched " << rows << " rows; expected "
                        << spec_.table->row_count;
                throw std::runtime_error(message.str());
            }
            const auto end = std::chrono::steady_clock::now();

            RetrievalMetrics metrics;
            metrics.rows = rows;
            metrics.cells = rows * static_cast<std::uint64_t>(columns_.size());
            metrics.logical_bytes = logical_bytes_;
            metrics.get_data_calls = get_data_calls_;
            metrics.total_seconds = seconds_between(start, end);
            metrics.execute_seconds = seconds_between(start, after_execute);
            metrics.metadata_bind_seconds = seconds_between(after_execute, after_bind);
            if (spec_.access == AccessMode::bound_bind_cycle) {
                // Describe/bind repeats inside the fetch loop for this workload,
                // so there is no honest one-time metadata phase to report.
                metrics.metadata_bind_seconds = -1.0;
            }
            metrics.fetch_seconds = seconds_between(after_bind, end);

            cleanup_statement();
            return metrics;
        } catch (...) {
            cleanup_statement_noexcept();
            throw;
        }
    }

    // Bind once, then fetch rowsets until the cursor is exhausted.
    std::uint64_t drain_bound(PreflightValidator* validator) {
        std::uint64_t rows = 0;
        for (;;) {
            const SQLULEN fetched = fetch_one_rowset(validator, rows);
            if (fetched == 0) {
                break;
            }
            rows += fetched;
        }
        return rows;
    }

    // Repeat the complete describe/bind/fetch/reset/unbind lifecycle that
    // mssql-python's fetchmany() performs on every call, so the pair with the
    // matching bound_drain workload isolates exactly that per-call cost.
    std::uint64_t drain_bind_cycle(PreflightValidator* validator) {
        const bool validate = validator != nullptr;
        std::uint64_t rows = 0;
        for (;;) {
            describe_columns(validate);
            bind_columns();
            set_rowset(static_cast<SQLULEN>(spec_.rowset_size), true);
            const SQLULEN fetched = fetch_one_rowset(validator, rows);
            set_rowset(1, false);
            unbind_columns();
            if (fetched == 0) {
                break;
            }
            rows += fetched;
        }
        return rows;
    }

    // One SQLFetchScroll, validated for rowset sanity. Returns 0 at end of cursor.
    SQLULEN fetch_one_rowset(PreflightValidator* validator, std::uint64_t rows_so_far) {
        if (validator != nullptr) {
            for (auto& buffer : buffers_) {
                buffer.reset_indicators();
            }
        }
        // Microsoft ODBC leaves this unchanged on terminal SQL_NO_DATA.
        rows_fetched_ = 0;
        const SQLRETURN rc = SQLFetchScroll(stmt(), SQL_FETCH_NEXT, 0);
        if (rc == SQL_NO_DATA) {
            if (rows_fetched_ != 0) {
                throw std::runtime_error(
                    "SQLFetchScroll reported SQL_NO_DATA with rows fetched");
            }
            return 0;
        }
        require_exact_success(rc, "SQLFetchScroll", SQL_HANDLE_STMT, stmt());
        if (rows_fetched_ == 0 || rows_fetched_ > spec_.rowset_size ||
            rows_fetched_ > spec_.table->row_count ||
            rows_so_far > spec_.table->row_count - rows_fetched_) {
            throw std::runtime_error("SQLFetchScroll returned an invalid rowset size");
        }
        if (validator != nullptr) {
            validator->accept_rowset(buffers_, rows_fetched_);
        }
        return rows_fetched_;
    }

    // SQLFetch plus SQLGetData per column, which is what a LOB or sql_variant
    // column forces on the entire result.
    std::uint64_t drain_row_wise(PreflightValidator* validator) {
        const bool validate = validator != nullptr;
        std::uint64_t rows = 0;
        for (;;) {
            const SQLRETURN rc = SQLFetch(stmt());
            if (rc == SQL_NO_DATA) {
                break;
            }
            require_exact_success(rc, "SQLFetch", SQL_HANDLE_STMT, stmt());
            if (rows >= spec_.table->row_count) {
                throw std::runtime_error("SQLFetch returned more rows than the table holds");
            }

            // ODBC allows SQLGetData in column order only, and the constructor
            // has already checked that the identity column precedes every column
            // whose validation needs the row id.
            std::uint64_t row_id = 0;
            for (std::size_t index = 0; index < columns_.size(); ++index) {
                read_row_wise_column(index, validate, row_id);
                if (index == spec_.table->row_id_column()) {
                    row_id = static_cast<std::uint64_t>(
                        buffers_[index].read<SQLINTEGER>(0));
                }
            }
            if (validate) {
                (void)validator->accept_row(buffers_, 0);
                if (row_payload_bytes() == 0) {
                    throw std::runtime_error(
                        "preflight row delivered no payload from any column");
                }
            }
            ++rows;
        }
        return rows;
    }

    OdbcSession& session_;
    const WorkloadSpec& spec_;
    std::vector<ColumnSpec> columns_;
    std::vector<ColumnBuffer> buffers_;
    SqlString query_;
    SQLULEN rows_fetched_ = 0;
    std::uint64_t logical_bytes_ = 0;
    std::uint64_t get_data_calls_ = 0;
    std::vector<SQLSMALLINT> variant_types_;
    std::vector<std::uint64_t> lob_bytes_;
    std::vector<std::uint64_t> lob_calls_;
    std::vector<unsigned char> chunk_;
    std::vector<unsigned char> lob_payload_;
    // Non-null destination for the zero-length sql_variant probe; nothing is ever
    // written into it.
    std::array<unsigned char, 8> probe_sink_{};
};

// Allocate descriptors once per catalog workload; each iteration still rebinds.
WorkloadRunner::WorkloadRunner(OdbcSession& session, const WorkloadSpec& spec)
    : impl_(std::make_unique<Impl>(session, spec)) {}

WorkloadRunner::~WorkloadRunner() = default;

// Preserve the stable benchmark name selected by the shared workload catalog.
const WorkloadSpec& WorkloadRunner::spec() const {
    return impl_->spec();
}

// Validate correctness through the same implementation used by timed retrieval.
void WorkloadRunner::preflight() {
    impl_->preflight();
}

// Return all phase metrics from one complete result-set consumption.
RetrievalMetrics WorkloadRunner::retrieve() {
    return impl_->retrieve();
}

}  // namespace mssql::odbc::bench

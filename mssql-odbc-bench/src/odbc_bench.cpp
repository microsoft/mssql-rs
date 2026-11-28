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
#include <map>
#include <optional>
#include <sstream>
#include <stdexcept>
#include <string_view>
#include <tuple>
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
constexpr std::size_t kParameterArrayRows = 2000;
constexpr std::size_t kParameterTextCapacity = 16;
constexpr char kParameterArrayTable[] = "[dbo].[mssql_odbc_bench_param_array]";
constexpr char kParameterArrayBenchmark[] = "write/insert_2000_c3/executemany";
constexpr std::size_t kExecuteManyRows = 1000;

// Insert targets of the pywrite scenario. Columns use ordinary application types
// (INT, NVARCHAR, DATETIME2, DECIMAL), not the narrowed parameter types, so the
// server converts some values; that conversion is identical on every driver leg.
struct ExecuteManyTarget {
    const char* table;
    const char* benchmark;
    const char* columns_ddl;
};

constexpr ExecuteManyTarget kExecuteManyTargets[] = {
    {"[dbo].[mssql_odbc_bench_pyshape_c3]",
     "write/insert_1000_rows_c3_33kb/prepare_bind_execute",
     "[id] INT NOT NULL, [name] NVARCHAR(50) NOT NULL, [created] DATETIME2(6) NOT NULL"},
    {"[dbo].[mssql_odbc_bench_pyshape_c10]",
     "write/insert_1000_rows_c10_128kb/prepare_bind_execute",
     "[id] INT NOT NULL, [qty] INT NOT NULL, [ref] INT NOT NULL, "
     "[name] NVARCHAR(50) NOT NULL, [city] NVARCHAR(30) NOT NULL, "
     "[note] NVARCHAR(100) NOT NULL, [created] DATETIME2(6) NOT NULL, "
     "[updated] DATETIME2(6) NOT NULL, [price] DECIMAL(18,4) NOT NULL, "
     "[total] DECIMAL(18,4) NOT NULL"},
};

const ExecuteManyTarget& execute_many_target(ExecuteManyShape shape) {
    return kExecuteManyTargets[shape == ExecuteManyShape::c3 ? 0 : 1];
}

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

// Append a Unicode scalar as UTF-16.
void append_code_point(std::u16string& output, std::uint32_t code_point) {
    if (code_point <= 0xFFFF) {
        output.push_back(static_cast<char16_t>(code_point));
        return;
    }
    code_point -= 0x10000;
    output.push_back(static_cast<char16_t>(0xD800 + (code_point >> 10)));
    output.push_back(static_cast<char16_t>(0xDC00 + (code_point & 0x3FF)));
}

// Validate pipeline-provided UTF-8 while converting it to UTF-16.
std::u16string utf8_to_utf16(std::string_view input) {
    std::u16string output;
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

#ifdef UNICODE
SqlString to_sql_string(std::string_view input) {
    const auto wide = utf8_to_utf16(input);
    return SqlString(wide.begin(), wide.end());
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
// VARBINARY is deliberately absent. mssql-odbc implements only the zero-length
// SQL_C_BINARY length probe; binary *delivery* into a real buffer is still
// HYC00 (AB#47239, `mssql-odbc/docs/typed-columnar-fetch-plan.md`), and
// binary-to-character hex rendering is not implemented either. Adding a
// VARBINARY column would fail the candidate and baseline legs while the
// Microsoft leg passed, which is a broken comparison rather than a measurement.
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
            // Both values need at least three 8192-byte calls. Transcoding may
            // deliver shorter chunks and require additional calls.
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
// Most row counts are chosen per access shape so one full retrieval takes
// roughly a second on mssql-odbc. The shared rowset-cadence table stays at
// 100,000 rows: increasing it to 1,000,000 did not materially improve
// confirmation stability and added about four minutes per run. A rowset-1
// workload issues one driver round trip per row, so it cannot use the same row
// count as a rowset-1000 one and still finish in comparable time.
const std::array<TableSpec, kTableCount>& tables() {
    static const std::array<TableSpec, kTableCount> catalog = {{
        {"mssql_odbc_bench_fixed_2m_c15", TableShape::fixed_pattern, 2'000'000, 1},
        {"mssql_odbc_bench_fixed_40k_c600", TableShape::fixed_pattern, 40'000, 40},
        {"mssql_odbc_bench_fixed_100k_c15", TableShape::fixed_pattern, 100'000, 1},
        {"mssql_odbc_bench_fixed_200k_c15", TableShape::fixed_pattern, 200'000, 1},
        {"mssql_odbc_bench_varwidth_100k_c7", TableShape::variable_width, 100'000, 0},
        {"mssql_odbc_bench_lobmax_10k_c3", TableShape::lob_max, 10'000, 0},
        {"mssql_odbc_bench_mixedlob_200k_c16", TableShape::mixed_lob, 200'000, 1},
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
        {"fetch/wide_40k_c600_fixed/bound_rowset_1000", "wide",
         &table_by_name("mssql_odbc_bench_fixed_40k_c600"), AccessMode::bound_drain,
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
        {"fetch/rowset_200k_c15_fixed/bind_cycle_rowset_1", "rowset",
         &table_by_name("mssql_odbc_bench_fixed_200k_c15"), AccessMode::bound_bind_cycle,
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
        {"getdata/rowwise_200k_c15_fixed/inline_values", "getdata",
         &table_by_name("mssql_odbc_bench_fixed_200k_c15"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
        // MAX text past one chunk, so every value needs repeated 8192-byte calls.
        {"getdata/rowwise_10k_c3_lob_max/chunked_8192", "getdata",
         &table_by_name("mssql_odbc_bench_lobmax_10k_c3"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
        // One small MAX column forcing all 16 columns onto the row-at-a-time path.
        {"getdata/rowwise_200k_c16_mixed_lob/whole_result_rowwise", "getdata",
         &table_by_name("mssql_odbc_bench_mixedlob_200k_c16"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
        // The sql_variant sequence: zero-length SQL_C_BINARY probe, then
        // SQLColAttribute(SQL_CA_SS_VARIANT_TYPE), then the typed read.
        {"getdata/rowwise_20k_c4_variant/probe_colattribute", "getdata",
         &table_by_name("mssql_odbc_bench_variant_20k_c4"), AccessMode::row_wise_get_data,
         kFetchManyDefaultRowset},
    }};
    return specs;
}

// Small calls where per-call driver overhead, not payload, decides latency. Batch
// sizes put one sample at about 1-2 s on msodbcsql18 and mssql-odbc.
const std::array<LatencySpec, kLatencyWorkloadCount>& latency_workloads() {
    static const std::array<LatencySpec, kLatencyWorkloadCount> specs = {{
        // The shortest unprepared round trip.
        {"latency/exec_direct_select1", 0, 5'000},
        // Statement text large enough that client-side scanning and UTF-16
        // handling dominate; the trailing comment keeps server work trivial.
        {"latency/exec_direct_16k_text", 16'384, 2'000},
    }};
    return specs;
}

// mssql-python `performance/scenarios/odbc_path_workloads.py` and `workloads.py`
// workloads whose ODBC path no other benchmark here already measures. Sample sizes put one sample at 0.8-1.1 s on
// msodbcsql18 18.7 against a local SQL Server 2022.
const std::array<PyCursorSpec, kPyCursorWorkloadCount>& py_cursor_workloads() {
    constexpr std::size_t kMiB = 1024 * 1024;
    static const std::array<PyCursorSpec, kPyCursorWorkloadCount> specs = {{
        {"param/bind_1_int_4b/execute_fetchone", "params", "query_scalar",
         PyCursorWorkload::query_scalar, 1, 5'000},
        {"param/bind_100_int_400b/execute_fetchone", "params", "param_count_int_100",
         PyCursorWorkload::param_count_int, 100, 4'000},
        {"param/bind_1000_int_4kb/execute_fetchone", "params", "param_count_int_1000",
         PyCursorWorkload::param_count_int, 1'000, 1'200},
        {"param/bind_100_nvarchar20_4kb/execute_fetchone", "params",
         "param_count_nvarchar_100", PyCursorWorkload::param_count_nvarchar, 100, 3'500},
        {"param/bind_10_mixed_types_100b/execute_fetchone", "params", "param_types_mixed_10",
         PyCursorWorkload::param_types_mixed, 10, 4'000},
        {"write/stream_varbinary_1mb/insert_putdata", "lobwrite",
         "insert_lob_varbinary_1mb", PyCursorWorkload::insert_lob_varbinary, kMiB, 120},
        {"write/stream_nvarchar_1mb/insert_putdata", "lobwrite",
         "insert_lob_nvarchar_1mb", PyCursorWorkload::insert_lob_nvarchar, kMiB, 150},
        {"fetch/mixed_1k_rows_c4_63kb/fetchmany_100", "cursorfetch",
         "fetchmany_mixed_1000_batch100", PyCursorWorkload::fetchmany_mixed, 1'000, 800},
    }};
    return specs;
}

// SQLAlchemy and Django operations, each the ODBC call sequence mssql-python 1.18
// was traced issuing for it. Sample sizes put one sample at roughly 1 s on
// msodbcsql18 against a local SQL Server 2022.
const std::array<OrmSpec, kOrmWorkloadCount>& orm_workloads() {
    static const std::array<OrmSpec, kOrmWorkloadCount> specs = {{
        {"orm/select1_4b/sqlalchemy_checkout_rollback", "orm",
         OrmWorkload::sqlalchemy_checkout_select1, 2'000},
        {"orm/get_by_pk_1_row_c2_8b/django_get", "orm", OrmWorkload::django_get_by_pk, 2'000},
        {"connect/sql_login_tls/connect_close", "connect", OrmWorkload::connect_close, 100},
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
    config.write_mode =
        environment_value_or("ODBC_BENCH_WRITE_MODE", "parameter_array");

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
        const bool known =
            config.scenario == "write" || config.scenario == kExecuteManyScenario ||
            config.scenario == "latency" ||
            std::any_of(workloads().begin(), workloads().end(),
                        [&config](const WorkloadSpec& spec) {
                            return config.scenario == spec.scenario;
                        }) ||
            std::any_of(py_cursor_workloads().begin(), py_cursor_workloads().end(),
                        [&config](const PyCursorSpec& spec) {
                            return config.scenario == spec.scenario;
                        }) ||
            std::any_of(orm_workloads().begin(), orm_workloads().end(),
                        [&config](const OrmSpec& spec) {
                            return config.scenario == spec.scenario;
                        });
        if (!known) {
            throw std::runtime_error(
                "ODBC_BENCH_SCENARIO must be one of narrow, wide, rowset, varwidth, "
                "getdata, write, pywrite, latency, params, lobwrite, cursorfetch, orm, "
                "connect, or unset");
        }
    }
    const auto calls_override = environment_value("ODBC_BENCH_CALLS");
    if (!calls_override.empty()) {
        if (calls_override.size() > 7 ||
            !std::all_of(calls_override.begin(), calls_override.end(),
                         [](unsigned char character) {
                             return std::isdigit(character) != 0;
                         })) {
            throw std::runtime_error(
                "ODBC_BENCH_CALLS must be an integer between 1 and 1000000");
        }
        config.calls_override = std::stoul(calls_override);
        if (config.calls_override == 0 || config.calls_override > 1'000'000) {
            throw std::runtime_error(
                "ODBC_BENCH_CALLS must be an integer between 1 and 1000000");
        }
    }
    if (config.write_mode != "parameter_array" && config.write_mode != "sequential") {
        throw std::runtime_error(
            "ODBC_BENCH_WRITE_MODE must be parameter_array or sequential");
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
        rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &admin_stmt_);
        require_exact_success(rc, "SQLAllocHandle(SQL_HANDLE_STMT admin)",
                              SQL_HANDLE_DBC, dbc_);
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
    if (admin_stmt_ != SQL_NULL_HSTMT) {
        SQLFreeHandle(SQL_HANDLE_STMT, admin_stmt_);
        admin_stmt_ = SQL_NULL_HSTMT;
    }
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

SQLHENV OdbcSession::environment() const {
    return env_;
}

SQLHDBC OdbcSession::connection() const {
    return dbc_;
}

// The runner owns statement state transitions; other code receives no ownership.
SQLHSTMT OdbcSession::statement() const {
    return stmt_;
}

SQLHSTMT OdbcSession::admin_statement() const {
    return admin_stmt_;
}

// Drain all results so the administrative statement is clean for its next use.
void OdbcSession::execute_non_query(const std::string& sql) {
    auto text = to_sql_string(sql);
    try {
        require_exact_success(SQLExecDirect(admin_stmt_, text.data(), SQL_NTS),
                              "SQLExecDirect", SQL_HANDLE_STMT, admin_stmt_);
        for (;;) {
            const SQLRETURN rc = SQLMoreResults(admin_stmt_);
            if (rc == SQL_NO_DATA) {
                break;
            }
            require_exact_success(rc, "SQLMoreResults", SQL_HANDLE_STMT, admin_stmt_);
        }
    } catch (...) {
        SQLFreeStmt(admin_stmt_, SQL_CLOSE);
        throw;
    }
}

void OdbcSession::set_autocommit(bool enabled) {
    const SQLULEN value = enabled ? SQL_AUTOCOMMIT_ON : SQL_AUTOCOMMIT_OFF;
    require_exact_success(
        SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT, attribute_value(value), 0),
        "SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT)", SQL_HANDLE_DBC, dbc_);
}

void OdbcSession::commit() {
    require_exact_success(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT),
                          "SQLEndTran(SQL_COMMIT)", SQL_HANDLE_DBC, dbc_);
}

void OdbcSession::rollback_noexcept() noexcept {
    if (dbc_ != SQL_NULL_HDBC) {
        SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK);
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
        require_exact_success(SQLExecDirect(admin_stmt_, text.data(), SQL_NTS),
                              "SQLExecDirect(COUNT_BIG)", SQL_HANDLE_STMT, admin_stmt_);
        SQLSMALLINT columns = 0;
        require_exact_success(SQLNumResultCols(admin_stmt_, &columns), "SQLNumResultCols",
                              SQL_HANDLE_STMT, admin_stmt_);
        if (columns != 1) {
            throw std::runtime_error("COUNT_BIG query returned the wrong column count");
        }
        require_exact_success(
            SQLBindCol(admin_stmt_, 1, SQL_C_SBIGINT, &count, sizeof(count), &indicator),
            "SQLBindCol(COUNT_BIG)", SQL_HANDLE_STMT, admin_stmt_);
        require_exact_success(SQLFetch(admin_stmt_), "SQLFetch(COUNT_BIG)",
                              SQL_HANDLE_STMT, admin_stmt_);
        if (indicator != static_cast<SQLLEN>(sizeof(count)) || count < 0) {
            throw std::runtime_error("COUNT_BIG query returned an invalid value");
        }
        const SQLRETURN final_fetch = SQLFetch(admin_stmt_);
        if (final_fetch != SQL_NO_DATA) {
            throw_odbc_error("SQLFetch(COUNT_BIG final)", final_fetch,
                             SQL_HANDLE_STMT, admin_stmt_);
        }
        require_exact_success(SQLCloseCursor(admin_stmt_), "SQLCloseCursor(COUNT_BIG)",
                              SQL_HANDLE_STMT, admin_stmt_);
        require_exact_success(SQLFreeStmt(admin_stmt_, SQL_UNBIND),
                              "SQLFreeStmt(SQL_UNBIND COUNT_BIG)",
                              SQL_HANDLE_STMT, admin_stmt_);
    } catch (...) {
        SQLFreeStmt(admin_stmt_, SQL_CLOSE);
        SQLFreeStmt(admin_stmt_, SQL_UNBIND);
        throw;
    }
    return static_cast<std::uint64_t>(count);
}

// Drop in reverse catalog order to keep cleanup safe if dependencies are added later.
void cleanup_benchmark_tables(OdbcSession& session) {
    for (const auto& target : kExecuteManyTargets) {
        session.execute_non_query("DROP TABLE IF EXISTS " + std::string(target.table));
    }
    session.execute_non_query("DROP TABLE IF EXISTS " +
                              std::string(kParameterArrayTable));
    for (auto iterator = tables().rbegin(); iterator != tables().rend(); ++iterator) {
        session.execute_non_query("DROP TABLE IF EXISTS " + qualified_table(*iterator));
    }
    std::cout << "Benchmark tables removed\n";
}

// Recreate and count each table before any timed process can consume it.
void setup_benchmark_tables(OdbcSession& session) {
    cleanup_benchmark_tables(session);
    session.execute_non_query(
        "CREATE TABLE " + std::string(kParameterArrayTable) +
        " ([id] INT NOT NULL PRIMARY KEY, [payload] NVARCHAR(15) NOT NULL, "
        "[value] BIGINT NOT NULL)");
    for (const auto& target : kExecuteManyTargets) {
        session.execute_non_query("CREATE TABLE " + std::string(target.table) + " (" +
                                  target.columns_ddl + ")");
    }
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
    output << "-- parameter-array write target: initially empty\n";
    output << "DROP TABLE IF EXISTS " << kParameterArrayTable << ";\nGO\n";
    output << "CREATE TABLE " << kParameterArrayTable
           << " ([id] INT NOT NULL PRIMARY KEY, [payload] NVARCHAR(15) NOT NULL, "
              "[value] BIGINT NOT NULL);\nGO\n";
    for (const auto& target : kExecuteManyTargets) {
        output << "-- executemany write target for " << target.benchmark
               << ": initially empty\n";
        output << "DROP TABLE IF EXISTS " << target.table << ";\nGO\n";
        output << "CREATE TABLE " << target.table << " (" << target.columns_ddl
               << ");\nGO\n";
    }
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

// A statement whose prepared plan and bindings persist across samples. The
// caller frees it; its runner is destroyed before the session disconnects.
SQLHSTMT allocate_statement(OdbcSession& session) {
    SQLHSTMT stmt = SQL_NULL_HSTMT;
    require_exact_success(SQLAllocHandle(SQL_HANDLE_STMT, session.connection(), &stmt),
                          "SQLAllocHandle(SQL_HANDLE_STMT)", SQL_HANDLE_DBC,
                          session.connection());
    return stmt;
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

            // Generated text has no embedded NUL. Short chunks can leave stale
            // bytes after the current terminator in this reused buffer.
            std::size_t payload = 0;
            for (; payload + column.unit_bytes <= kLobChunkBytes;
                 payload += column.unit_bytes) {
                if (std::all_of(chunk_.data() + payload,
                                chunk_.data() + payload + column.unit_bytes,
                                [](unsigned char byte) { return byte == 0; })) {
                    break;
                }
            }
            if (payload + column.unit_bytes > kLobChunkBytes) {
                throw std::runtime_error("SQLGetData returned LOB text without a terminator");
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

// Manual commit stops the sequential leg paying one implicit transaction per row,
// which is what makes it comparable to the single array execute. Scope it to the
// run so it cannot leak into read benchmarks sharing the session.
class AutocommitOffScope {
public:
    explicit AutocommitOffScope(OdbcSession& session) : session_(session) {
        session_.set_autocommit(false);
    }

    ~AutocommitOffScope() {
        try {
            session_.set_autocommit(true);
        } catch (...) {
            // A destructor cannot report this, and the run already has its result.
        }
    }

    AutocommitOffScope(const AutocommitOffScope&) = delete;
    AutocommitOffScope& operator=(const AutocommitOffScope&) = delete;

private:
    OdbcSession& session_;
};

class ParameterArrayWriteRunner::Impl {
public:
    struct SequentialRow {
        SQLINTEGER id = 0;
        SQLLEN id_indicator = sizeof(SQLINTEGER);
        std::array<SQLWCHAR, kParameterTextCapacity> payload = {};
        SQLLEN payload_indicator = 0;
        SQLBIGINT value = 0;
        SQLLEN value_indicator = sizeof(SQLBIGINT);
    };

    Impl(OdbcSession& session, std::string mode)
        : session_(session),
          mode_(std::move(mode)),
          ids_(kParameterArrayRows),
          payloads_(kParameterArrayRows),
          values_(kParameterArrayRows),
          id_indicators_(kParameterArrayRows, sizeof(SQLINTEGER)),
          payload_indicators_(kParameterArrayRows),
          value_indicators_(kParameterArrayRows, sizeof(SQLBIGINT)),
          statuses_(kParameterArrayRows, SQL_PARAM_UNUSED),
          sequential_rows_(kParameterArrayRows) {
        for (std::size_t row = 0; row < kParameterArrayRows; ++row) {
            ids_[row] = static_cast<SQLINTEGER>(row + 1);
            values_[row] = static_cast<SQLBIGINT>((row + 1) * 17);
            sequential_rows_[row].id = ids_[row];
            sequential_rows_[row].value = values_[row];

            char text[kParameterTextCapacity] = {};
            const int written =
                std::snprintf(text, sizeof(text), "row-%06zu", row + 1);
            if (written <= 0 ||
                static_cast<std::size_t>(written) >= kParameterTextCapacity) {
                throw std::logic_error("failed to format parameter-array payload");
            }
            for (int index = 0; index < written; ++index) {
                payloads_[row][static_cast<std::size_t>(index)] =
                    static_cast<SQLWCHAR>(
                        static_cast<unsigned char>(text[static_cast<std::size_t>(index)]));
                sequential_rows_[row].payload[static_cast<std::size_t>(index)] =
                    payloads_[row][static_cast<std::size_t>(index)];
            }
            payload_indicators_[row] =
                static_cast<SQLLEN>(written * sizeof(SQLWCHAR));
            sequential_rows_[row].payload_indicator = payload_indicators_[row];
        }
        prepare();
        if (mode_ == "parameter_array") {
            bind_array();
        } else {
            bind_sequential_rows();
        }
    }

    const char* name() const {
        return kParameterArrayBenchmark;
    }

    void preflight() {
        const auto metrics = run();
        std::cerr << "Preflight passed for " << name() << " (" << mode_ << "): "
                  << metrics.rows << " rows, " << metrics.execute_calls
                  << " SQLExecute calls\n";
    }

    RetrievalMetrics execute() {
        return run();
    }

private:
    SQLHSTMT stmt() const {
        return session_.statement();
    }

    void reset_statement_noexcept() noexcept {
        SQLSetStmtAttr(stmt(), SQL_ATTR_PARAMSET_SIZE, attribute_value(1), 0);
        SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_STATUS_PTR, nullptr, 0);
        SQLSetStmtAttr(stmt(), SQL_ATTR_PARAMS_PROCESSED_PTR, nullptr, 0);
        SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_BIND_OFFSET_PTR, nullptr, 0);
        SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_BIND_TYPE,
                       attribute_value(SQL_BIND_BY_COLUMN), 0);
        SQLFreeStmt(stmt(), SQL_RESET_PARAMS);
        SQLFreeStmt(stmt(), SQL_CLOSE);
    }

    void prepare() {
        auto sql = to_sql_string(
            "INSERT INTO [dbo].[mssql_odbc_bench_param_array] "
            "([id], [payload], [value]) VALUES (?, ?, ?)");
        require_exact_success(SQLPrepare(stmt(), sql.data(), SQL_NTS), "SQLPrepare(write)",
                              SQL_HANDLE_STMT, stmt());
    }

    void bind_array() {
        processed_ = 0;
        std::fill(statuses_.begin(), statuses_.end(), SQL_PARAM_UNUSED);
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_BIND_TYPE,
                           attribute_value(SQL_BIND_BY_COLUMN), 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAM_BIND_TYPE)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAMSET_SIZE,
                           attribute_value(kParameterArrayRows), 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAMSET_SIZE)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_STATUS_PTR, statuses_.data(), 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAM_STATUS_PTR)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAMS_PROCESSED_PTR, &processed_, 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAMS_PROCESSED_PTR)", SQL_HANDLE_STMT, stmt());

        bind_row(0, true);
    }

    void bind_sequential_rows() {
        sequential_offset_ = 0;
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAMSET_SIZE, attribute_value(1), 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAMSET_SIZE)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_BIND_TYPE,
                           attribute_value(sizeof(SequentialRow)), 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAM_BIND_TYPE)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLSetStmtAttr(stmt(), SQL_ATTR_PARAM_BIND_OFFSET_PTR,
                           &sequential_offset_, 0),
            "SQLSetStmtAttr(SQL_ATTR_PARAM_BIND_OFFSET_PTR)", SQL_HANDLE_STMT,
            stmt());

        auto& first = sequential_rows_.front();
        require_exact_success(
            SQLBindParameter(stmt(), 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                             &first.id, sizeof(first.id), &first.id_indicator),
            "SQLBindParameter(id)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLBindParameter(
                stmt(), 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR,
                kParameterTextCapacity - 1, 0, first.payload.data(),
                static_cast<SQLLEN>(sizeof(first.payload)), &first.payload_indicator),
            "SQLBindParameter(payload)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLBindParameter(stmt(), 3, SQL_PARAM_INPUT, SQL_C_SBIGINT, SQL_BIGINT, 0, 0,
                             &first.value, sizeof(first.value), &first.value_indicator),
            "SQLBindParameter(value)", SQL_HANDLE_STMT, stmt());
    }

    void bind_row(std::size_t row, bool array) {
        SQLINTEGER* ids = array ? ids_.data() : &ids_[row];
        SQLBIGINT* values = array ? values_.data() : &values_[row];
        auto* payloads = array ? payloads_.data() : &payloads_[row];
        SQLLEN* id_indicators =
            array ? id_indicators_.data() : &id_indicators_[row];
        SQLLEN* payload_indicators =
            array ? payload_indicators_.data() : &payload_indicators_[row];
        SQLLEN* value_indicators =
            array ? value_indicators_.data() : &value_indicators_[row];

        require_exact_success(
            SQLBindParameter(stmt(), 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                             ids, sizeof(SQLINTEGER), id_indicators),
            "SQLBindParameter(id)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLBindParameter(
                stmt(), 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR,
                kParameterTextCapacity - 1, 0, payloads,
                static_cast<SQLLEN>(sizeof(payloads_[0])), payload_indicators),
            "SQLBindParameter(payload)", SQL_HANDLE_STMT, stmt());
        require_exact_success(
            SQLBindParameter(stmt(), 3, SQL_PARAM_INPUT, SQL_C_SBIGINT, SQL_BIGINT, 0, 0,
                             values, sizeof(SQLBIGINT), value_indicators),
            "SQLBindParameter(value)", SQL_HANDLE_STMT, stmt());
    }

    void validate_written_rows() {
        const SQLHSTMT validation_stmt = session_.admin_statement();
        auto query = to_sql_string(
            "SELECT COUNT_BIG(*), COALESCE(SUM(CAST([id] AS BIGINT)), 0), "
            "COALESCE(SUM([value]), 0), COALESCE(SUM(CAST(LEN([payload]) AS BIGINT)), 0) "
            "FROM [dbo].[mssql_odbc_bench_param_array]");
        std::array<SQLBIGINT, 4> actual = {};
        std::array<SQLLEN, 4> indicators = {};
        require_exact_success(SQLExecDirect(validation_stmt, query.data(), SQL_NTS),
                              "SQLExecDirect(write validation)", SQL_HANDLE_STMT,
                              validation_stmt);
        for (std::size_t column = 0; column < actual.size(); ++column) {
            require_exact_success(
                SQLBindCol(validation_stmt, static_cast<SQLUSMALLINT>(column + 1), SQL_C_SBIGINT,
                           &actual[column], sizeof(SQLBIGINT), &indicators[column]),
                "SQLBindCol(write validation)", SQL_HANDLE_STMT, validation_stmt);
        }
        require_exact_success(SQLFetch(validation_stmt), "SQLFetch(write validation)",
                              SQL_HANDLE_STMT, validation_stmt);
        if (SQLFetch(validation_stmt) != SQL_NO_DATA) {
            throw std::runtime_error("write validation returned more than one row");
        }
        require_exact_success(SQLCloseCursor(validation_stmt), "SQLCloseCursor(write validation)",
                              SQL_HANDLE_STMT, validation_stmt);
        require_exact_success(SQLFreeStmt(validation_stmt, SQL_UNBIND),
                              "SQLFreeStmt(SQL_UNBIND write validation)",
                              SQL_HANDLE_STMT, validation_stmt);

        const auto rows = static_cast<SQLBIGINT>(kParameterArrayRows);
        const std::array<SQLBIGINT, 4> expected = {
            rows,
            rows * (rows + 1) / 2,
            17 * rows * (rows + 1) / 2,
            10 * rows,
        };
        if (actual != expected ||
            !std::all_of(indicators.begin(), indicators.end(), [](SQLLEN indicator) {
                return indicator == static_cast<SQLLEN>(sizeof(SQLBIGINT));
            })) {
            throw std::runtime_error("parameter-array write correctness check failed");
        }
    }

    RetrievalMetrics run() {
        const AutocommitOffScope manual_commit(session_);
        session_.execute_non_query("TRUNCATE TABLE " +
                                   std::string(kParameterArrayTable));
        session_.commit();

        std::uint64_t execute_calls = 0;
        SQLLEN affected = 0;
        if (mode_ == "parameter_array") {
            processed_ = 0;
            std::fill(statuses_.begin(), statuses_.end(), SQL_PARAM_UNUSED);
        } else {
            sequential_offset_ = 0;
        }
        double execute_seconds = 0.0;
        try {
            if (mode_ == "parameter_array") {
                const auto start = std::chrono::steady_clock::now();
                require_exact_success(SQLExecute(stmt()), "SQLExecute(parameter array)",
                                      SQL_HANDLE_STMT, stmt());
                execute_seconds +=
                    seconds_between(start, std::chrono::steady_clock::now());
                execute_calls = 1;
                require_exact_success(SQLRowCount(stmt(), &affected), "SQLRowCount",
                                      SQL_HANDLE_STMT, stmt());
                if (affected != static_cast<SQLLEN>(kParameterArrayRows) ||
                    processed_ != kParameterArrayRows ||
                    !std::all_of(statuses_.begin(), statuses_.end(), [](SQLUSMALLINT status) {
                        return status == SQL_PARAM_SUCCESS;
                    })) {
                    throw std::runtime_error(
                        "parameter-array status, processed count, or row count was incorrect");
                }
            } else {
                for (std::size_t row = 0; row < kParameterArrayRows; ++row) {
                    sequential_offset_ =
                        static_cast<SQLLEN>(row * sizeof(SequentialRow));
                    const auto start = std::chrono::steady_clock::now();
                    require_exact_success(SQLExecute(stmt()), "SQLExecute(sequential)",
                                          SQL_HANDLE_STMT, stmt());
                    execute_seconds +=
                        seconds_between(start, std::chrono::steady_clock::now());
                    ++execute_calls;
                    require_exact_success(SQLRowCount(stmt(), &affected), "SQLRowCount",
                                          SQL_HANDLE_STMT, stmt());
                    if (affected != 1) {
                        throw std::runtime_error(
                            "sequential write returned an incorrect row count");
                    }
                }
                affected = static_cast<SQLLEN>(kParameterArrayRows);
            }
            session_.commit();
            validate_written_rows();
            session_.commit();

            RetrievalMetrics metrics;
            metrics.rows = kParameterArrayRows;
            metrics.cells = kParameterArrayRows * 3;
            metrics.logical_bytes =
                kParameterArrayRows * (sizeof(SQLINTEGER) + 10 * sizeof(SQLWCHAR) +
                                       sizeof(SQLBIGINT));
            metrics.total_seconds = execute_seconds;
            metrics.execute_seconds = execute_seconds;
            metrics.metadata_bind_seconds = -1.0;
            metrics.fetch_seconds = 0.0;
            metrics.execute_calls = execute_calls;
            return metrics;
        } catch (...) {
            session_.rollback_noexcept();
            reset_statement_noexcept();
            throw;
        }
    }

    OdbcSession& session_;
    std::string mode_;
    std::vector<SQLINTEGER> ids_;
    std::vector<std::array<SQLWCHAR, kParameterTextCapacity>> payloads_;
    std::vector<SQLBIGINT> values_;
    std::vector<SQLLEN> id_indicators_;
    std::vector<SQLLEN> payload_indicators_;
    std::vector<SQLLEN> value_indicators_;
    std::vector<SQLUSMALLINT> statuses_;
    std::vector<SequentialRow> sequential_rows_;
    SQLULEN processed_ = 0;
    SQLLEN sequential_offset_ = 0;
};

class LatencyRunner::Impl {
public:
    Impl(OdbcSession& session, const LatencySpec& spec, std::size_t calls_override)
        : spec_(spec),
          calls_(calls_override != 0 ? calls_override : spec.calls_per_sample),
          text_(statement_text(spec)),
          stmt_(allocate_statement(session)) {
        try {
            // Bindings survive SQLCloseCursor, so binding once is what an
            // application that reuses a cursor does.
            require_exact_success(
                SQLBindCol(stmt_, 1, SQL_C_SLONG, &value_, sizeof(value_), &indicator_),
                "SQLBindCol(latency)", SQL_HANDLE_STMT, stmt_);
        } catch (...) {
            SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
            throw;
        }
    }

    ~Impl() {
        SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
    }

    Impl(const Impl&) = delete;
    Impl& operator=(const Impl&) = delete;

    const char* name() const {
        return spec_.benchmark_name;
    }

    void preflight() {
        verify_result_shape();
        const auto metrics = run(std::min(kWarmupCalls, calls_));
        std::cerr << "Preflight passed for " << name() << ": " << metrics.calls
                  << " warm-up calls at "
                  << metrics.total_seconds * 1e6 / static_cast<double>(metrics.calls)
                  << " us/call; " << calls_ << " calls per sample, "
                  << text_.size() - 1 << " statement characters\n";
    }

    RetrievalMetrics execute() {
        return run(calls_);
    }

private:
    // Enough to fault in driver code, settle the server plan cache, and warm the
    // socket path before the first timed sample; capped at one sample.
    static constexpr std::size_t kWarmupCalls = 500;

    static std::vector<SQLWCHAR> statement_text(const LatencySpec& spec) {
        std::string text = "SELECT 1";
        if (spec.text_chars != 0) {
            // The filler contains no '?', '{', or "*/", so the driver has nothing
            // to rewrite and the server nothing to parse beyond `SELECT 1`.
            constexpr std::string_view kFiller = "padding-text-0123456789abcdef ";
            text += " /*";
            while (text.size() < spec.text_chars - 2) {
                text += kFiller;
            }
            text.resize(spec.text_chars - 2);
            text += "*/";
        }
        std::vector<SQLWCHAR> wide;
        wide.reserve(text.size() + 1);
        for (const char character : text) {
            wide.push_back(static_cast<SQLWCHAR>(static_cast<unsigned char>(character)));
        }
        wide.push_back(0);
        return wide;
    }

    void submit() {
        value_ = 0;
        indicator_ = kIndicatorSentinel;
        require_exact_success(SQLExecDirectW(stmt_, text_.data(), SQL_NTS),
                              "SQLExecDirectW(latency)", SQL_HANDLE_STMT, stmt_);
    }

    void fetch_row() {
        require_exact_success(SQLFetch(stmt_), "SQLFetch(latency)", SQL_HANDLE_STMT, stmt_);
        if (value_ != 1 || indicator_ != static_cast<SQLLEN>(sizeof(value_))) {
            std::ostringstream message;
            message << name() << " fetched " << value_ << " (indicator " << indicator_
                    << "); expected 1";
            throw std::runtime_error(message.str());
        }
    }

    void close_cursor() {
        require_exact_success(SQLCloseCursor(stmt_), "SQLCloseCursor(latency)",
                              SQL_HANDLE_STMT, stmt_);
    }

    // The timed loop fetches exactly one row and closes, so prove once that the
    // result really is one INT column and one row on this driver.
    void verify_result_shape() {
        try {
            submit();
            SQLSMALLINT columns = 0;
            require_exact_success(SQLNumResultCols(stmt_, &columns),
                                  "SQLNumResultCols(latency)", SQL_HANDLE_STMT, stmt_);
            SQLSMALLINT type = 0;
            SQLULEN size = 0;
            SQLSMALLINT digits = 0;
            SQLSMALLINT nullable = 0;
            require_exact_success(
                SQLDescribeCol(stmt_, 1, nullptr, 0, nullptr, &type, &size, &digits,
                               &nullable),
                "SQLDescribeCol(latency)", SQL_HANDLE_STMT, stmt_);
            if (columns != 1 || type != SQL_INTEGER) {
                throw std::runtime_error(std::string(name()) +
                                         " did not return one INT column");
            }
            fetch_row();
            const SQLRETURN rc = SQLFetch(stmt_);
            if (rc != SQL_NO_DATA) {
                throw_odbc_error("SQLFetch(latency final)", rc, SQL_HANDLE_STMT, stmt_);
            }
            close_cursor();
        } catch (...) {
            SQLFreeStmt(stmt_, SQL_CLOSE);
            throw;
        }
    }

    RetrievalMetrics run(std::size_t calls) {
        double total_seconds = 0.0;
        try {
            const auto start = std::chrono::steady_clock::now();
            for (std::size_t call = 0; call < calls; ++call) {
                submit();
                fetch_row();
                close_cursor();
            }
            total_seconds = seconds_between(start, std::chrono::steady_clock::now());
        } catch (...) {
            SQLFreeStmt(stmt_, SQL_CLOSE);
            throw;
        }
        RetrievalMetrics metrics;
        metrics.rows = calls;
        metrics.cells = calls;
        metrics.logical_bytes = calls * sizeof(SQLINTEGER);
        metrics.total_seconds = total_seconds;
        metrics.metadata_bind_seconds = -1.0;
        metrics.calls = calls;
        return metrics;
    }

    const LatencySpec& spec_;
    std::size_t calls_;
    std::vector<SQLWCHAR> text_;
    SQLHSTMT stmt_;
    SQLINTEGER value_ = 0;
    SQLLEN indicator_ = kIndicatorSentinel;
};

namespace {

// One generated `datetime.datetime` value and its exact distance from the
// validation epoch, so the stored DATETIME2(6) can be checked as an integer sum.
struct GeneratedTimestamp {
    SQL_TIMESTAMP_STRUCT value;
    std::int64_t micros_since_2026;
};

GeneratedTimestamp generated_timestamp(std::size_t day_seed, std::size_t second_seed) {
    static constexpr int kDaysBeforeMonth[] = {0,   31,  59,  90,  120, 151,
                                               181, 212, 243, 273, 304, 334};
    GeneratedTimestamp generated{};
    auto& ts = generated.value;
    ts.year = 2026;
    ts.month = static_cast<SQLUSMALLINT>(1 + day_seed % 12);
    ts.day = static_cast<SQLUSMALLINT>(1 + day_seed % 28);
    ts.hour = static_cast<SQLUSMALLINT>(second_seed / 3600 % 24);
    ts.minute = static_cast<SQLUSMALLINT>(second_seed / 60 % 60);
    ts.second = static_cast<SQLUSMALLINT>(second_seed % 60);
    const auto micros = static_cast<std::int64_t>(second_seed * 7919 % 1000000);
    ts.fraction = static_cast<SQLUINTEGER>(micros * 1000);
    const std::int64_t day_of_year = kDaysBeforeMonth[ts.month - 1] + ts.day - 1;
    const std::int64_t seconds =
        day_of_year * 86400 + ts.hour * 3600 + ts.minute * 60 + ts.second;
    generated.micros_since_2026 = seconds * 1000000 + micros;
    return generated;
}

}  // namespace

// Replays mssql-python 8603956f `Cursor.executemany` -> `SQLExecuteMany_wrap`.
// Every call frees and reallocates the statement (`_reset_cursor`), prepares,
// binds freshly allocated column-wise arrays (`BindParameterArray`), sets only
// SQL_ATTR_PARAMSET_SIZE, executes once, then reads diagnostics, the row count,
// the result-column count, and the row count again. No status array, processed
// pointer, or bind type is set. Binding types are chosen by the same rules as
// `Cursor._map_sql_type` on the executemany path.
class ExecuteManyWriteRunner::Impl {
public:
    Impl(OdbcSession& session, ExecuteManyShape shape)
        : session_(session), target_(execute_many_target(shape)) {
        const bool ten_columns = shape == ExecuteManyShape::c10;
        std::vector<std::int64_t> ids(kExecuteManyRows);
        std::vector<std::int64_t> quantities(kExecuteManyRows);
        std::vector<std::int64_t> references(kExecuteManyRows);
        std::vector<std::string> names(kExecuteManyRows);
        std::vector<std::string> cities(kExecuteManyRows);
        std::vector<std::string> notes(kExecuteManyRows);
        std::vector<std::string> prices(kExecuteManyRows);
        std::vector<std::string> totals(kExecuteManyRows);
        std::vector<GeneratedTimestamp> created(kExecuteManyRows);
        std::vector<GeneratedTimestamp> updated(kExecuteManyRows);
        static constexpr const char* kCities[] = {"Seattle", "Bengaluru", "Dublin",
                                                  "Redmond", "Hyderabad"};
        for (std::size_t row = 0; row < kExecuteManyRows; ++row) {
            ids[row] = static_cast<std::int64_t>(row + 1);
            quantities[row] = static_cast<std::int64_t>(row % 100);
            references[row] = static_cast<std::int64_t>(100000 + row);
            names[row] = format_text("customer-%06zu", row + 1);
            cities[row] = kCities[row % std::size(kCities)];
            notes[row] = format_text("note for row %zu: lorem ipsum dolor sit amet %zu",
                                     row + 1, row * 31 % 997);
            // Money-range Decimal values, which executemany sends as format(d, "f").
            prices[row] = format_text("%zu.%04zu", 10 + row * 37 % 9000, row * 13 % 10000);
            totals[row] = format_text("%zu.%04zu", 100 + row * 53 % 90000, row * 17 % 10000);
            created[row] = generated_timestamp(row, row * 17);
            updated[row] = generated_timestamp(row + 3, row * 29);
        }

        columns_.reserve(10);
        add_integer("id", ids);
        if (ten_columns) {
            add_integer("qty", quantities);
            add_integer("ref", references);
        }
        add_text("name", names);
        if (ten_columns) {
            add_text("city", cities);
            add_text("note", notes);
        }
        add_timestamp("created", created);
        if (ten_columns) {
            add_timestamp("updated", updated);
            add_decimal_text("price", prices);
            add_decimal_text("total", totals);
        }

        std::string column_list;
        std::string markers;
        for (const auto& column : columns_) {
            column_list += (column_list.empty() ? "[" : ", [") + column.name + "]";
            markers += markers.empty() ? "?" : ", ?";
        }
        const std::string insert = "INSERT INTO " + std::string(target_.table) + " (" +
                                   column_list + ") VALUES (" + markers + ")";
        insert_sql_.assign(insert.begin(), insert.end());
    }

    ~Impl() {
        if (stmt_ != SQL_NULL_HSTMT) {
            SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
        }
    }

    Impl(const Impl&) = delete;
    Impl& operator=(const Impl&) = delete;

    const char* name() const {
        return target_.benchmark;
    }

    void preflight() {
        const auto metrics = run();
        std::cerr << "Preflight passed for " << name() << ": " << metrics.rows
                  << " rows in one SQLExecute; binds";
        for (const auto& column : columns_) {
            std::cerr << ' ' << column.name << '=' << column.c_type << "->"
                      << column.sql_type << '(' << column.column_size << ','
                      << column.decimal_digits << ')';
        }
        std::cerr << '\n';
    }

    RetrievalMetrics execute() {
        return run();
    }

private:
    struct Column {
        std::string name;
        SQLSMALLINT c_type = 0;
        SQLSMALLINT sql_type = 0;
        SQLULEN column_size = 0;
        SQLSMALLINT decimal_digits = 0;
        SQLLEN buffer_length = 0;
        std::size_t stride = 0;
        // Two identical copies, alternated per call, so each call binds different
        // addresses as mssql-python's freshly allocated buffers do.
        std::array<std::vector<unsigned char>, 2> data;
        std::array<std::vector<SQLLEN>, 2> indicators;
    };

    template <typename... Args>
    static std::string format_text(const char* format, Args... args) {
        char text[128] = {};
        const int written = std::snprintf(text, sizeof(text), format, args...);
        if (written <= 0 || static_cast<std::size_t>(written) >= sizeof(text)) {
            throw std::logic_error("failed to format executemany value");
        }
        return std::string(text, static_cast<std::size_t>(written));
    }

    // One projected validation column and its expected value for every row, in
    // `id` order. Exactly one of the vectors is filled.
    struct Check {
        std::string expression;
        std::vector<std::int64_t> numbers;
        std::vector<std::string> texts;
    };

    void expect_numbers(std::string expression, std::vector<std::int64_t> values) {
        checks_.push_back({std::move(expression), std::move(values), {}});
    }

    Column& add_column(const char* name, SQLSMALLINT c_type, SQLSMALLINT sql_type,
                       SQLULEN column_size, SQLSMALLINT decimal_digits,
                       SQLLEN buffer_length, std::size_t stride) {
        Column column;
        column.name = name;
        column.c_type = c_type;
        column.sql_type = sql_type;
        column.column_size = column_size;
        column.decimal_digits = decimal_digits;
        column.buffer_length = buffer_length;
        column.stride = stride;
        for (std::size_t set = 0; set < 2; ++set) {
            column.data[set].assign(kExecuteManyRows * stride, 0);
            column.indicators[set].assign(kExecuteManyRows, 0);
        }
        columns_.push_back(std::move(column));
        return columns_.back();
    }

    void store(Column& column, std::size_t row, const void* value, std::size_t size,
               SQLLEN indicator) {
        for (std::size_t set = 0; set < 2; ++set) {
            std::memcpy(column.data[set].data() + row * column.stride, value, size);
            column.indicators[set][row] = indicator;
        }
        logical_bytes_ += size;
    }

    // `_map_sql_type` narrows a Python int column by its min/max; BindParameterArray
    // leaves BufferLength 0 for SQL_C_LONG.
    void add_integer(const char* name, const std::vector<std::int64_t>& values) {
        const auto [low, high] = std::minmax_element(values.begin(), values.end());
        Column* column = nullptr;
        if (*low >= 0 && *high <= 255) {
            column = &add_column(name, SQL_C_TINYINT, SQL_TINYINT, 3, 0, 1, 1);
        } else if (*low >= -32768 && *high <= 32767) {
            column = &add_column(name, SQL_C_SHORT, SQL_SMALLINT, 5, 0, sizeof(SQLSMALLINT),
                                 sizeof(SQLSMALLINT));
        } else if (*low >= std::numeric_limits<SQLINTEGER>::min() &&
                   *high <= std::numeric_limits<SQLINTEGER>::max()) {
            column = &add_column(name, SQL_C_LONG, SQL_INTEGER, 10, 0, 0, sizeof(SQLINTEGER));
        } else {
            throw std::logic_error("executemany integer column needs BIGINT");
        }
        for (std::size_t row = 0; row < values.size(); ++row) {
            if (column->stride == 1) {
                const auto value = static_cast<unsigned char>(values[row]);
                store(*column, row, &value, sizeof(value), 0);
            } else if (column->stride == sizeof(SQLSMALLINT)) {
                const auto value = static_cast<SQLSMALLINT>(values[row]);
                store(*column, row, &value, sizeof(value), 0);
            } else {
                const auto value = static_cast<SQLINTEGER>(values[row]);
                store(*column, row, &value, sizeof(value), 0);
            }
        }
        expect_numbers("CAST([" + std::string(name) + "] AS BIGINT)", values);
    }

    // An ASCII `str` column binds SQL_C_CHAR -> SQL_VARCHAR sized to the longest
    // value, with BufferLength size + 1 and a byte-length indicator.
    Column& add_varchar(const char* name, const std::vector<std::string>& values) {
        std::size_t longest = 0;
        for (const auto& value : values) {
            if (std::any_of(value.begin(), value.end(), [](char character) {
                    return static_cast<unsigned char>(character) > 0x7F;
                })) {
                throw std::logic_error("non-ASCII text would bind as SQL_C_WCHAR");
            }
            longest = std::max(longest, value.size());
        }
        auto& column = add_column(name, SQL_C_CHAR, SQL_VARCHAR, longest, 0,
                                  static_cast<SQLLEN>(longest + 1), longest + 1);
        for (std::size_t row = 0; row < values.size(); ++row) {
            store(column, row, values[row].data(), values[row].size(),
                  static_cast<SQLLEN>(values[row].size()));
        }
        return column;
    }

    void add_text(const char* name, const std::vector<std::string>& values) {
        add_varchar(name, values);
        checks_.push_back({"[" + std::string(name) + "]", {}, values});
    }

    // executemany string-binds money-range Decimal values as VARCHAR (GH-503).
    void add_decimal_text(const char* name, const std::vector<std::string>& values) {
        add_varchar(name, values);
        std::vector<std::int64_t> scaled;
        scaled.reserve(values.size());
        for (const auto& value : values) {
            const auto point = value.find('.');
            scaled.push_back(std::stoll(value.substr(0, point)) * 10000 +
                             std::stoll(value.substr(point + 1)));
        }
        expect_numbers("CAST([" + std::string(name) + "] * 10000 AS BIGINT)",
                       std::move(scaled));
    }

    // A naive `datetime.datetime` binds SQL_C_TYPE_TIMESTAMP -> SQL_TYPE_TIMESTAMP(26, 6).
    void add_timestamp(const char* name, const std::vector<GeneratedTimestamp>& values) {
        auto& column = add_column(name, SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, 26, 6,
                                  sizeof(SQL_TIMESTAMP_STRUCT),
                                  sizeof(SQL_TIMESTAMP_STRUCT));
        std::vector<std::int64_t> micros;
        micros.reserve(values.size());
        for (std::size_t row = 0; row < values.size(); ++row) {
            store(column, row, &values[row].value, sizeof(SQL_TIMESTAMP_STRUCT), 0);
            micros.push_back(values[row].micros_since_2026);
        }
        expect_numbers("DATEDIFF_BIG(MICROSECOND, CAST('2026-01-01' AS DATETIME2(6)), [" +
                           std::string(name) + "])",
                       std::move(micros));
    }

    // Compares every written cell with the value generated for that row. Ordering
    // by `id` (the first check) ties each column to its row, so a parameter array
    // delivered with values attached to the wrong rows fails here.
    void validate_written_rows() {
        const SQLHSTMT validation_stmt = session_.admin_statement();
        std::string select;
        for (const auto& check : checks_) {
            select += (select.empty() ? "SELECT " : ", ") + check.expression;
        }
        auto query = to_sql_string(select + " FROM " + target_.table + " ORDER BY [id]");
        std::array<char, 256> text = {};
        try {
            require_exact_success(SQLExecDirect(validation_stmt, query.data(), SQL_NTS),
                                  "SQLExecDirect(executemany validation)", SQL_HANDLE_STMT,
                                  validation_stmt);
            for (std::size_t row = 0; row < kExecuteManyRows; ++row) {
                const SQLRETURN fetched = SQLFetch(validation_stmt);
                if (fetched == SQL_NO_DATA) {
                    throw std::runtime_error(std::string(name()) + " wrote only " +
                                             std::to_string(row) + " rows");
                }
                require_exact_success(fetched, "SQLFetch(executemany validation)",
                                      SQL_HANDLE_STMT, validation_stmt);
                for (std::size_t index = 0; index < checks_.size(); ++index) {
                    const auto& check = checks_[index];
                    const auto ordinal = static_cast<SQLUSMALLINT>(index + 1);
                    SQLLEN indicator = kIndicatorSentinel;
                    bool matches = false;
                    if (check.texts.empty()) {
                        SQLBIGINT actual = 0;
                        require_exact_success(
                            SQLGetData(validation_stmt, ordinal, SQL_C_SBIGINT, &actual,
                                       sizeof(actual), &indicator),
                            "SQLGetData(executemany validation)", SQL_HANDLE_STMT,
                            validation_stmt);
                        matches = indicator == static_cast<SQLLEN>(sizeof(actual)) &&
                                  actual == check.numbers[row];
                    } else {
                        require_exact_success(
                            SQLGetData(validation_stmt, ordinal, SQL_C_CHAR, text.data(),
                                       static_cast<SQLLEN>(text.size()), &indicator),
                            "SQLGetData(executemany validation)", SQL_HANDLE_STMT,
                            validation_stmt);
                        const auto& expected = check.texts[row];
                        matches = indicator == static_cast<SQLLEN>(expected.size()) &&
                                  expected.compare(0, expected.size(), text.data(),
                                                   expected.size()) == 0;
                    }
                    if (!matches) {
                        throw std::runtime_error(std::string(name()) +
                                                 " correctness check failed: " +
                                                 check.expression + " in row " +
                                                 std::to_string(row + 1));
                    }
                }
            }
            if (SQLFetch(validation_stmt) != SQL_NO_DATA) {
                throw std::runtime_error(std::string(name()) + " wrote extra rows");
            }
            require_exact_success(SQLCloseCursor(validation_stmt),
                                  "SQLCloseCursor(executemany validation)", SQL_HANDLE_STMT,
                                  validation_stmt);
        } catch (...) {
            SQLFreeStmt(validation_stmt, SQL_CLOSE);
            throw;
        }
    }

    RetrievalMetrics run() {
        const AutocommitOffScope manual_commit(session_);
        session_.execute_non_query("TRUNCATE TABLE " + std::string(target_.table));
        session_.commit();
        buffer_set_ ^= 1;

        try {
            const auto start = std::chrono::steady_clock::now();
            // `_reset_cursor` frees the previous call's statement first; the
            // preflight call guarantees every timed sample has one to free.
            if (stmt_ != SQL_NULL_HSTMT) {
                require_exact_success(SQLFreeHandle(SQL_HANDLE_STMT, stmt_),
                                      "SQLFreeHandle(executemany statement)",
                                      SQL_HANDLE_STMT, stmt_);
                stmt_ = SQL_NULL_HSTMT;
            }
            require_exact_success(
                SQLAllocHandle(SQL_HANDLE_STMT, session_.connection(), &stmt_),
                "SQLAllocHandle(executemany statement)", SQL_HANDLE_DBC,
                session_.connection());
            require_exact_success(SQLPrepareW(stmt_, insert_sql_.data(), SQL_NTS),
                                  "SQLPrepareW(executemany)", SQL_HANDLE_STMT, stmt_);
            for (std::size_t index = 0; index < columns_.size(); ++index) {
                auto& column = columns_[index];
                require_exact_success(
                    SQLBindParameter(stmt_, static_cast<SQLUSMALLINT>(index + 1),
                                     SQL_PARAM_INPUT, column.c_type, column.sql_type,
                                     column.column_size, column.decimal_digits,
                                     column.data[buffer_set_].data(), column.buffer_length,
                                     column.indicators[buffer_set_].data()),
                    "SQLBindParameter(executemany)", SQL_HANDLE_STMT, stmt_);
            }
            require_exact_success(
                SQLSetStmtAttrW(stmt_, SQL_ATTR_PARAMSET_SIZE,
                                attribute_value(kExecuteManyRows), 0),
                "SQLSetStmtAttrW(SQL_ATTR_PARAMSET_SIZE)", SQL_HANDLE_STMT, stmt_);
            const auto execute_start = std::chrono::steady_clock::now();
            require_exact_success(SQLExecute(stmt_), "SQLExecute(executemany)",
                                  SQL_HANDLE_STMT, stmt_);
            const auto execute_end = std::chrono::steady_clock::now();

            SQLWCHAR state[8] = {};
            SQLINTEGER native_error = 0;
            SQLWCHAR message[512] = {};
            SQLSMALLINT message_length = 0;
            const SQLRETURN diagnostic =
                SQLGetDiagRecW(SQL_HANDLE_STMT, stmt_, 1, state, &native_error, message,
                               static_cast<SQLSMALLINT>(std::size(message)), &message_length);
            if (diagnostic != SQL_NO_DATA) {
                throw std::runtime_error("executemany produced unexpected diagnostics: " +
                                         diagnostics(SQL_HANDLE_STMT, stmt_));
            }
            SQLLEN affected = -1;
            require_exact_success(SQLRowCount(stmt_, &affected), "SQLRowCount(executemany)",
                                  SQL_HANDLE_STMT, stmt_);
            SQLSMALLINT result_columns = -1;
            require_exact_success(SQLNumResultCols(stmt_, &result_columns),
                                  "SQLNumResultCols(executemany)", SQL_HANDLE_STMT, stmt_);
            SQLLEN affected_again = -1;
            require_exact_success(SQLRowCount(stmt_, &affected_again),
                                  "SQLRowCount(executemany repeat)", SQL_HANDLE_STMT, stmt_);
            const auto end = std::chrono::steady_clock::now();

            if (affected != static_cast<SQLLEN>(kExecuteManyRows) ||
                affected_again != affected || result_columns != 0) {
                throw std::runtime_error(
                    "executemany row count or result-column count was incorrect");
            }
            session_.commit();
            validate_written_rows();
            session_.commit();

            RetrievalMetrics metrics;
            metrics.rows = kExecuteManyRows;
            metrics.cells = kExecuteManyRows * columns_.size();
            metrics.logical_bytes = logical_bytes_;
            metrics.total_seconds = seconds_between(start, end);
            metrics.execute_seconds = seconds_between(execute_start, execute_end);
            metrics.metadata_bind_seconds = -1.0;
            metrics.fetch_seconds = 0.0;
            metrics.execute_calls = 1;
            return metrics;
        } catch (...) {
            session_.rollback_noexcept();
            throw;
        }
    }

    OdbcSession& session_;
    const ExecuteManyTarget& target_;
    std::vector<Column> columns_;
    std::vector<Check> checks_;
    // mssql-python calls the wide entry points; the ANSI ones would measure a
    // different Driver Manager path on non-UNICODE builds.
    std::basic_string<SQLWCHAR> insert_sql_;
    // Both buffer sets hold identical values, so this counts one set.
    std::uint64_t logical_bytes_ = 0;
    std::size_t buffer_set_ = 0;
    SQLHSTMT stmt_ = SQL_NULL_HSTMT;
};

namespace {

// Constants of mssql-python 239f79fd `ddbc_bindings.cpp` and `param_detect.hpp`.
constexpr std::size_t kPyDaeChunkBytes = 8192;     // DAE_CHUNK_SIZE
constexpr std::size_t kPyMaxInlineChars = 4000;    // MAX_INLINE_CHAR
constexpr std::size_t kPyMaxInlineBinary = 8000;   // MAX_INLINE_BINARY
constexpr SQLULEN kPyMaxLobSize = 8000;            // SQL_MAX_LOB_SIZE
constexpr SQLLEN kPyNumericTextBytes = 64;         // MAX_DIGITS_IN_NUMERIC
constexpr SQLULEN kPyZeroColumnSize = 4096;        // HandleZeroColumnSizeAtFetch
constexpr std::size_t kPyFetchManyRows = 100;      // fetchmany_mixed batch

std::vector<SQLWCHAR> wide_text(std::u16string_view text) {
    std::vector<SQLWCHAR> wide(text.begin(), text.end());
    wide.push_back(0);
    return wide;
}

std::u16string ascii_u16(std::string_view text) {
    return std::u16string(text.begin(), text.end());
}

// One Python argument after `DetectParamTypes` and `PrepareParamBuffer`.
struct PyParameter {
    SQLSMALLINT c_type = 0;
    SQLSMALLINT sql_type = 0;
    SQLULEN column_size = 0;
    SQLSMALLINT decimal_digits = 0;
    SQLLEN buffer_length = 0;
    bool dae = false;
    bool numeric = false;
    // `None`: SQL_C_DEFAULT with a NULL data pointer; the SQL type comes from
    // SQLDescribeParam (cached per prepare) or falls back to VARCHAR(1).
    bool null = false;
    // StrLen_or_IndPtr stays null for the fixed-size C types.
    bool has_indicator = false;
    SQLLEN indicator = 0;
    // Two identical copies alternated per execute: mssql-python binds freshly
    // allocated buffers on every call, so consecutive calls never share addresses.
    std::array<std::vector<unsigned char>, 2> value;
    std::array<SQLLEN, 2> indicators = {};
    // A DAE parameter's payload, sent with SQLPutData in DAE_CHUNK_SIZE pieces.
    std::vector<unsigned char> stream;
};

template <typename T>
void store_py_value(PyParameter& parameter, const T& value) {
    const auto* bytes = reinterpret_cast<const unsigned char*>(&value);
    for (auto& copy : parameter.value) {
        copy.assign(bytes, bytes + sizeof(T));
    }
}

// Python `int`: the narrowest of TINYINT/SMALLINT/INTEGER/BIGINT that fits. The
// two narrow types use an `int` buffer, the wide ones an `int64_t`.
void assign_py_int(PyParameter& parameter, std::int64_t value) {
    parameter.decimal_digits = 0;
    if (value >= 0 && value <= 255) {
        parameter.c_type = SQL_C_TINYINT;
        parameter.sql_type = SQL_TINYINT;
        parameter.column_size = 3;
        store_py_value(parameter, static_cast<std::int32_t>(value));
    } else if (value >= -32768 && value <= 32767) {
        parameter.c_type = SQL_C_SHORT;
        parameter.sql_type = SQL_SMALLINT;
        parameter.column_size = 5;
        store_py_value(parameter, static_cast<std::int32_t>(value));
    } else if (value >= std::numeric_limits<std::int32_t>::min() &&
               value <= std::numeric_limits<std::int32_t>::max()) {
        parameter.c_type = SQL_C_LONG;
        parameter.sql_type = SQL_INTEGER;
        parameter.column_size = 10;
        store_py_value(parameter, value);
    } else {
        parameter.c_type = SQL_C_SBIGINT;
        parameter.sql_type = SQL_BIGINT;
        parameter.column_size = 19;
        store_py_value(parameter, value);
    }
}

PyParameter py_int(std::int64_t value) {
    PyParameter parameter;
    assign_py_int(parameter, value);
    return parameter;
}

// Python `str`, always SQL_C_WCHAR. ASCII text declares SQL_VARCHAR, anything
// wider SQL_WVARCHAR; more than 4000 UTF-16 units streams with columnSize 0.
PyParameter py_str(std::u16string_view text, bool unicode) {
    PyParameter parameter;
    parameter.c_type = SQL_C_WCHAR;
    parameter.sql_type = unicode ? SQL_WVARCHAR : SQL_VARCHAR;
    std::vector<unsigned char> bytes(text.size() * sizeof(SQLWCHAR));
    for (std::size_t index = 0; index < text.size(); ++index) {
        const auto unit = static_cast<SQLWCHAR>(text[index]);
        std::memcpy(bytes.data() + index * sizeof(SQLWCHAR), &unit, sizeof(unit));
    }
    parameter.has_indicator = true;
    if (text.size() > kPyMaxInlineChars) {
        parameter.dae = true;
        parameter.indicator = SQL_LEN_DATA_AT_EXEC(0);
        parameter.stream = std::move(bytes);
    } else {
        parameter.column_size = text.size();
        parameter.buffer_length = static_cast<SQLLEN>(bytes.size());
        parameter.indicator = parameter.buffer_length;
        parameter.value = {bytes, bytes};
    }
    return parameter;
}

// Python `bytes`: SQL_C_BINARY -> SQL_VARBINARY, streamed above 8000 bytes.
PyParameter py_bytes(std::vector<unsigned char> bytes) {
    PyParameter parameter;
    parameter.c_type = SQL_C_BINARY;
    parameter.sql_type = SQL_VARBINARY;
    parameter.has_indicator = true;
    if (bytes.size() > kPyMaxInlineBinary) {
        parameter.dae = true;
        parameter.indicator = SQL_LEN_DATA_AT_EXEC(0);
        parameter.stream = std::move(bytes);
    } else {
        parameter.column_size = std::max<SQLULEN>(bytes.size(), 1);
        parameter.buffer_length = static_cast<SQLLEN>(bytes.size());
        parameter.indicator = parameter.buffer_length;
        parameter.value = {bytes, bytes};
    }
    return parameter;
}

template <typename T>
PyParameter py_fixed(SQLSMALLINT c_type, SQLSMALLINT sql_type, SQLULEN column_size,
                     SQLSMALLINT decimal_digits, const T& value) {
    PyParameter parameter;
    parameter.c_type = c_type;
    parameter.sql_type = sql_type;
    parameter.column_size = column_size;
    parameter.decimal_digits = decimal_digits;
    store_py_value(parameter, value);
    return parameter;
}

// Python `Decimal`: SQL_C_NUMERIC with its own precision and scale; the binder
// then sets the APD record's type, precision, scale, and data pointer.
PyParameter py_decimal(SQLCHAR precision, SQLSCHAR scale, std::uint64_t unscaled) {
    SQL_NUMERIC_STRUCT numeric = {};
    numeric.precision = precision;
    numeric.scale = scale;
    numeric.sign = 1;
    for (std::size_t index = 0; index < sizeof(unscaled); ++index) {
        numeric.val[index] = static_cast<SQLCHAR>(unscaled >> (8 * index));
    }
    auto parameter = py_fixed(SQL_C_NUMERIC, SQL_NUMERIC, precision, scale, numeric);
    parameter.numeric = true;
    return parameter;
}

PyParameter py_none() {
    PyParameter parameter;
    parameter.c_type = SQL_C_DEFAULT;
    parameter.sql_type = SQL_UNKNOWN_TYPE;
    parameter.column_size = 1;
    parameter.null = true;
    parameter.has_indicator = true;
    parameter.indicator = SQL_NULL_DATA;
    return parameter;
}

// One `fetchone` cell as `GetRowData` read it.
struct PyCell {
    SQLSMALLINT c_type = 0;
    SQLLEN indicator = 0;
    std::vector<unsigned char> data;

    template <typename T>
    T as() const {
        T value{};
        std::memcpy(&value, data.data(), std::min(sizeof(T), data.size()));
        return value;
    }
};

// One column bound by `SQLBindColums`, `rows` slots wide.
struct PyBoundColumn {
    SQLSMALLINT c_type = 0;
    SQLLEN width = 0;
    std::vector<unsigned char> data;
    std::vector<SQLLEN> indicators;

    const unsigned char* at(std::size_t row) const {
        return data.data() + row * static_cast<std::size_t>(width);
    }
};

struct PyColumn {
    SQLSMALLINT type = 0;
    SQLULEN size = 0;
};

// Decimal text in any driver's SQL_C_CHAR spelling ("1.5000", ".5000", "-2.1")
// as an integer scaled by 10^scale.
std::int64_t parse_scaled_decimal(const unsigned char* text, std::size_t length, int scale) {
    std::size_t index = 0;
    const bool negative = length != 0 && text[0] == '-';
    index += negative ? 1 : 0;
    std::int64_t value = 0;
    int fraction_digits = -1;
    for (; index < length; ++index) {
        if (text[index] == '.') {
            fraction_digits = 0;
            continue;
        }
        if (text[index] < '0' || text[index] > '9') {
            throw std::runtime_error("decimal text contains a non-digit");
        }
        if (fraction_digits >= scale) {
            continue;
        }
        value = value * 10 + (text[index] - '0');
        fraction_digits += fraction_digits >= 0 ? 1 : 0;
    }
    for (int pad = std::max(fraction_digits, 0); pad < scale; ++pad) {
        value *= 10;
    }
    return negative ? -value : value;
}

bool wide_equals(const unsigned char* data, SQLLEN indicator, std::u16string_view expected) {
    if (indicator != static_cast<SQLLEN>(expected.size() * sizeof(SQLWCHAR))) {
        return false;
    }
    for (std::size_t index = 0; index < expected.size(); ++index) {
        SQLWCHAR unit = 0;
        std::memcpy(&unit, data + index * sizeof(SQLWCHAR), sizeof(unit));
        if (unit != static_cast<SQLWCHAR>(expected[index])) {
            return false;
        }
    }
    return true;
}

// The ODBC side of one mssql-python `Cursor` on one statement handle: the call
// sequence, entry points, statement attributes, and buffer layouts of
// `cursor.py` and `ddbc_bindings.cpp` at 239f79fd.
class PyCursor {
public:
    explicit PyCursor(OdbcSession& session) : stmt_(allocate_statement(session)) {}

    ~PyCursor() {
        if (stmt_ != SQL_NULL_HSTMT) {
            SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
        }
    }

    PyCursor(const PyCursor&) = delete;
    PyCursor& operator=(const PyCursor&) = delete;

    // `Cursor.close`: SQLFreeHandle with the result left open, as mssql-python
    // frees it; checked here because the ORM replays time it.
    void free_handle() {
        const SQLHSTMT stmt = std::exchange(stmt_, SQLHSTMT{SQL_NULL_HSTMT});
        require_exact_success(SQLFreeHandle(SQL_HANDLE_STMT, stmt),
                              "SQLFreeHandle(SQL_HANDLE_STMT)", SQL_HANDLE_STMT, stmt);
    }

    SQLHSTMT handle() const {
        return stmt_;
    }

    SQLLEN row_count() const {
        return row_count_;
    }

    // `Cursor.execute`: soft reset; SQLExecDirectW without parameters, otherwise a
    // cached prepare, bind, and SQLExecute with the DAE loop; then the row count
    // and `description`.
    void execute(const std::vector<SQLWCHAR>& sql, std::vector<PyParameter>& params) {
        metadata_.clear();
        metadata_complete_ = false;
        check(SQLFreeStmt(stmt_, SQL_CLOSE), "SQLFreeStmt(SQL_CLOSE)");
        check(SQLSetStmtAttrW(stmt_, SQL_ATTR_PARAMSET_SIZE, attribute_value(1), 0),
              "SQLSetStmtAttrW(SQL_ATTR_PARAMSET_SIZE)");
        const bool same_sql = !params.empty() && prepared_ && sql == last_sql_;
        if (!same_sql) {
            prepared_ = false;
        }
        if (params.empty()) {
            apply_cursor_attrs();
            if (params_bound_) {
                reset_params();
            }
            check(SQLExecDirectW(stmt_, const_cast<SQLWCHAR*>(sql.data()), SQL_NTS),
                  "SQLExecDirectW");
        } else {
            if (!prepared_) {
                apply_cursor_attrs();
                reset_params();
                check(SQLPrepareW(stmt_, const_cast<SQLWCHAR*>(sql.data()), SQL_NTS),
                      "SQLPrepareW");
                describe_cache_.clear();
                prepared_ = true;
            }
            bind_parameters(params);
            SQLRETURN rc = SQLExecute(stmt_);
            if (rc == SQL_NEED_DATA) {
                rc = stream_parameters(params);
            }
            check(rc, "SQLExecute");
        }
        last_sql_ = sql;
        check(SQLRowCount(stmt_, &row_count_), "SQLRowCount");
        if (describe_columns(nullptr) == 0) {
            check(SQLRowCount(stmt_, &row_count_), "SQLRowCount(no result)");
        }
    }

    // `Cursor.fetchone` -> `FetchSingleRow` -> `GetRowData`.
    bool fetchone(std::vector<PyCell>& row) {
        if (!columns_unbound_) {
            check(SQLFreeStmt(stmt_, SQL_UNBIND), "SQLFreeStmt(SQL_UNBIND)");
            columns_unbound_ = true;
        }
        const SQLRETURN rc = SQLFetch(stmt_);
        if (rc == SQL_NO_DATA) {
            require_no_diagnostics();
            return false;
        }
        check(rc, "SQLFetch");
        const auto count = metadata_complete_ ? static_cast<SQLSMALLINT>(metadata_.size())
                                              : num_result_cols();
        row.resize(static_cast<std::size_t>(count));
        std::vector<PyColumn> pending;
        for (SQLSMALLINT column = 1; column <= count; ++column) {
            PyColumn meta;
            if (metadata_complete_) {
                meta = metadata_[static_cast<std::size_t>(column - 1)];
            } else {
                meta = describe_column(column);
                pending.push_back(meta);
            }
            read_cell(column, meta, row[static_cast<std::size_t>(column - 1)]);
        }
        if (!metadata_complete_) {
            metadata_ = std::move(pending);
            metadata_complete_ = true;
        }
        return true;
    }

    // `Cursor.fetchmany` -> `FetchMany_wrap` on a result with no LOB columns.
    template <typename Sink>
    std::size_t fetchmany(std::size_t size, Sink&& sink) {
        if (!metadata_complete_) {
            num_result_cols();
            describe_columns(&metadata_);
            metadata_complete_ = true;
        }
        auto columns = bind_columns(metadata_, size);
        SQLULEN fetched = 0;
        try {
            configure_rowset(size, &fetched);
            const SQLRETURN rc = SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0);
            if (rc == SQL_NO_DATA) {
                require_no_diagnostics();
                fetched = 0;
            } else {
                check(rc, "SQLFetchScroll");
                sink(columns, static_cast<std::size_t>(fetched));
            }
        } catch (...) {
            close_rowset_noexcept();
            throw;
        }
        close_rowset();
        return static_cast<std::size_t>(fetched);
    }

    // Untimed: release the result so other statements on the connection can run.
    void close() {
        check(SQLFreeStmt(stmt_, SQL_CLOSE), "SQLFreeStmt(SQL_CLOSE)");
    }

    void close_noexcept() noexcept {
        SQLFreeStmt(stmt_, SQL_CLOSE);
    }

    // After a raw statement issued outside the replay, the next execute re-prepares.
    void forget_statement() {
        close();
        prepared_ = false;
        last_sql_.clear();
    }

private:
    void check(SQLRETURN rc, const char* operation) const {
        require_exact_success(rc, operation, SQL_HANDLE_STMT, stmt_);
    }

    // `ApplyForwardOnlyCursorAttrs`: once per handle; failures are ignored.
    void apply_cursor_attrs() {
        if (cursor_attrs_applied_) {
            return;
        }
        const SQLRETURN type_rc = SQLSetStmtAttrW(
            stmt_, SQL_ATTR_CURSOR_TYPE, attribute_value(SQL_CURSOR_FORWARD_ONLY), 0);
        const SQLRETURN concurrency_rc = SQLSetStmtAttrW(
            stmt_, SQL_ATTR_CONCURRENCY, attribute_value(SQL_CONCUR_READ_ONLY), 0);
        cursor_attrs_applied_ = SQL_SUCCEEDED(type_rc) && SQL_SUCCEEDED(concurrency_rc);
    }

    // `ResetParamBindings`.
    void reset_params() {
        bound_signature_.clear();
        check(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), "SQLFreeStmt(SQL_RESET_PARAMS)");
        params_bound_ = false;
    }

    // `PreResolveUnknownNullTypes` + `BindParameters(reuseMatchingBindings=true)`.
    void bind_parameters(std::vector<PyParameter>& params) {
        for (std::size_t index = 0; index < params.size(); ++index) {
            auto& parameter = params[index];
            if (!parameter.null) {
                continue;
            }
            auto cached = describe_cache_.find(index);
            if (cached == describe_cache_.end()) {
                if (params_bound_) {
                    reset_params();
                }
                SQLSMALLINT type = 0;
                SQLULEN size = 0;
                SQLSMALLINT digits = 0;
                SQLSMALLINT nullable = 0;
                const SQLRETURN rc =
                    SQLDescribeParam(stmt_, static_cast<SQLUSMALLINT>(index + 1), &type,
                                     &size, &digits, &nullable);
                using Described = std::tuple<SQLSMALLINT, SQLULEN, SQLSMALLINT>;
                const Described fallback(static_cast<SQLSMALLINT>(SQL_VARCHAR), SQLULEN{1},
                                         SQLSMALLINT{0});
                const Described resolved =
                    SQL_SUCCEEDED(rc) ? Described(type, size, digits) : fallback;
                cached = describe_cache_.emplace(index, resolved).first;
            }
            std::tie(parameter.sql_type, parameter.column_size, parameter.decimal_digits) =
                cached->second;
        }

        std::vector<std::tuple<SQLSMALLINT, SQLSMALLINT, SQLULEN, SQLSMALLINT, bool>> signature;
        signature.reserve(params.size());
        for (const auto& parameter : params) {
            signature.emplace_back(parameter.c_type, parameter.sql_type, parameter.column_size,
                                   parameter.decimal_digits, parameter.dae);
        }
        if (params_bound_ && signature != bound_signature_) {
            reset_params();
        }
        bound_signature_.clear();
        params_bound_ = true;
        buffer_set_ ^= 1;

        for (std::size_t index = 0; index < params.size(); ++index) {
            auto& parameter = params[index];
            SQLPOINTER data = nullptr;
            if (parameter.dae) {
                data = &parameter;
            } else if (!parameter.null) {
                data = parameter.value[buffer_set_].data();
            }
            SQLLEN* indicator = nullptr;
            if (parameter.has_indicator) {
                parameter.indicators[buffer_set_] = parameter.indicator;
                indicator = &parameter.indicators[buffer_set_];
            }
            const auto ordinal = static_cast<SQLUSMALLINT>(index + 1);
            check(SQLBindParameter(stmt_, ordinal, SQL_PARAM_INPUT, parameter.c_type,
                                   parameter.sql_type, parameter.column_size,
                                   parameter.decimal_digits, data, parameter.buffer_length,
                                   indicator),
                  "SQLBindParameter");
            if (parameter.numeric) {
                const auto* numeric = static_cast<const SQL_NUMERIC_STRUCT*>(data);
                SQLHDESC descriptor = SQL_NULL_HDESC;
                check(SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_PARAM_DESC, &descriptor, 0, nullptr),
                      "SQLGetStmtAttrW(SQL_ATTR_APP_PARAM_DESC)");
                const auto record = static_cast<SQLSMALLINT>(ordinal);
                require_exact_success(
                    SQLSetDescFieldW(descriptor, record, SQL_DESC_TYPE,
                                     attribute_value(SQL_C_NUMERIC), 0),
                    "SQLSetDescFieldW(SQL_DESC_TYPE)", SQL_HANDLE_DESC, descriptor);
                require_exact_success(
                    SQLSetDescFieldW(descriptor, record, SQL_DESC_PRECISION,
                                     attribute_value(numeric->precision), 0),
                    "SQLSetDescFieldW(SQL_DESC_PRECISION)", SQL_HANDLE_DESC, descriptor);
                require_exact_success(
                    SQLSetDescFieldW(descriptor, record, SQL_DESC_SCALE,
                                     attribute_value(static_cast<SQLULEN>(numeric->scale)), 0),
                    "SQLSetDescFieldW(SQL_DESC_SCALE)", SQL_HANDLE_DESC, descriptor);
                require_exact_success(
                    SQLSetDescFieldW(descriptor, record, SQL_DESC_DATA_PTR, data, 0),
                    "SQLSetDescFieldW(SQL_DESC_DATA_PTR)", SQL_HANDLE_DESC, descriptor);
            }
        }
        bound_signature_ = std::move(signature);
    }

    // The DAE loop: the token is the bound parameter, and its payload is sent in
    // DAE_CHUNK_SIZE pieces.
    SQLRETURN stream_parameters(std::vector<PyParameter>& params) {
        for (;;) {
            SQLPOINTER token = nullptr;
            const SQLRETURN rc = SQLParamData(stmt_, &token);
            if (rc != SQL_NEED_DATA) {
                return rc;
            }
            const auto* parameter = static_cast<const PyParameter*>(token);
            if (parameter < params.data() || parameter >= params.data() + params.size() ||
                !parameter->dae) {
                throw std::runtime_error("SQLParamData returned an unknown parameter token");
            }
            const auto& bytes = parameter->stream;
            if (bytes.empty()) {
                check(SQLPutData(stmt_, const_cast<unsigned char*>(bytes.data()), 0),
                      "SQLPutData");
            }
            for (std::size_t offset = 0; offset < bytes.size(); offset += kPyDaeChunkBytes) {
                const auto length = std::min(kPyDaeChunkBytes, bytes.size() - offset);
                check(SQLPutData(stmt_, const_cast<unsigned char*>(bytes.data() + offset),
                                 static_cast<SQLLEN>(length)),
                      "SQLPutData");
            }
        }
    }

    SQLSMALLINT num_result_cols() {
        SQLSMALLINT count = 0;
        check(SQLNumResultCols(stmt_, &count), "SQLNumResultCols");
        return count;
    }

    PyColumn describe_column(SQLSMALLINT column) {
        SQLWCHAR name[256] = {};
        SQLSMALLINT name_length = 0;
        SQLSMALLINT type = 0;
        SQLULEN size = 0;
        SQLSMALLINT digits = 0;
        SQLSMALLINT nullable = 0;
        check(SQLDescribeColW(stmt_, static_cast<SQLUSMALLINT>(column), name,
                              static_cast<SQLSMALLINT>(std::size(name)), &name_length, &type,
                              &size, &digits, &nullable),
              "SQLDescribeColW");
        return {type, size};
    }

    // `DescribeColumns`: SQLNumResultCols, then SQLDescribeColW per column.
    SQLSMALLINT describe_columns(std::vector<PyColumn>* sink) {
        const SQLSMALLINT count = num_result_cols();
        for (SQLSMALLINT column = 1; column <= count; ++column) {
            const auto meta = describe_column(column);
            if (sink != nullptr) {
                sink->push_back(meta);
            }
        }
        return count;
    }

    // `CaptureFetchDiagnostics` on SQL_NO_DATA reads the record count; records
    // would be appended to `cursor.messages`, and none are expected here.
    void require_no_diagnostics() {
        SQLINTEGER count = -1;
        const SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_STMT, stmt_, 0, SQL_DIAG_NUMBER,
                                              &count, 0, nullptr);
        if (rc != SQL_SUCCESS || count != 0) {
            throw std::runtime_error("SQL_NO_DATA carried diagnostics: " +
                                     diagnostics(SQL_HANDLE_STMT, stmt_));
        }
    }

    // One `GetRowData` cell: the C type and buffer length the switch picks for
    // the described SQL type. Text and binary above the inline limits would take
    // `FetchLobColumnData`, which no workload here reaches.
    void read_cell(SQLSMALLINT column, const PyColumn& meta, PyCell& cell) {
        const bool lob = meta.size == static_cast<SQLULEN>(SQL_NO_TOTAL) || meta.size == 0;
        SQLLEN length = 0;
        std::size_t storage = 0;
        switch (meta.type) {
        case SQL_CHAR:
        case SQL_VARCHAR:
        case SQL_LONGVARCHAR:
        case SQL_WCHAR:
        case SQL_WVARCHAR:
        case SQL_WLONGVARCHAR: {
            const SQLULEN limit = (meta.type == SQL_CHAR || meta.type == SQL_VARCHAR ||
                                   meta.type == SQL_LONGVARCHAR)
                                      ? kPyMaxLobSize
                                      : kPyMaxInlineChars;
            if (lob || meta.size > limit) {
                throw std::logic_error("py-cursor fetchone does not replay LOB text columns");
            }
            cell.c_type = SQL_C_WCHAR;
            length = static_cast<SQLLEN>((meta.size + 1) * sizeof(SQLWCHAR));
            storage = static_cast<std::size_t>(length);
            break;
        }
        case SQL_INTEGER:
            cell.c_type = SQL_C_LONG;
            storage = sizeof(SQLINTEGER);
            break;
        case SQL_SMALLINT:
            cell.c_type = SQL_C_SHORT;
            storage = sizeof(SQLSMALLINT);
            break;
        case SQL_TINYINT:
            cell.c_type = SQL_C_TINYINT;
            storage = 1;
            break;
        case SQL_BIT:
            cell.c_type = SQL_C_BIT;
            storage = 1;
            break;
        case SQL_REAL:
            cell.c_type = SQL_C_FLOAT;
            storage = sizeof(SQLREAL);
            break;
        case SQL_DECIMAL:
        case SQL_NUMERIC:
            cell.c_type = SQL_C_CHAR;
            length = kPyNumericTextBytes;
            storage = static_cast<std::size_t>(length);
            break;
        case SQL_DOUBLE:
        case SQL_FLOAT:
            cell.c_type = SQL_C_DOUBLE;
            storage = sizeof(SQLDOUBLE);
            break;
        case SQL_BIGINT:
            cell.c_type = SQL_C_SBIGINT;
            storage = sizeof(SQLBIGINT);
            break;
        case SQL_TYPE_DATE:
            cell.c_type = SQL_C_TYPE_DATE;
            length = sizeof(SQL_DATE_STRUCT);
            storage = sizeof(SQL_DATE_STRUCT);
            break;
        case SQL_TIMESTAMP:
        case SQL_TYPE_TIMESTAMP:
        case SQL_DATETIME:
            cell.c_type = SQL_C_TYPE_TIMESTAMP;
            length = sizeof(SQL_TIMESTAMP_STRUCT);
            storage = sizeof(SQL_TIMESTAMP_STRUCT);
            break;
        case SQL_BINARY:
        case SQL_VARBINARY:
        case SQL_LONGVARBINARY:
            if (lob || meta.size > kPyMaxLobSize) {
                throw std::logic_error("py-cursor fetchone does not replay LOB binary columns");
            }
            cell.c_type = SQL_C_BINARY;
            length = static_cast<SQLLEN>(meta.size);
            storage = static_cast<std::size_t>(length);
            break;
        default:
            throw std::logic_error("py-cursor fetchone met an unmapped SQL type " +
                                   std::to_string(meta.type));
        }
        cell.data.assign(storage, 0);
        cell.indicator = kIndicatorSentinel;
        check(SQLGetData(stmt_, static_cast<SQLUSMALLINT>(column), cell.c_type,
                         cell.data.data(), length, &cell.indicator),
              "SQLGetData");
    }

    // `SQLBindColums` for the column types the fetch workload binds: one freshly
    // allocated column-wise array per column.
    std::vector<PyBoundColumn> bind_columns(const std::vector<PyColumn>& metadata,
                                            std::size_t rows) {
        columns_unbound_ = false;
        std::vector<PyBoundColumn> columns(metadata.size());
        for (std::size_t index = 0; index < metadata.size(); ++index) {
            const auto& meta = metadata[index];
            auto& column = columns[index];
            const SQLULEN size = meta.size == 0 ? kPyZeroColumnSize : meta.size;
            switch (meta.type) {
            case SQL_CHAR:
            case SQL_VARCHAR:
            case SQL_LONGVARCHAR:
            case SQL_WCHAR:
            case SQL_WVARCHAR:
            case SQL_WLONGVARCHAR:
                column.c_type = SQL_C_WCHAR;
                column.width = static_cast<SQLLEN>((size + 1) * sizeof(SQLWCHAR));
                break;
            case SQL_INTEGER:
                column.c_type = SQL_C_SLONG;
                column.width = sizeof(SQLINTEGER);
                break;
            case SQL_DECIMAL:
            case SQL_NUMERIC:
                column.c_type = SQL_C_CHAR;
                column.width = kPyNumericTextBytes;
                break;
            case SQL_BINARY:
            case SQL_VARBINARY:
            case SQL_LONGVARBINARY:
                column.c_type = SQL_C_BINARY;
                column.width = static_cast<SQLLEN>(size);
                break;
            default:
                throw std::logic_error("py-cursor bound fetch met an unmapped SQL type " +
                                       std::to_string(meta.type));
            }
            column.data.assign(rows * static_cast<std::size_t>(column.width), 0);
            column.indicators.assign(rows, kIndicatorSentinel);
            check(SQLBindCol(stmt_, static_cast<SQLUSMALLINT>(index + 1), column.c_type,
                             column.data.data(), column.width, column.indicators.data()),
                  "SQLBindCol");
        }
        return columns;
    }

    // `FetchStateGuard::configure`.
    void configure_rowset(std::size_t rows, SQLULEN* fetched) {
        check(SQLSetStmtAttrW(stmt_, SQL_ATTR_ROW_ARRAY_SIZE, attribute_value(rows), 0),
              "SQLSetStmtAttrW(SQL_ATTR_ROW_ARRAY_SIZE)");
        check(SQLSetStmtAttrW(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, fetched, 0),
              "SQLSetStmtAttrW(SQL_ATTR_ROWS_FETCHED_PTR)");
    }

    // `FetchStateGuard::close`.
    void close_rowset() {
        check(SQLSetStmtAttrW(stmt_, SQL_ATTR_ROW_ARRAY_SIZE, attribute_value(1), 0),
              "SQLSetStmtAttrW(SQL_ATTR_ROW_ARRAY_SIZE reset)");
        check(SQLSetStmtAttrW(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, nullptr, 0),
              "SQLSetStmtAttrW(SQL_ATTR_ROWS_FETCHED_PTR reset)");
        check(SQLFreeStmt(stmt_, SQL_UNBIND), "SQLFreeStmt(SQL_UNBIND)");
        columns_unbound_ = true;
    }

    void close_rowset_noexcept() noexcept {
        SQLSetStmtAttrW(stmt_, SQL_ATTR_ROW_ARRAY_SIZE, attribute_value(1), 0);
        SQLSetStmtAttrW(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, nullptr, 0);
        SQLFreeStmt(stmt_, SQL_UNBIND);
    }

    SQLHSTMT stmt_;
    std::vector<SQLWCHAR> last_sql_;
    bool prepared_ = false;
    bool cursor_attrs_applied_ = false;
    bool params_bound_ = false;
    bool columns_unbound_ = false;
    std::size_t buffer_set_ = 0;
    std::vector<std::tuple<SQLSMALLINT, SQLSMALLINT, SQLULEN, SQLSMALLINT, bool>>
        bound_signature_;
    std::map<std::size_t, std::tuple<SQLSMALLINT, SQLULEN, SQLSMALLINT>> describe_cache_;
    std::vector<PyColumn> metadata_;
    bool metadata_complete_ = false;
    SQLLEN row_count_ = 0;
};

}  // namespace

// Each operation is one Python `run()`/loop body; `before_operation` and
// `validate` steps that the Python workload keeps outside its timer stay untimed.
class PyCursorRunner::Impl {
public:
    Impl(OdbcSession& session, const PyCursorSpec& spec, std::size_t operations_override)
        : session_(session),
          spec_(spec),
          operations_(operations_override != 0 ? operations_override
                                               : spec.operations_per_sample),
          cursor_(session),
          table_(std::string("#mssql_odbc_bench_") + spec.python_name) {
        build();
    }

    const char* name() const {
        return spec_.benchmark_name;
    }

    void preflight() {
        create_table();
        verify_everything_ = true;
        run(1);
        if (is_lob()) {
            verify_lob_content();
        }
        verify_everything_ = false;
        const std::size_t warmup = std::clamp<std::size_t>(operations_ / 10, 1, 500);
        const auto metrics = run(warmup);
        std::cerr << "Preflight passed for " << name() << " (mssql-python "
                  << spec_.python_name << "): " << warmup << " warm-up operations at "
                  << metrics.total_seconds * 1e6 / static_cast<double>(warmup)
                  << " us/operation; " << operations_ << " operations per sample\n";
    }

    RetrievalMetrics execute() {
        return run(operations_);
    }

private:
    bool is_lob() const {
        return spec_.kind == PyCursorWorkload::insert_lob_varbinary ||
               spec_.kind == PyCursorWorkload::insert_lob_nvarchar;
    }

    void build() {
        switch (spec_.kind) {
        case PyCursorWorkload::query_scalar:
            sql_ = wide_text(u"SELECT CAST(? AS INT)");
            params_.push_back(py_int(0));
            break;
        case PyCursorWorkload::param_count_int:
        case PyCursorWorkload::param_count_nvarchar: {
            const bool text = spec_.kind == PyCursorWorkload::param_count_nvarchar;
            std::string statement = text ? "SELECT SUM(LEN(v)) FROM (VALUES "
                                         : "SELECT SUM(v) FROM (VALUES ";
            for (std::size_t index = 0; index < spec_.size; ++index) {
                statement += index == 0 ? "(?)" : ", (?)";
                if (text) {
                    char value[32] = {};
                    std::snprintf(value, sizeof(value), "value-%014zu", index);
                    params_.push_back(py_str(ascii_u16(value), false));
                    expected_ += static_cast<std::int64_t>(std::strlen(value));
                } else {
                    params_.push_back(py_int(static_cast<std::int64_t>(index)));
                    expected_ += static_cast<std::int64_t>(index);
                }
            }
            statement += ") AS t(v)";
            sql_ = wide_text(ascii_u16(statement));
            break;
        }
        case PyCursorWorkload::param_types_mixed: {
            SQL_TIMESTAMP_STRUCT stamp = {2024, 2, 29, 23, 59, 58, 123456000};
            SQL_DATE_STRUCT day = {2024, 2, 29};
            params_.push_back(py_int(2'147'483'647));
            params_.push_back(py_int(9'007'199'254'740'993));
            params_.push_back(py_fixed(SQL_C_DOUBLE, SQL_DOUBLE, 15, 0, 1.5));
            params_.push_back(py_decimal(9, 4, 123456789));
            params_.push_back(py_str(u"unicode-\u00fc-\u5024", true));
            params_.push_back(py_bytes(kMixedBytes));
            params_.push_back(py_fixed(SQL_C_BIT, SQL_BIT, 1, 0, static_cast<unsigned char>(1)));
            params_.push_back(py_fixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, 26, 6, stamp));
            params_.push_back(py_fixed(SQL_C_TYPE_DATE, SQL_TYPE_DATE, 10, 0, day));
            params_.push_back(py_none());
            sql_ = wide_text(u"SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?");
            break;
        }
        case PyCursorWorkload::insert_lob_varbinary: {
            std::vector<unsigned char> payload(spec_.size);
            for (std::size_t index = 0; index < payload.size(); ++index) {
                payload[index] = static_cast<unsigned char>(index % 251);
            }
            params_.push_back(py_bytes(std::move(payload)));
            sql_ = wide_text(ascii_u16("INSERT INTO " + table_ + " (v) VALUES (?)"));
            break;
        }
        case PyCursorWorkload::insert_lob_nvarchar: {
            std::u16string text(spec_.size / sizeof(SQLWCHAR), u'A');
            for (std::size_t index = 0; index < text.size(); ++index) {
                text[index] = static_cast<char16_t>(u'A' + index % 26);
            }
            params_.push_back(py_str(text, false));
            sql_ = wide_text(ascii_u16("INSERT INTO " + table_ + " (v) VALUES (?)"));
            break;
        }
        case PyCursorWorkload::fetchmany_mixed:
            sql_ = wide_text(ascii_u16("SELECT id, amount, label, payload FROM " + table_ +
                                       " ORDER BY id"));
            break;
        }
        if (is_lob()) {
            truncate_sql_ = wide_text(ascii_u16("TRUNCATE TABLE " + table_));
            check_sql_ = wide_text(ascii_u16("SELECT COUNT_BIG(*), MAX(DATALENGTH(v)) FROM " +
                                             table_));
        }
        // Throughput counters: values and bytes one operation binds, streams, or
        // fetches. Fetched bytes are SQL Server storage sizes of the generated values.
        switch (spec_.kind) {
        case PyCursorWorkload::insert_lob_varbinary:
        case PyCursorWorkload::insert_lob_nvarchar:
            cells_per_operation_ = 1;
            bytes_per_operation_ = spec_.size;
            break;
        case PyCursorWorkload::fetchmany_mixed:
            cells_per_operation_ = spec_.size * 4;
            for (std::size_t id = 0; id < spec_.size; ++id) {
                bytes_per_operation_ +=
                    4 + 9 + 2 * std::to_string(id).size() + 2 * std::strlen("value-") + 32;
            }
            break;
        default:
            cells_per_operation_ = params_.size();
            for (const auto& parameter : params_) {
                bytes_per_operation_ += parameter.value[0].size();
            }
            break;
        }
    }

    // The Python workloads build these tables in `_prepare` on the same
    // connection; here the generators are set-based T-SQL with the same values.
    void create_table() {
        if (!is_lob() && spec_.kind != PyCursorWorkload::fetchmany_mixed) {
            return;
        }
        session_.execute_non_query("DROP TABLE IF EXISTS " + table_);
        switch (spec_.kind) {
        case PyCursorWorkload::insert_lob_varbinary:
            session_.execute_non_query("CREATE TABLE " + table_ +
                                       " (v VARBINARY(MAX) NOT NULL)");
            break;
        case PyCursorWorkload::insert_lob_nvarchar:
            session_.execute_non_query("CREATE TABLE " + table_ +
                                       " (v NVARCHAR(MAX) NOT NULL)");
            break;
        case PyCursorWorkload::fetchmany_mixed:
            session_.execute_non_query(
                "CREATE TABLE " + table_ +
                " (id INT NOT NULL, amount DECIMAL(18, 4) NOT NULL, "
                "label NVARCHAR(100) NOT NULL, payload VARBINARY(64) NOT NULL)");
            session_.execute_non_query(
                "INSERT INTO " + table_ +
                " (id, amount, label, payload) SELECT value, "
                "CAST(value AS DECIMAL(18, 4)) / 10, CONCAT(N'value-', value), "
                "CAST(REPLICATE(CAST(CAST(value % 251 AS BINARY(1)) AS VARBINARY(64)), 32) "
                "AS VARBINARY(64)) FROM GENERATE_SERIES(0, " +
                std::to_string(spec_.size - 1) + ")");
            break;
        default:
            break;
        }
    }

    RetrievalMetrics run(std::size_t operations) {
        double timed = 0.0;
        std::uint64_t rows = 0;
        try {
            for (std::size_t operation = 0; operation < operations; ++operation) {
                if (is_lob()) {
                    cursor_.execute(truncate_sql_, no_params_);
                }
                const auto start = std::chrono::steady_clock::now();
                rows += run_one(operation);
                timed += seconds_between(start, std::chrono::steady_clock::now());
                if (is_lob()) {
                    validate_lob_row();
                }
            }
            cursor_.close();
        } catch (...) {
            cursor_.close_noexcept();
            throw;
        }
        RetrievalMetrics metrics;
        metrics.rows = rows;
        metrics.cells = operations * cells_per_operation_;
        metrics.logical_bytes = operations * bytes_per_operation_;
        metrics.total_seconds = timed;
        metrics.metadata_bind_seconds = -1.0;
        metrics.calls = operations;
        return metrics;
    }

    std::uint64_t run_one(std::size_t operation) {
        switch (spec_.kind) {
        case PyCursorWorkload::query_scalar:
            // `for value in range(loops)` restarts at 0 every sample, so the
            // binding narrows from TINYINT to SMALLINT partway through, as in Python.
            assign_py_int(params_[0], static_cast<std::int64_t>(operation));
            cursor_.execute(sql_, params_);
            expect_int(static_cast<std::int64_t>(operation));
            return 1;
        case PyCursorWorkload::param_count_int:
        case PyCursorWorkload::param_count_nvarchar:
            cursor_.execute(sql_, params_);
            expect_int(expected_);
            return 1;
        case PyCursorWorkload::param_types_mixed:
            cursor_.execute(sql_, params_);
            expect_mixed_row();
            return 1;
        case PyCursorWorkload::insert_lob_varbinary:
        case PyCursorWorkload::insert_lob_nvarchar:
            cursor_.execute(sql_, params_);
            if (cursor_.row_count() != 1) {
                throw std::runtime_error(std::string(name()) + " inserted " +
                                         std::to_string(cursor_.row_count()) + " rows");
            }
            return 1;
        case PyCursorWorkload::fetchmany_mixed:
            return fetchmany_all();
        }
        throw std::logic_error("unknown py-cursor workload");
    }

    void fail(const std::string& what) const {
        throw std::runtime_error(std::string(name()) + " correctness check failed: " + what);
    }

    void expect_int(std::int64_t expected) {
        if (!cursor_.fetchone(row_) || row_.size() != 1 ||
            row_[0].indicator == SQL_NULL_DATA) {
            fail("expected one non-NULL value");
        }
        const auto& cell = row_[0];
        const std::int64_t actual = cell.c_type == SQL_C_SBIGINT
                                        ? cell.as<SQLBIGINT>()
                                        : static_cast<std::int64_t>(cell.as<SQLINTEGER>());
        if (actual != expected) {
            fail("fetched " + std::to_string(actual) + ", expected " + std::to_string(expected));
        }
    }

    void expect_mixed_row() {
        if (!cursor_.fetchone(row_) || row_.size() != params_.size()) {
            fail("expected one ten-column row");
        }
        const auto timestamp = row_[7].as<SQL_TIMESTAMP_STRUCT>();
        const auto day = row_[8].as<SQL_DATE_STRUCT>();
        const bool matches =
            row_[0].as<SQLINTEGER>() == 2'147'483'647 &&
            row_[1].as<SQLBIGINT>() == 9'007'199'254'740'993 &&
            row_[2].as<SQLDOUBLE>() == 1.5 &&
            parse_scaled_decimal(row_[3].data.data(),
                                 static_cast<std::size_t>(row_[3].indicator), 4) == 123456789 &&
            wide_equals(row_[4].data.data(), row_[4].indicator, u"unicode-\u00fc-\u5024") &&
            row_[5].indicator == static_cast<SQLLEN>(kMixedBytes.size()) &&
            std::equal(kMixedBytes.begin(), kMixedBytes.end(), row_[5].data.begin()) &&
            row_[6].as<unsigned char>() == 1 && timestamp.year == 2024 &&
            timestamp.month == 2 && timestamp.day == 29 && timestamp.hour == 23 &&
            timestamp.minute == 59 && timestamp.second == 58 &&
            timestamp.fraction == 123456000 && day.year == 2024 && day.month == 2 &&
            day.day == 29 && row_[9].indicator == SQL_NULL_DATA;
        if (!matches) {
            fail("a mixed-type value did not round-trip");
        }
    }

    std::uint64_t fetchmany_all() {
        cursor_.execute(sql_, no_params_);
        std::int32_t next = 0;
        auto sink = [this, &next](const std::vector<PyBoundColumn>& columns, std::size_t rows) {
            for (std::size_t row = 0; row < rows; ++row, ++next) {
                SQLINTEGER id = 0;
                std::memcpy(&id, columns[0].at(row), sizeof(id));
                if (id != next) {
                    fail("rows out of id order");
                }
                if (!verify_everything_) {
                    continue;
                }
                char label[32] = {};
                std::snprintf(label, sizeof(label), "value-%d", id);
                const bool payload_ok =
                    columns[3].indicators[row] == 32 &&
                    std::all_of(columns[3].at(row), columns[3].at(row) + 32,
                                [id](unsigned char byte) { return byte == id % 251; });
                if (parse_scaled_decimal(columns[1].at(row),
                                         static_cast<std::size_t>(columns[1].indicators[row]),
                                         4) != static_cast<std::int64_t>(id) * 1000 ||
                    !wide_equals(columns[2].at(row), columns[2].indicators[row],
                                 ascii_u16(label)) ||
                    !payload_ok) {
                    fail("row " + std::to_string(id) + " differs from its generator");
                }
            }
        };
        std::uint64_t total = 0;
        while (const auto fetched = cursor_.fetchmany(kPyFetchManyRows, sink)) {
            total += fetched;
        }
        if (total != spec_.size) {
            fail("fetched " + std::to_string(total) + " rows");
        }
        return total;
    }

    // The Python `validate` is untimed and runs on the same cursor; its SHA-256
    // is replaced by this length check plus the preflight byte comparison.
    void validate_lob_row() {
        cursor_.execute(check_sql_, no_params_);
        if (!cursor_.fetchone(row_) || row_.size() != 2 ||
            row_[0].as<SQLBIGINT>() != 1 ||
            row_[1].as<SQLBIGINT>() != static_cast<SQLBIGINT>(spec_.size)) {
            fail("stored value has the wrong row count or length");
        }
    }

    // Read the stored value back in chunks and compare every byte with what was
    // streamed. Outside the replay, so the cursor forgets its prepared state.
    void verify_lob_content() {
        const bool text = spec_.kind == PyCursorWorkload::insert_lob_nvarchar;
        const auto& expected = params_[0].stream;
        const SQLHSTMT stmt = cursor_.handle();
        auto query = wide_text(ascii_u16("SELECT v FROM " + table_));
        std::vector<unsigned char> actual;
        actual.reserve(expected.size());
        std::vector<unsigned char> chunk(1 << 20);
        try {
            require_exact_success(SQLExecDirectW(stmt, query.data(), SQL_NTS),
                                  "SQLExecDirectW(LOB readback)", SQL_HANDLE_STMT, stmt);
            require_exact_success(SQLFetch(stmt), "SQLFetch(LOB readback)", SQL_HANDLE_STMT,
                                  stmt);
            const SQLLEN capacity = static_cast<SQLLEN>(chunk.size()) -
                                    (text ? static_cast<SQLLEN>(sizeof(SQLWCHAR)) : 0);
            for (;;) {
                SQLLEN indicator = 0;
                const SQLRETURN rc =
                    SQLGetData(stmt, 1, text ? SQL_C_WCHAR : SQL_C_BINARY, chunk.data(),
                               static_cast<SQLLEN>(chunk.size()), &indicator);
                if (rc == SQL_NO_DATA) {
                    break;
                }
                require_succeeded(rc, "SQLGetData(LOB readback)", stmt);
                const SQLLEN got =
                    (indicator == SQL_NO_TOTAL || indicator > capacity) ? capacity : indicator;
                actual.insert(actual.end(), chunk.begin(), chunk.begin() + got);
                if (rc == SQL_SUCCESS) {
                    break;
                }
            }
        } catch (...) {
            cursor_.forget_statement();
            throw;
        }
        cursor_.forget_statement();
        if (actual != expected) {
            fail("streamed value was not stored intact (" + std::to_string(actual.size()) +
                 " of " + std::to_string(expected.size()) + " bytes read back)");
        }
    }

    inline static const std::vector<unsigned char> kMixedBytes = {
        0x00, 0x01, 'b', 'i', 'n', 'a', 'r', 'y', 0xFF};

    OdbcSession& session_;
    const PyCursorSpec& spec_;
    std::size_t operations_;
    PyCursor cursor_;
    std::string table_;
    std::vector<SQLWCHAR> sql_;
    std::vector<SQLWCHAR> truncate_sql_;
    std::vector<SQLWCHAR> check_sql_;
    std::vector<PyParameter> params_;
    std::vector<PyParameter> no_params_;
    std::vector<PyCell> row_;
    std::int64_t expected_ = 0;
    std::uint64_t cells_per_operation_ = 0;
    std::uint64_t bytes_per_operation_ = 0;
    bool verify_everything_ = false;
};

namespace {

constexpr std::size_t kDjangoChunkRows = 100;   // django GET_ITERATOR_CHUNK_SIZE
constexpr std::int64_t kWorldRows = 10'000;

// TechEmpower `World.randomNumber`, in a closed form setup T-SQL also computes.
std::int64_t world_random_number(std::int64_t id) {
    return id * 7919 % 10000 + 1;
}

}  // namespace

// Each operation is one framework call, replayed with the ODBC calls mssql-python
// 1.18 makes for it under SQLAlchemy 2.1.9 `mssql+mssqlpython` or Django 6.1 with
// mssql-django 2.0 `python_driver="mssql_python"`. A `PyCursor` per framework
// cursor reproduces the per-cursor handle lifecycle; the transaction end is what
// the framework issues on the connection.
class OrmRunner::Impl {
public:
    Impl(OdbcSession& session, const OrmSpec& spec, const Config& config)
        : session_(session),
          spec_(spec),
          operations_(config.calls_override != 0 ? config.calls_override
                                                 : spec.operations_per_sample),
          table_("#mssql_odbc_bench_orm_" + std::to_string(static_cast<int>(spec.kind))) {
        switch (spec_.kind) {
        case OrmWorkload::sqlalchemy_checkout_select1:
            sql_ = wide_text(u"SELECT 1");
            bytes_per_operation_ = sizeof(SQLINTEGER);
            cells_per_operation_ = 1;
            break;
        case OrmWorkload::django_get_by_pk:
            sql_ = wide_text(ascii_u16("SELECT TOP 21 [world].[id], [world].[randomnumber] FROM [" +
                                       table_ + "] AS [world] WHERE [world].[id] = ?"));
            bytes_per_operation_ = 2 * sizeof(SQLINTEGER);
            cells_per_operation_ = 2;
            // A fixed list per process, as the Python harness draws its ids once.
            for (std::size_t k = 0; k < operations_; ++k) {
                ids_.push_back(1 + static_cast<std::int64_t>(
                                       splitmix64(k) % static_cast<std::uint64_t>(kWorldRows)));
            }
            break;
        case OrmWorkload::connect_close: {
            const auto wide = utf8_to_utf16(config.connection_string());
            connection_string_.assign(wide.begin(), wide.end());
            connection_string_.push_back(0);
            break;
        }
        }
    }

    const char* name() const {
        return spec_.benchmark_name;
    }

    void preflight() {
        create_table();
        run(1);
        const std::size_t warmup = std::clamp<std::size_t>(operations_ / 10, 1, 200);
        const auto metrics = run(warmup);
        std::cerr << "Preflight passed for " << name() << ": " << warmup
                  << " warm-up operations at "
                  << metrics.total_seconds * 1e6 / static_cast<double>(warmup)
                  << " us/operation; " << operations_ << " operations per sample\n";
    }

    RetrievalMetrics execute() {
        return run(operations_);
    }

private:
    void create_table() {
        if (spec_.kind != OrmWorkload::django_get_by_pk) {
            return;
        }
        session_.execute_non_query("DROP TABLE IF EXISTS " + table_);
        session_.execute_non_query("CREATE TABLE " + table_ +
                                   " (id INT NOT NULL PRIMARY KEY, randomnumber INT NOT NULL)");
        session_.execute_non_query("INSERT INTO " + table_ +
                                   " SELECT value, value * 7919 % 10000 + 1 FROM "
                                   "GENERATE_SERIES(1, " + std::to_string(kWorldRows) + ")");
    }

    RetrievalMetrics run(std::size_t operations) {
        // SQLAlchemy connections run with mssql-python's default autocommit=False;
        // Django switches its connection to autocommit.
        std::optional<AutocommitOffScope> manual_commit;
        if (spec_.kind == OrmWorkload::sqlalchemy_checkout_select1) {
            manual_commit.emplace(session_);
        }
        double timed = 0.0;
        std::uint64_t rows = 0;
        for (std::size_t operation = 0; operation < operations; ++operation) {
            const auto start = std::chrono::steady_clock::now();
            rows += run_one(operation);
            timed += seconds_between(start, std::chrono::steady_clock::now());
        }
        RetrievalMetrics metrics;
        metrics.rows = rows;
        metrics.cells = operations * cells_per_operation_;
        metrics.logical_bytes = operations * bytes_per_operation_;
        metrics.total_seconds = timed;
        metrics.metadata_bind_seconds = -1.0;
        metrics.calls = operations;
        return metrics;
    }

    std::uint64_t run_one(std::size_t operation) {
        switch (spec_.kind) {
        case OrmWorkload::sqlalchemy_checkout_select1: {
            PyCursor cursor(session_);
            cursor.execute(sql_, no_params_);
            expect_scalar(cursor, 1);
            cursor.free_handle();
            // The pool's reset-on-return.
            require_exact_success(
                SQLEndTran(SQL_HANDLE_DBC, session_.connection(), SQL_ROLLBACK),
                "SQLEndTran(SQL_ROLLBACK)", SQL_HANDLE_DBC, session_.connection());
            return 1;
        }
        case OrmWorkload::django_get_by_pk: {
            const auto id = ids_[operation % ids_.size()];
            PyCursor cursor(session_);
            params_.assign(1, py_int(id));
            cursor.execute(sql_, params_);
            std::size_t rows = 0;
            auto sink = [this, id, &rows](const std::vector<PyBoundColumn>& columns,
                                          std::size_t fetched) {
                rows += fetched;
                if (fetched != 0) {
                    expect_world_row(columns, id);
                }
            };
            while (cursor.fetchmany(kDjangoChunkRows, sink) != 0) {
            }
            if (rows != 1) {
                fail("get returned " + std::to_string(rows) + " rows");
            }
            // Frees a prepared statement: the driver releases the server handle.
            cursor.free_handle();
            return 1;
        }
        case OrmWorkload::connect_close:
            connect_close();
            return 0;
        }
        throw std::logic_error("unknown ORM workload");
    }

    void expect_world_row(const std::vector<PyBoundColumn>& columns, std::int64_t id) {
        SQLINTEGER actual_id = 0;
        SQLINTEGER number = 0;
        std::memcpy(&actual_id, columns[0].at(0), sizeof(actual_id));
        std::memcpy(&number, columns[1].at(0), sizeof(number));
        if (actual_id != id || number != world_random_number(id)) {
            fail("world " + std::to_string(id) + " returned (" + std::to_string(actual_id) +
                 ", " + std::to_string(number) + ")");
        }
    }

    void expect_scalar(PyCursor& cursor, std::int64_t expected) {
        if (!cursor.fetchone(row_) || row_.size() != 1 || row_[0].indicator == SQL_NULL_DATA) {
            fail("expected one non-NULL value");
        }
        const auto actual = static_cast<std::int64_t>(row_[0].as<SQLINTEGER>());
        if (actual != expected) {
            fail("fetched " + std::to_string(actual) + ", expected " + std::to_string(expected));
        }
    }

    // `mssql_python.connect()` then `close()`: AUTOCOMMIT is set before connecting
    // and read back twice; close rolls back, disconnects, and frees.
    void connect_close() {
        SQLHDBC dbc = SQL_NULL_HDBC;
        require_exact_success(SQLAllocHandle(SQL_HANDLE_DBC, session_.environment(), &dbc),
                              "SQLAllocHandle(SQL_HANDLE_DBC)", SQL_HANDLE_ENV,
                              session_.environment());
        bool connected = false;
        try {
            require_exact_success(
                SQLSetConnectAttrW(dbc, SQL_ATTR_AUTOCOMMIT,
                                   attribute_value(SQL_AUTOCOMMIT_OFF), SQL_IS_INTEGER),
                "SQLSetConnectAttrW(SQL_ATTR_AUTOCOMMIT)", SQL_HANDLE_DBC, dbc);
            const SQLRETURN rc =
                SQLDriverConnectW(dbc, nullptr, connection_string_.data(), SQL_NTS, nullptr, 0,
                                  nullptr, SQL_DRIVER_NOPROMPT);
            if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
                throw_odbc_error("SQLDriverConnectW", rc, SQL_HANDLE_DBC, dbc);
            }
            connected = true;
            for (int read = 0; read < 2; ++read) {
                SQLUINTEGER autocommit = SQL_AUTOCOMMIT_ON;
                SQLINTEGER length = 0;
                require_exact_success(SQLGetConnectAttrW(dbc, SQL_ATTR_AUTOCOMMIT, &autocommit,
                                                         sizeof(autocommit), &length),
                                      "SQLGetConnectAttrW(SQL_ATTR_AUTOCOMMIT)",
                                      SQL_HANDLE_DBC, dbc);
                if (autocommit != SQL_AUTOCOMMIT_OFF) {
                    fail("new connection reports autocommit on");
                }
            }
            require_exact_success(SQLEndTran(SQL_HANDLE_DBC, dbc, SQL_ROLLBACK),
                                  "SQLEndTran(SQL_ROLLBACK)", SQL_HANDLE_DBC, dbc);
            connected = false;
            require_exact_success(SQLDisconnect(dbc), "SQLDisconnect", SQL_HANDLE_DBC, dbc);
        } catch (...) {
            if (connected) {
                SQLDisconnect(dbc);
            }
            SQLFreeHandle(SQL_HANDLE_DBC, dbc);
            throw;
        }
        require_exact_success(SQLFreeHandle(SQL_HANDLE_DBC, dbc),
                              "SQLFreeHandle(SQL_HANDLE_DBC)", SQL_HANDLE_DBC, dbc);
    }

    [[noreturn]] void fail(const std::string& what) const {
        throw std::runtime_error(std::string(name()) + " correctness check failed: " + what);
    }

    OdbcSession& session_;
    const OrmSpec& spec_;
    std::size_t operations_;
    std::string table_;
    std::vector<SQLWCHAR> sql_;
    std::vector<SQLWCHAR> connection_string_;
    std::vector<PyParameter> params_;
    std::vector<PyParameter> no_params_;
    std::vector<std::int64_t> ids_;
    std::vector<PyCell> row_;
    std::uint64_t bytes_per_operation_ = 0;
    std::uint64_t cells_per_operation_ = 0;
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

ParameterArrayWriteRunner::ParameterArrayWriteRunner(OdbcSession& session,
                                                     std::string mode)
    : impl_(std::make_unique<Impl>(session, std::move(mode))) {}

ParameterArrayWriteRunner::~ParameterArrayWriteRunner() = default;

const char* ParameterArrayWriteRunner::name() const {
    return impl_->name();
}

void ParameterArrayWriteRunner::preflight() {
    impl_->preflight();
}

RetrievalMetrics ParameterArrayWriteRunner::execute() {
    return impl_->execute();
}

ExecuteManyWriteRunner::ExecuteManyWriteRunner(OdbcSession& session, ExecuteManyShape shape)
    : impl_(std::make_unique<Impl>(session, shape)) {}

ExecuteManyWriteRunner::~ExecuteManyWriteRunner() = default;

const char* ExecuteManyWriteRunner::name() const {
    return impl_->name();
}

void ExecuteManyWriteRunner::preflight() {
    impl_->preflight();
}

RetrievalMetrics ExecuteManyWriteRunner::execute() {
    return impl_->execute();
}

LatencyRunner::LatencyRunner(OdbcSession& session, const LatencySpec& spec,
                             std::size_t calls_override)
    : impl_(std::make_unique<Impl>(session, spec, calls_override)) {}

LatencyRunner::~LatencyRunner() = default;

const char* LatencyRunner::name() const {
    return impl_->name();
}

void LatencyRunner::preflight() {
    impl_->preflight();
}

RetrievalMetrics LatencyRunner::execute() {
    return impl_->execute();
}

PyCursorRunner::PyCursorRunner(OdbcSession& session, const PyCursorSpec& spec,
                               std::size_t operations_override)
    : impl_(std::make_unique<Impl>(session, spec, operations_override)) {}

PyCursorRunner::~PyCursorRunner() = default;

const char* PyCursorRunner::name() const {
    return impl_->name();
}

void PyCursorRunner::preflight() {
    impl_->preflight();
}

RetrievalMetrics PyCursorRunner::execute() {
    return impl_->execute();
}

OrmRunner::OrmRunner(OdbcSession& session, const OrmSpec& spec, const Config& config)
    : impl_(std::make_unique<Impl>(session, spec, config)) {}

OrmRunner::~OrmRunner() = default;

const char* OrmRunner::name() const {
    return impl_->name();
}

void OrmRunner::preflight() {
    impl_->preflight();
}

RetrievalMetrics OrmRunner::execute() {
    return impl_->execute();
}

}  // namespace mssql::odbc::bench

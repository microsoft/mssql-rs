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
constexpr std::size_t kPatternSize = 15;
constexpr SQLLEN kIndicatorSentinel = -777;

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

static_assert(sizeof(SqlSsTime2) == 12);
static_assert(sizeof(SqlSsTimestampOffset) == 20);
static_assert(sizeof(SqlGuid) == 16);

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
};

struct TypeDescriptor {
    ValueKind kind;
    const char* sql_type;
    const char* sql_expression;
    SQLSMALLINT c_type;
    std::size_t slot_size;
    std::uint64_t logical_bytes;
    SQLLEN expected_indicator;
};

const std::array<TypeDescriptor, kPatternSize>& type_pattern() {
    static const std::array<TypeDescriptor, kPatternSize> pattern = {{
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

struct ColumnSpec {
    std::string name;
    const TypeDescriptor* type;
};

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

std::string environment_value_or(const char* name, const char* fallback) {
    auto value = environment_value(name);
    return value.empty() ? std::string(fallback) : value;
}

std::string first_environment_value(const char* primary, const char* fallback) {
    auto value = environment_value(primary);
    return value.empty() ? environment_value(fallback) : value;
}

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
void append_code_point(SqlString& output, std::uint32_t code_point) {
    if (code_point <= 0xFFFF) {
        output.push_back(static_cast<SQLTCHAR>(code_point));
        return;
    }
    code_point -= 0x10000;
    output.push_back(static_cast<SQLTCHAR>(0xD800 + (code_point >> 10)));
    output.push_back(static_cast<SQLTCHAR>(0xDC00 + (code_point & 0x3FF)));
}

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
SqlString to_sql_string(std::string_view input) {
    return SqlString(reinterpret_cast<const SQLTCHAR*>(input.data()), input.size());
}
#endif

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

[[noreturn]] void throw_odbc_error(const char* operation, SQLRETURN rc,
                                   SQLSMALLINT handle_type, SQLHANDLE handle) {
    std::ostringstream message;
    message << operation << " returned " << static_cast<long>(rc) << ": "
            << diagnostics(handle_type, handle);
    throw std::runtime_error(message.str());
}

void require_exact_success(SQLRETURN rc, const char* operation,
                           SQLSMALLINT handle_type, SQLHANDLE handle) {
    if (rc != SQL_SUCCESS) {
        throw_odbc_error(operation, rc, handle_type, handle);
    }
}

void require_connection_success(SQLRETURN rc, SQLHDBC dbc) {
    if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
        throw_odbc_error("SQLDriverConnect", rc, SQL_HANDLE_DBC, dbc);
    }
    if (rc == SQL_SUCCESS_WITH_INFO) {
        std::cerr << "SQLDriverConnect completed with diagnostics: "
                  << diagnostics(SQL_HANDLE_DBC, dbc) << '\n';
    }
}

std::string column_name(std::size_t ordinal) {
    char buffer[16] = {};
    const int written = std::snprintf(buffer, sizeof(buffer), "c%03zu", ordinal);
    if (written <= 0 || static_cast<std::size_t>(written) >= sizeof(buffer)) {
        throw std::runtime_error("failed to generate benchmark column name");
    }
    return buffer;
}

std::vector<ColumnSpec> columns_for(const WorkloadSpec& spec) {
    std::vector<ColumnSpec> columns;
    columns.reserve(spec.column_count());
    for (std::size_t repeat = 0; repeat < spec.pattern_repetitions; ++repeat) {
        for (const auto& type : type_pattern()) {
            columns.push_back({column_name(columns.size() + 1), &type});
        }
    }
    return columns;
}

std::string qualified_table(const WorkloadSpec& spec) {
    return std::string("[dbo].[") + spec.table_name + ']';
}

std::string create_table_sql(const WorkloadSpec& spec) {
    const auto columns = columns_for(spec);
    std::ostringstream sql;
    sql << "CREATE TABLE " << qualified_table(spec) << " (";
    for (std::size_t index = 0; index < columns.size(); ++index) {
        if (index != 0) {
            sql << ',';
        }
        sql << '[' << columns[index].name << "] " << columns[index].type->sql_type
            << " NOT NULL";
    }
    sql << ')';
    return sql.str();
}

std::string insert_sql(const WorkloadSpec& spec) {
    const auto columns = columns_for(spec);
    std::ostringstream sql;
    sql << "INSERT INTO " << qualified_table(spec) << " (";
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
        sql << columns[index].type->sql_expression;
    }
    sql << " FROM GENERATE_SERIES(CAST(1 AS BIGINT), CAST(" << spec.row_count
        << " AS BIGINT)) AS g OPTION (MAXDOP 1)";
    return sql.str();
}

std::string select_sql(const WorkloadSpec& spec,
                       const std::vector<ColumnSpec>& columns) {
    std::ostringstream sql;
    sql << "SELECT ";
    for (std::size_t index = 0; index < columns.size(); ++index) {
        if (index != 0) {
            sql << ',';
        }
        sql << '[' << columns[index].name << ']';
    }
    sql << " FROM " << qualified_table(spec) << " OPTION (MAXDOP 1)";
    return sql.str();
}

std::uint64_t splitmix64(std::uint64_t value) {
    value += 0x9E3779B97F4A7C15ULL;
    value = (value ^ (value >> 30)) * 0xBF58476D1CE4E5B9ULL;
    value = (value ^ (value >> 27)) * 0x94D049BB133111EBULL;
    return value ^ (value >> 31);
}

class ColumnBuffer {
public:
    explicit ColumnBuffer(const TypeDescriptor& type)
        : type_(&type),
          storage_((type.slot_size * kRowArraySize +
                    sizeof(std::max_align_t) - 1) /
                   sizeof(std::max_align_t)),
          indicators_(kRowArraySize, kIndicatorSentinel) {}

    SQLPOINTER data() {
        return static_cast<SQLPOINTER>(storage_.data());
    }

    SQLLEN* indicators() {
        return indicators_.data();
    }

    SQLLEN indicator(std::size_t row) const {
        return indicators_[row];
    }

    void reset_indicators() {
        std::fill(indicators_.begin(), indicators_.end(), kIndicatorSentinel);
    }

    const TypeDescriptor& type() const {
        return *type_;
    }

    template <typename T>
    T read(std::size_t row) const {
        if (sizeof(T) > type_->slot_size || row >= kRowArraySize) {
            throw std::logic_error("invalid typed read from column buffer");
        }
        T value{};
        const auto* bytes = reinterpret_cast<const unsigned char*>(storage_.data());
        std::memcpy(&value, bytes + row * type_->slot_size, sizeof(T));
        return value;
    }

    const unsigned char* row_bytes(std::size_t row) const {
        const auto* bytes = reinterpret_cast<const unsigned char*>(storage_.data());
        return bytes + row * type_->slot_size;
    }

private:
    const TypeDescriptor* type_;
    std::vector<std::max_align_t> storage_;
    std::vector<SQLLEN> indicators_;
};

void validate_indicator(const ColumnBuffer& buffer, std::size_t row) {
    const SQLLEN indicator = buffer.indicator(row);
    if (indicator == SQL_NULL_DATA) {
        throw std::runtime_error("preflight found NULL in a NOT NULL workload column");
    }
    if (indicator < 0 || indicator == kIndicatorSentinel) {
        throw std::runtime_error("preflight found an invalid or unwritten indicator");
    }

    const auto& type = buffer.type();
    if (type.expected_indicator != 0 && indicator != type.expected_indicator) {
        std::ostringstream message;
        message << "preflight indicator mismatch: expected " << type.expected_indicator
                << ", got " << indicator;
        throw std::runtime_error(message.str());
    }
    if (type.kind == ValueKind::decimal &&
        (indicator == 0 || static_cast<std::size_t>(indicator) >= type.slot_size)) {
        throw std::runtime_error("preflight decimal indicator is outside its fixed slot");
    }
}

void require_value(bool condition, const char* description, std::uint64_t row_id) {
    if (!condition) {
        std::ostringstream message;
        message << "preflight value mismatch for " << description << " at row id "
                << row_id;
        throw std::runtime_error(message.str());
    }
}

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

void validate_representative_value(const ColumnBuffer& buffer, std::size_t row,
                                   std::uint64_t row_id) {
    switch (buffer.type().kind) {
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
    }
}

class PreflightValidator {
public:
    explicit PreflightValidator(const WorkloadSpec& spec)
        : spec_(spec),
          seen_(static_cast<std::size_t>(spec.row_count + 1), false),
          representatives_({1, 2, 1024, spec.row_count / 2, spec.row_count}),
          representative_seen_(representatives_.size(), false) {
        std::sort(representatives_.begin(), representatives_.end());
        representatives_.erase(
            std::unique(representatives_.begin(), representatives_.end()),
            representatives_.end());
        representative_seen_.assign(representatives_.size(), false);
        for (std::uint64_t row_id = 1; row_id <= spec_.row_count; ++row_id) {
            expected_checksum_ += splitmix64(row_id);
        }
    }

    void accept(const std::vector<ColumnBuffer>& buffers, SQLULEN rows_fetched) {
        for (SQLULEN row = 0; row < rows_fetched; ++row) {
            const auto row_index = static_cast<std::size_t>(row);
            const SQLINTEGER signed_id = buffers[3].read<SQLINTEGER>(row_index);
            if (signed_id <= 0 ||
                static_cast<std::uint64_t>(signed_id) > spec_.row_count) {
                throw std::runtime_error("preflight found an out-of-range row id");
            }
            const auto row_id = static_cast<std::uint64_t>(signed_id);
            if (seen_[static_cast<std::size_t>(row_id)]) {
                throw std::runtime_error("preflight found a duplicate row id");
            }
            seen_[static_cast<std::size_t>(row_id)] = true;
            ++accepted_rows_;
            checksum_ += splitmix64(row_id);

            for (std::size_t repeat = 0; repeat < spec_.pattern_repetitions;
                 ++repeat) {
                require_value(
                    buffers[repeat * kPatternSize + 3].read<SQLINTEGER>(row_index) ==
                        signed_id,
                    "repeated INT row id", row_id);
            }

            for (const auto& buffer : buffers) {
                validate_indicator(buffer, row_index);
            }

            const auto representative =
                std::lower_bound(representatives_.begin(), representatives_.end(),
                                 row_id);
            if (representative != representatives_.end() &&
                *representative == row_id) {
                const auto representative_index =
                    static_cast<std::size_t>(representative -
                                             representatives_.begin());
                representative_seen_[representative_index] = true;
                for (const auto& buffer : buffers) {
                    validate_representative_value(buffer, row_index, row_id);
                }
            }
        }
    }

    std::uint64_t finish() const {
        if (accepted_rows_ != spec_.row_count) {
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
    const WorkloadSpec& spec_;
    std::vector<bool> seen_;
    std::vector<std::uint64_t> representatives_;
    std::vector<bool> representative_seen_;
    std::uint64_t accepted_rows_ = 0;
    std::uint64_t checksum_ = 0;
    std::uint64_t expected_checksum_ = 0;
};

double seconds_between(std::chrono::steady_clock::time_point start,
                       std::chrono::steady_clock::time_point end) {
    return std::chrono::duration<double>(end - start).count();
}

SQLPOINTER attribute_value(SQLULEN value) {
    return reinterpret_cast<SQLPOINTER>(static_cast<std::uintptr_t>(value));
}

}  // namespace

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
        environment_value_or("ODBC_BENCH_PACKET_SIZE", "32768");
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
    if (packet_size < 512 || packet_size > 32768) {
        throw std::runtime_error(
            "ODBC_BENCH_PACKET_SIZE must be between 512 and 32768");
    }
    if (!config.scenario.empty() && config.scenario != "narrow" &&
        config.scenario != "wide") {
        throw std::runtime_error(
            "ODBC_BENCH_SCENARIO must be narrow, wide, or unset");
    }
    return config;
}

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
               << ";PacketSize=" << packet_size << ';';
    return connection.str();
}

std::size_t WorkloadSpec::column_count() const {
    return pattern_repetitions * kPatternSize;
}

const std::array<WorkloadSpec, 2>& workloads() {
    static const std::array<WorkloadSpec, 2> specs = {{
        {"fetch/narrow_2m_c15_mixed_fixed/rowset_1024",
         "narrow",
         "mssql_odbc_bench_narrow_2m_c15_mixed_fixed", 2'000'000, 1},
        {"fetch/wide_10k_c600_mixed_fixed/rowset_1024",
         "wide",
         "mssql_odbc_bench_wide_10k_c600_mixed_fixed", 10'000, 40},
    }};
    return specs;
}

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

        auto connection = to_sql_string(config.connection_string());
        rc = SQLDriverConnect(dbc_, nullptr, connection.data(), SQL_NTS, nullptr, 0,
                              nullptr, SQL_DRIVER_NOPROMPT);
        require_connection_success(rc, dbc_);

        rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_);
        require_exact_success(rc, "SQLAllocHandle(SQL_HANDLE_STMT)", SQL_HANDLE_DBC,
                              dbc_);
    } catch (...) {
        release();
        throw;
    }
}

OdbcSession::~OdbcSession() {
    release();
}

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

SQLHSTMT OdbcSession::statement() const {
    return stmt_;
}

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

void cleanup_benchmark_tables(OdbcSession& session) {
    for (auto iterator = workloads().rbegin(); iterator != workloads().rend();
         ++iterator) {
        session.execute_non_query("DROP TABLE IF EXISTS " + qualified_table(*iterator));
    }
    std::cout << "Benchmark tables removed\n";
}

void setup_benchmark_tables(OdbcSession& session) {
    cleanup_benchmark_tables(session);
    for (const auto& spec : workloads()) {
        std::cout << "Creating " << qualified_table(spec) << " with " << spec.row_count
                  << " rows and " << spec.column_count() << " columns\n";
        session.execute_non_query(create_table_sql(spec));
        session.execute_non_query(insert_sql(spec));
        const auto actual_rows = session.query_count(qualified_table(spec));
        if (actual_rows != spec.row_count) {
            std::ostringstream message;
            message << qualified_table(spec) << " contains " << actual_rows
                    << " rows; expected " << spec.row_count;
            throw std::runtime_error(message.str());
        }
    }
    std::cout << "Benchmark setup complete\n";
}

class WorkloadRunner::Impl {
public:
    Impl(OdbcSession& session, const WorkloadSpec& spec)
        : session_(session),
          spec_(spec),
          columns_(columns_for(spec)),
          query_(to_sql_string(select_sql(spec, columns_))) {
        buffers_.reserve(columns_.size());
        for (const auto& column : columns_) {
            buffers_.emplace_back(*column.type);
            logical_bytes_per_row_ += column.type->logical_bytes;
        }
        if (logical_bytes_per_row_ >= 8060) {
            throw std::logic_error(
                "generated fixed-width row is not below SQL Server's 8060-byte limit");
        }
    }

    const WorkloadSpec& spec() const {
        return spec_;
    }

    std::uint64_t logical_bytes_per_row() const {
        return logical_bytes_per_row_;
    }

    void preflight() {
        PreflightValidator validator(spec_);
        (void)run(&validator);
        const auto checksum = validator.finish();
        std::ostringstream message;
        message << "Preflight passed for " << spec_.benchmark_name << ": "
                << spec_.row_count << " rows, " << spec_.column_count()
                << " columns, checksum=0x" << std::hex << std::setw(16)
                << std::setfill('0') << checksum;
        std::cerr << message.str() << '\n';
    }

    RetrievalMetrics retrieve() {
        return run(nullptr);
    }

private:
    void prepare_statement() {
        rows_fetched_ = 0;
        require_exact_success(
            SQLSetStmtAttr(session_.statement(), SQL_ATTR_ROW_BIND_TYPE,
                           attribute_value(SQL_BIND_BY_COLUMN), 0),
            "SQLSetStmtAttr(SQL_ATTR_ROW_BIND_TYPE)", SQL_HANDLE_STMT,
            session_.statement());
        require_exact_success(
            SQLSetStmtAttr(session_.statement(), SQL_ATTR_ROW_ARRAY_SIZE,
                           attribute_value(kRowArraySize), 0),
            "SQLSetStmtAttr(SQL_ATTR_ROW_ARRAY_SIZE)", SQL_HANDLE_STMT,
            session_.statement());
        require_exact_success(
            SQLSetStmtAttr(session_.statement(), SQL_ATTR_ROWS_FETCHED_PTR,
                           &rows_fetched_, 0),
            "SQLSetStmtAttr(SQL_ATTR_ROWS_FETCHED_PTR)", SQL_HANDLE_STMT,
            session_.statement());
    }

    void describe_and_bind() {
        SQLSMALLINT result_columns = 0;
        require_exact_success(
            SQLNumResultCols(session_.statement(), &result_columns),
            "SQLNumResultCols", SQL_HANDLE_STMT, session_.statement());
        if (result_columns != static_cast<SQLSMALLINT>(columns_.size())) {
            std::ostringstream message;
            message << "result has " << result_columns << " columns; expected "
                    << columns_.size();
            throw std::runtime_error(message.str());
        }

        for (std::size_t index = 0; index < columns_.size(); ++index) {
            SQLTCHAR name[32] = {};
            SQLSMALLINT name_length = 0;
            SQLSMALLINT data_type = 0;
            SQLULEN column_size = 0;
            SQLSMALLINT decimal_digits = 0;
            SQLSMALLINT nullable = SQL_NULLABLE_UNKNOWN;
            require_exact_success(
                SQLDescribeCol(
                    session_.statement(), static_cast<SQLUSMALLINT>(index + 1), name,
                    static_cast<SQLSMALLINT>(std::size(name)), &name_length,
                    &data_type, &column_size, &decimal_digits, &nullable),
                "SQLDescribeCol", SQL_HANDLE_STMT, session_.statement());

            const auto& expected_name = columns_[index].name;
            bool name_matches =
                name_length == static_cast<SQLSMALLINT>(expected_name.size());
            for (std::size_t name_index = 0;
                 name_index < expected_name.size() && name_matches; ++name_index) {
                name_matches =
                    name[name_index] ==
                    static_cast<SQLTCHAR>(
                        static_cast<unsigned char>(expected_name[name_index]));
            }
            if (!name_matches || nullable != SQL_NO_NULLS || data_type == 0 ||
                column_size == 0) {
                throw std::runtime_error(
                    "SQLDescribeCol returned unexpected workload metadata");
            }
            (void)decimal_digits;

            auto& buffer = buffers_[index];
            require_exact_success(
                SQLBindCol(
                    session_.statement(), static_cast<SQLUSMALLINT>(index + 1),
                    buffer.type().c_type, buffer.data(),
                    static_cast<SQLLEN>(buffer.type().slot_size),
                    buffer.indicators()),
                "SQLBindCol", SQL_HANDLE_STMT, session_.statement());
        }
    }

    void cleanup_statement() {
        require_exact_success(SQLCloseCursor(session_.statement()), "SQLCloseCursor",
                              SQL_HANDLE_STMT, session_.statement());
        require_exact_success(SQLFreeStmt(session_.statement(), SQL_UNBIND),
                              "SQLFreeStmt(SQL_UNBIND)", SQL_HANDLE_STMT,
                              session_.statement());
    }

    void cleanup_statement_noexcept() noexcept {
        SQLFreeStmt(session_.statement(), SQL_CLOSE);
        SQLFreeStmt(session_.statement(), SQL_UNBIND);
    }

    RetrievalMetrics run(PreflightValidator* validator) {
        prepare_statement();
        try {
            const auto start = std::chrono::steady_clock::now();
            require_exact_success(
                SQLExecDirect(session_.statement(), query_.data(), SQL_NTS),
                "SQLExecDirect", SQL_HANDLE_STMT, session_.statement());
            const auto after_execute = std::chrono::steady_clock::now();

            describe_and_bind();
            const auto after_bind = std::chrono::steady_clock::now();

            std::uint64_t rows = 0;
            for (;;) {
                if (validator != nullptr) {
                    for (auto& buffer : buffers_) {
                        buffer.reset_indicators();
                    }
                }
                rows_fetched_ = std::numeric_limits<SQLULEN>::max();
                const SQLRETURN rc =
                    SQLFetchScroll(session_.statement(), SQL_FETCH_NEXT, 0);
                if (rc == SQL_NO_DATA) {
                    if (rows_fetched_ != 0) {
                        throw std::runtime_error(
                            "SQLFetchScroll reported SQL_NO_DATA with rows fetched");
                    }
                    break;
                }
                require_exact_success(rc, "SQLFetchScroll", SQL_HANDLE_STMT,
                                      session_.statement());
                if (rows_fetched_ == 0 || rows_fetched_ > kRowArraySize ||
                    rows_fetched_ > spec_.row_count ||
                    rows > spec_.row_count - rows_fetched_) {
                    throw std::runtime_error(
                        "SQLFetchScroll returned an invalid rowset size");
                }
                if (validator != nullptr) {
                    validator->accept(buffers_, rows_fetched_);
                }
                rows += rows_fetched_;
            }

            if (rows != spec_.row_count) {
                std::ostringstream message;
                message << "retrieval fetched " << rows << " rows; expected "
                        << spec_.row_count;
                throw std::runtime_error(message.str());
            }
            const auto end = std::chrono::steady_clock::now();

            RetrievalMetrics metrics;
            metrics.rows = rows;
            metrics.cells = rows * static_cast<std::uint64_t>(columns_.size());
            metrics.logical_bytes = rows * logical_bytes_per_row_;
            metrics.total_seconds = seconds_between(start, end);
            metrics.execute_seconds = seconds_between(start, after_execute);
            metrics.metadata_bind_seconds =
                seconds_between(after_execute, after_bind);
            metrics.fetch_seconds = seconds_between(after_bind, end);

            cleanup_statement();
            return metrics;
        } catch (...) {
            cleanup_statement_noexcept();
            throw;
        }
    }

    OdbcSession& session_;
    const WorkloadSpec& spec_;
    std::vector<ColumnSpec> columns_;
    std::vector<ColumnBuffer> buffers_;
    SqlString query_;
    SQLULEN rows_fetched_ = 0;
    std::uint64_t logical_bytes_per_row_ = 0;
};

WorkloadRunner::WorkloadRunner(OdbcSession& session, const WorkloadSpec& spec)
    : impl_(std::make_unique<Impl>(session, spec)) {}

WorkloadRunner::~WorkloadRunner() = default;

const WorkloadSpec& WorkloadRunner::spec() const {
    return impl_->spec();
}

std::uint64_t WorkloadRunner::logical_bytes_per_row() const {
    return impl_->logical_bytes_per_row();
}

void WorkloadRunner::preflight() {
    impl_->preflight();
}

RetrievalMetrics WorkloadRunner::retrieve() {
    return impl_->retrieve();
}

}  // namespace mssql::odbc::bench

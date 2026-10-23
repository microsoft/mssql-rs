// Copyright (c) Microsoft Corporation. All rights reserved.
// msodbcsql_types_check.cpp  –  Compile-time check that include/odbc_sql_server_types.h
// matches an installed msodbcsql.h. Built only when CMake finds msodbcsql.h
// (see CMakeLists.txt); nothing here is linked into a test. A mismatch fails
// the build.
//
// Both headers define the same macros and structs, so they cannot be included
// side by side. Ours is included inside a namespace, which makes its structs
// ours::*; its macro values are captured as constants and the macros undefined
// before msodbcsql.h defines them again.

#ifdef _WIN32
#include <windows.h>
#endif
#include <sql.h>
#include <sqlext.h>

#include <cstddef>
#include <type_traits>

namespace ours {
#include "odbc_sql_server_types.h"

constexpr long kCTypesExtended = SQL_C_TYPES_EXTENDED;
constexpr long kCSsTime2 = SQL_C_SS_TIME2;
constexpr long kCSsTimestampOffset = SQL_C_SS_TIMESTAMPOFFSET;
constexpr long kSsVariant = SQL_SS_VARIANT;
constexpr long kSsUdt = SQL_SS_UDT;
constexpr long kSsXml = SQL_SS_XML;
constexpr long kSsTime2 = SQL_SS_TIME2;
constexpr long kSsTimestampOffset = SQL_SS_TIMESTAMPOFFSET;
constexpr long kCaSsVariantType = SQL_CA_SS_VARIANT_TYPE;
constexpr long kCaSsUdtCatalogName = SQL_CA_SS_UDT_CATALOG_NAME;
constexpr long kCaSsUdtSchemaName = SQL_CA_SS_UDT_SCHEMA_NAME;
constexpr long kCaSsUdtTypeName = SQL_CA_SS_UDT_TYPE_NAME;
constexpr long kCaSsUdtAssemblyTypeName = SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME;
}  // namespace ours

#undef SQL_C_TYPES_EXTENDED
#undef SQL_C_SS_TIME2
#undef SQL_C_SS_TIMESTAMPOFFSET
#undef SQL_SS_VARIANT
#undef SQL_SS_UDT
#undef SQL_SS_XML
#undef SQL_SS_TIME2
#undef SQL_SS_TIMESTAMPOFFSET
#undef SQL_CA_SS_VARIANT_TYPE
#undef SQL_CA_SS_UDT_CATALOG_NAME
#undef SQL_CA_SS_UDT_SCHEMA_NAME
#undef SQL_CA_SS_UDT_TYPE_NAME
#undef SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME

#include <msodbcsql.h>

static_assert(ours::kCTypesExtended == SQL_C_TYPES_EXTENDED, "SQL_C_TYPES_EXTENDED");
static_assert(ours::kCSsTime2 == SQL_C_SS_TIME2, "SQL_C_SS_TIME2");
static_assert(ours::kCSsTimestampOffset == SQL_C_SS_TIMESTAMPOFFSET, "SQL_C_SS_TIMESTAMPOFFSET");
static_assert(ours::kSsVariant == SQL_SS_VARIANT, "SQL_SS_VARIANT");
static_assert(ours::kSsUdt == SQL_SS_UDT, "SQL_SS_UDT");
static_assert(ours::kSsXml == SQL_SS_XML, "SQL_SS_XML");
static_assert(ours::kSsTime2 == SQL_SS_TIME2, "SQL_SS_TIME2");
static_assert(ours::kSsTimestampOffset == SQL_SS_TIMESTAMPOFFSET, "SQL_SS_TIMESTAMPOFFSET");
static_assert(ours::kCaSsVariantType == SQL_CA_SS_VARIANT_TYPE, "SQL_CA_SS_VARIANT_TYPE");
static_assert(ours::kCaSsUdtCatalogName == SQL_CA_SS_UDT_CATALOG_NAME,
              "SQL_CA_SS_UDT_CATALOG_NAME");
static_assert(ours::kCaSsUdtSchemaName == SQL_CA_SS_UDT_SCHEMA_NAME, "SQL_CA_SS_UDT_SCHEMA_NAME");
static_assert(ours::kCaSsUdtTypeName == SQL_CA_SS_UDT_TYPE_NAME, "SQL_CA_SS_UDT_TYPE_NAME");
static_assert(ours::kCaSsUdtAssemblyTypeName == SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME,
              "SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME");

// Same size, and every member at the same offset with the same type.
#define ODBC_CHECK_MEMBER(Struct, member)                                                \
    static_assert(offsetof(ours::Struct, member) == offsetof(::Struct, member),          \
                  #Struct "::" #member " offset");                                       \
    static_assert(                                                                       \
        std::is_same<decltype(ours::Struct::member), decltype(::Struct::member)>::value, \
        #Struct "::" #member " type")

static_assert(sizeof(ours::SQL_SS_TIME2_STRUCT) == sizeof(::SQL_SS_TIME2_STRUCT),
              "SQL_SS_TIME2_STRUCT size");
ODBC_CHECK_MEMBER(SQL_SS_TIME2_STRUCT, hour);
ODBC_CHECK_MEMBER(SQL_SS_TIME2_STRUCT, minute);
ODBC_CHECK_MEMBER(SQL_SS_TIME2_STRUCT, second);
ODBC_CHECK_MEMBER(SQL_SS_TIME2_STRUCT, fraction);

static_assert(sizeof(ours::SQL_SS_TIMESTAMPOFFSET_STRUCT) ==
                  sizeof(::SQL_SS_TIMESTAMPOFFSET_STRUCT),
              "SQL_SS_TIMESTAMPOFFSET_STRUCT size");
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, year);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, month);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, day);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, hour);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, minute);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, second);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, fraction);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, timezone_hour);
ODBC_CHECK_MEMBER(SQL_SS_TIMESTAMPOFFSET_STRUCT, timezone_minute);

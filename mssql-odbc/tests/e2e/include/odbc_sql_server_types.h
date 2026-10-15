// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_sql_server_types.h  –  SQL Server ODBC extension identifiers and structs.
//
// Declared here rather than pulled from msodbcsql.h: the e2e suite builds on
// Linux and Windows, where that header lives in different places. Layouts and
// values are copied from msodbcsql.h. The static_asserts fail the build rather
// than silently misread a buffer if the ABI drifts.

#pragma once

#include <sql.h>
#include <sqlext.h>

#ifndef SQL_C_SS_TIME2
#define SQL_C_TYPES_EXTENDED 0x04000L
#define SQL_C_SS_TIME2 (SQL_C_TYPES_EXTENDED + 0)
#define SQL_C_SS_TIMESTAMPOFFSET (SQL_C_TYPES_EXTENDED + 1)

typedef struct tagSS_TIME2_STRUCT {
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
} SQL_SS_TIME2_STRUCT;

typedef struct tagSS_TIMESTAMPOFFSET_STRUCT {
    SQLSMALLINT year;
    SQLUSMALLINT month;
    SQLUSMALLINT day;
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
    SQLSMALLINT timezone_hour;
    SQLSMALLINT timezone_minute;
} SQL_SS_TIMESTAMPOFFSET_STRUCT;
#endif

#ifndef SQL_SS_VARIANT
#define SQL_SS_VARIANT (-150)
#endif
#ifndef SQL_SS_UDT
#define SQL_SS_UDT (-151)
#endif
#ifndef SQL_SS_XML
#define SQL_SS_XML (-152)
#endif
#ifndef SQL_SS_TIME2
#define SQL_SS_TIME2 (-154)
#endif
#ifndef SQL_SS_TIMESTAMPOFFSET
#define SQL_SS_TIMESTAMPOFFSET (-155)
#endif

#ifndef SQL_CA_SS_VARIANT_TYPE
#define SQL_CA_SS_VARIANT_TYPE (1215)
#endif
#ifndef SQL_CA_SS_UDT_CATALOG_NAME
#define SQL_CA_SS_UDT_CATALOG_NAME (1218)
#endif
#ifndef SQL_CA_SS_UDT_SCHEMA_NAME
#define SQL_CA_SS_UDT_SCHEMA_NAME (1219)
#endif
#ifndef SQL_CA_SS_UDT_TYPE_NAME
#define SQL_CA_SS_UDT_TYPE_NAME (1220)
#endif
#ifndef SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME
#define SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME (1221)
#endif

static_assert(sizeof(SQL_SS_TIME2_STRUCT) == 12,
              "SQL_SS_TIME2_STRUCT layout does not match the msodbcsql ABI");
static_assert(sizeof(SQL_SS_TIMESTAMPOFFSET_STRUCT) == 20,
              "SQL_SS_TIMESTAMPOFFSET_STRUCT layout does not match the msodbcsql ABI");

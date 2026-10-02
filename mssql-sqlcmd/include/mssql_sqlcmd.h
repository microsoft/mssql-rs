// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// C ABI of the mssql-sqlcmd static library. See src/ffi.rs for the contract.

#ifndef MSSQL_SQLCMD_H
#define MSSQL_SQLCMD_H

#include <stddef.h>
#include <stdint.h>

#ifdef _MSC_VER
#define MSSQL_SQLCMD_CALL __cdecl
#else
#define MSSQL_SQLCMD_CALL
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define MSSQL_SQLCMD_OK 0
#define MSSQL_SQLCMD_NULL_ARGUMENT 1
#define MSSQL_SQLCMD_INVALID_STATE 2
#define MSSQL_SQLCMD_INTERNAL_ERROR 3

/* The library's version as a static, NUL-terminated UTF-8 string, e.g.
   "0.1.0". Do not free it. */
const char* MSSQL_SQLCMD_CALL mssql_sqlcmd_version(void);

/* UTF-16 text with an explicit length. data == NULL means "absent"
   (SQL NULL for a row value). Invalid UTF-16 is replaced with U+FFFD. */
typedef struct MssqlSqlcmdText {
    const uint16_t* data;
    size_t len;
} MssqlSqlcmdText;

typedef struct MssqlSqlcmdConnection {
    MssqlSqlcmdText server;
    MssqlSqlcmdText database;
    MssqlSqlcmdText authentication;
    int32_t encrypt; /* non-zero when encrypted */
} MssqlSqlcmdConnection;

/* Opaque document handle. Not thread-safe: calls on one handle, including
   mssql_sqlcmd_json_free, must not run concurrently. Different handles are
   independent. */
typedef struct MssqlSqlcmdJsonDocument MssqlSqlcmdJsonDocument;

MssqlSqlcmdJsonDocument* MSSQL_SQLCMD_CALL mssql_sqlcmd_json_new(void);
void MSSQL_SQLCMD_CALL mssql_sqlcmd_json_free(MssqlSqlcmdJsonDocument* document);

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_begin_result_set(
    MssqlSqlcmdJsonDocument* document,
    const MssqlSqlcmdText* columns,
    size_t column_count);

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_row(
    MssqlSqlcmdJsonDocument* document,
    const MssqlSqlcmdText* values,
    size_t value_count);

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_rows_affected(
    MssqlSqlcmdJsonDocument* document,
    int64_t count);

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_message(
    MssqlSqlcmdJsonDocument* document,
    int32_t is_error,
    int32_t number,
    int32_t state,
    int32_t severity,
    MssqlSqlcmdText text);

/* Renders the document as UTF-16 into *out / *out_len. Release it with
   mssql_sqlcmd_free_text. The document still has to be freed. On failure
   *out is NULL and *out_len is 0. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_render(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText version,
    MssqlSqlcmdConnection connection,
    int32_t exit_code,
    uint16_t** out,
    size_t* out_len);

void MSSQL_SQLCMD_CALL mssql_sqlcmd_free_text(uint16_t* text, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* MSSQL_SQLCMD_H */

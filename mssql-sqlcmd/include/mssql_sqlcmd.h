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
#define MSSQL_SQLCMD_INVALID_ARGUMENT 4

/* Why a run failed: the failure argument of mssql_sqlcmd_json_render. */
#define MSSQL_SQLCMD_FAILURE_NONE 0
#define MSSQL_SQLCMD_FAILURE_CONNECTION 1
#define MSSQL_SQLCMD_FAILURE_AUTHENTICATION 2
#define MSSQL_SQLCMD_FAILURE_QUERY 3
#define MSSQL_SQLCMD_FAILURE_TIMEOUT 4
#define MSSQL_SQLCMD_FAILURE_CANCELLED 5
#define MSSQL_SQLCMD_FAILURE_OTHER 6

/* The library's version as a static, NUL-terminated UTF-8 string, e.g.
   "0.1.0". Do not free it. */
const char* MSSQL_SQLCMD_CALL mssql_sqlcmd_version(void);

/* UTF-16 text with an explicit length. data == NULL means "absent"
   (SQL NULL for a row value). Invalid UTF-16 is replaced with U+FFFD. */
typedef struct MssqlSqlcmdText {
    const uint16_t* data;
    size_t len;
} MssqlSqlcmdText;

/* A result-set column, typed as the driver describes it. */
typedef struct MssqlSqlcmdColumn {
    MssqlSqlcmdText name;      /* not NULL; empty for an unnamed column */
    MssqlSqlcmdText type_name; /* the server's type name, e.g. "nvarchar"; not NULL */
    int64_t length;            /* characters or bytes; 0 or less means max */
    int32_t precision;         /* decimal, numeric */
    int32_t scale;             /* decimal, numeric; fractional seconds of
                                  datetime2, time, datetimeoffset */
} MssqlSqlcmdColumn;

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

/* Creates a document and starts its clock (startTime, durationMs), so create
   it at startup, before connecting. */
MssqlSqlcmdJsonDocument* MSSQL_SQLCMD_CALL mssql_sqlcmd_json_new(void);
void MSSQL_SQLCMD_CALL mssql_sqlcmd_json_free(MssqlSqlcmdJsonDocument* document);

/* Bracket a connection attempt; the time between becomes connectMs.
   server_version may be NULL or empty when unknown. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_connecting(MssqlSqlcmdJsonDocument* document);
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_connected(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText server_version);

/* A batch sent to the server. text is the batch as sent, or NULL to leave it
   out. Beginning a batch ends any batch still running. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_begin_batch(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText text);
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_end_batch(MssqlSqlcmdJsonDocument* document);

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_begin_result_set(
    MssqlSqlcmdJsonDocument* document,
    const MssqlSqlcmdColumn* columns,
    size_t column_count);

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_row(
    MssqlSqlcmdJsonDocument* document,
    const MssqlSqlcmdText* values,
    size_t value_count);

/* Ends the current result set with its "(n rows affected)" when has_count is
   non-zero, or with none (SET NOCOUNT ON). */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_end_result_set(
    MssqlSqlcmdJsonDocument* document,
    int32_t has_count,
    int64_t count);

/* The "(n rows affected)" of a statement that returned no result set. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_rows_affected(
    MssqlSqlcmdJsonDocument* document,
    int64_t count);

/* line: 0 or less when the server reported none. procedure: NULL or empty
   when the message did not come from a stored procedure. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_message(
    MssqlSqlcmdJsonDocument* document,
    int32_t is_error,
    int32_t number,
    int32_t state,
    int32_t severity,
    int32_t line,
    MssqlSqlcmdText procedure,
    MssqlSqlcmdText text);

/* Renders the document as UTF-16 into *out / *out_len. Release it with
   mssql_sqlcmd_free_text. The document still has to be freed. On failure
   *out is NULL and *out_len is 0. failure is a MSSQL_SQLCMD_FAILURE_* value. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_render(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText version,
    MssqlSqlcmdConnection connection,
    int32_t exit_code,
    int32_t failure,
    uint16_t** out,
    size_t* out_len);

void MSSQL_SQLCMD_CALL mssql_sqlcmd_free_text(uint16_t* text, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* MSSQL_SQLCMD_H */

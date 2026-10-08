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

/* How the run ended: the end argument of mssql_sqlcmd_json_render. With the
   exit code it decides executionStatus and operationOutcome. */
#define MSSQL_SQLCMD_RUN_FINISHED 0           /* the exit code decides the outcome */
#define MSSQL_SQLCMD_RUN_CANCELED 1           /* e.g. Ctrl+C */
#define MSSQL_SQLCMD_RUN_INVALID_INVOCATION 2 /* the command line was rejected */

/* The library's version as a static, NUL-terminated UTF-8 string, e.g.
   "0.1.0". Do not free it. */
const char* MSSQL_SQLCMD_CALL mssql_sqlcmd_version(void);

/* UTF-16 text with an explicit length. It is not NUL-terminated and may
   contain embedded NULs. data may be NULL only where a value is optional, and
   then means "absent" (SQL NULL for a row value). An empty optional text is
   absent too, except a row value, which is then the empty string. Invalid
   UTF-16 is replaced with U+FFFD. */
typedef struct MssqlSqlcmdText {
    const uint16_t* data;
    size_t len; /* in UTF-16 code units, not bytes */
} MssqlSqlcmdText;

/* A result-set column, as the driver describes it. Only the name is always
   given; a negative number or a NULL or empty type name means the driver did
   not expose it, and the field is left out of the document. */
typedef struct MssqlSqlcmdColumn {
    MssqlSqlcmdText name;        /* NULL or empty for an unnamed column */
    MssqlSqlcmdText driver_type; /* the type name exactly as the driver reports it */
    int64_t size;                /* characters or bytes */
    int32_t precision;
    int32_t scale;
    int32_t nullable;            /* 0 no, 1 yes, anything else unknown */
} MssqlSqlcmdColumn;

/* The document's connection object. A NULL or empty text is unknown (null). */
typedef struct MssqlSqlcmdConnection {
    MssqlSqlcmdText server;
    MssqlSqlcmdText database;
    MssqlSqlcmdText authentication;
    int32_t encrypt; /* non-zero when encrypted */
} MssqlSqlcmdConnection;

/* A message or error, with what the console shows for it. Every text but text
   is optional (NULL or empty when absent). */
typedef struct MssqlSqlcmdMessage {
    int32_t is_error;          /* non-zero for an error */
    int32_t number;            /* Msg; 0 when none */
    int32_t severity;          /* Level */
    int32_t state;
    int32_t line;              /* 0 or less when the server reported none */
    MssqlSqlcmdText server;    /* the server that sent it */
    MssqlSqlcmdText procedure; /* the stored procedure it came from */
    MssqlSqlcmdText source;    /* who reported it when not the server: the driver, or "Sqlcmd" */
    MssqlSqlcmdText sql_state; /* the ODBC SQLSTATE */
    MssqlSqlcmdText text;      /* required */
} MssqlSqlcmdMessage;

/* Opaque document handle. Not thread-safe: calls on one handle, including
   mssql_sqlcmd_json_free, must not run concurrently. Different handles are
   independent. */
typedef struct MssqlSqlcmdJsonDocument MssqlSqlcmdJsonDocument;

/* Creates a document and starts its clock (startTime, durationMs), so create
   it at startup, before connecting. Returns NULL only if an internal panic was
   caught; check for it, since every other entry point accepts NULL and just
   returns MSSQL_SQLCMD_NULL_ARGUMENT. */
MssqlSqlcmdJsonDocument* MSSQL_SQLCMD_CALL mssql_sqlcmd_json_new(void);
void MSSQL_SQLCMD_CALL mssql_sqlcmd_json_free(MssqlSqlcmdJsonDocument* document);

/* Bracket a connection attempt; the time between becomes connectMs.
   server_version may be NULL or empty when unknown. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_connecting(MssqlSqlcmdJsonDocument* document);
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_connected(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText server_version);

/* A batch sent to the server. text is the batch as sent, or NULL or empty to
   leave it out. Beginning a batch ends any batch still running, and closes a
   result set still open without a count, so the new batch's rows cannot land
   in it. That close is not reported here; a following mssql_sqlcmd_json_add_row
   or mssql_sqlcmd_json_end_result_set returns MSSQL_SQLCMD_INVALID_STATE. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_begin_batch(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText text);
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_end_batch(MssqlSqlcmdJsonDocument* document);

/* columns may be NULL when column_count is 0. The previous result set must have
   been ended: starting one while another is open is MSSQL_SQLCMD_INVALID_STATE,
   and closes the open one without a count, so no later row can land in it. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_begin_result_set(
    MssqlSqlcmdJsonDocument* document,
    const MssqlSqlcmdColumn* columns,
    size_t column_count);

/* A NULL value data is SQL NULL. values may be NULL when value_count is 0. */
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

int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_add_message(
    MssqlSqlcmdJsonDocument* document,
    const MssqlSqlcmdMessage* message);

/* Renders the document as UTF-16 into *out, with its length in UTF-16 code
   units (not bytes) in *out_len. Release it with mssql_sqlcmd_free_text,
   passing that same length back. The document still has to be freed. On failure
   *out is NULL and *out_len is 0: each one passed is cleared first, before the
   handle is checked, so even a NULL document or the other output being NULL
   leaves them so. end is a MSSQL_SQLCMD_RUN_* value. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_json_render(
    MssqlSqlcmdJsonDocument* document,
    MssqlSqlcmdText version,
    MssqlSqlcmdConnection connection,
    int32_t exit_code,
    int32_t end,
    const uint16_t** out,
    size_t* out_len);

/* text and len (UTF-16 code units) exactly as mssql_sqlcmd_json_render returned them. */
void MSSQL_SQLCMD_CALL mssql_sqlcmd_free_text(const uint16_t* text, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* MSSQL_SQLCMD_H */

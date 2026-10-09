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
#define MSSQL_SQLCMD_UNSUPPORTED 5 /* not in this build: diagnostics when 32-bit */

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
/* Sets the locale used by Rust-generated human text. Native sqlcmd calls this
   once at startup with the language it resolved for SQLCMD.rll; on Windows this
   may be an LCID as decimal text, for example "1031". If never called, the
   library resolves the locale from the environment and then the user default UI
   language on Windows. Returns MSSQL_SQLCMD_INVALID_ARGUMENT when the locale is
   not recognized; English fallback is still selected. A NULL pointer with zero
   length is treated as an empty locale: it resets to English fallback and
   returns MSSQL_SQLCMD_INVALID_ARGUMENT. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_set_locale(MssqlSqlcmdText locale);

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

/* sqlcmd diagnose -------------------------------------------------------- */

#define MSSQL_SQLCMD_AUTH_SQL_PASSWORD 0 /* user and password */
#define MSSQL_SQLCMD_AUTH_INTEGRATED 1   /* Windows; Kerberos elsewhere */

#define MSSQL_SQLCMD_ENCRYPT_OPTIONAL 0  /* -No */
#define MSSQL_SQLCMD_ENCRYPT_MANDATORY 1 /* -N, -Nm (sqlcmd's default) */
#define MSSQL_SQLCMD_ENCRYPT_STRICT 2    /* -Ns */

#define MSSQL_SQLCMD_REPORT_TEXT 0
#define MSSQL_SQLCMD_REPORT_JSON 1

/* How far the diagnosis goes; each depth includes the ones before it. */
#define MSSQL_SQLCMD_DEPTH_DEFAULT -1             /* session validation */
#define MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT 0     /* parse -S; no external call */
#define MSSQL_SQLCMD_DEPTH_ENDPOINT_RESOLUTION 1  /* + DNS, SQL Server Browser */
#define MSSQL_SQLCMD_DEPTH_NETWORK_REACHABILITY 2 /* + TCP connect */
#define MSSQL_SQLCMD_DEPTH_CONNECTION_ATTEMPT 3   /* + one connection attempt */
#define MSSQL_SQLCMD_DEPTH_SESSION_VALIDATION 4   /* + a minimal query */

typedef struct MssqlSqlcmdDiagnosticsRequest {
    MssqlSqlcmdText server;   /* -S; required unless invalid_reason is given */
    MssqlSqlcmdText database; /* -d; NULL or empty for the login's default */
    int32_t authentication;   /* MSSQL_SQLCMD_AUTH_* */
    MssqlSqlcmdText user;     /* -U */
    MssqlSqlcmdText password; /* -P */
    int32_t encrypt;          /* MSSQL_SQLCMD_ENCRYPT_* */
    int32_t trust_server_certificate; /* -C: non-zero to trust */
    MssqlSqlcmdText host_name_in_certificate; /* -F; NULL or empty if not given */
    int32_t login_timeout_seconds; /* -l; 0 or less for the default */
    int32_t depth;            /* MSSQL_SQLCMD_DEPTH_* */
    int32_t local_detail;     /* non-zero: show identifiers (not share-safe) */
    MssqlSqlcmdText invalid_reason; /* why sqlcmd rejected the request; NULL or
                                       empty when it is valid */
} MssqlSqlcmdDiagnosticsRequest;

/* Diagnoses the connection up to request->depth (name resolution, SQL Server
   Browser, TCP connect, one connection attempt with its pre-login, TLS and
   login phases, then a minimal query) and renders the report as text or JSON
   (format: MSSQL_SQLCMD_REPORT_*) into *out / *out_len as UTF-16. Release it
   with mssql_sqlcmd_free_text. *exit_code is sqlcmd's exit code: 0 passed,
   1 issue detected, 2 inconclusive or not evaluated, 3 partial, 4 canceled,
   5 internal failure, 6 invalid invocation. A failed connection still returns
   MSSQL_SQLCMD_OK with a report; on an error *out is NULL and *exit_code 5.
   32-bit builds return MSSQL_SQLCMD_UNSUPPORTED (mssql-tds is 64-bit only).
   version is sqlcmd's, for the JSON report. Blocks until the diagnosis ends. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_diagnostics_run(
    const MssqlSqlcmdDiagnosticsRequest* request,
    MssqlSqlcmdText version,
    int32_t format,
    const uint16_t** out,
    size_t* out_len,
    int32_t* exit_code);

/* Cancels the diagnosis mssql_sqlcmd_diagnostics_run is running: it returns
   soon after with the checks that finished, the others skipped as canceled,
   and exit code 4. Callable from any thread, a console control or signal
   handler included (it only updates an atomic, without locking). Does nothing
   when no diagnosis is running, and never cancels a later one. 32-bit builds
   return MSSQL_SQLCMD_UNSUPPORTED. */
int32_t MSSQL_SQLCMD_CALL mssql_sqlcmd_diagnostics_cancel(void);
#ifdef __cplusplus
}
#endif

#endif /* MSSQL_SQLCMD_H */

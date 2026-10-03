// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// C ABI of the mssql-sqlcmd static library. See src/ffi.rs for the contract.

#ifndef MSSQL_SQLCMD_H
#define MSSQL_SQLCMD_H

#ifdef _MSC_VER
#define MSSQL_SQLCMD_CALL __cdecl
#else
#define MSSQL_SQLCMD_CALL
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* The library's version as a static, NUL-terminated UTF-8 string, e.g.
   "0.1.0". Do not free it. */
const char* MSSQL_SQLCMD_CALL mssql_sqlcmd_version(void);

#ifdef __cplusplus
}
#endif

#endif /* MSSQL_SQLCMD_H */

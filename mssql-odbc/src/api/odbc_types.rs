// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ODBC C type aliases matching the ODBC 3.x specification.
#![allow(dead_code)]

use std::ffi::c_void;

pub type SqlSmallInt = i16;
pub type SqlInteger = i32;
pub type SqlReturn = SqlSmallInt;
pub type SqlHandle = *mut c_void;
pub type SqlPointer = *mut c_void;

// Return codes
pub const SQL_SUCCESS: SqlReturn = 0;
pub const SQL_SUCCESS_WITH_INFO: SqlReturn = 1;
pub const SQL_NO_DATA: SqlReturn = 100;
pub const SQL_PARAM_DATA_AVAILABLE: SqlReturn = 101;
pub const SQL_ERROR: SqlReturn = -1;
pub const SQL_INVALID_HANDLE: SqlReturn = -2;
pub const SQL_STILL_EXECUTING: SqlReturn = 2;
pub const SQL_NEED_DATA: SqlReturn = 99;

// Null handle sentinel
pub const SQL_NULL_HANDLE: SqlHandle = std::ptr::null_mut();

// Handle type constants
pub const SQL_HANDLE_ENV: SqlSmallInt = 1;
pub const SQL_HANDLE_DBC: SqlSmallInt = 2;
pub const SQL_HANDLE_STMT: SqlSmallInt = 3;
pub const SQL_HANDLE_DESC: SqlSmallInt = 4;
// Internal handle type used by Driver Manager for connection pool awareness.
// Applications should not use this directly.
pub const SQL_HANDLE_DBC_INFO_TOKEN: SqlSmallInt = 6;

// Environment attribute identifiers (SQLSetEnvAttr / SQLGetEnvAttr).
pub const SQL_ATTR_ODBC_VERSION: SqlInteger = 200;

// SQL_ATTR_ODBC_VERSION values.
pub const SQL_OV_ODBC2: u32 = 2;
pub const SQL_OV_ODBC3: u32 = 3;
pub const SQL_OV_ODBC3_80: u32 = 380;

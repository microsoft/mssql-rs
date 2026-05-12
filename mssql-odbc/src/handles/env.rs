// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::Mutex;

use super::{HandleHeader, HandleType, HasHeader};

/// ODBC environment attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum OdbcVersion {
    /// Not yet set — calls requiring a version will fail with HY010.
    Unset = 0,
    Odbc3 = 3,
    Odbc3_80 = 380,
}

/// Environment handle — equivalent to msodbcsql's `struct tagENV`.
///
/// One ENV is typically allocated per application. It owns connection handles
/// and stores environment-level attributes (ODBC version, connection pooling mode).
///
/// Thread-safety: The `inner` mutex protects mutable state. msodbcsql uses
/// `csEnv` (Unix) or relies on the Driver Manager (Windows) for serialization.
/// We always protect with a mutex for safety regardless of platform.
#[derive(Debug)]
pub(crate) struct EnvHandle {
    #[allow(dead_code)]
    pub(crate) header: HandleHeader,
    #[allow(dead_code)]
    pub(crate) inner: Mutex<EnvState>,
}

/// Mutable state within an environment handle, protected by the mutex.
#[derive(Debug)]
pub(crate) struct EnvState {
    #[allow(dead_code)]
    pub(crate) odbc_version: OdbcVersion,
    #[allow(dead_code)]
    pub(crate) output_nts: bool,
    /// Active child DBC handles, mirroring msodbcsql's `lppllpdbc`.
    /// SQLFreeHandle(ENV) checks this is empty before freeing.
    pub(crate) connections: Vec<*mut c_void>,
}

impl EnvHandle {
    pub(crate) fn new() -> Self {
        Self {
            header: HandleHeader {
                object_type: HandleType::Env,
            },
            inner: Mutex::new(EnvState {
                odbc_version: OdbcVersion::Unset,
                output_nts: true, // SQL_ATTR_OUTPUT_NTS defaults to SQL_TRUE
                connections: Vec::new(),
            }),
        }
    }
}

impl HasHeader for EnvHandle {
    fn header_mut(&mut self) -> &mut HandleHeader {
        &mut self.header
    }
}

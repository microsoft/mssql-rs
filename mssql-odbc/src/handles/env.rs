// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::io;
use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;
use tracing::error;

use super::{HandleType, HasObjectType};
use crate::api::odbc_types::{SQL_OV_ODBC2, SQL_OV_ODBC3, SQL_OV_ODBC3_80};
use crate::error::{DiagRecord, HasDiagnostics};

/// ODBC environment attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum OdbcVersion {
    /// Not yet set — calls requiring a version will fail with HY010.
    Unset = 0,
    Odbc2 = 2,
    Odbc3 = 3,
    Odbc3_80 = 380,
}

impl TryFrom<u32> for OdbcVersion {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            SQL_OV_ODBC2 => Ok(OdbcVersion::Odbc2),
            SQL_OV_ODBC3 => Ok(OdbcVersion::Odbc3),
            SQL_OV_ODBC3_80 => Ok(OdbcVersion::Odbc3_80),
            _ => Err(()),
        }
    }
}

/// Environment handle — equivalent to msodbcsql's `struct tagENV`.
///
/// One ENV is typically allocated per application. It owns connection handles
/// and stores environment-level attributes (ODBC version, connection pooling mode).
///
/// Thread-safety: The `inner` mutex protects mutable state. msodbcsql uses
/// `csEnv` (Unix) or relies on the Driver Manager (Windows) for serialization.
/// We always protect with a mutex for safety regardless of platform.
/// `object_type` is set once at construction and never mutated; `inner` (`≈ csEnv`) protects all mutable state.
#[derive(Debug)]
pub(crate) struct EnvHandle {
    pub(crate) object_type: HandleType,
    pub(crate) inner: Mutex<EnvState>,
    /// Shared Tokio runtime for all connections on this ENV.
    /// Wrapped in `Arc` so DBCs can hold a reference without lifetime issues.
    pub(crate) runtime: Arc<Runtime>,
}

/// Mutable state within an environment handle, protected by `inner`.
#[derive(Debug)]
pub(crate) struct EnvState {
    pub(crate) diag_records: Vec<DiagRecord>,
    // ---- derived tagENV fields below ----
    #[allow(dead_code)]
    pub(crate) odbc_version: OdbcVersion,
    #[allow(dead_code)]
    pub(crate) output_nts: bool,
    /// Active child DBC handles, mirroring msodbcsql's `lppllpdbc`.
    pub(crate) connections: Vec<*mut c_void>,
}

impl HasDiagnostics for EnvState {
    fn diag_records(&self) -> &[DiagRecord] {
        &self.diag_records
    }
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        &mut self.diag_records
    }
}

impl EnvHandle {
    pub(crate) fn new() -> io::Result<Self> {
        let runtime = Runtime::new().inspect_err(|e| {
            error!(%e, "failed to create Tokio runtime");
        })?;
        Ok(Self {
            object_type: HandleType::Env,
            inner: Mutex::new(EnvState {
                diag_records: Vec::new(),
                odbc_version: OdbcVersion::Unset,
                output_nts: true, // SQL_ATTR_OUTPUT_NTS defaults to SQL_TRUE
                connections: Vec::new(),
            }),
            runtime: Arc::new(runtime),
        })
    }
}

impl HasObjectType for EnvHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

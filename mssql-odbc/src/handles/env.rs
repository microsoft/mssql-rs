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

/// Environment handle
///
/// One ENV is typically allocated per application. It owns connection handles
/// and stores environment-level attributes (ODBC version, connection pooling mode).
///
/// Thread-safety: The `inner` mutex protects mutable state. msodbcsql serializes
/// via an environment-level critical section (Unix) or relies on the Driver
/// Manager (Windows). We always protect with a mutex for safety regardless of platform.
/// `object_type` is set once at construction and never mutated; `inner` protects all mutable state.
#[derive(Debug)]
pub(crate) struct EnvHandle {
    pub(crate) object_type: HandleType,
    pub(crate) inner: Mutex<EnvState>,
    /// Shared Tokio runtime for all connections on this ENV.
    /// Wrapped in `Arc` so DBCs can hold a reference without lifetime issues,
    /// and in `Option` so `Drop` can take ownership to shut it down (see the
    /// `Drop` impl below). Only `None` after this handle starts dropping.
    pub(crate) runtime: Option<Arc<Runtime>>,
}

/// Mutable state within an environment handle, protected by `inner`.
#[derive(Debug)]
pub(crate) struct EnvState {
    pub(crate) diag_records: Vec<DiagRecord>,
    pub(crate) odbc_version: OdbcVersion,
    #[allow(dead_code)]
    pub(crate) output_nts: bool,
    /// Active child DBC handles
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .inspect_err(|e| {
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
            runtime: Some(Arc::new(runtime)),
        })
    }
}

impl HasObjectType for EnvHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

impl Drop for EnvHandle {
    /// Shuts down the per-ENV Tokio runtime without joining its worker thread.
    ///
    /// `SQLFreeHandle(SQL_HANDLE_ENV)` can run long after the OS has already
    /// force-terminated background threads — e.g. a caller that loads this
    /// driver directly instead of through the ODBC Driver Manager may defer
    /// the free to a C++ static destructor that only runs at
    /// `DLL_PROCESS_DETACH`, by which point Windows has already killed every
    /// thread but the one tearing the process down (AB#47509). The default
    /// `Runtime` drop unconditionally joins its worker thread, which panics
    /// ("threads should not terminate unexpectedly") if that thread no
    /// longer exists. `shutdown_background` detaches instead of joining, so
    /// it's safe no matter when this runs.
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take().and_then(|rt| Arc::try_unwrap(rt).ok()) {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against reintroducing a blocking join in `Drop`: this must
    /// return promptly rather than waiting out the runtime's worker thread,
    /// which is what `shutdown_background` (vs. the default `Runtime` drop)
    /// buys us. Can't reproduce the OS-forcibly-killed-the-thread case that
    /// actually panicked (AB#47509) from a unit test — that needs a real
    /// process/DLL teardown — so this exercises the ordinary path instead.
    #[test]
    fn dropping_env_handle_does_not_hang_or_panic() {
        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        drop(env);
    }
}

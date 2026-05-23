// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::Mutex;

use super::{DiagRecord, HandleType, HasObjectType};
use crate::api::odbc_types::{SQL_OV_ODBC2, SQL_OV_ODBC3, SQL_OV_ODBC3_80};

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

/// Environment handle — Rust port of msodbcsql's `struct tagENV : tagOBJBASE`.
///
/// One ENV is typically allocated per application. Owns connection handles and
/// environment attributes (ODBC version, connection pooling mode).
///
/// Field order mirrors msodbcsql's `tagENV`:
/// - `object_type`  — inherited `tagOBJBASE.ObjectType`, read lock-free
/// - `inner`        — Rust analog of `tagENV.csEnv`; the `Mutex<T>` covers the
///   inherited `tagOBJBASE.errinfo` (as `EnvState::diag_records`) plus the
///   derived `tagENV` fields (`dwOptionsE`, `lppllpdbc`, ...). One lock per
///   handle, matching msodbcsql.
///
/// Rust note: `Mutex<T>` must wrap a `T`, so the locked fields live inside the
/// nested `EnvState` struct rather than being peer members of `EnvHandle` the
/// way they're peer members of `tagENV`. Everything else matches msodbcsql.
#[derive(Debug)]
pub(crate) struct EnvHandle {
    #[allow(dead_code)]
    pub(crate) object_type: HandleType,
    #[allow(dead_code)]
    pub(crate) inner: Mutex<EnvState>,
}

/// Fields of `tagENV` protected by `csEnv`. Layout mirrors C++ inheritance:
/// inherited `tagOBJBASE` fields first, then derived `tagENV` fields.
#[derive(Debug)]
pub(crate) struct EnvState {
    /// Inherited from `tagOBJBASE.errinfo` — diagnostic records read by
    /// `SQLGetDiagRec`, cleared at the start of each API call (msodbcsql's
    /// `FreeErrors`).
    pub(crate) diag_records: Vec<DiagRecord>,
    // ---- derived tagENV fields below ----
    #[allow(dead_code)]
    pub(crate) odbc_version: OdbcVersion,
    #[allow(dead_code)]
    pub(crate) output_nts: bool,
    /// Active child DBC handles, mirroring msodbcsql's `lppllpdbc`.
    pub(crate) connections: Vec<*mut c_void>,
}

impl EnvHandle {
    pub(crate) fn new() -> Self {
        Self {
            object_type: HandleType::Env,
            inner: Mutex::new(EnvState {
                diag_records: Vec::new(),
                odbc_version: OdbcVersion::Unset,
                output_nts: true, // SQL_ATTR_OUTPUT_NTS defaults to SQL_TRUE
                connections: Vec::new(),
            }),
        }
    }
}

impl HasObjectType for EnvHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

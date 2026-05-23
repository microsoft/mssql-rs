// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod dbc;
pub(crate) mod diag;
mod env;
mod stmt;

pub(crate) use dbc::DbcHandle;
pub(crate) use diag::DiagRecord;
pub(crate) use env::EnvHandle;
pub(crate) use env::OdbcVersion;
pub(crate) use stmt::StmtHandle;

use std::ffi::c_void;

use tracing::{debug, trace};

/// Discriminant stored inside each handle for runtime type-checking.
/// Mirrors msodbcsql's `OBJECTTYPE` enum — guards against misuse of freed or wrong-type handles.
///
/// Lives as a direct field on each `XxxHandle` (mirroring `tagOBJBASE::ObjectType`
/// which is inlined into `tagENV` / `tagDBC` / `tagSTMT` via C++ inheritance).
/// Read without taking the per-handle lock (msodbcsql reads `ObjectType` outside
/// `csEnv`/`csDbc`/`csStmt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
#[allow(dead_code)]
pub(crate) enum HandleType {
    Env = 1,
    Dbc = 2,
    Stmt = 3,
    Desc = 4,
    Invalid = 0xDEADBEEF,
}

/// Converts a heap-allocated handle into an opaque `*mut c_void` for return through FFI.
/// Ownership transfers to the caller (ODBC Driver Manager).
pub(crate) fn handle_to_raw<T>(handle: Box<T>) -> *mut c_void {
    Box::into_raw(handle) as *mut c_void
}

/// Recovers a reference to a typed handle from an opaque `*mut c_void`.
///
/// The returned lifetime `'a` is chosen by the caller — no Rust borrow tracks
/// this allocation. The pointer was surrendered by `Box::into_raw` in
/// `handle_to_raw`, making it "unowned" from the borrow checker's perspective.
/// The caller must ensure the reference is not used after `free_handle` is called.
///
/// # Safety
/// - `raw` must have been created by `handle_to_raw` for the same type `T`.
/// - The handle must not have been freed yet (`free_handle` not yet called).
/// - The caller must not use the returned reference after `free_handle` is called.
#[allow(dead_code)]
pub(crate) unsafe fn handle_from_raw<'a, T>(raw: *mut c_void) -> &'a T {
    unsafe { &*(raw as *const T) }
}

/// Recovers a mutable reference to a typed handle from an opaque `*mut c_void`.
///
/// Same caller-chosen lifetime as `handle_from_raw`. The caller is responsible
/// for ensuring exclusive access — creating two `&mut` references to the same
/// handle is instant UB. Prefer `handle_from_raw` (shared ref) + interior
/// mutability (`Mutex`) when concurrent access is possible.
///
/// # Safety
/// - All requirements of `handle_from_raw`, plus:
/// - The caller must guarantee exclusive access to the handle for the
///   duration of the returned reference.
#[allow(dead_code)]
pub(crate) unsafe fn handle_from_raw_mut<'a, T>(raw: *mut c_void) -> &'a mut T {
    unsafe { &mut *(raw as *mut T) }
}

/// Frees a handle that was allocated via `handle_to_raw`.
///
/// Marks the handle's `object_type` as `Invalid` before dropping, so that
/// use-after-free attempts can be detected (mirrors msodbcsql setting
/// `ObjectType = LPINVALIDType` on free).
///
/// # Safety
/// Must only be called once per handle. The pointer is invalid after this call.
#[allow(dead_code)]
pub(crate) unsafe fn free_handle<T: HasObjectType>(raw: *mut c_void) {
    if !raw.is_null() {
        let handle = unsafe { &mut *(raw as *mut T) };
        let object_type = *handle.object_type_mut();
        debug!(?raw, ?object_type, "Freeing handle");
        *handle.object_type_mut() = HandleType::Invalid;
        let _ = unsafe { Box::from_raw(raw as *mut T) };
        trace!(?raw, "Handle freed");
    }
}

/// Trait for handle types that expose the lock-free `ObjectType` field
/// (used by `free_handle` to stamp `Invalid` on free for use-after-free
/// detection — mirrors msodbcsql setting `ObjectType = LPINVALIDType`).
pub(crate) trait HasObjectType {
    fn object_type_mut(&mut self) -> &mut HandleType;
}

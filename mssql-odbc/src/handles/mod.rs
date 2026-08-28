// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod dbc;
pub(crate) mod desc;
mod env;
pub(crate) mod stmt;

pub(crate) use dbc::DbcHandle;
pub(crate) use desc::DescHandle;
pub(crate) use env::EnvHandle;
pub(crate) use env::OdbcVersion;
pub(crate) use stmt::StmtHandle;

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};

use tracing::{debug, trace};

/// Discriminant stored inside each handle.
/// Mirrors msodbcsql's handle object-type tag. Checked in debug builds via `debug_assert_eq!`;
/// in release builds the DM is trusted to pass the correct handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum HandleType {
    Env = 1,
    Dbc = 2,
    Stmt = 3,
    Desc = 4,
    Invalid = 0xDEADBEEF,
}

/// Tracks every currently-allocated handle by raw address, independent of the
/// handle's own memory. This is what lets a free path confirm a handle is
/// still live *before* dereferencing it, instead of dereferencing first and
/// only checking a parent's child list afterward.
///
/// Closes a real gap: `SQLDisconnect` cascades-frees every STMT and explicit
/// DESC on a connection (`sql_disconnect_safe`), which the ODBC Driver
/// Manager does not observe — an application legitimately holding one of
/// those now-stale handles can still call `SQLFreeHandle` on it afterward
/// (confirmed reachable during PR #370's review, not merely hypothetical).
/// Reading through such a handle before confirming it is still live is a
/// genuine use-after-free once the memory has actually been reused —
/// observed allocator-dependently on macOS in CI. See mssql-rs#400.
///
/// Deliberately narrower than a full refcounted-handle-lifetime redesign
/// (tracked separately, `disconnect.rs`'s existing TODO): this only
/// distinguishes "already freed" from "still live," not "live but
/// concurrently being freed on another thread right now" — that TOCTOU
/// window is the pre-existing, wider concurrent-use race this crate already
/// documents and does not attempt to close here.
static LIVE_HANDLES: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Returns whether `raw` currently refers to a handle that has been
/// allocated (via `handle_to_raw`) and not yet freed (via `free_handle`).
///
/// Safe to call on any pointer value, including a stale one whose memory may
/// already be deallocated or reused: this never dereferences `raw`, so
/// callers can use it to decide whether dereferencing is safe in the first
/// place. If the registry lock is itself poisoned, conservatively reports
/// "not live" — treating an ambiguous handle as already-freed risks a leak,
/// treating it as live risks a genuine use-after-free.
pub(crate) fn is_live(raw: *mut c_void) -> bool {
    LIVE_HANDLES
        .lock()
        .map(|live| live.contains(&(raw as usize)))
        .unwrap_or(false)
}

/// Converts a heap-allocated handle into an opaque `*mut c_void` for return through FFI.
/// Ownership transfers to the caller (ODBC Driver Manager).
pub(crate) fn handle_to_raw<T>(handle: Box<T>) -> *mut c_void {
    let raw = Box::into_raw(handle) as *mut c_void;
    if let Ok(mut live) = LIVE_HANDLES.lock() {
        live.insert(raw as usize);
    }
    raw
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
/// use-after-free attempts can be detected
///
/// # Safety
/// Must only be called once per handle. The pointer is invalid after this call.
pub(crate) unsafe fn free_handle<T: HasObjectType>(raw: *mut c_void) {
    if !raw.is_null() {
        if let Ok(mut live) = LIVE_HANDLES.lock() {
            live.remove(&(raw as usize));
        }
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
/// detection
pub(crate) trait HasObjectType {
    fn object_type_mut(&mut self) -> &mut HandleType;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_is_live_after_alloc_and_not_after_free() {
        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        let raw = handle_to_raw(Box::new(env));

        assert!(is_live(raw), "a freshly allocated handle must be live");

        unsafe { free_handle::<EnvHandle>(raw) };

        assert!(
            !is_live(raw),
            "a freed handle must no longer be reported live"
        );
    }

    /// `is_live` must never dereference its argument: a pointer value that
    /// was never handed out by `handle_to_raw` at all (as opposed to one that
    /// was handed out and later freed) is exactly the kind of garbage input
    /// it has to tolerate without touching the memory it (doesn't) point at.
    #[test]
    fn is_live_is_false_for_an_address_never_allocated() {
        let never_allocated = 0xDEAD_BEEF_usize as *mut c_void;
        assert!(!is_live(never_allocated));
    }
}

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
pub(crate) use env::process_is_shutting_down;
pub(crate) use stmt::StmtHandle;

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, MutexGuard};

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

/// Tracks every currently-allocated handle's `HandleType` by raw address,
/// independent of the handle's own memory. Lets a free path confirm a
/// handle is still live, and of the expected type, *before* dereferencing
/// it — instead of dereferencing first and checking a parent's child list,
/// or the handle's own type tag, afterward.
///
/// Closes two real gaps:
/// - Address liveness: `SQLDisconnect` cascade-frees every STMT and
///   explicit DESC on a connection (`sql_disconnect_safe`) behind the
///   Driver Manager's back, so an application legitimately holding one of
///   those now-stale handles can still call `SQLFreeHandle` on it
///   afterward. Dereferencing before confirming it's still live is a
///   genuine use-after-free once the memory is reused. See #400.
/// - Type confusion: `SQLFreeHandle` dispatches on the caller-supplied
///   `handle_type` with no cross-check, so a live handle of the wrong type
///   (e.g. a DESC passed where a STMT was expected) used to reach
///   `handle_from_raw::<StmtHandle>` and get reinterpreted — no address
///   reuse required. Recording the type here answers that without the
///   same dereference-to-check-the-type problem this registry exists to
///   avoid in the first place.
///
/// # Known limitation: address reuse (ABA)
///
/// Tracking `HandleType` doesn't extend to telling one allocation apart
/// from a *later, same-typed* one that the allocator places at the same
/// freed address: free handle A, allocate a new same-typed handle B at A's
/// old address, and this registry reports A's address as live with the
/// expected type again — not because A is valid, but because it cannot
/// distinguish A from B. See
/// `tests::is_live_cannot_distinguish_a_reused_address_from_the_original_allocation`
/// for a direct demonstration (real OS-level reuse can't be forced
/// portably/deterministically from a test). Closing this needs handle
/// identity that survives address reuse — a generation-tracked indirection
/// layer, the same class of redesign as the pre-existing refcounted-
/// handle-lifetime TODO in `disconnect.rs` — tracked separately as #422.
///
/// Also narrower in a second way: this only distinguishes "already freed
/// or wrong type" from "still live as expected," not "live but
/// concurrently being freed on another thread right now" — that TOCTOU
/// window is the pre-existing, wider concurrent-use race this crate
/// already documents and does not attempt to close here.
static LIVE_HANDLES: LazyLock<Mutex<HashMap<usize, HandleType>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Locks `LIVE_HANDLES`, recovering the guard even if the mutex is
/// poisoned. Its critical sections are a single `HashMap` operation each,
/// with no user code, I/O, or user-supplied `Hash` impl to panic mid-update
/// and leave the map inconsistent, so there is no invariant recovering
/// could violate here — only a handle-freeing path that would otherwise
/// break for the rest of the process over a poisoning that can't happen.
fn live_handles() -> MutexGuard<'static, HashMap<usize, HandleType>> {
    LIVE_HANDLES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Returns the `HandleType` recorded for `raw` if it currently refers to a
/// live handle (allocated via `handle_to_raw`, not yet freed via
/// `free_handle`), or `None` otherwise.
///
/// Safe to call on any pointer value, including one never allocated or
/// already freed, whose memory may be deallocated or reused: this never
/// dereferences `raw`, so callers can use it to decide whether
/// dereferencing is safe — and as what type — in the first place.
pub(crate) fn live_type(raw: *mut c_void) -> Option<HandleType> {
    live_handles().get(&(raw as usize)).copied()
}

/// Returns whether `raw` currently refers to a live handle of any type.
/// Only used by tests today — production call sites need the type check
/// `live_type` itself provides, so they match on it directly.
#[cfg(test)]
pub(crate) fn is_live(raw: *mut c_void) -> bool {
    live_type(raw).is_some()
}

/// Converts a heap-allocated handle into an opaque `*mut c_void` for return through FFI.
/// Ownership transfers to the caller (ODBC Driver Manager). Records the
/// handle's `HandleType` in `LIVE_HANDLES` before it does, so `live_type`
/// can answer for it later without a dereference.
pub(crate) fn handle_to_raw<T: HasObjectType>(mut handle: Box<T>) -> *mut c_void {
    let object_type = *handle.object_type_mut();
    let raw = Box::into_raw(handle) as *mut c_void;
    live_handles().insert(raw as usize, object_type);
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
        live_handles().remove(&(raw as usize));
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

    /// A poisoned `LIVE_HANDLES` mutex must not permanently blind the
    /// registry: `live_handles()` recovers the guard instead of treating
    /// poison as fatal, since its critical sections are a single `HashMap`
    /// op each with no invariant a panic mid-update could leave broken.
    /// Poisons the lock directly (panicking while holding it, without
    /// mutating the map), then confirms handle tracking still works
    /// normally afterward rather than silently going blind for the rest of
    /// the process.
    #[test]
    fn live_handles_registry_recovers_from_a_poisoned_lock() {
        let _ = std::panic::catch_unwind(|| {
            let _guard = LIVE_HANDLES.lock().unwrap();
            panic!("poison the live-handles registry lock");
        });

        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        let raw = handle_to_raw(Box::new(env));
        assert!(
            is_live(raw),
            "a poisoned registry must recover and keep tracking new handles"
        );

        unsafe { free_handle::<EnvHandle>(raw) };
        assert!(
            !is_live(raw),
            "a poisoned registry must recover and still untrack freed handles"
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

    /// Documents the registry's known ABA gap (see the doc comment on
    /// `LIVE_HANDLES`): tracking `HandleType` closes type confusion, but not
    /// address reuse — it still cannot tell an original allocation apart
    /// from an unrelated, *same-typed* later one that the allocator places
    /// at the same now-freed address.
    ///
    /// Real OS-level address reuse can't be forced portably or
    /// deterministically from a test, so this demonstrates the exact
    /// consequence directly against the registry: free a handle, then
    /// insert its address back under the same type (standing in for the
    /// allocator reusing that address for a new, unrelated allocation of
    /// the same type), and show `live_type` reports the stale original as
    /// live and correctly-typed again — not because the original
    /// allocation is in any way still valid.
    #[test]
    fn is_live_cannot_distinguish_a_reused_address_from_the_original_allocation() {
        let original = EnvHandle::new().expect("failed to create EnvHandle for test");
        let raw = handle_to_raw(Box::new(original));

        unsafe { free_handle::<EnvHandle>(raw) };
        assert!(!is_live(raw), "freed handle must not be reported live");

        // Stand in for the allocator handing the same address back out for
        // an unrelated new allocation of the same type, without going
        // through `handle_to_raw` (there is no portable way to force the
        // real allocator to reuse this exact address deterministically).
        live_handles().insert(raw as usize, HandleType::Env);

        assert_eq!(
            live_type(raw),
            Some(HandleType::Env),
            "registry cannot distinguish a reused address from the stale original, \
             even with type tracking, once the new allocation shares the same type: \
             this is the documented ABA limitation, not a passing safety check"
        );

        // Clean up: this test shares `LIVE_HANDLES` with every other test in
        // the process under plain `cargo test` (not `cargo nextest`, which
        // gives each test its own process), so leaving `raw` behind would
        // seed a phantom-live address into state other tests read.
        live_handles().remove(&(raw as usize));
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GSSAPI FFI bindings for Kerberos authentication.
//!
//! This module provides FFI bindings to the MIT Kerberos GSSAPI library
//! (`libgssapi_krb5`). The library is loaded dynamically at runtime using
//! dlopen, so binaries can run on systems without krb5 installed - GSSAPI
//! is only required if integrated authentication is actually used.
//!
//! This approach matches how ODBC handles GSSAPI: no compile-time dependency
//! on krb5, with runtime detection and graceful fallback.
//!
//! # Safety
//!
//! All FFI functions are inherently unsafe. The safe wrappers in `GssapiContext`
//! should be used instead of calling these directly.

use std::ffi::CString;
use std::os::raw::c_void;
use std::ptr;
use std::sync::OnceLock;

// =============================================================================
// GSSAPI Type Definitions
// =============================================================================

/// GSSAPI OM_uint32 type (status codes and flags)
pub(crate) type GssOmUint32 = u32;

/// GSSAPI context handle (opaque pointer)
pub(crate) type GssCtxIdT = *mut c_void;

/// GSSAPI credential handle (opaque pointer)
pub(crate) type GssCredIdT = *mut c_void;

/// GSSAPI name handle (opaque pointer)
pub(crate) type GssNameT = *mut c_void;

/// GSSAPI OID structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct GssOidDesc {
    pub length: GssOmUint32,
    pub elements: *mut c_void,
}

/// GSSAPI OID pointer type
pub(crate) type GssOid = *mut GssOidDesc;

/// GSSAPI buffer descriptor
#[repr(C)]
#[derive(Debug)]
pub(crate) struct GssBufferDesc {
    pub length: usize,
    pub value: *mut c_void,
}

impl Default for GssBufferDesc {
    fn default() -> Self {
        Self {
            length: 0,
            value: ptr::null_mut(),
        }
    }
}

impl GssBufferDesc {
    /// Creates a buffer from a byte slice (for input)
    pub fn from_slice(data: &[u8]) -> Self {
        Self {
            length: data.len(),
            value: data.as_ptr() as *mut c_void,
        }
    }

    /// Creates a buffer from a string (for SPN)
    pub fn from_str(s: &str) -> Self {
        Self {
            length: s.len(),
            value: s.as_ptr() as *mut c_void,
        }
    }

    /// Converts the buffer to a byte vector (for output)
    ///
    /// # Safety
    ///
    /// The buffer must contain valid data of the specified length.
    pub unsafe fn to_vec(&self) -> Vec<u8> {
        if self.value.is_null() || self.length == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(self.value as *const u8, self.length).to_vec() }
        }
    }
}

/// GSSAPI buffer pointer type
type GssBufferT = *mut GssBufferDesc;

/// GSSAPI channel bindings structure
#[repr(C)]
#[derive(Debug)]
pub(super) struct GssChannelBindingsStruct {
    pub(super) initiator_addrtype: GssOmUint32,
    pub(super) initiator_address: GssBufferDesc,
    pub(super) acceptor_addrtype: GssOmUint32,
    pub(super) acceptor_address: GssBufferDesc,
    pub(super) application_data: GssBufferDesc,
}

pub(super) type GssChannelBindingsT = *mut GssChannelBindingsStruct;

// =============================================================================
// GSSAPI Constants
// =============================================================================

/// Null context handle
pub(super) const GSS_C_NO_CONTEXT: GssCtxIdT = ptr::null_mut();

/// Null credential handle (use default credentials)
pub(super) const GSS_C_NO_CREDENTIAL: GssCredIdT = ptr::null_mut();

/// Null OID (use default mechanism)
pub(super) const GSS_C_NO_OID: GssOid = ptr::null_mut();

/// Null channel bindings
pub(super) const GSS_C_NO_CHANNEL_BINDINGS: GssChannelBindingsT = ptr::null_mut();

/// Null buffer
const GSS_C_NO_BUFFER: GssBufferT = ptr::null_mut();

// GSSAPI Major Status Codes
/// Operation completed successfully
pub(super) const GSS_S_COMPLETE: GssOmUint32 = 0;

/// Continuation call needed (not an error)
pub(super) const GSS_S_CONTINUE_NEEDED: GssOmUint32 = 1 << 0;

/// Unsupported mechanism
const GSS_S_BAD_MECH: GssOmUint32 = 1 << 16;

/// No credentials available
pub(super) const GSS_S_NO_CRED: GssOmUint32 = 7 << 16;

/// Credentials expired
const GSS_S_CREDENTIALS_EXPIRED: GssOmUint32 = 11 << 16;

/// Invalid context
const GSS_S_NO_CONTEXT: GssOmUint32 = 8 << 16;

/// Defective token
const GSS_S_DEFECTIVE_TOKEN: GssOmUint32 = 9 << 16;

/// Invalid name
const GSS_S_BAD_NAME: GssOmUint32 = 2 << 16;

/// Name type not supported
const GSS_S_BAD_NAMETYPE: GssOmUint32 = 3 << 16;

/// General failure
const GSS_S_FAILURE: GssOmUint32 = 13 << 16;

// GSSAPI Request Flags
/// Request mutual authentication
pub(super) const GSS_C_MUTUAL_FLAG: GssOmUint32 = 2;

/// Request credential delegation
pub(super) const GSS_C_DELEG_FLAG: GssOmUint32 = 1;

/// Request replay detection
const GSS_C_REPLAY_FLAG: GssOmUint32 = 4;

/// Request sequence detection
const GSS_C_SEQUENCE_FLAG: GssOmUint32 = 8;

// Status type constants for gss_display_status
/// Major status (calling/routine error)
const GSS_C_GSS_CODE: i32 = 1;

/// Minor status (mechanism-specific)
const GSS_C_MECH_CODE: i32 = 2;

// =============================================================================
// GSSAPI Function Pointer Types
// =============================================================================

type GssInitSecContextFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    cred_handle: GssCredIdT,
    context_handle: *mut GssCtxIdT,
    target_name: GssNameT,
    mech_type: GssOid,
    req_flags: GssOmUint32,
    time_req: GssOmUint32,
    input_chan_bindings: GssChannelBindingsT,
    input_token: GssBufferT,
    actual_mech_type: *mut GssOid,
    output_token: GssBufferT,
    ret_flags: *mut GssOmUint32,
    time_rec: *mut GssOmUint32,
) -> GssOmUint32;

type GssImportNameFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    input_name_buffer: GssBufferT,
    input_name_type: GssOid,
    output_name: *mut GssNameT,
) -> GssOmUint32;

type GssReleaseBufferFn =
    unsafe extern "C" fn(minor_status: *mut GssOmUint32, buffer: GssBufferT) -> GssOmUint32;

type GssReleaseNameFn =
    unsafe extern "C" fn(minor_status: *mut GssOmUint32, name: *mut GssNameT) -> GssOmUint32;

type GssDeleteSecContextFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    context_handle: *mut GssCtxIdT,
    output_token: GssBufferT,
) -> GssOmUint32;

type GssDisplayStatusFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    status_value: GssOmUint32,
    status_type: i32,
    mech_type: GssOid,
    message_context: *mut GssOmUint32,
    status_string: GssBufferT,
) -> GssOmUint32;

type GssAcquireCredFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    desired_name: GssNameT,
    time_req: GssOmUint32,
    desired_mechs: *mut GssOidDesc,
    cred_usage: i32,
    output_cred_handle: *mut GssCredIdT,
    actual_mechs: *mut *mut GssOidDesc,
    time_rec: *mut GssOmUint32,
) -> GssOmUint32;

type GssReleaseCredFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    cred_handle: *mut GssCredIdT,
) -> GssOmUint32;

type GssInquireCredFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    cred_handle: GssCredIdT,
    name: *mut GssNameT,
    lifetime: *mut GssOmUint32,
    cred_usage: *mut i32,
    mechanisms: *mut *mut c_void,
) -> GssOmUint32;

type GssDisplayNameFn = unsafe extern "C" fn(
    minor_status: *mut GssOmUint32,
    input_name: GssNameT,
    output_name_buffer: GssBufferT,
    output_name_type: *mut GssOid,
) -> GssOmUint32;

/// SPNEGO mechanism OID 1.3.6.1.5.5.2 (RFC 4178), the mechanism HTTP
/// `Negotiate` (RFC 4559) requires.
static GSS_SPNEGO_MECHANISM_BYTES: [u8; 6] = [0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];

static GSS_SPNEGO_MECHANISM_DESC: SyncOidDesc = SyncOidDesc(GssOidDesc {
    length: 6,
    elements: GSS_SPNEGO_MECHANISM_BYTES.as_ptr() as *mut c_void,
});

/// Returns the SPNEGO mechanism OID. The descriptor is static and never
/// written through, so handing out a `*mut` for the C ABI is sound.
pub(super) fn spnego_mechanism() -> GssOid {
    &GSS_SPNEGO_MECHANISM_DESC.0 as *const GssOidDesc as *mut GssOidDesc
}

// =============================================================================
// Static OID Definition (fallback)
// =============================================================================

/// Static OID bytes for GSS_C_NT_HOSTBASED_SERVICE.
///
/// This is the OID {iso(1) member-body(2) United-States(840) mit(113554)
/// infosys(1) gssapi(2) generic(1) service_name(4)}.
/// OID: 1.2.840.113554.1.2.1.4
///
/// # Why is this hardcoded?
///
/// This OID is standardized by RFC 2078 and has not changed since 1997.
/// It tells the GSSAPI library how to interpret a name string - in this case,
/// that the string is a "host-based service name" like `MSSQLSvc/server:1433`.
///
/// The OID is a LOCAL instruction to the GSSAPI library only - it is NOT
/// transmitted over the wire. The library uses it to construct a Kerberos
/// principal name in the standard format that the KDC and SQL Server understand.
///
/// We could always use this hardcoded value, but we first try to load it from
/// the library for idiomatic usage and defensive programming (in case some
/// exotic GSSAPI implementation has a quirk). The hardcoded fallback ensures
/// we work even if the library symbol is unexpectedly null.
static GSS_C_NT_HOSTBASED_SERVICE_BYTES: [u8; 10] =
    [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x01, 0x04];

/// Wrapper type to make the OID descriptor Sync-safe.
struct SyncOidDesc(GssOidDesc);

// SAFETY: The GssOidDesc contains a pointer to static data which is never modified.
unsafe impl Sync for SyncOidDesc {}

/// Static OID descriptor for GSS_C_NT_HOSTBASED_SERVICE (fallback).
static GSS_C_NT_HOSTBASED_SERVICE_DESC: SyncOidDesc = SyncOidDesc(GssOidDesc {
    length: 10,
    elements: GSS_C_NT_HOSTBASED_SERVICE_BYTES.as_ptr() as *mut c_void,
});

// =============================================================================
// Dynamic Library Loading
// =============================================================================

/// Holds function pointers loaded from libgssapi_krb5 via dlopen.
struct GssapiLibrary {
    _handle: *mut c_void,
    gss_init_sec_context: GssInitSecContextFn,
    gss_import_name: GssImportNameFn,
    gss_release_buffer: GssReleaseBufferFn,
    gss_release_name: GssReleaseNameFn,
    gss_delete_sec_context: GssDeleteSecContextFn,
    gss_display_status: GssDisplayStatusFn,
    gss_acquire_cred: GssAcquireCredFn,
    gss_release_cred: GssReleaseCredFn,
    /// Optional: only needed to read the default principal name, so a library
    /// without it still serves TDS integrated authentication.
    gss_inquire_cred: Option<GssInquireCredFn>,
    gss_display_name: Option<GssDisplayNameFn>,
    /// GSS_C_NT_HOSTBASED_SERVICE OID pointer (from library or fallback)
    gss_c_nt_hostbased_service: GssOid,
}

// SAFETY: The library handle and function pointers are only accessed through
// immutable references after initialization. GSSAPI is thread-safe.
unsafe impl Send for GssapiLibrary {}
unsafe impl Sync for GssapiLibrary {}

/// Global GSSAPI library instance, loaded once on first use.
static GSSAPI_LIB: OnceLock<Option<GssapiLibrary>> = OnceLock::new();

/// Library names to try loading (in order)
#[cfg(target_os = "linux")]
const GSSAPI_LIB_NAMES: &[&str] = &[
    "libgssapi_krb5.so.2", // Linux (versioned)
    "libgssapi_krb5.so",   // Linux (unversioned)
];

#[cfg(target_os = "macos")]
const GSSAPI_LIB_NAMES: &[&str] = &[
    "libgssapi_krb5.dylib",          // macOS Homebrew
    "/usr/lib/libgssapi_krb5.dylib", // macOS system
];

// Fallback for other Unix-like systems (FreeBSD, OpenBSD, etc.).
// Uses a generic .so name which may work on most BSD variants.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const GSSAPI_LIB_NAMES: &[&str] = &["libgssapi_krb5.so"];

impl GssapiLibrary {
    /// Attempts to load the GSSAPI library and resolve all required symbols.
    fn load() -> Option<Self> {
        // Try each library name
        for lib_name in GSSAPI_LIB_NAMES {
            if let Some(lib) = Self::try_load(lib_name) {
                tracing::debug!("Loaded GSSAPI library: {}", lib_name);
                return Some(lib);
            }
        }

        tracing::debug!("GSSAPI library not available (krb5 not installed)");
        None
    }

    #[allow(clippy::missing_transmute_annotations)]
    fn try_load(lib_name: &str) -> Option<Self> {
        let lib_cstr = CString::new(lib_name).ok()?;

        // dlopen with RTLD_LAZY | RTLD_LOCAL
        let handle = unsafe { libc::dlopen(lib_cstr.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };

        if handle.is_null() {
            return None;
        }

        // Helper to load a symbol
        let load_sym = |name: &str| -> *mut c_void {
            let sym_name = match CString::new(name) {
                Ok(s) => s,
                Err(_) => return ptr::null_mut(),
            };
            unsafe { libc::dlsym(handle, sym_name.as_ptr()) }
        };

        // Load all required function symbols
        let gss_init_sec_context = load_sym("gss_init_sec_context");
        let gss_import_name = load_sym("gss_import_name");
        let gss_release_buffer = load_sym("gss_release_buffer");
        let gss_release_name = load_sym("gss_release_name");
        let gss_delete_sec_context = load_sym("gss_delete_sec_context");
        let gss_display_status = load_sym("gss_display_status");
        let gss_acquire_cred = load_sym("gss_acquire_cred");
        let gss_release_cred = load_sym("gss_release_cred");
        let gss_inquire_cred = load_sym("gss_inquire_cred");
        let gss_display_name = load_sym("gss_display_name");

        // Check all required symbols were loaded
        if gss_init_sec_context.is_null()
            || gss_import_name.is_null()
            || gss_release_buffer.is_null()
            || gss_release_name.is_null()
            || gss_delete_sec_context.is_null()
            || gss_display_status.is_null()
            || gss_acquire_cred.is_null()
            || gss_release_cred.is_null()
        {
            tracing::warn!("Failed to load required GSSAPI symbols");
            unsafe { libc::dlclose(handle) };
            return None;
        }

        // Load the GSS_C_NT_HOSTBASED_SERVICE OID symbol.
        // We try to load from the library first for idiomatic GSSAPI usage,
        // but fall back to our hardcoded OID if the symbol is unavailable.
        // The hardcoded value is RFC-standardized and functionally equivalent.
        let gss_c_nt_hostbased_service: GssOid = {
            // Try GSS_C_NT_HOSTBASED_SERVICE first
            let sym = load_sym("GSS_C_NT_HOSTBASED_SERVICE");
            if !sym.is_null() {
                // The symbol is a pointer to the OID
                let oid = unsafe { *(sym as *const GssOid) };
                if !oid.is_null() {
                    oid
                } else {
                    // Use fallback
                    &GSS_C_NT_HOSTBASED_SERVICE_DESC.0 as *const GssOidDesc as *mut GssOidDesc
                }
            } else {
                // Try gss_nt_service_name as alternative
                let alt_sym = load_sym("gss_nt_service_name");
                if !alt_sym.is_null() {
                    let oid = unsafe { *(alt_sym as *const GssOid) };
                    if !oid.is_null() {
                        oid
                    } else {
                        &GSS_C_NT_HOSTBASED_SERVICE_DESC.0 as *const GssOidDesc as *mut GssOidDesc
                    }
                } else {
                    // Use our static fallback OID
                    &GSS_C_NT_HOSTBASED_SERVICE_DESC.0 as *const GssOidDesc as *mut GssOidDesc
                }
            }
        };

        Some(Self {
            _handle: handle,
            gss_init_sec_context: unsafe { std::mem::transmute(gss_init_sec_context) },
            gss_import_name: unsafe { std::mem::transmute(gss_import_name) },
            gss_release_buffer: unsafe { std::mem::transmute(gss_release_buffer) },
            gss_release_name: unsafe { std::mem::transmute(gss_release_name) },
            gss_delete_sec_context: unsafe { std::mem::transmute(gss_delete_sec_context) },
            gss_display_status: unsafe { std::mem::transmute(gss_display_status) },
            gss_acquire_cred: unsafe { std::mem::transmute(gss_acquire_cred) },
            gss_release_cred: unsafe { std::mem::transmute(gss_release_cred) },
            gss_inquire_cred: (!gss_inquire_cred.is_null())
                .then(|| unsafe { std::mem::transmute(gss_inquire_cred) }),
            gss_display_name: (!gss_display_name.is_null())
                .then(|| unsafe { std::mem::transmute(gss_display_name) }),
            gss_c_nt_hostbased_service,
        })
    }
}

/// Gets a reference to the loaded GSSAPI library, if available.
fn get_gssapi_lib() -> Option<&'static GssapiLibrary> {
    GSSAPI_LIB.get_or_init(GssapiLibrary::load).as_ref()
}

// =============================================================================
// Public API - GSSAPI Function Wrappers
// =============================================================================

/// Checks if GSSAPI library is available at runtime.
///
/// This attempts to load libgssapi_krb5 via dlopen. If the library is not
/// installed (krb5 package not present), this returns false and GSSAPI
/// authentication will not be available.
///
/// # Returns
///
/// `true` if GSSAPI library was successfully loaded, `false` otherwise.
pub fn is_gssapi_available() -> bool {
    get_gssapi_lib().is_some()
}

/// Gets the GSS_C_NT_HOSTBASED_SERVICE OID for importing service names.
///
/// # Panics
///
/// Panics if GSSAPI library is not available. Call `is_gssapi_available()` first.
pub fn get_gss_nt_service_name() -> GssOid {
    get_gssapi_lib()
        .expect("GSSAPI library not loaded - call is_gssapi_available() first")
        .gss_c_nt_hostbased_service
}

/// Initialize a security context with a server.
///
/// # Safety
///
/// All pointer arguments must be valid or null as appropriate.
/// GSSAPI library must be loaded.
#[allow(clippy::too_many_arguments)]
pub unsafe fn gss_init_sec_context(
    minor_status: *mut GssOmUint32,
    cred_handle: GssCredIdT,
    context_handle: *mut GssCtxIdT,
    target_name: GssNameT,
    mech_type: GssOid,
    req_flags: GssOmUint32,
    time_req: GssOmUint32,
    input_chan_bindings: GssChannelBindingsT,
    input_token: GssBufferT,
    actual_mech_type: *mut GssOid,
    output_token: GssBufferT,
    ret_flags: *mut GssOmUint32,
    time_rec: *mut GssOmUint32,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures all pointers are valid
    unsafe {
        (lib.gss_init_sec_context)(
            minor_status,
            cred_handle,
            context_handle,
            target_name,
            mech_type,
            req_flags,
            time_req,
            input_chan_bindings,
            input_token,
            actual_mech_type,
            output_token,
            ret_flags,
            time_rec,
        )
    }
}

/// Import a name (like an SPN) into internal GSSAPI format.
///
/// # Safety
///
/// All pointer arguments must be valid. GSSAPI library must be loaded.
pub unsafe fn gss_import_name(
    minor_status: *mut GssOmUint32,
    input_name_buffer: GssBufferT,
    input_name_type: GssOid,
    output_name: *mut GssNameT,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures all pointers are valid
    unsafe {
        (lib.gss_import_name)(
            minor_status,
            input_name_buffer,
            input_name_type,
            output_name,
        )
    }
}

/// Release a GSSAPI buffer.
///
/// # Safety
///
/// Buffer must have been allocated by GSSAPI.
pub unsafe fn gss_release_buffer(
    minor_status: *mut GssOmUint32,
    buffer: GssBufferT,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures buffer is valid
    unsafe { (lib.gss_release_buffer)(minor_status, buffer) }
}

/// Release a GSSAPI name.
///
/// # Safety
///
/// Name must have been created by GSSAPI.
pub unsafe fn gss_release_name(minor_status: *mut GssOmUint32, name: *mut GssNameT) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures name is valid
    unsafe { (lib.gss_release_name)(minor_status, name) }
}

/// Delete a security context.
///
/// # Safety
///
/// Context must have been created by GSSAPI.
pub unsafe fn gss_delete_sec_context(
    minor_status: *mut GssOmUint32,
    context_handle: *mut GssCtxIdT,
    output_token: GssBufferT,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures context is valid
    unsafe { (lib.gss_delete_sec_context)(minor_status, context_handle, output_token) }
}

/// Display a status message for debugging.
///
/// # Safety
///
/// All pointer arguments must be valid.
pub unsafe fn gss_display_status(
    minor_status: *mut GssOmUint32,
    status_value: GssOmUint32,
    status_type: i32,
    mech_type: GssOid,
    message_context: *mut GssOmUint32,
    status_string: GssBufferT,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures all pointers are valid
    unsafe {
        (lib.gss_display_status)(
            minor_status,
            status_value,
            status_type,
            mech_type,
            message_context,
            status_string,
        )
    }
}

/// Acquire credentials.
///
/// # Safety
///
/// All pointer arguments must be valid or null as appropriate.
#[allow(clippy::too_many_arguments)]
pub unsafe fn gss_acquire_cred(
    minor_status: *mut GssOmUint32,
    desired_name: GssNameT,
    time_req: GssOmUint32,
    desired_mechs: *mut GssOidDesc,
    cred_usage: i32,
    output_cred_handle: *mut GssCredIdT,
    actual_mechs: *mut *mut GssOidDesc,
    time_rec: *mut GssOmUint32,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures all pointers are valid
    unsafe {
        (lib.gss_acquire_cred)(
            minor_status,
            desired_name,
            time_req,
            desired_mechs,
            cred_usage,
            output_cred_handle,
            actual_mechs,
            time_rec,
        )
    }
}

/// Release credentials.
///
/// # Safety
///
/// Credentials must have been acquired by GSSAPI.
pub unsafe fn gss_release_cred(
    minor_status: *mut GssOmUint32,
    cred_handle: *mut GssCredIdT,
) -> GssOmUint32 {
    let lib = get_gssapi_lib().expect("GSSAPI library not loaded");
    // SAFETY: Caller ensures credentials are valid
    unsafe { (lib.gss_release_cred)(minor_status, cred_handle) }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Extracts a human-readable error message from GSSAPI status codes.
///
/// Returns a generic message if GSSAPI is not available.
pub fn get_gssapi_error(major: GssOmUint32, minor: GssOmUint32) -> String {
    // If GSSAPI isn't loaded, return a basic message
    if !is_gssapi_available() {
        return format!(
            "GSSAPI error (major=0x{:08X}, minor=0x{:08X}) - library not loaded",
            major, minor
        );
    }

    let mut messages = Vec::new();

    // Get major status message
    if let Some(msg) = get_status_message(major, GSS_C_GSS_CODE) {
        messages.push(format!("Major: {}", msg));
    }

    // Get minor status message (mechanism-specific)
    if let Some(msg) = get_status_message(minor, GSS_C_MECH_CODE).filter(|_| minor != 0) {
        messages.push(format!("Minor: {}", msg));
    }

    if messages.is_empty() {
        format!(
            "GSSAPI error (major=0x{:08X}, minor=0x{:08X})",
            major, minor
        )
    } else {
        messages.join("; ")
    }
}

/// Gets a single status message from GSSAPI.
fn get_status_message(status: GssOmUint32, status_type: i32) -> Option<String> {
    let mut minor_status: GssOmUint32 = 0;
    let mut message_context: GssOmUint32 = 0;
    let mut status_string = GssBufferDesc::default();
    let mut messages = Vec::new();

    loop {
        let major = unsafe {
            gss_display_status(
                &mut minor_status,
                status,
                status_type,
                GSS_C_NO_OID,
                &mut message_context,
                &mut status_string,
            )
        };

        if major != GSS_S_COMPLETE {
            break;
        }

        if status_string.length > 0 && !status_string.value.is_null() {
            let msg = unsafe {
                let slice = std::slice::from_raw_parts(
                    status_string.value as *const u8,
                    status_string.length,
                );
                String::from_utf8_lossy(slice).to_string()
            };
            messages.push(msg);

            // Release the buffer
            unsafe {
                gss_release_buffer(&mut minor_status, &mut status_string);
            }
        }

        // If message_context is 0, we've retrieved all messages
        if message_context == 0 {
            break;
        }
    }

    if messages.is_empty() {
        None
    } else {
        Some(messages.join(" "))
    }
}

/// Checks if a valid Kerberos ticket is available.
///
/// Attempts to acquire default credentials to verify a TGT exists.
/// Returns false if GSSAPI is not available or no credentials exist.
pub fn has_valid_credentials() -> bool {
    if !is_gssapi_available() {
        return false;
    }

    let mut minor_status: GssOmUint32 = 0;
    let mut cred_handle: GssCredIdT = ptr::null_mut();
    let mut time_rec: GssOmUint32 = 0;

    // Try to acquire default credentials (initiator)
    let major = unsafe {
        gss_acquire_cred(
            &mut minor_status,
            ptr::null_mut(), // GSS_C_NO_NAME - use default identity
            0,               // GSS_C_INDEFINITE
            ptr::null_mut(), // GSS_C_NO_OID_SET - default mechanisms
            1,               // GSS_C_INITIATE
            &mut cred_handle,
            ptr::null_mut(),
            &mut time_rec,
        )
    };

    // Clean up if we got a credential
    if !cred_handle.is_null() {
        unsafe {
            gss_release_cred(&mut minor_status, &mut cred_handle);
        }
    }

    major == GSS_S_COMPLETE
}

/// Returns the display name of the default initiator credential's principal
/// (e.g. `user@REALM`), i.e. whoever `kinit` last authenticated.
///
/// Mirrors msodbcsql's `gss_acquire_cred` → `gss_inquire_cred` →
/// `gss_display_name` sequence (`AzureADAuth.cpp` `CheckFederated`), which
/// derives the Entra user realm lookup from the Kerberos identity.
pub fn default_principal_name() -> Result<String, crate::security::SecurityError> {
    use crate::security::SecurityError;

    let lib = get_gssapi_lib().ok_or_else(|| {
        SecurityError::LoadLibraryFailed("libgssapi_krb5: GSSAPI library not available".to_string())
    })?;
    let (Some(inquire_cred), Some(display_name)) = (lib.gss_inquire_cred, lib.gss_display_name)
    else {
        return Err(SecurityError::LoadLibraryFailed(
            "libgssapi_krb5 does not export gss_inquire_cred/gss_display_name".to_string(),
        ));
    };

    let failure = |what: &str, major: GssOmUint32, minor: GssOmUint32| SecurityError::GssapiError {
        major,
        minor,
        message: format!("{what}: {}", get_gssapi_error(major, minor)),
    };

    let mut minor: GssOmUint32 = 0;
    let mut release_minor: GssOmUint32 = 0;
    let mut cred: GssCredIdT = ptr::null_mut();
    let major = unsafe {
        (lib.gss_acquire_cred)(
            &mut minor,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            1, // GSS_C_INITIATE
            &mut cred,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if major != GSS_S_COMPLETE {
        return Err(failure(
            "Error acquiring Kerberos credentials",
            major,
            minor,
        ));
    }

    let mut name: GssNameT = ptr::null_mut();
    let major = unsafe {
        inquire_cred(
            &mut minor,
            cred,
            &mut name,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    unsafe { (lib.gss_release_cred)(&mut release_minor, &mut cred) };
    if major != GSS_S_COMPLETE {
        return Err(failure(
            "Error retrieving current credential name",
            major,
            minor,
        ));
    }

    let mut buffer = GssBufferDesc::default();
    let major = unsafe { display_name(&mut minor, name, &mut buffer, ptr::null_mut()) };
    unsafe { (lib.gss_release_name)(&mut release_minor, &mut name) };
    if major != GSS_S_COMPLETE {
        return Err(failure("Error retrieving principal name", major, minor));
    }

    // Some implementations count the C terminator in `length`.
    let principal = String::from_utf8_lossy(&unsafe { buffer.to_vec() })
        .trim_end_matches('\0')
        .to_string();
    unsafe { (lib.gss_release_buffer)(&mut release_minor, &mut buffer) };
    Ok(principal)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gss_buffer_desc_default() {
        let buf = GssBufferDesc::default();
        assert_eq!(buf.length, 0);
        assert!(buf.value.is_null());
    }

    #[test]
    fn test_gss_buffer_desc_from_str() {
        let spn = "MSSQLSvc/server:1433";
        let buf = GssBufferDesc::from_str(spn);
        assert_eq!(buf.length, spn.len());
        assert!(!buf.value.is_null());
    }

    #[test]
    fn test_gss_buffer_desc_from_slice() {
        let data = [0xDE, 0xAD, 0xBE, 0xEF];
        let buf = GssBufferDesc::from_slice(&data);
        assert_eq!(buf.length, 4);
        assert!(!buf.value.is_null());
    }

    #[test]
    fn test_status_codes() {
        // Verify our constants match expected values
        assert_eq!(GSS_S_COMPLETE, 0);
        assert_eq!(GSS_S_CONTINUE_NEEDED, 1);
        assert_eq!(GSS_C_MUTUAL_FLAG, 2);
        assert_eq!(GSS_C_DELEG_FLAG, 1);
    }

    #[test]
    fn test_is_gssapi_available() {
        // This should not panic, regardless of whether krb5 is installed
        let available = is_gssapi_available();
        println!("GSSAPI available via dlopen: {}", available);
        // We don't assert the value - it depends on the system configuration
    }

    #[test]
    fn test_has_valid_credentials() {
        // This should not panic, regardless of whether krb5 is installed
        // or whether the user has a valid ticket
        let has_creds = has_valid_credentials();
        println!("Has valid Kerberos credentials: {}", has_creds);
        // We don't assert - just verify it doesn't crash
    }

    #[test]
    fn test_get_gss_nt_service_name_when_available() {
        if is_gssapi_available() {
            let oid = get_gss_nt_service_name();
            assert!(
                !oid.is_null(),
                "OID should not be null when GSSAPI is available"
            );
            let oid_desc = unsafe { &*oid };
            assert!(oid_desc.length > 0, "OID length should be > 0");
            println!("GSS_C_NT_HOSTBASED_SERVICE OID length: {}", oid_desc.length);
        } else {
            println!("Skipping OID test - GSSAPI not available (krb5 not installed)");
        }
    }

    #[test]
    fn test_static_oid_fallback() {
        // Test that our static fallback OID is valid
        let oid = &GSS_C_NT_HOSTBASED_SERVICE_DESC.0 as *const GssOidDesc;
        let oid_desc = unsafe { &*oid };
        assert_eq!(oid_desc.length, 10, "Static OID should have length 10");
        assert!(
            !oid_desc.elements.is_null(),
            "Static OID elements should not be null"
        );
    }
}

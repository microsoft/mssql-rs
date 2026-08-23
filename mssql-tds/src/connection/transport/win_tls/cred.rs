// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process-wide cached Schannel client credentials.
//!
//! ODBC keeps three process-static `CredHandle`s — `s_hClientCred`,
//! `s_hClientCredValidate`, and `s_hClientCredManualValidate` — and reuses
//! them across every handshake in the process
//! (`SNI_SslProvider.cpp:2185/2192/2199`). All three are acquired with
//! identical `SCH_CREDENTIALS.dwFlags`
//! (`SCH_CRED_NO_DEFAULT_CREDS | SCH_CRED_AUTO_CRED_VALIDATION`) and the
//! validation behaviour is switched at ISC time via
//! `ISC_REQ_MANUAL_CRED_VALIDATION`. We follow the same per-call ISC bit
//! mechanism (see [`CredKind::manual_validation_isc_bit`]) but additionally
//! differentiate `SCH_CREDENTIALS.dwFlags` per kind so that:
//!
//! - [`CredKind::NoValidate`] sets `SCH_CRED_NO_SERVERNAME_CHECK`. Without
//!   it Schannel's TLS 1.3 path raises `SEC_E_WRONG_PRINCIPAL` against a
//!   self-signed server cert whose CN does not match the connect string —
//!   even when the caller passed `ISC_REQ_MANUAL_CRED_VALIDATION`. ODBC
//!   doesn't trip on this because the SNI hot path predates the TLS 1.3
//!   internal hostname check.
//! - [`CredKind::AutoValidate`] sets `SCH_CRED_AUTO_CRED_VALIDATION` so
//!   Schannel performs full chain build + policy check inline (identical
//!   semantics to ODBC's `s_hClientCredAutoValidate`).
//!
//! We also set `SCH_CRED_NO_DEFAULT_CREDS` (never attach the user's
//! personal store as a client-auth cert) and `SCH_USE_STRONG_CRYPTO`
//! (reject legacy ciphers / protocols) on all kinds. ODBC sets only
//! `SCH_CRED_NO_DEFAULT_CREDS`; `SCH_USE_STRONG_CRYPTO` is a deliberate
//! hardening divergence — `mssql-rs` does not need to interop with
//! pre-TLS-1.0 servers, so the legacy-cipher allowance is unnecessary.
//!
//! Why three credential handles when the flag differences are minor?
//! Because SSPI partitions its internal TLS session cache by `CredHandle`.
//! Mixing handshakes with different post-handshake validation semantics on
//! the same cred would pollute the cache. ODBC sorts traffic into three
//! buckets at ISC time via the `ISC_REQ_MANUAL_CRED_VALIDATION` flag and a
//! few SSL extra-param bits.
//!
//! The validation-mode-vs-cred routing rule (mirrors
//! `SNI_SslProvider.cpp:1818-1821`):
//!
//! | App config                                  | [`CredKind`]                    |
//! |---------------------------------------------|---------------------------------|
//! | `TrustServerCertificate=Yes`, LoginOnly     | [`CredKind::NoValidate`]        |
//! | `ServerCertificate=<path>` (cert pinning)   | [`CredKind::ManualValidate`]    |
//! | `Encrypt=Strict` or full chain validation   | [`CredKind::AutoValidate`]      |

use std::io;
use std::ptr;
use std::sync::{Arc, OnceLock};

use tracing::debug;
use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Authentication::Identity;
use windows_sys::Win32::Security::Credentials;

use super::errors::sec_status_to_io_error;

/// Owned `CredHandle`. Drops by calling `FreeCredentialsHandle`.
pub(crate) struct CredHandle(Credentials::SecHandle);

// SAFETY: `SecHandle` is two pointer-sized integers. SChannel allows
// `InitializeSecurityContextW` to be called from multiple threads against
// the same credential handle (it's reference-counted internally), and ODBC
// relies on exactly that — see `s_hClientCred` shared across SNI worker
// threads.
unsafe impl Send for CredHandle {}
unsafe impl Sync for CredHandle {}

impl Drop for CredHandle {
    fn drop(&mut self) {
        // SAFETY: handle was acquired via AcquireCredentialsHandleW and not
        // freed elsewhere; matched pair.
        unsafe {
            Identity::FreeCredentialsHandle(&self.0);
        }
    }
}

impl CredHandle {
    pub(crate) fn raw(&self) -> &Credentials::SecHandle {
        &self.0
    }
}

/// Which of the three ODBC-equivalent credential buckets a handshake wants.
///
/// All three buckets are acquired with identical `SCH_CREDENTIALS.dwFlags`
/// (see module docs); the bucket discriminates the SSPI session-cache
/// partition and tells the handshake driver which `ISC_REQ_*` bits to add.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[allow(clippy::enum_variant_names)] // mirrors ODBC's s_hClientCred / Validate / ManualValidate naming
pub(crate) enum CredKind {
    /// Skip post-handshake validation entirely. `TrustServerCertificate=Yes`
    /// and `LoginOnly` route here.
    ///
    /// Handshake passes `ISC_REQ_MANUAL_CRED_VALIDATION` so SChannel does
    /// not call `CertGetCertificateChain` (the source of Adam's 15s stall).
    /// The driver then performs no chain build / hostname check afterward.
    NoValidate,
    /// Manual post-handshake validation: cert pinning mode
    /// (`ServerCertificate=<path>`). The driver compares the server's DER
    /// against the pinned bytes after `SEC_E_OK`.
    ///
    /// Handshake passes `ISC_REQ_MANUAL_CRED_VALIDATION` for the same
    /// chain-skip reason as `NoValidate`; the actual validation is in
    /// `validate.rs`.
    ManualValidate,
    /// Full automatic chain validation by SChannel. Used for `Encrypt=Strict`
    /// and `Encrypt=Mandatory` without `TrustServerCertificate=Yes`.
    ///
    /// Handshake does NOT pass `ISC_REQ_MANUAL_CRED_VALIDATION`, so SChannel
    /// builds the chain and verifies the SSL policy inline. The driver may
    /// still perform additional checks (e.g. host-name match) afterward.
    AutoValidate,
}

impl CredKind {
    /// Bit to OR into the per-call `fContextReq` for handshakes using this
    /// kind. Returns 0 for [`CredKind::AutoValidate`] (i.e., let SChannel
    /// auto-validate).
    pub(crate) fn manual_validation_isc_bit(self) -> u32 {
        match self {
            CredKind::NoValidate | CredKind::ManualValidate => {
                Identity::ISC_REQ_MANUAL_CRED_VALIDATION
            }
            CredKind::AutoValidate => 0,
        }
    }
}

struct CredCache {
    no_validate: OnceLock<Arc<CredHandle>>,
    manual_validate: OnceLock<Arc<CredHandle>>,
    auto_validate: OnceLock<Arc<CredHandle>>,
}

static CACHE: CredCache = CredCache {
    no_validate: OnceLock::new(),
    manual_validate: OnceLock::new(),
    auto_validate: OnceLock::new(),
};

/// Return the process-wide cached credential for `kind`, acquiring it the
/// first time it's requested. Subsequent calls hand back the same `Arc`.
pub(crate) fn get_or_acquire(kind: CredKind) -> io::Result<Arc<CredHandle>> {
    let slot = match kind {
        CredKind::NoValidate => &CACHE.no_validate,
        CredKind::ManualValidate => &CACHE.manual_validate,
        CredKind::AutoValidate => &CACHE.auto_validate,
    };
    if let Some(existing) = slot.get() {
        debug!(kind = ?kind, "win_tls: credential cache HIT");
        return Ok(existing.clone());
    }
    debug!(kind = ?kind, "win_tls: credential cache MISS, acquiring");
    let acquired = Arc::new(acquire_client_cred(kind)?);
    // Race-safe: if another thread won, prefer its Arc (ours Drop-frees).
    Ok(slot.get_or_init(move || acquired).clone())
}

/// Compute the `SCH_CREDENTIALS.dwFlags` for a credential bucket.
///
/// NOTE — divergence from ODBC (msodbcsql SNI_SslProvider.cpp).
///
/// ODBC's `AcquireCredentialsForClient` builds every credential handle
/// (s_hClientCred, s_hClientCredValidate, s_hClientCredManualValidate)
/// with the SAME dwFlags — `SCH_CRED_NO_DEFAULT_CREDS |
/// SCH_CRED_AUTO_CRED_VALIDATION` (lines 213, 218–219) — and never sets
/// SCH_USE_STRONG_CRYPTO or SCH_CRED_NO_SERVERNAME_CHECK. The three
/// statics exist purely to partition Schannel's per-cred session cache
/// across validation policies; the actual policy is chosen at
/// InitializeSecurityContext time via the per-call
/// ISC_REQ_MANUAL_CRED_VALIDATION bit (lines 1818–1821). Manual cases
/// then run a post-handshake CertGetCertificateChain +
/// CertVerifyCertificateChainPolicy with policy-specific overrides.
///
/// We diverge on two axes:
///
/// 1. We add SCH_USE_STRONG_CRYPTO unconditionally. ODBC doesn't,
///    deferring to the system SCHANNEL\Protocols / Ciphers registry
///    keys. We prefer an explicit minimum to avoid weak suites if a
///    machine has loosened defaults. (See
///    <https://learn.microsoft.com/en-us/windows/win32/api/schannel/ns-schannel-sch_credentials>
///    — "Restrict the connection to use only cipher suites that comply
///    with [SP800-131A] requirements.")
///
/// 2. For the two "skip validation" buckets we additionally set
///    SCH_CRED_NO_SERVERNAME_CHECK at credential-creation time:
///      * NoValidate     -> + SCH_CRED_NO_SERVERNAME_CHECK
///      * ManualValidate -> + SCH_CRED_NO_SERVERNAME_CHECK
///      * AutoValidate   -> + SCH_CRED_AUTO_CRED_VALIDATION (same as ODBC)
///
///    Microsoft's documented behaviour for ISC_REQ_MANUAL_CRED_VALIDATION
///    only covers *chain trust* suppression
///    (<https://learn.microsoft.com/en-us/windows/win32/secauthn/manually-validating-schannel-credentials>);
///    server-name checking has always been gated by
///    SCH_CRED_NO_SERVERNAME_CHECK on the credential. Empirically, when
///    Schannel negotiates TLS 1.3 against a self-signed or pinned cert
///    whose subject doesn't match the connect-string host, omitting
///    SCH_CRED_NO_SERVERNAME_CHECK produces SEC_E_WRONG_PRINCIPAL even
///    with the per-call MANUAL_CRED bit set. ODBC avoids this by
///    *always* enabling Schannel's name+chain check and overriding the
///    result post-handshake; we instead suppress at the credential
///    level. Both approaches are valid; ours keeps the per-call ISC
///    flags minimal at the cost of a richer cache key (CredKind,
///    server_name).
fn cred_flags_for(kind: CredKind) -> u32 {
    // Common across all kinds:
    //   SCH_USE_STRONG_CRYPTO: rejects weak ciphers + protocols.
    //   SCH_CRED_NO_DEFAULT_CREDS: never silently attach the user's
    //     personal store as a client-auth cert. SQL Server clients
    //     don't use mTLS by default.
    let base = Identity::SCH_USE_STRONG_CRYPTO | Identity::SCH_CRED_NO_DEFAULT_CREDS;
    match kind {
        // TrustServerCertificate=Yes / LoginOnly: skip BOTH chain
        // validation AND server-name comparison. The chain-skip is
        // additionally enforced per-call via ISC_REQ_MANUAL_CRED_VALIDATION
        // (matching ODBC's SNI_SslProvider.cpp:55-65), so this dwFlag set
        // only needs SCH_CRED_NO_SERVERNAME_CHECK — without it Schannel's
        // TLS 1.3 path raises SEC_E_WRONG_PRINCIPAL even when the caller
        // asked for manual validation. ODBC doesn't hit that because its
        // hot path predates TLS 1.3 hostname enforcement.
        CredKind::NoValidate => base | Identity::SCH_CRED_NO_SERVERNAME_CHECK,
        // ServerCertificate=<path>: chain validation is skipped at the
        // per-call ISC bit; we run a file-pin DER compare ourselves
        // post-handshake. Skip the server-name check too — a pinned cert
        // routinely has a CN that doesn't match the connect-string host
        // (that's the entire reason for pinning), and Schannel's TLS 1.3
        // internal name comparison would otherwise reject it.
        CredKind::ManualValidate => base | Identity::SCH_CRED_NO_SERVERNAME_CHECK,
        // Encrypt=Strict / default: let Schannel validate everything
        // inline, exactly like ODBC's s_hClientCredAutoValidate.
        CredKind::AutoValidate => base | Identity::SCH_CRED_AUTO_CRED_VALIDATION,
    }
}

/// Acquire one outbound Schannel client credential, with flags chosen
/// to match the validation posture implied by `kind`.
///
/// The three CredKinds historically used identical `dwFlags` and relied
/// on the per-call `ISC_REQ_MANUAL_CRED_VALIDATION` bit to switch
/// validation off; that bit suppresses chain build, but Schannel's
/// TLS 1.3 path still performs an internal server-name comparison that
/// can fail with `SEC_E_WRONG_PRINCIPAL` even when the caller does not
/// care (e.g. `TrustServerCertificate=Yes` against a self-signed cert
/// whose CN does not match the connect string). Differentiating the
/// dwFlags per kind avoids that.
fn acquire_client_cred(kind: CredKind) -> io::Result<CredHandle> {
    let cred_flags = cred_flags_for(kind);

    // SAFETY: all pointers below are either valid Rust references or the
    // documented null values. The returned `handle` is initialised by
    // AcquireCredentialsHandleW on SEC_E_OK.
    unsafe {
        // Use the SCH_CREDENTIALS path (Windows 10 1809+ / Server 2019+)
        // so TLS 1.3 is negotiable. We intentionally do NOT fall back to
        // the legacy SCHANNEL_CRED (v4) struct on older OSes — Windows
        // Server 2016 is therefore unsupported. AcquireCredentialsHandleW
        // will fail with SEC_E_UNKNOWN_CREDENTIALS on those builds and the
        // connection will be rejected at handshake time.
        let mut cred_data: Identity::SCH_CREDENTIALS = std::mem::zeroed();
        cred_data.dwVersion = Identity::SCH_CREDENTIALS_VERSION;
        cred_data.dwFlags = cred_flags;

        let mut handle: Credentials::SecHandle = std::mem::zeroed();
        let status = Identity::AcquireCredentialsHandleW(
            ptr::null(),
            Identity::UNISP_NAME_W,
            Identity::SECPKG_CRED_OUTBOUND,
            ptr::null_mut(),
            &cred_data as *const _ as *const _,
            None,
            ptr::null_mut(),
            &mut handle,
            ptr::null_mut(),
        );
        if status == Foundation::SEC_E_OK {
            debug!(
                kind = ?kind,
                flags = format!("0x{:08x}", cred_flags),
                "win_tls: AcquireCredentialsHandle OK"
            );
            Ok(CredHandle(handle))
        } else {
            debug!(
                kind = ?kind,
                flags = format!("0x{:08x}", cred_flags),
                status = format!("0x{:08x}", status as u32),
                "win_tls: AcquireCredentialsHandle FAILED"
            );
            Err(sec_status_to_io_error(
                status,
                "AcquireCredentialsHandleW(UNISP_NAME) failed",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_each_kind_succeeds() {
        for kind in [
            CredKind::NoValidate,
            CredKind::ManualValidate,
            CredKind::AutoValidate,
        ] {
            let h = get_or_acquire(kind).unwrap_or_else(|e| panic!("acquire {kind:?} failed: {e}"));
            assert!(h.raw().dwLower != 0 || h.raw().dwUpper != 0);
        }
    }

    #[test]
    fn same_kind_returns_same_handle() {
        let a = get_or_acquire(CredKind::NoValidate).unwrap();
        let b = get_or_acquire(CredKind::NoValidate).unwrap();
        assert_eq!(a.raw().dwLower, b.raw().dwLower);
        assert_eq!(a.raw().dwUpper, b.raw().dwUpper);
    }

    #[test]
    fn different_kinds_return_different_handles() {
        let no = get_or_acquire(CredKind::NoValidate).unwrap();
        let auto = get_or_acquire(CredKind::AutoValidate).unwrap();
        // Two distinct AcquireCredentialsHandle calls → different SecHandle
        // bit patterns. (If Windows ever returns the same handle for both
        // we still partition Arcs, but in practice SChannel returns unique
        // handles per call.)
        assert!(
            no.raw().dwLower != auto.raw().dwLower || no.raw().dwUpper != auto.raw().dwUpper,
            "expected distinct SecHandles for different CredKinds"
        );
    }

    #[test]
    fn manual_validation_isc_bit_matches_kind() {
        assert_ne!(CredKind::NoValidate.manual_validation_isc_bit(), 0);
        assert_ne!(CredKind::ManualValidate.manual_validation_isc_bit(), 0);
        assert_eq!(CredKind::AutoValidate.manual_validation_isc_bit(), 0);
    }

    #[test]
    fn cred_flags_for_each_kind() {
        let base = Identity::SCH_USE_STRONG_CRYPTO | Identity::SCH_CRED_NO_DEFAULT_CREDS;

        let no = cred_flags_for(CredKind::NoValidate);
        assert_eq!(no, base | Identity::SCH_CRED_NO_SERVERNAME_CHECK);

        let manual = cred_flags_for(CredKind::ManualValidate);
        assert_eq!(manual, base | Identity::SCH_CRED_NO_SERVERNAME_CHECK);

        let auto = cred_flags_for(CredKind::AutoValidate);
        assert_eq!(auto, base | Identity::SCH_CRED_AUTO_CRED_VALIDATION);

        // AutoValidate must NOT suppress the server-name check.
        assert_eq!(auto & Identity::SCH_CRED_NO_SERVERNAME_CHECK, 0);
        // Every kind sets the strong-crypto + no-default-creds base.
        for f in [no, manual, auto] {
            assert_eq!(f & base, base);
        }
    }

    #[test]
    fn drop_frees_zeroed_handle_without_panicking() {
        // FreeCredentialsHandle on a zeroed handle returns SEC_E_INVALID_HANDLE
        // rather than dereferencing memory, so dropping is safe. Exercises the
        // Drop impl without acquiring a real credential.
        let h = CredHandle(unsafe { std::mem::zeroed() });
        drop(h);
    }
}

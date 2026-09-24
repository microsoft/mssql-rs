// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLS engine abstraction.
//!
//! The [`TlsEngine`] trait separates *which* TLS implementation runs the
//! handshake from the surrounding TDS plumbing (prelogin framing,
//! connection-state callbacks, certificate-pinning hooks). Windows uses the
//! Schannel-direct engine by default; other configurations use native-tls.

#[cfg(any(test, not(all(windows, feature = "tls-schannel-direct"))))]
pub(crate) mod native_tls_engine;

use crate::connection::transport::network_transport::Stream;
use crate::core::{CertificateSource, TdsResult};

/// Per-connection TLS validation configuration resolved from the user's
/// encryption options and the negotiated encryption setting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TlsValidationConfig {
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub use_alpn: bool,
    /// Connection-scoped trust anchors; `None` uses only the platform roots.
    pub custom_roots: Option<CustomRoots>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CustomRoots {
    pub source: CertificateSource,
    pub include_platform_roots: bool,
}

/// Inputs passed to [`TlsEngine::connect`] for a single handshake.
pub(crate) struct TlsConnectParams<'a> {
    pub validation: &'a TlsValidationConfig,
    /// Host name to validate the server certificate against. May differ
    /// from `server_host_name` if `HostnameInCertificate` is set.
    pub host_name: &'a str,
    /// Host actually being connected to. Used for log messages and the
    /// error returned on handshake failure.
    pub server_host_name: &'a str,
    /// Pinned server certificate. When `Some`, the engine MUST validate the
    /// peer certificate's DER against it and return `Err` on mismatch.
    pub pinned_certificate: Option<&'a CertificateSource>,
}

/// Abstraction over a TLS handshake implementation.
///
/// Implementors own the platform-specific TLS library (native-tls,
/// schannel, etc.), perform the handshake against `base_stream`, apply
/// any post-handshake validation (cert pinning), and return a wrapped
/// `Box<dyn Stream>` that transports encrypted application data.
///
/// Implementations are expected to call `tls_handshake_starting()` on
/// the base stream before the handshake and `tls_handshake_completed()`
/// on the resulting wrapped stream after the handshake succeeds.
#[async_trait::async_trait]
pub(crate) trait TlsEngine: Send + Sync {
    async fn connect(
        &self,
        base_stream: Box<dyn Stream>,
        params: TlsConnectParams<'_>,
    ) -> TdsResult<Box<dyn Stream>>;
}

/// Returns the default TLS engine for this platform.
///
/// On Windows, returns the in-tree Schannel-direct engine when the
/// `tls-schannel-direct` feature is enabled (default), including connections
/// with custom CA roots. On other platforms or when the feature is disabled,
/// returns the `native-tls`-backed engine.
///
/// The Schannel-direct engine fixes two Windows-only TLS bugs that
/// produced the bulkcopy timeout regression observed in production:
/// (1) chain-build / CTL auto-update being triggered even with
/// `TrustServerCertificate=Yes`, and (2) the `MidHandshakeTlsStream`
/// waker-park race in `tokio-native-tls`. See
/// `.github/prompts/plan-decoupleTlsBackendSchannelOdbcParity.prompt.md`
/// for the full analysis.
pub(crate) fn default_engine(_validation: &TlsValidationConfig) -> &'static dyn TlsEngine {
    #[cfg(all(windows, feature = "tls-schannel-direct"))]
    {
        &crate::connection::transport::win_tls::engine::SCHANNEL_ENGINE
    }
    #[cfg(not(all(windows, feature = "tls-schannel-direct")))]
    {
        &native_tls_engine::NATIVE_TLS_ENGINE
    }
}

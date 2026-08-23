// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLS engine abstraction.
//!
//! The [`TlsEngine`] trait separates *which* TLS implementation runs the
//! handshake from the surrounding TDS plumbing (prelogin framing,
//! connection-state callbacks, certificate-pinning hooks). Today there is
//! exactly one implementation: [`native_tls_engine::NativeTlsEngine`].
//! A subsequent PR in this stack will add a Windows-only Schannel-direct
//! engine and route to it on `cfg(windows)` from [`default_engine`].
//!
//! This module is purely a refactor — no behavior changes.

pub(crate) mod native_tls_engine;

use crate::connection::transport::network_transport::Stream;
use crate::core::TdsResult;

/// Per-connection TLS validation configuration resolved from the user's
/// encryption options and the negotiated encryption setting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TlsValidationConfig {
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub use_alpn: bool,
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
    /// Optional path to a server certificate file for pinning mode. When
    /// `Some`, the engine MUST validate the peer certificate's DER against
    /// the file and return `Err` on mismatch.
    pub server_certificate_path: Option<&'a std::path::PathBuf>,
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
/// On Windows, returns the in-tree Schannel-direct engine
/// when the `tls-schannel-direct` feature is enabled (default). On all
/// other platforms and when the feature is disabled, returns the
/// `native-tls`-backed engine.
///
/// The Schannel-direct engine fixes two Windows-only TLS bugs that
/// produced the bulkcopy timeout regression observed in production:
/// (1) chain-build / CTL auto-update being triggered even with
/// `TrustServerCertificate=Yes`, and (2) the `MidHandshakeTlsStream`
/// waker-park race in `tokio-native-tls`. See
/// `.github/prompts/plan-decoupleTlsBackendSchannelOdbcParity.prompt.md`
/// for the full analysis.
pub(crate) fn default_engine() -> &'static dyn TlsEngine {
    #[cfg(all(windows, feature = "tls-schannel-direct"))]
    {
        return &crate::connection::transport::win_tls::engine::SCHANNEL_ENGINE;
    }
    #[allow(unreachable_code)]
    {
        &native_tls_engine::NATIVE_TLS_ENGINE
    }
}

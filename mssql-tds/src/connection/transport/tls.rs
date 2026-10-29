// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLS engine abstraction.
//!
//! The [`TlsEngine`] trait separates *which* TLS implementation runs the
//! handshake from the surrounding TDS plumbing (prelogin framing,
//! connection-state callbacks, certificate-pinning hooks). There are three
//! implementations: [`native_tls_engine::NativeTlsEngine`], the Schannel-direct
//! engine in `win_tls` on Windows, and the Network.framework engine on macOS.
//! [`default_engine`] picks Schannel-direct on Windows and Network.framework on
//! macOS when the `tls-schannel-direct` / `tls-network-framework` feature is
//! enabled (the default), and native-tls everywhere else. The macOS engine only
//! takes over direct TDS 8.0 handshakes and hands the rest to native-tls.

pub(crate) mod native_tls_engine;
#[cfg(all(target_os = "macos", feature = "tls-network-framework"))]
pub(crate) mod network_framework_engine;

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
/// when the `tls-schannel-direct` feature is enabled (default). On macOS,
/// returns the Network.framework engine when the `tls-network-framework`
/// feature is enabled (default). It runs direct TDS 8.0 handshakes, the only
/// ones SQL Server negotiates TLS 1.3 on, and defers the PreLogin-tunneled
/// handshakes to `native-tls`, because Secure Transport cannot negotiate TLS 1.3
/// or post-quantum key exchange. On all other platforms and when the feature is
/// disabled, returns the `native-tls`-backed engine.
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
    #[cfg(all(target_os = "macos", feature = "tls-network-framework"))]
    {
        return &network_framework_engine::NETWORK_FRAMEWORK_ENGINE;
    }
    #[allow(unreachable_code)]
    {
        &native_tls_engine::NATIVE_TLS_ENGINE
    }
}

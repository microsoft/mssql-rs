// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::error::Error;
use crate::error::Error::OperationCancelledError;
use futures::FutureExt;
use futures::future::Either;
use std::future::Future;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Alias for `Result<T, crate::error::Error>` used throughout the crate.
pub type TdsResult<T> = Result<T, Error>;

/// ALPN protocol identifier for TDS 8.0 connections.
pub const TDS_8_ALPN_PROTOCOL: &str = "tds/8.0";

/// Cooperative cancellation handle backed by a [`CancellationToken`].
///
/// Pass to [`TdsConnectionProvider::create_client()`](crate::connection_provider::tds_connection_provider::TdsConnectionProvider::create_client)
/// to cancel a pending connect, or hold for later query cancellation.
#[derive(Debug)]
pub struct CancelHandle {
    pub(crate) cancel_token: CancellationToken,
}

impl CancelHandle {
    /// Create a new, uncancelled handle.
    pub fn new() -> Self {
        CancelHandle {
            cancel_token: CancellationToken::new(),
        }
    }

    /// Trigger cancellation, notifying all child handles.
    pub fn cancel(self) {
        self.cancel_token.cancel();
    }

    /// Derive a child handle that is cancelled when this handle is.
    pub fn child_handle(&self) -> Self {
        Self::from(self.cancel_token.child_token())
    }

    pub(crate) fn run_until_cancelled<'a, F, ResultType>(
        cancel_handle: Option<&'a CancelHandle>,
        f: F,
    ) -> impl Future<Output = F::Output> + Send + 'a
    where
        F: Future<Output = TdsResult<ResultType>> + Send + 'a,
    {
        match cancel_handle {
            Some(handle) => Either::Left(handle.cancel_token.run_until_cancelled(f).map(
                |result| match result {
                    Some(result) => result,
                    None => Err(OperationCancelledError("Request was cancelled".to_string())),
                },
            )),
            None => Either::Right(f),
        }
    }
}

impl From<CancellationToken> for CancelHandle {
    fn from(value: CancellationToken) -> Self {
        CancelHandle {
            cancel_token: value,
        }
    }
}

impl Default for CancelHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// SQL Server major-version discriminant derived from the server's reported version.
#[derive(PartialEq, Debug)]
pub enum SQLServerVersion {
    /// Unsupported or unknown server version.
    SqlServerNotsupported = 0,
    /// SQL Server 2000.
    SqlServer2000 = 8,
    /// SQL Server 2005.
    SqlServer2005 = 9,
    /// SQL Server 2008 / 2008 R2.
    SqlServer2008 = 10,
    /// SQL Server 2012.
    SqlServer2012 = 11,
    /// SQL Server 2014.
    SqlServer2014 = 12,
    /// SQL Server 2016.
    SqlServer2016 = 13,
    /// SQL Server 2017.
    SqlServer2017 = 14,
    /// SQL Server 2019.
    SqlServer2019 = 15,
    /// SQL Server 2022.
    SqlServer2022 = 16,
    /// SQL Server 2022+ (version 17).
    SqlServer2022lus = 17,
}

impl From<u8> for SQLServerVersion {
    fn from(v: u8) -> Self {
        match v {
            0 => SQLServerVersion::SqlServerNotsupported,
            8 => SQLServerVersion::SqlServer2000,
            9 => SQLServerVersion::SqlServer2005,
            10 => SQLServerVersion::SqlServer2008,
            11 => SQLServerVersion::SqlServer2012,
            12 => SQLServerVersion::SqlServer2014,
            13 => SQLServerVersion::SqlServer2016,
            14 => SQLServerVersion::SqlServer2017,
            15 => SQLServerVersion::SqlServer2019,
            16 => SQLServerVersion::SqlServer2022,
            17 => SQLServerVersion::SqlServer2022lus,
            _ => SQLServerVersion::SqlServerNotsupported,
        }
    }
}

/// Four-part server version reported during the TDS pre-login handshake.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Version {
    /// Major version number.
    pub major: u8,
    /// Minor version number.
    pub minor: u8,
    /// Build number.
    pub build: u16,
    /// Revision number.
    pub revision: u16,
}

impl Version {
    /// Creates a new `Version`.
    pub fn new(major: u8, minor: u8, build: u16, revision: u16) -> Self {
        Version {
            major,
            minor,
            build,
            revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_server_version_from_known_values() {
        assert_eq!(
            SQLServerVersion::from(0),
            SQLServerVersion::SqlServerNotsupported
        );
        assert_eq!(SQLServerVersion::from(8), SQLServerVersion::SqlServer2000);
        assert_eq!(SQLServerVersion::from(9), SQLServerVersion::SqlServer2005);
        assert_eq!(SQLServerVersion::from(10), SQLServerVersion::SqlServer2008);
        assert_eq!(SQLServerVersion::from(11), SQLServerVersion::SqlServer2012);
        assert_eq!(SQLServerVersion::from(12), SQLServerVersion::SqlServer2014);
        assert_eq!(SQLServerVersion::from(13), SQLServerVersion::SqlServer2016);
        assert_eq!(SQLServerVersion::from(14), SQLServerVersion::SqlServer2017);
        assert_eq!(SQLServerVersion::from(15), SQLServerVersion::SqlServer2019);
        assert_eq!(SQLServerVersion::from(16), SQLServerVersion::SqlServer2022);
        assert_eq!(
            SQLServerVersion::from(17),
            SQLServerVersion::SqlServer2022lus
        );
    }

    #[test]
    fn sql_server_version_from_unknown_defaults_to_not_supported() {
        assert_eq!(
            SQLServerVersion::from(1),
            SQLServerVersion::SqlServerNotsupported
        );
        assert_eq!(
            SQLServerVersion::from(7),
            SQLServerVersion::SqlServerNotsupported
        );
        assert_eq!(
            SQLServerVersion::from(18),
            SQLServerVersion::SqlServerNotsupported
        );
        assert_eq!(
            SQLServerVersion::from(255),
            SQLServerVersion::SqlServerNotsupported
        );
    }

    #[test]
    fn cancel_handle_default() {
        let handle = CancelHandle::default();
        assert!(!handle.cancel_token.is_cancelled());
    }

    #[tokio::test]
    async fn run_until_cancelled_none_handle() {
        let result: TdsResult<i32> =
            CancelHandle::run_until_cancelled(None, async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn run_until_cancelled_with_handle_completes() {
        let handle = CancelHandle::new();
        let result: TdsResult<i32> =
            CancelHandle::run_until_cancelled(Some(&handle), async { Ok(99) }).await;
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn run_until_cancelled_with_cancelled_handle_stops_pending_future() {
        let handle = CancelHandle::new();
        handle.cancel_token.cancel();

        let result = CancelHandle::run_until_cancelled(
            Some(&handle),
            std::future::pending::<TdsResult<i32>>(),
        )
        .await;

        assert!(matches!(result, Err(OperationCancelledError(_))));
    }

    fn keywords(
        mode: EncryptionSetting,
        trust: bool,
        host_name: Option<&str>,
        certificate: Option<&str>,
    ) -> TdsResult<ServerTrust> {
        EncryptionOptions::from_connection_keywords(
            mode,
            trust,
            host_name.map(str::to_string),
            certificate.map(PathBuf::from),
        )
        .map(|options| options.server_trust)
    }

    #[test]
    fn connection_keywords_default_to_platform_verification() {
        assert_eq!(
            keywords(EncryptionSetting::On, false, None, None).unwrap(),
            ServerTrust::default()
        );
        assert_eq!(
            keywords(EncryptionSetting::On, false, Some(""), None).unwrap(),
            ServerTrust::default()
        );
        assert_eq!(
            keywords(EncryptionSetting::On, false, Some("cn.example"), None).unwrap(),
            ServerTrust::Verify {
                roots: TrustRoots::Platform,
                host_name: Some("cn.example".to_string()),
            }
        );
    }

    #[test]
    fn connection_keywords_trust_server_certificate_is_ignored_under_strict() {
        assert_eq!(
            keywords(EncryptionSetting::Required, true, None, None).unwrap(),
            ServerTrust::DangerAcceptAny
        );
        assert_eq!(
            keywords(EncryptionSetting::Strict, true, None, None).unwrap(),
            ServerTrust::default()
        );
    }

    #[test]
    fn connection_keywords_server_certificate_takes_precedence() {
        for mode in [EncryptionSetting::On, EncryptionSetting::Strict] {
            assert_eq!(
                keywords(mode, true, None, Some("server.cer")).unwrap(),
                ServerTrust::Pinned(CertificateSource::File("server.cer".into()))
            );
        }
    }

    #[test]
    fn connection_keywords_reject_pin_with_host_name() {
        assert!(matches!(
            keywords(
                EncryptionSetting::On,
                false,
                Some("cn.example"),
                Some("server.cer")
            ),
            Err(Error::UsageError(_))
        ));
    }
}

/// TLS and encryption settings for a TDS connection.
///
/// ```
/// use mssql_tds::core::{
///     CertificateSource, EncryptionOptions, EncryptionSetting, ServerTrust, TrustRoots,
/// };
///
/// let options = EncryptionOptions::new()
///     .with_mode(EncryptionSetting::Strict)
///     .with_server_trust(ServerTrust::verify(TrustRoots::PlatformAndCustom(
///         CertificateSource::File("/etc/ssl/private-ca.pem".into()),
///     )));
/// # let _ = options;
/// ```
#[derive(Clone, PartialEq, Debug)]
#[non_exhaustive]
pub struct EncryptionOptions {
    /// Encryption mode negotiated with the server.
    pub mode: EncryptionSetting,
    /// How the server's certificate is authenticated.
    pub server_trust: ServerTrust,
}

impl EncryptionOptions {
    /// Creates encryption options defaulting to `Strict` mode with platform
    /// certificate validation.
    pub fn new() -> Self {
        EncryptionOptions {
            mode: EncryptionSetting::Strict,
            server_trust: ServerTrust::default(),
        }
    }

    /// Sets the encryption mode.
    pub fn with_mode(mut self, mode: EncryptionSetting) -> Self {
        self.mode = mode;
        self
    }

    /// Sets how the server certificate is authenticated.
    pub fn with_server_trust(mut self, server_trust: ServerTrust) -> Self {
        self.server_trust = server_trust;
        self
    }

    /// Maps the ODBC-style connection keywords shared by the driver bindings
    /// onto encryption options, including msodbcsql's rules for how they
    /// interact: `ServerCertificate` wins over `TrustServerCertificate`, and
    /// `TrustServerCertificate` is ignored under `Strict` encryption.
    pub fn from_connection_keywords(
        mode: EncryptionSetting,
        trust_server_certificate: bool,
        host_name_in_certificate: Option<String>,
        server_certificate: Option<PathBuf>,
    ) -> TdsResult<Self> {
        let host_name = host_name_in_certificate.filter(|name| !name.is_empty());
        let server_trust = if let Some(path) = server_certificate {
            if host_name.is_some() {
                return Err(Error::UsageError(
                    "ServerCertificate and HostnameInCertificate are mutually exclusive. Use only one."
                        .to_string(),
                ));
            }
            if trust_server_certificate {
                tracing::warn!(
                    "Both ServerCertificate and TrustServerCertificate are specified. ServerCertificate takes precedence."
                );
            }
            ServerTrust::Pinned(CertificateSource::File(path))
        } else if trust_server_certificate && mode != EncryptionSetting::Strict {
            ServerTrust::DangerAcceptAny
        } else {
            if trust_server_certificate {
                tracing::warn!(
                    "TrustServerCertificate is ignored for Strict encryption mode. Certificate validation will be enforced."
                );
            }
            ServerTrust::Verify {
                roots: TrustRoots::Platform,
                host_name,
            }
        };
        Ok(EncryptionOptions { mode, server_trust })
    }
}

impl Default for EncryptionOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// How the server's TLS certificate is authenticated.
///
/// Each variant is a complete trust model, so contradictory combinations
/// (e.g. pinning a certificate while also skipping validation) cannot be
/// expressed.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ServerTrust {
    /// Validate the certificate chain, validity period and host name.
    Verify {
        /// Trust anchors the chain must terminate in.
        roots: TrustRoots,
        /// Host name expected in the certificate instead of the server name
        /// being connected to (`HostNameInCertificate`).
        host_name: Option<String>,
    },
    /// Accept only a certificate byte-identical to this one
    /// (`ServerCertificate`). Chain and host name are not checked; expiry is.
    Pinned(CertificateSource),
    /// Accept any certificate without validation (`TrustServerCertificate`).
    /// The connection is encrypted but not authenticated.
    DangerAcceptAny,
}

impl ServerTrust {
    /// Validation against `roots`, checking the server name being connected to.
    pub fn verify(roots: TrustRoots) -> Self {
        ServerTrust::Verify {
            roots,
            host_name: None,
        }
    }
}

impl Default for ServerTrust {
    fn default() -> Self {
        ServerTrust::verify(TrustRoots::Platform)
    }
}

/// Trust anchors used by [`ServerTrust::Verify`].
///
/// Custom certificates must be CA certificates that terminate the server's
/// chain; they are scoped to the connection and never installed system wide.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum TrustRoots {
    /// The operating system's trusted roots.
    Platform,
    /// Only these certificates; the platform roots are not trusted.
    Custom(CertificateSource),
    /// The platform roots plus these certificates.
    PlatformAndCustom(CertificateSource),
}

/// Where certificate material comes from.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum CertificateSource {
    /// A PEM (single certificate or bundle) or DER file, re-read on every
    /// connection so rotated files take effect without restarting.
    File(PathBuf),
    /// PEM-encoded certificate or bundle.
    Pem(Vec<u8>),
    /// A single DER-encoded certificate.
    Der(Vec<u8>),
}

/// Encryption level requested by the client during the TDS pre-login.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EncryptionSetting {
    /// Don't encrypt if the server allows it.
    PreferOff,
    /// Encrypt the connection after pre-login.
    On,
    /// Require encryption after pre-login (semantically identical to `On`).
    Required,
    /// Encrypt the entire stream including pre-login (TDS 8.0).
    Strict,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum NegotiatedEncryptionSetting {
    Strict,
    LoginOnly,
    Mandatory,
    NoEncryption,
}

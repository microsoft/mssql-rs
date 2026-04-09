// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::error::Error;
use crate::error::Error::OperationCancelledError;
use std::future::Future;
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

    pub(crate) async fn run_until_cancelled<F, ResultType>(
        cancel_handle: Option<&CancelHandle>,
        f: F,
    ) -> F::Output
    where
        F: Future<Output = TdsResult<ResultType>> + Send,
    {
        match cancel_handle {
            Some(handle) => match handle.cancel_token.run_until_cancelled(f).await {
                Some(result) => result,
                None => Err(OperationCancelledError("Request was cancelled".to_string())),
            },
            None => f.await,
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
#[derive(PartialEq, Debug)]
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

/// TLS and encryption settings for a TDS connection.
#[derive(Clone, PartialEq, Debug)]
pub struct EncryptionOptions {
    /// Encryption mode negotiated with the server.
    pub mode: EncryptionSetting,
    /// Skip server certificate chain validation.
    pub trust_server_certificate: bool,
    /// Expected CN or SAN in the server certificate.
    pub host_name_in_cert: Option<String>,
    /// Path to a DER or PEM encoded X.509 certificate file for certificate pinning.
    /// When specified, the driver performs an exact binary match between the provided
    /// certificate and the server's certificate, bypassing standard CA chain validation.
    pub server_certificate: Option<String>,
}

impl EncryptionOptions {
    /// Creates encryption options defaulting to `Strict` mode.
    pub fn new() -> Self {
        EncryptionOptions {
            mode: EncryptionSetting::Strict,
            trust_server_certificate: false,
            host_name_in_cert: None,
            server_certificate: None,
        }
    }
}

impl Default for EncryptionOptions {
    fn default() -> Self {
        Self::new()
    }
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

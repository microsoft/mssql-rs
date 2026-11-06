// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;

use mssql_tds::error::Error as TdsError;

pub(crate) type PoolResult<T> = Result<T, PoolError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcquireStage {
    Admission,
    Wait,
    Open,
    Reset,
    Isolation,
    Delivery,
}

#[derive(thiserror::Error)]
pub(crate) enum OpenError {
    #[error("TDS login failed")]
    Tds(#[source] Box<TdsError>),
    #[error("credential provider failed")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(thiserror::Error)]
pub(crate) enum PoolError {
    #[error("invalid pool configuration: {field}: {reason}")]
    InvalidConfig {
        field: &'static str,
        reason: &'static str,
    },
    #[error("invalid pool request: {field}: {reason}")]
    InvalidRequest {
        field: &'static str,
        reason: &'static str,
    },
    #[error("pool generation is closed")]
    Closed,
    #[error("pool acquisition timed out at {stage:?}")]
    AcquireTimeout { stage: AcquireStage },
    #[error("pool wait queue is full")]
    WaitQueueFull,
    #[error("pool acquisition was cancelled")]
    Cancelled,
    #[error("physical connection open failed: {0}")]
    Open(#[from] OpenError),
    #[error("authentication binding does not match")]
    IdentityMismatch,
    #[error("known token expiry does not permit reuse")]
    TokenExpired,
    #[error("connection cleanup failed")]
    Cleanup(#[source] Box<TdsError>),
    #[error("connection cleanup timed out")]
    CleanupTimeout,
    #[error("pool shutdown timed out")]
    ShutdownTimeout,
}

// Causes remain available to callers, but provider/server messages are not safe
// to include in routine Debug/Display diagnostics.
impl fmt::Debug for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Debug for PoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn diagnostics_redact_provider_and_tds_causes_but_keep_them_typed() {
        let provider: PoolError =
            OpenError::Provider(Box::new(std::io::Error::other("provider-secret"))).into();
        let open = provider
            .source()
            .unwrap()
            .downcast_ref::<OpenError>()
            .unwrap();
        assert_eq!(
            open.source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .to_string(),
            "provider-secret"
        );
        let tds: PoolError =
            OpenError::Tds(Box::new(TdsError::ConnectionError("tds-secret".into()))).into();
        let open = tds.source().unwrap().downcast_ref::<OpenError>().unwrap();
        assert!(matches!(
            open.source().unwrap().downcast_ref::<Box<TdsError>>(),
            Some(error) if matches!(error.as_ref(), TdsError::ConnectionError(message) if message == "tds-secret")
        ));
        let cleanup = PoolError::Cleanup(Box::new(TdsError::ConnectionError("tds-secret".into())));
        assert!(
            cleanup
                .source()
                .unwrap()
                .downcast_ref::<Box<TdsError>>()
                .is_some()
        );
        for error in [&provider, &tds, &cleanup] {
            for rendered in [format!("{error}"), format!("{error:?}")] {
                assert!(!rendered.contains("provider-secret"));
                assert!(!rendered.contains("tds-secret"));
            }
        }
        for error in [&provider, &tds] {
            let open = error.source().unwrap().downcast_ref::<OpenError>().unwrap();
            assert_eq!(format!("{open:?}"), open.to_string());
            assert!(!format!("{open:?}").contains("secret"));
        }
        assert!(matches!(
            cleanup.source().unwrap().downcast_ref::<Box<TdsError>>(),
            Some(error) if matches!(error.as_ref(), TdsError::ConnectionError(message) if message == "tds-secret")
        ));
    }

    #[test]
    fn error_categories_and_timeout_stages_remain_distinct() {
        let errors = [
            PoolError::InvalidConfig {
                field: "max_size",
                reason: "must be positive",
            },
            PoolError::InvalidRequest {
                field: "key",
                reason: "must not be empty",
            },
            PoolError::Closed,
            PoolError::WaitQueueFull,
            PoolError::Cancelled,
            PoolError::IdentityMismatch,
            PoolError::TokenExpired,
            PoolError::CleanupTimeout,
            PoolError::ShutdownTimeout,
        ];
        let messages: std::collections::HashSet<_> =
            errors.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), errors.len());
        for error in &errors {
            assert!(error.source().is_none());
            assert_eq!(format!("{error:?}"), error.to_string());
        }
        for stage in [
            AcquireStage::Admission,
            AcquireStage::Wait,
            AcquireStage::Open,
            AcquireStage::Reset,
            AcquireStage::Isolation,
            AcquireStage::Delivery,
        ] {
            let error = PoolError::AcquireTimeout { stage };
            assert!(error.to_string().contains(&format!("{stage:?}")));
            assert!(error.source().is_none());
            assert_eq!(format!("{error:?}"), error.to_string());
        }
    }
}

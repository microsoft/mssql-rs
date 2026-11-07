// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mssql_tds::connection::{client_context::ClientContext, tds_client::TdsClient};
use mssql_tds::core::CancelHandle;

use super::error::{OpenError, PoolError, PoolResult};

/// Accepts the key adapter's fingerprint, never a connection string or token.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct PoolKey(Box<[u8]>);

impl PoolKey {
    pub(crate) fn new(fingerprint: Box<[u8]>) -> PoolResult<Self> {
        if fingerprint.is_empty() {
            return Err(PoolError::InvalidRequest {
                field: "key",
                reason: "must not be empty",
            });
        }
        Ok(Self(fingerprint))
    }
}

impl fmt::Debug for PoolKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PoolKey([REDACTED])")
    }
}

/// Opaque identity/credential-version binding supplied by the authentication adapter.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthBinding(Box<[u8]>);

impl AuthBinding {
    pub(crate) fn new(fingerprint: Box<[u8]>) -> PoolResult<Self> {
        if fingerprint.is_empty() {
            return Err(PoolError::InvalidRequest {
                field: "auth_binding",
                reason: "must not be empty",
            });
        }
        Ok(Self(fingerprint))
    }
}

impl fmt::Debug for AuthBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthBinding([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TokenExpiry {
    NonToken,
    Known(DateTime<Utc>),
    UnknownRawToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthMetadata {
    pub(crate) binding: AuthBinding,
    pub(crate) expiry: TokenExpiry,
}

impl AuthMetadata {
    pub(crate) fn verify_binding(&self, expected: &AuthBinding) -> PoolResult<()> {
        if self.binding != *expected {
            return Err(PoolError::IdentityMismatch);
        }
        Ok(())
    }
}

/// The caller supplies already-validated effective options, including recovery policy.
pub(crate) struct ConnectSpec {
    context: ClientContext,
    expected_auth: AuthBinding,
}

impl ConnectSpec {
    pub(crate) fn new(context: ClientContext, expected_auth: AuthBinding) -> Self {
        Self {
            context,
            expected_auth,
        }
    }

    pub(crate) fn context(&self) -> &ClientContext {
        &self.context
    }
    pub(crate) fn expected_auth(&self) -> &AuthBinding {
        &self.expected_auth
    }
}

pub(crate) struct OpenedConnection {
    client: TdsClient,
    auth: AuthMetadata,
}

impl OpenedConnection {
    pub(crate) fn new(client: TdsClient, auth: AuthMetadata) -> Self {
        Self { client, auth }
    }

    pub(crate) fn into_parts(self) -> (TdsClient, AuthMetadata) {
        (self.client, self.auth)
    }

    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        &mut self.client
    }
}

#[async_trait]
pub(crate) trait ConnectionFactory: Send + Sync {
    async fn open(
        &self,
        spec: &ConnectSpec,
        cancel: &CancelHandle,
    ) -> Result<OpenedConnection, OpenError>;
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn keys_and_bindings_are_opaque_and_distinct() {
        let key = PoolKey::new(b"private-key-fingerprint".to_vec().into_boxed_slice()).unwrap();
        let other = PoolKey::new(b"different-key-fingerprint".to_vec().into_boxed_slice()).unwrap();
        assert_ne!(key, other);
        assert_eq!(HashSet::from([key.clone(), key.clone(), other]).len(), 2);
        assert_eq!(format!("{key:?}"), "PoolKey([REDACTED])");
        let binding =
            AuthBinding::new(b"private-auth-fingerprint".to_vec().into_boxed_slice()).unwrap();
        assert_eq!(format!("{binding:?}"), "AuthBinding([REDACTED])");
        assert!(matches!(
            PoolKey::new(Box::default()),
            Err(PoolError::InvalidRequest { field: "key", .. })
        ));
        assert!(matches!(
            AuthBinding::new(Box::default()),
            Err(PoolError::InvalidRequest {
                field: "auth_binding",
                ..
            })
        ));
    }

    #[test]
    fn auth_metadata_distinguishes_non_token_known_and_unknown_expiry() {
        let binding = AuthBinding::new(vec![1].into_boxed_slice()).unwrap();
        let other = AuthBinding::new(vec![2].into_boxed_slice()).unwrap();
        for expiry in [
            TokenExpiry::NonToken,
            TokenExpiry::Known(Utc::now()),
            TokenExpiry::UnknownRawToken,
        ] {
            let metadata = AuthMetadata {
                binding: binding.clone(),
                expiry: expiry.clone(),
            };
            assert_eq!(metadata.expiry, expiry);
            metadata.verify_binding(&binding).unwrap();
            assert!(matches!(
                metadata.verify_binding(&other),
                Err(PoolError::IdentityMismatch)
            ));
        }
    }

    struct RejectingFactory(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl ConnectionFactory for RejectingFactory {
        async fn open(
            &self,
            spec: &ConnectSpec,
            _: &CancelHandle,
        ) -> Result<OpenedConnection, OpenError> {
            self.0
                .lock()
                .unwrap()
                .push(spec.context().data_source.clone());
            Err(OpenError::Provider(Box::new(std::io::Error::other(
                "provider sentinel",
            ))))
        }
    }

    #[tokio::test]
    async fn factory_is_object_safe_and_uses_each_calls_immutable_spec() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory: Arc<dyn ConnectionFactory> = Arc::new(RejectingFactory(calls.clone()));
        let binding = AuthBinding::new(vec![1].into_boxed_slice()).unwrap();
        for endpoint in ["first-server", "second-server"] {
            let mut context = ClientContext::default();
            context.data_source = endpoint.into();
            let spec = ConnectSpec::new(context, binding.clone());
            assert_eq!(spec.expected_auth(), &binding);
            assert!(matches!(
                factory.open(&spec, &CancelHandle::new()).await,
                Err(OpenError::Provider(_))
            ));
        }
        assert_eq!(*calls.lock().unwrap(), ["first-server", "second-server"]);
    }

    #[test]
    fn physical_open_contract_moves_the_client_by_value_and_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<OpenedConnection>();
        let _: fn(TdsClient, AuthMetadata) -> OpenedConnection = OpenedConnection::new;
        let _: fn(OpenedConnection) -> (TdsClient, AuthMetadata) = OpenedConnection::into_parts;
    }

    #[tokio::test]
    async fn opened_connection_transfers_client_state_and_auth_metadata_unchanged() {
        let mut client = mssql_tds::test_client_support::tds_client_from_tokens(vec![]);
        let connection_id = client.client_connection_id();
        client.mark_connection_dead();
        let auth = AuthMetadata {
            binding: AuthBinding::new(vec![0, 255, 0].into_boxed_slice()).unwrap(),
            expiry: TokenExpiry::Known(DateTime::UNIX_EPOCH),
        };
        let opened = OpenedConnection::new(client, auth.clone());
        let (client, returned_auth) = opened.into_parts();
        assert_eq!(client.client_connection_id(), connection_id);
        assert!(client.is_connection_dead());
        assert_eq!(returned_auth, auth);
        assert_ne!(returned_auth.expiry, TokenExpiry::NonToken);
    }

    #[test]
    fn opaque_fingerprints_preserve_zero_bytes_lengths_and_credential_versions() {
        let fingerprints = [vec![0], vec![0, 0], vec![0, 255], vec![0, 255, 0]];
        let mut keys = HashSet::new();
        for fingerprint in fingerprints {
            let key = PoolKey::new(fingerprint.clone().into_boxed_slice()).unwrap();
            assert!(keys.insert(key.clone()));
            assert!(!keys.insert(key));
            let binding = AuthBinding::new(fingerprint.clone().into_boxed_slice()).unwrap();
            assert_eq!(
                binding,
                AuthBinding::new(fingerprint.into_boxed_slice()).unwrap()
            );
            let mut changed = binding.0.to_vec();
            *changed.last_mut().unwrap() ^= 1;
            assert_ne!(
                binding,
                AuthBinding::new(changed.into_boxed_slice()).unwrap()
            );
        }
        assert_eq!(keys.len(), 4);
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mssql_tds::connection::{
    client_context::{ClientContext, ColumnEncryptionSetting, TdsAuthenticationMethod},
    tds_client::TdsClient,
};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::CancelHandle;

use super::error::{OpenError, PoolError, PoolResult};

/// Accepts the key adapter's fingerprint, never a connection string or token.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct PoolKey(Box<[u8]>);

impl PoolKey {
    /// Rejects empty fingerprints; the caller owns canonicalization.
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
    /// Rejects empty bindings; the caller owns identity and credential-version mapping.
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

/// Expiry information for evaluating token reuse, not a credential-refresh policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TokenExpiry {
    /// No token-expiry policy applies to this authentication method.
    NonToken,
    /// Known UTC expiry supplied by the authentication adapter.
    Known(DateTime<Utc>),
    /// A raw token's expiry is unknown, not unlimited.
    UnknownRawToken,
}

/// Authentication binding and expiry reported by a physical open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthMetadata {
    pub(crate) binding: AuthBinding,
    pub(crate) expiry: TokenExpiry,
}

impl AuthMetadata {
    /// Compares fingerprints only; it does not verify the server's login principal.
    pub(crate) fn verify_binding(&self, expected: &AuthBinding) -> PoolResult<()> {
        if self.binding != *expected {
            return Err(PoolError::IdentityMismatch);
        }
        Ok(())
    }
}

/// Caller-validated connection options and expected authentication binding.
pub(crate) struct ConnectSpec {
    context: ClientContext,
    expected_auth: AuthBinding,
}

impl ConnectSpec {
    /// Takes ownership of effective options without repeating caller validation.
    pub(crate) fn new(mut context: ClientContext, expected_auth: AuthBinding) -> Self {
        context.abort_on_drop = true;
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

    pub(super) fn validate_pool(&self) -> PoolResult<()> {
        let context = self.context();
        if context.connect_retry_count != 0 {
            return Err(PoolError::InvalidRequest {
                field: "connect_retry_count",
                reason: "pooled sessions require zero",
            });
        }
        if context.tds_authentication_method != TdsAuthenticationMethod::Password
            || context.access_token.is_some()
            || !context.auth_method_map.is_empty()
            || !context.change_password.is_empty()
        {
            return Err(PoolError::InvalidRequest {
                field: "authentication",
                reason: "only SQL password login is enabled for pooled sessions",
            });
        }
        if context.column_encryption_setting != ColumnEncryptionSetting::Disabled {
            return Err(PoolError::InvalidRequest {
                field: "column_encryption",
                reason: "pooled cache isolation is not yet verified",
            });
        }
        context
            .validate()
            .map_err(|error| PoolError::Open(OpenError::Tds(Box::new(error))))
    }
}

/// Moves a physical client and its authentication metadata without shared ownership.
pub(crate) struct OpenedConnection {
    client: TdsClient,
    auth: AuthMetadata,
}

impl OpenedConnection {
    pub(crate) fn new(client: TdsClient, auth: AuthMetadata) -> Self {
        Self { client, auth }
    }

    /// Transfers the client and authentication metadata to their next owner.
    pub(crate) fn into_parts(self) -> (TdsClient, AuthMetadata) {
        (self.client, self.auth)
    }

    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        &mut self.client
    }

    pub(super) fn verify_auth(&self, spec: &ConnectSpec) -> PoolResult<()> {
        self.auth.verify_binding(spec.expected_auth())?;
        if self.auth.expiry != TokenExpiry::NonToken {
            return Err(PoolError::InvalidRequest {
                field: "authentication",
                reason: "token pooling is not enabled",
            });
        }
        Ok(())
    }
}

pub(crate) struct SqlConnectionFactory;

#[async_trait]
impl ConnectionFactory for SqlConnectionFactory {
    async fn open(
        &self,
        spec: &ConnectSpec,
        cancel: &CancelHandle,
    ) -> Result<OpenedConnection, OpenError> {
        let client = TdsConnectionProvider {}
            .create_client(
                spec.context().clone(),
                &spec.context().data_source,
                Some(cancel),
            )
            .await
            .map_err(|error| OpenError::Tds(Box::new(error)))?;
        Ok(OpenedConnection::new(
            client,
            AuthMetadata {
                binding: spec.expected_auth().clone(),
                expiry: TokenExpiry::NonToken,
            },
        ))
    }
}

/// Opens physical connections with per-call options, not shared request state.
#[async_trait]
pub(crate) trait ConnectionFactory: Send + Sync {
    /// The caller must enforce deadlines and settle any transport or provider work.
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
    fn pooled_specs_require_synchronous_transport_discard() {
        let context = ClientContext::with_data_source("tcp:localhost,1433");
        assert!(!context.abort_on_drop);
        let binding = AuthBinding::new(vec![1].into_boxed_slice()).unwrap();
        let spec = ConnectSpec::new(context, binding);
        assert!(spec.context().abort_on_drop);
        assert!(spec.context().clone().abort_on_drop);
    }

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

    #[tokio::test]
    async fn sql_factory_honors_cancelled_login_without_opening_a_transport() {
        let mut context = ClientContext::with_data_source("tcp:localhost,1433");
        context.user_name = "test-user".into();
        context.password = "test-password".into();
        context.connect_retry_count = 0;
        let spec = ConnectSpec::new(
            context,
            AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
        );
        spec.validate_pool().unwrap();
        let cancel = CancelHandle::new();
        cancel.cancel();
        assert!(matches!(
            SqlConnectionFactory.open(&spec, &cancel).await,
            Err(OpenError::Tds(error))
                if matches!(*error, mssql_tds::error::Error::OperationCancelledError(_))
        ));
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

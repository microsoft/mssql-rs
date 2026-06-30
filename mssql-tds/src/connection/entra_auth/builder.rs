// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Maps a [`ClientContext`]'s authentication method and credential inputs onto a
//! built-in [`AzureIdentityTokenFactory`] and registers it.

use crate::connection::client_context::{
    ClientContext, CloneableEntraIdTokenFactory, TdsAuthenticationMethod,
};
use crate::connection::entra_auth::factory::{AzureIdentityTokenFactory, CredentialConfig};

/// Derives the [`CredentialConfig`] for the context's authentication method, or
/// `None` for methods without a built-in `azure_identity` credential.
pub(crate) fn config_for_method(context: &ClientContext) -> Option<CredentialConfig> {
    match context.tds_authentication_method {
        TdsAuthenticationMethod::ActiveDirectoryServicePrincipal => {
            Some(CredentialConfig::ServicePrincipalSecret {
                client_id: context.user_name.clone(),
                secret: context.password.clone(),
            })
        }
        TdsAuthenticationMethod::ActiveDirectoryManagedIdentity
        | TdsAuthenticationMethod::ActiveDirectoryMSI => Some(CredentialConfig::ManagedIdentity {
            user_assigned_client_id: non_empty(&context.user_name),
        }),
        TdsAuthenticationMethod::ActiveDirectoryDefault => Some(CredentialConfig::Default),
        TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity => {
            Some(CredentialConfig::WorkloadIdentity {
                client_id: non_empty(&context.user_name),
                tenant_id: None,
            })
        }
        _ => None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

impl ClientContext {
    /// Installs a built-in [`AzureIdentityTokenFactory`] for the context's
    /// authentication method.
    ///
    /// A factory already present in `auth_method_map` for that method is left
    /// untouched, so a caller-injected factory always wins. Methods without a
    /// built-in `azure_identity` credential (e.g. `ActiveDirectoryPassword`,
    /// `SSPI`, `AccessToken`) are a no-op here.
    ///
    /// Invoked automatically from the connect path when the `entra-auth`
    /// feature is enabled; also available for explicit use.
    pub fn register_builtin_entra_factories(&mut self) {
        if self
            .auth_method_map
            .contains_key(&self.tds_authentication_method)
        {
            return;
        }
        if let Some(config) = config_for_method(self) {
            let factory: Box<dyn CloneableEntraIdTokenFactory> =
                Box::new(AzureIdentityTokenFactory::new(config));
            self.auth_method_map
                .insert(self.tds_authentication_method.clone(), factory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::client_context::EntraIdTokenFactory;
    use crate::core::TdsResult;
    use async_trait::async_trait;

    fn context_for(method: TdsAuthenticationMethod) -> ClientContext {
        ClientContext {
            tds_authentication_method: method,
            ..Default::default()
        }
    }

    #[test]
    fn service_principal_maps_user_and_password() {
        let mut context = context_for(TdsAuthenticationMethod::ActiveDirectoryServicePrincipal);
        context.user_name = "client-123".to_string();
        context.password = "secret-xyz".to_string();
        match config_for_method(&context) {
            Some(CredentialConfig::ServicePrincipalSecret { client_id, secret }) => {
                assert_eq!(client_id, "client-123");
                assert_eq!(secret, "secret-xyz");
            }
            other => panic!("expected service principal config, got {other:?}"),
        }
    }

    #[test]
    fn managed_identity_user_assigned_id_is_optional() {
        assert!(matches!(
            config_for_method(&context_for(
                TdsAuthenticationMethod::ActiveDirectoryManagedIdentity
            )),
            Some(CredentialConfig::ManagedIdentity {
                user_assigned_client_id: None
            })
        ));

        let mut context = context_for(TdsAuthenticationMethod::ActiveDirectoryMSI);
        context.user_name = "uami-client-id".to_string();
        match config_for_method(&context) {
            Some(CredentialConfig::ManagedIdentity {
                user_assigned_client_id: Some(id),
            }) => assert_eq!(id, "uami-client-id"),
            other => panic!("expected user-assigned managed identity, got {other:?}"),
        }
    }

    #[test]
    fn default_and_workload_identity_map() {
        assert!(matches!(
            config_for_method(&context_for(
                TdsAuthenticationMethod::ActiveDirectoryDefault
            )),
            Some(CredentialConfig::Default)
        ));
        assert!(matches!(
            config_for_method(&context_for(
                TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity
            )),
            Some(CredentialConfig::WorkloadIdentity { .. })
        ));
    }

    #[test]
    fn unsupported_methods_have_no_builtin() {
        for method in [
            TdsAuthenticationMethod::Password,
            TdsAuthenticationMethod::SSPI,
            TdsAuthenticationMethod::ActiveDirectoryPassword,
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow,
            TdsAuthenticationMethod::ActiveDirectoryIntegrated,
            TdsAuthenticationMethod::AccessToken,
        ] {
            assert!(config_for_method(&context_for(method)).is_none());
        }
    }

    #[test]
    fn register_inserts_builtin_when_absent() {
        let mut context = context_for(TdsAuthenticationMethod::ActiveDirectoryDefault);
        assert!(context.auth_method_map.is_empty());
        context.register_builtin_entra_factories();
        assert!(
            context
                .auth_method_map
                .contains_key(&TdsAuthenticationMethod::ActiveDirectoryDefault)
        );
    }

    #[test]
    fn register_is_noop_for_unsupported_method() {
        let mut context = context_for(TdsAuthenticationMethod::AccessToken);
        context.register_builtin_entra_factories();
        assert!(context.auth_method_map.is_empty());
    }

    #[derive(Clone)]
    struct StubFactory;

    #[async_trait]
    impl EntraIdTokenFactory for StubFactory {
        async fn create_token(
            &self,
            _spn: String,
            _sts_url: String,
            _auth_method: TdsAuthenticationMethod,
        ) -> TdsResult<Vec<u8>> {
            Ok(vec![0xAB])
        }
    }

    #[tokio::test]
    async fn caller_injected_factory_wins() {
        let mut context = context_for(TdsAuthenticationMethod::ActiveDirectoryDefault);
        let stub: Box<dyn CloneableEntraIdTokenFactory> = Box::new(StubFactory);
        context
            .auth_method_map
            .insert(TdsAuthenticationMethod::ActiveDirectoryDefault, stub);

        context.register_builtin_entra_factories();

        let factory = context
            .auth_method_map
            .get(&TdsAuthenticationMethod::ActiveDirectoryDefault)
            .expect("factory present");
        let token = factory
            .create_token(
                String::new(),
                String::new(),
                TdsAuthenticationMethod::ActiveDirectoryDefault,
            )
            .await
            .unwrap();
        assert_eq!(
            token,
            vec![0xAB],
            "built-in must not replace caller factory"
        );
    }
}

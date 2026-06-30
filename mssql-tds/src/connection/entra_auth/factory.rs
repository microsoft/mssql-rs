// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`AzureIdentityTokenFactory`] — an [`EntraIdTokenFactory`] that acquires
//! tokens through `azure_identity` credentials.

use async_trait::async_trait;
use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::{
    ClientAssertionCredentialOptions, ClientSecretCredential, ClientSecretCredentialOptions,
    DefaultAzureCredential, ManagedIdentityCredential, ManagedIdentityCredentialOptions,
    TokenCredentialOptions, UserAssignedId, WorkloadIdentityCredential,
    WorkloadIdentityCredentialOptions,
};

use crate::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use crate::connection::entra_auth::encoding::{
    StsUrl, encode_jwt_utf16le, normalize_scope, parse_sts_url,
};
use crate::core::TdsResult;
use crate::error::Error;

/// Which `azure_identity` credential to build, plus the inputs it needs.
#[derive(Clone, Debug)]
pub(crate) enum CredentialConfig {
    /// `ClientSecretCredential` — service principal with a client secret.
    ServicePrincipalSecret { client_id: String, secret: String },
    /// `ManagedIdentityCredential` — system- or user-assigned managed identity.
    ManagedIdentity {
        user_assigned_client_id: Option<String>,
    },
    /// `DefaultAzureCredential` — the default credential chain.
    Default,
    /// `WorkloadIdentityCredential` — federated Kubernetes/AKS workload identity.
    WorkloadIdentity {
        client_id: Option<String>,
        tenant_id: Option<String>,
    },
}

/// A built-in [`EntraIdTokenFactory`] backed by `azure_identity`.
///
/// One factory wraps one [`CredentialConfig`]; the FedAuth handshake supplies
/// the SPN (resource) and STS authority at token-acquisition time.
#[derive(Clone, Debug)]
pub struct AzureIdentityTokenFactory {
    config: CredentialConfig,
}

impl AzureIdentityTokenFactory {
    pub(crate) fn new(config: CredentialConfig) -> Self {
        Self { config }
    }

    async fn acquire_jwt(&self, scope: &str, sts: &StsUrl) -> TdsResult<String> {
        let response = match &self.config {
            CredentialConfig::ServicePrincipalSecret { client_id, secret } => {
                let tenant_id = sts.tenant_id.as_deref().ok_or_else(|| {
                    Error::ConnectionError(
                        "Service principal authentication requires a tenant, but none could be \
                         determined from the server's STS URL."
                            .to_string(),
                    )
                })?;
                let credential = ClientSecretCredential::new(
                    tenant_id,
                    client_id.clone(),
                    Secret::from(secret.clone()),
                    Some(ClientSecretCredentialOptions {
                        credential_options: authority_options(sts),
                    }),
                )
                .map_err(token_error)?;
                credential.get_token(&[scope], None).await
            }
            CredentialConfig::ManagedIdentity {
                user_assigned_client_id,
            } => {
                let options = ManagedIdentityCredentialOptions {
                    credential_options: authority_options(sts),
                    user_assigned_id: user_assigned_client_id
                        .clone()
                        .map(UserAssignedId::ClientId),
                };
                let credential =
                    ManagedIdentityCredential::new(Some(options)).map_err(token_error)?;
                credential.get_token(&[scope], None).await
            }
            CredentialConfig::Default => {
                let credential = DefaultAzureCredential::with_options(authority_options(sts))
                    .map_err(token_error)?;
                credential.get_token(&[scope], None).await
            }
            CredentialConfig::WorkloadIdentity {
                client_id,
                tenant_id,
            } => {
                let options = WorkloadIdentityCredentialOptions {
                    credential_options: ClientAssertionCredentialOptions {
                        credential_options: authority_options(sts),
                        ..Default::default()
                    },
                    client_id: client_id.clone(),
                    tenant_id: tenant_id.clone().or_else(|| sts.tenant_id.clone()),
                    token_file_path: None,
                };
                let credential =
                    WorkloadIdentityCredential::new(Some(options)).map_err(token_error)?;
                credential.get_token(&[scope], None).await
            }
        };

        Ok(response.map_err(token_error)?.token.secret().to_string())
    }
}

#[async_trait]
impl EntraIdTokenFactory for AzureIdentityTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let scope = normalize_scope(&spn);
        let sts = parse_sts_url(&sts_url);
        let jwt = self.acquire_jwt(&scope, &sts).await?;
        Ok(encode_jwt_utf16le(&jwt))
    }
}

fn authority_options(sts: &StsUrl) -> TokenCredentialOptions {
    let mut options = TokenCredentialOptions::default();
    options.set_authority_host(sts.authority_host.clone());
    options
}

fn token_error(error: azure_core::Error) -> Error {
    Error::ConnectionError(format!("Entra ID token acquisition failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_principal_without_tenant_errors() {
        let factory = AzureIdentityTokenFactory::new(CredentialConfig::ServicePrincipalSecret {
            client_id: "client-id".to_string(),
            secret: "client-secret".to_string(),
        });
        // STS URL carries no tenant segment, so the service-principal flow has
        // no tenant to authenticate against and must fail fast (no network).
        let result = factory
            .create_token(
                "https://database.windows.net/".to_string(),
                "https://login.microsoftonline.com/".to_string(),
                TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
            )
            .await;
        assert!(matches!(result, Err(Error::ConnectionError(_))));
    }
}

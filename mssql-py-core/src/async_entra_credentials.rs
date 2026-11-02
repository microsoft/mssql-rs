// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Built-in Azure Identity configuration for acquisition-scoped async credentials.

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use pyo3::exceptions::{PyImportError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::async_errors::{InterfaceError, NotSupportedError, OperationalError};

const PUBLIC_AUTHORITY: &str = "login.microsoftonline.com";

pub(crate) enum BuiltInEntraCredential {
    Default,
    ManagedIdentity { client_id: Option<String> },
    ClientSecret { client_id: String, secret: String },
}

impl BuiltInEntraCredential {
    pub(crate) fn from_context(context: &mut ClientContext) -> PyResult<Option<Self>> {
        use TdsAuthenticationMethod::*;

        if matches!(
            context.tds_authentication_method,
            ActiveDirectoryDefault
                | ActiveDirectoryMSI
                | ActiveDirectoryManagedIdentity
                | ActiveDirectoryServicePrincipal
        ) && !context.auth_method_map.is_empty()
        {
            return Err(InterfaceError::new_err(
                "Built-in async Entra authentication cannot be combined with entra_id_token_factory",
            ));
        }
        Ok(Some(match context.tds_authentication_method {
            Password | SSPI | AccessToken => return Ok(None),
            ActiveDirectoryDefault => Self::Default,
            ActiveDirectoryMSI | ActiveDirectoryManagedIdentity => Self::ManagedIdentity {
                client_id: match std::mem::take(&mut context.user_name) {
                    value if value.trim().is_empty() => None,
                    value => Some(value.trim().to_owned()),
                },
            },
            ActiveDirectoryServicePrincipal => Self::ClientSecret {
                client_id: std::mem::take(&mut context.user_name),
                secret: std::mem::take(&mut context.password),
            },
            _ => {
                return Err(NotSupportedError::new_err(
                    "This Entra authentication mode has no built-in AQE async credential support",
                ));
            }
        }))
    }

    pub(crate) fn check_dependencies(py: Python<'_>) -> PyResult<()> {
        for module in ["azure.identity.aio", "aiohttp"] {
            if let Err(error) = py.import(module) {
                if error.is_instance_of::<PyImportError>(py) {
                    return Err(OperationalError::new_err(
                        "Built-in async Entra authentication requires azure-identity and aiohttp; \
                         install mssql-python-rs[azure-async]",
                    ));
                }
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn create(&self, py: Python<'_>, scope: &str, sts_url: &str) -> PyResult<Py<PyAny>> {
        if scope != "https://database.windows.net/.default" {
            return Err(PyValueError::new_err(
                "Built-in async Entra authentication supports only the Azure Public SQL scope",
            ));
        }
        let tenant = public_tenant(py, sts_url)?;
        let kwargs = PyDict::new(py);
        let class_name = match self {
            Self::Default => {
                kwargs.set_item("authority", PUBLIC_AUTHORITY)?;
                "DefaultAzureCredential"
            }
            Self::ManagedIdentity { client_id } => {
                if let Some(client_id) = client_id {
                    kwargs.set_item("client_id", client_id)?;
                }
                "ManagedIdentityCredential"
            }
            Self::ClientSecret { client_id, secret } => {
                kwargs.set_item("tenant_id", tenant)?;
                kwargs.set_item("client_id", client_id)?;
                kwargs.set_item("client_secret", secret)?;
                kwargs.set_item("authority", PUBLIC_AUTHORITY)?;
                "ClientSecretCredential"
            }
        };
        Ok(py
            .import("azure.identity.aio")?
            .getattr(class_name)?
            .call((), Some(&kwargs))?
            .unbind())
    }
}

fn public_tenant(py: Python<'_>, sts_url: &str) -> PyResult<String> {
    let parsed = py
        .import("urllib.parse")?
        .call_method1("urlsplit", (sts_url,))?;
    let scheme = parsed.getattr("scheme")?.extract::<String>()?;
    let host = parsed.getattr("hostname")?.extract::<Option<String>>()?;
    let port = parsed.getattr("port")?.extract::<Option<u16>>()?;
    let path = parsed.getattr("path")?.extract::<String>()?;
    let tenant = path.trim_start_matches('/').split('/').next().unwrap_or("");
    let valid_host = host.as_deref().is_some_and(|host| {
        matches!(
            host,
            "login.microsoftonline.com" | "login.windows.net" | "sts.windows.net"
        )
    });
    if scheme != "https"
        || !valid_host
        || port.is_some_and(|port| port != 443)
        || !parsed.getattr("username")?.is_none()
        || !parsed.getattr("password")?.is_none()
        || sts_url.chars().any(|c| c.is_whitespace() || c.is_control())
        || tenant.is_empty()
        || !tenant
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.'))
        || ["common", "organizations", "consumers"]
            .iter()
            .any(|reserved| tenant.eq_ignore_ascii_case(reserved))
    {
        return Err(PyValueError::new_err(
            "FedAuth STS URL must identify a tenant on an Azure Public HTTPS authority",
        ));
    }
    Ok(tenant.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_sts_tenant_formats() {
        Python::attach(|py| {
            for (url, tenant) in [
                (
                    "https://login.microsoftonline.com/test-tenant/",
                    "test-tenant",
                ),
                (
                    "https://login.windows.net/contoso.onmicrosoft.com/oauth2/token",
                    "contoso.onmicrosoft.com",
                ),
                (
                    "https://sts.windows.net/11111111-2222-3333-4444-555555555555/",
                    "11111111-2222-3333-4444-555555555555",
                ),
                (
                    "https://LOGIN.MICROSOFTONLINE.COM:443/test-tenant/?query=value",
                    "test-tenant",
                ),
            ] {
                assert_eq!(public_tenant(py, url).unwrap(), tenant);
            }
        });
    }

    #[test]
    fn non_public_sql_scope_is_rejected_before_sdk_construction() {
        Python::attach(|py| {
            for scope in [
                "https://database.usgovcloudapi.net/.default",
                "https://other.example/.default",
                "https://database.windows.net/tenant/.default",
            ] {
                let error = BuiltInEntraCredential::Default
                    .create(py, scope, "https://login.microsoftonline.com/test-tenant/")
                    .unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py));
                assert!(error.to_string().contains("Azure Public SQL scope"));
            }
        });
    }

    #[test]
    fn untrusted_or_ambiguous_sts_urls_are_rejected() {
        Python::attach(|py| {
            for url in [
                "http://login.microsoftonline.com/tenant/",
                "https://login.microsoftonline.us/tenant/",
                "https://login.microsoftonline.com.evil.example/tenant/",
                "https://login.microsoftonline.com@evil.example/tenant/",
                "https://user@login.microsoftonline.com/tenant/",
                "https://login.microsoftonline.com:8443/tenant/",
                "https://login.microsoftonline.com/",
                "https://login.microsoftonline.com/common/",
                "https://login.microsoftonline.com/ORGANIZATIONS/",
                "https://login.microsoftonline.com/consumers/",
                "https://login.microsoftonline.com/tenant%2Fother/",
                "https://login.microsoftonline.com/tenant/\n",
                "not-a-url",
            ] {
                assert!(public_tenant(py, url).is_err(), "{url}");
            }
        });
    }
}

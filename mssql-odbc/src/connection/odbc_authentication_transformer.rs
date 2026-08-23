// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::odbc_supported_auth_keywords::auth_method_from_keyword;
use mssql_tds::connection::client_context::TdsAuthenticationMethod;

/// Transformed authentication output produced by [`transform_auth`].
///
/// Contains only auth-related fields. The caller is responsible for
/// populating the remaining connection fields on `ClientContext` or
/// wherever they are needed.
#[derive(Debug, Clone)]
pub(crate) struct TransformedAuth {
    /// Resolved authentication method.
    pub(crate) method: TdsAuthenticationMethod,
    /// User name (may be empty if not applicable).
    pub(crate) user_name: String,
    /// Password (may be empty if not applicable).
    pub(crate) password: String,
    /// Pre-acquired access token, if any.
    pub(crate) access_token: Option<String>,
}

/// Clears credentials that the resolved auth method does not use.
///
/// - **SSPI / ADIntegrated**: both uid and pwd cleared
/// - **ADInteractive / ADManagedIdentity / ADDefault / ADDeviceCode / ADWorkloadIdentity**: pwd cleared, uid kept as hint/client_id
/// - **AccessToken**: both uid and pwd cleared (handled before this is called)
fn apply_silent_clears(method: &TdsAuthenticationMethod, uid: &mut String, pwd: &mut String) {
    match method {
        TdsAuthenticationMethod::SSPI | TdsAuthenticationMethod::ActiveDirectoryIntegrated => {
            uid.clear();
            pwd.clear();
        }
        TdsAuthenticationMethod::ActiveDirectoryInteractive
        | TdsAuthenticationMethod::ActiveDirectoryManagedIdentity
        | TdsAuthenticationMethod::ActiveDirectoryDefault
        | TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow
        | TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity => {
            pwd.clear();
        }
        _ => {}
    }
}

/// Resolves raw auth inputs into a [`TransformedAuth`] using ODBC-equivalent
/// precedence rules.
///
/// **Expects validated inputs.** If the caller skips validation and passes
/// conflicting values, the transformer resolves best-effort (same as ODBC's
/// internal flow after validation passes).
///
/// # Precedence (checked in order)
///
/// 1. `access_token` present → [`AccessToken`], uid/pwd cleared
/// 2. `trusted_connection == Some(true)` → [`SSPI`], uid/pwd cleared
/// 3. `authentication` keyword → mapped enum (SqlPassword → Password)
/// 4. Default → [`Password`], uid/pwd as-is
///
/// After resolution, silent clears remove credentials the selected method
/// doesn't use.
pub(crate) fn transform_auth(
    auth: Option<&str>,
    tc: Option<bool>,
    uid: &str,
    pwd: &str,
    token: Option<&str>,
) -> TransformedAuth {
    let mut user_name = uid.to_string();
    let mut password = pwd.to_string();

    // Access token takes highest precedence
    if let Some(tok) = token {
        return TransformedAuth {
            method: TdsAuthenticationMethod::AccessToken,
            user_name: String::new(),
            password: String::new(),
            access_token: Some(tok.to_string()),
        };
    }

    // Trusted_Connection=Yes → SSPI
    if tc == Some(true) {
        return TransformedAuth {
            method: TdsAuthenticationMethod::SSPI,
            user_name: String::new(),
            password: String::new(),
            access_token: None,
        };
    }

    // Authentication keyword → enum
    if let Some(auth_val) = auth
        && !auth_val.is_empty()
    {
        // auth_method_from_keyword handles SqlPassword → Password collapse
        let method =
            auth_method_from_keyword(auth_val).unwrap_or(TdsAuthenticationMethod::Password);

        // Silent clears per method
        apply_silent_clears(&method, &mut user_name, &mut password);

        return TransformedAuth {
            method,
            user_name,
            password,
            access_token: None,
        };
    }
    // auth == Some("") → intentional reset, fall through to default

    // Default → Password, no auto-promote to SSPI
    TransformedAuth {
        method: TdsAuthenticationMethod::Password,
        user_name,
        password,
        access_token: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Access Token path ───────────────────────────────────────

    #[test]
    fn access_token_resolves_to_access_token_method() {
        let r = transform_auth(None, None, "", "", Some("jwt123"));
        assert_eq!(r.method, TdsAuthenticationMethod::AccessToken);
        assert_eq!(r.access_token.as_deref(), Some("jwt123"));
        assert!(r.user_name.is_empty());
        assert!(r.password.is_empty());
    }

    #[test]
    fn access_token_takes_precedence_over_auth_keyword() {
        let r = transform_auth(Some("SqlPassword"), None, "sa", "pwd", Some("jwt"));
        assert_eq!(r.method, TdsAuthenticationMethod::AccessToken);
        assert!(r.user_name.is_empty());
    }

    #[test]
    fn access_token_takes_precedence_over_tc() {
        let r = transform_auth(None, Some(true), "sa", "pwd", Some("jwt"));
        assert_eq!(r.method, TdsAuthenticationMethod::AccessToken);
    }

    // ── Trusted_Connection → SSPI ────────────────────────────────

    #[test]
    fn tc_true_resolves_to_sspi() {
        let r = transform_auth(None, Some(true), "", "", None);
        assert_eq!(r.method, TdsAuthenticationMethod::SSPI);
        assert!(r.user_name.is_empty());
        assert!(r.password.is_empty());
    }

    #[test]
    fn tc_true_clears_uid_pwd() {
        let r = transform_auth(None, Some(true), "sa", "secret", None);
        assert_eq!(r.method, TdsAuthenticationMethod::SSPI);
        assert!(r.user_name.is_empty());
        assert!(r.password.is_empty());
    }

    #[test]
    fn tc_false_falls_through_to_default() {
        let r = transform_auth(None, Some(false), "sa", "secret", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
        assert_eq!(r.user_name, "sa");
        assert_eq!(r.password, "secret");
    }

    // ── Authentication keyword mapping ───────────────────────────

    #[test]
    fn ad_password_resolves() {
        let r = transform_auth(Some("ActiveDirectoryPassword"), None, "u", "p", None);
        assert_eq!(r.method, TdsAuthenticationMethod::ActiveDirectoryPassword);
        assert_eq!(r.user_name, "u");
        assert_eq!(r.password, "p");
    }

    #[test]
    fn ad_interactive_resolves_and_clears_pwd() {
        let r = transform_auth(
            Some("ActiveDirectoryInteractive"),
            None,
            "user@domain.com",
            "p",
            None,
        );
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryInteractive
        );
        assert_eq!(r.user_name, "user@domain.com");
        assert!(r.password.is_empty());
    }

    #[test]
    fn ad_interactive_no_hint() {
        let r = transform_auth(Some("ActiveDirectoryInteractive"), None, "", "", None);
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryInteractive
        );
        assert!(r.user_name.is_empty());
    }

    #[test]
    fn ad_msi_resolves_and_clears_pwd() {
        // The ActiveDirectoryMSI keyword resolves to the canonical ManagedIdentity method.
        let r = transform_auth(Some("ActiveDirectoryMSI"), None, "", "leftover", None);
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity
        );
        assert!(r.password.is_empty());
    }

    #[test]
    fn ad_msi_keeps_uid_as_client_id() {
        let r = transform_auth(Some("ActiveDirectoryMSI"), None, "client-id-123", "", None);
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity
        );
        assert_eq!(r.user_name, "client-id-123");
    }

    #[test]
    fn ad_service_principal_resolves() {
        let r = transform_auth(
            Some("ActiveDirectoryServicePrincipal"),
            None,
            "cid",
            "sec",
            None,
        );
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal
        );
        assert_eq!(r.user_name, "cid");
        assert_eq!(r.password, "sec");
    }

    #[test]
    fn ad_default_resolves_and_clears_pwd() {
        let r = transform_auth(Some("ActiveDirectoryDefault"), None, "", "leftover", None);
        assert_eq!(r.method, TdsAuthenticationMethod::ActiveDirectoryDefault);
        assert!(r.password.is_empty());
    }

    #[test]
    fn ad_device_code_flow_resolves_and_clears_pwd() {
        let r = transform_auth(
            Some("ActiveDirectoryDeviceCodeFlow"),
            None,
            "",
            "leftover",
            None,
        );
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow
        );
        assert!(r.password.is_empty());
    }

    #[test]
    fn ad_workload_identity_resolves_and_clears_pwd() {
        let r = transform_auth(
            Some("ActiveDirectoryWorkloadIdentity"),
            None,
            "",
            "leftover",
            None,
        );
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity
        );
        assert!(r.password.is_empty());
    }

    // ── SqlPassword → Password collapse ──────────────────────────

    #[test]
    fn sql_password_collapses_to_password() {
        let r = transform_auth(Some("SqlPassword"), None, "sa", "secret", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
        assert_eq!(r.user_name, "sa");
        assert_eq!(r.password, "secret");
    }

    // ── ADIntegrated clears uid + pwd ────────────────────────────

    #[test]
    fn ad_integrated_clears_uid_and_pwd() {
        let r = transform_auth(
            Some("ActiveDirectoryIntegrated"),
            None,
            "user@domain.com",
            "pass",
            None,
        );
        assert_eq!(r.method, TdsAuthenticationMethod::ActiveDirectoryIntegrated);
        assert!(r.user_name.is_empty());
        assert!(r.password.is_empty());
    }

    #[test]
    fn ad_integrated_no_credentials() {
        let r = transform_auth(Some("ActiveDirectoryIntegrated"), None, "", "", None);
        assert_eq!(r.method, TdsAuthenticationMethod::ActiveDirectoryIntegrated);
        assert!(r.user_name.is_empty());
    }

    // ── Default path ────────────────────────────────────────────

    #[test]
    fn default_no_inputs_resolves_to_password() {
        let r = transform_auth(None, None, "", "", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
        assert!(r.user_name.is_empty());
        assert!(r.password.is_empty());
    }

    #[test]
    fn uid_pwd_only_resolves_to_password() {
        let r = transform_auth(None, None, "sa", "secret", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
        assert_eq!(r.user_name, "sa");
        assert_eq!(r.password, "secret");
    }

    #[test]
    fn tc_none_uid_pwd_resolves_to_password() {
        let r = transform_auth(None, None, "sa", "secret", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
    }

    #[test]
    fn empty_auth_string_falls_through_to_default() {
        let r = transform_auth(Some(""), None, "sa", "secret", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
        assert_eq!(r.user_name, "sa");
        assert_eq!(r.password, "secret");
    }

    // ── Case insensitivity ──────────────────────────────────────

    #[test]
    fn auth_keyword_case_insensitive() {
        let r = transform_auth(Some("SQLPASSWORD"), None, "sa", "pwd", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
    }

    #[test]
    fn mixed_case_ad_interactive() {
        let r = transform_auth(Some("activedirectoryINTERACTIVE"), None, "u", "", None);
        assert_eq!(
            r.method,
            TdsAuthenticationMethod::ActiveDirectoryInteractive
        );
    }

    // ── Unknown auth keyword (best-effort) ──────────────────────

    #[test]
    fn unknown_auth_keyword_falls_back_to_password() {
        let r = transform_auth(Some("BogusValue"), None, "sa", "pwd", None);
        assert_eq!(r.method, TdsAuthenticationMethod::Password);
        assert_eq!(r.user_name, "sa");
        assert_eq!(r.password, "pwd");
    }
}

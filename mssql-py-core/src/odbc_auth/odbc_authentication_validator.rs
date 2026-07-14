// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::odbc_supported_auth_keywords::{auth_method_from_keyword, is_recognized_keyword};
use mssql_tds::connection::client_context::TdsAuthenticationMethod;
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;

/// Validates the connection string key values for conflicts (ODBC-equivalent checks).
/// Returns `Ok(())` if valid, or a `TdsError` listing all detected conflicts.
///
/// Standalone function — does not depend on `ClientContext`.
pub fn validate_auth(
    authentication: Option<&str>,
    trusted_connection: Option<bool>,
    user_name: &str,
    password: &str,
    access_token: Option<&str>,
) -> TdsResult<()> {
    // Access Token isolation — collect all conflicts
    if access_token.is_some() {
        let mut conflicts = Vec::new();
        if trusted_connection == Some(true) {
            conflicts.push("Trusted_Connection is set");
        }
        if authentication.is_some_and(|a| !a.is_empty()) {
            conflicts.push("Authentication keyword is provided");
        }
        if !user_name.is_empty() {
            conflicts.push("User is provided");
        }
        if !password.is_empty() {
            conflicts.push("Password is provided");
        }
        if !conflicts.is_empty() {
            return Err(Error::UsageError(format!(
                "Access Token cannot be used when: {}.",
                conflicts.join(", ")
            )));
        }
    }

    // Authentication + Trusted_Connection mutual exclusion
    if authentication.is_some_and(|a| !a.is_empty()) && trusted_connection == Some(true) {
        return Err(Error::UsageError(
            "Cannot use Authentication with Trusted_Connection.".to_string(),
        ));
    }

    if let Some(auth) = authentication
        && !auth.is_empty()
    {
        // Unknown keyword → reject before credential rules
        // so we don't give misleading "missing UID/PWD" for a bogus keyword
        if !is_recognized_keyword(auth) {
            return Err(Error::UsageError(format!(
                "Unsupported Authentication value: '{auth}'.",
            )));
        }

        match auth_method_from_keyword(auth) {
            // SqlPassword / ADPassword / ADSPA require UID + PWD
            Some(
                TdsAuthenticationMethod::Password
                | TdsAuthenticationMethod::ActiveDirectoryPassword
                | TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
            ) if user_name.is_empty() || password.is_empty() => {
                return Err(Error::UsageError(format!(
                    "Both User and Password must be specified when Authentication is '{auth}'."
                )));
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Happy paths (validator should pass)
    // ---------------------------------------------------------------

    #[test]
    fn happy_uid_pwd_only() {
        assert!(validate_auth(None, None, "sa", "secret", None).is_ok());
    }

    #[test]
    fn happy_nothing_set() {
        assert!(validate_auth(None, None, "", "", None).is_ok());
    }

    #[test]
    fn happy_tc_true() {
        assert!(validate_auth(None, Some(true), "", "", None).is_ok());
    }

    #[test]
    fn happy_tc_true_with_uid_pwd() {
        assert!(validate_auth(None, Some(true), "sa", "secret", None).is_ok());
    }

    #[test]
    fn happy_tc_false_with_uid_pwd() {
        assert!(validate_auth(None, Some(false), "sa", "secret", None).is_ok());
    }

    #[test]
    fn happy_sqlpassword_with_uid_pwd() {
        assert!(validate_auth(Some("SqlPassword"), None, "sa", "secret", None).is_ok());
    }

    #[test]
    fn happy_ad_password_with_uid_pwd() {
        assert!(validate_auth(Some("ActiveDirectoryPassword"), None, "u", "p", None).is_ok());
    }

    #[test]
    fn happy_ad_integrated_alone() {
        assert!(validate_auth(Some("ActiveDirectoryIntegrated"), None, "", "", None).is_ok());
    }

    #[test]
    fn happy_ad_interactive_with_uid() {
        assert!(validate_auth(Some("ActiveDirectoryInteractive"), None, "u", "", None).is_ok());
    }

    #[test]
    fn happy_ad_interactive_alone() {
        assert!(validate_auth(Some("ActiveDirectoryInteractive"), None, "", "", None).is_ok());
    }

    #[test]
    fn happy_admsi_alone() {
        assert!(validate_auth(Some("ActiveDirectoryMSI"), None, "", "", None).is_ok());
    }

    #[test]
    fn happy_admsi_with_client_id() {
        assert!(validate_auth(Some("ActiveDirectoryMSI"), None, "cid", "", None).is_ok());
    }

    #[test]
    fn happy_adspa_with_uid_pwd() {
        assert!(
            validate_auth(
                Some("ActiveDirectoryServicePrincipal"),
                None,
                "cid",
                "sec",
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn happy_access_token_alone() {
        assert!(validate_auth(None, None, "", "", Some("jwt")).is_ok());
    }

    #[test]
    fn happy_empty_auth_keyword() {
        assert!(validate_auth(Some(""), None, "sa", "secret", None).is_ok());
    }

    #[test]
    fn happy_access_token_plus_empty_auth() {
        assert!(validate_auth(Some(""), None, "", "", Some("jwt")).is_ok());
    }

    #[test]
    fn happy_empty_auth_plus_tc() {
        assert!(validate_auth(Some(""), Some(true), "", "", None).is_ok());
    }

    #[test]
    fn happy_ad_default_alone() {
        assert!(validate_auth(Some("ActiveDirectoryDefault"), None, "", "", None).is_ok());
    }

    #[test]
    fn happy_ad_device_code_flow_alone() {
        assert!(validate_auth(Some("ActiveDirectoryDeviceCodeFlow"), None, "", "", None).is_ok());
    }

    #[test]
    fn happy_ad_workload_identity_alone() {
        assert!(validate_auth(Some("ActiveDirectoryWorkloadIdentity"), None, "", "", None).is_ok());
    }

    // ---------------------------------------------------------------
    // Access Token isolation
    // ---------------------------------------------------------------

    #[test]
    fn access_token_plus_tc() {
        let err = validate_auth(None, Some(true), "", "", Some("jwt")).unwrap_err();
        assert!(err.to_string().contains("Access Token cannot be used"));
        assert!(err.to_string().contains("Trusted_Connection is set"));
    }

    #[test]
    fn access_token_plus_auth() {
        let err = validate_auth(Some("SqlPassword"), None, "", "", Some("jwt")).unwrap_err();
        assert!(err.to_string().contains("Access Token cannot be used"));
        assert!(
            err.to_string()
                .contains("Authentication keyword is provided")
        );
    }

    #[test]
    fn access_token_plus_uid() {
        let err = validate_auth(None, None, "sa", "", Some("jwt")).unwrap_err();
        assert!(err.to_string().contains("Access Token cannot be used"));
        assert!(err.to_string().contains("User is provided"));
    }

    #[test]
    fn access_token_plus_pwd() {
        let err = validate_auth(None, None, "", "secret", Some("jwt")).unwrap_err();
        assert!(err.to_string().contains("Access Token cannot be used"));
        assert!(err.to_string().contains("Password is provided"));
    }

    #[test]
    fn access_token_multiple_conflicts() {
        let err =
            validate_auth(Some("SqlPassword"), Some(true), "sa", "pwd", Some("jwt")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Trusted_Connection is set"));
        assert!(msg.contains("Authentication keyword is provided"));
        assert!(msg.contains("User is provided"));
        assert!(msg.contains("Password is provided"));
    }

    // ---------------------------------------------------------------
    // Authentication + TC mutual exclusion
    // ---------------------------------------------------------------

    #[test]
    fn tc_plus_sqlpassword() {
        let err = validate_auth(Some("SqlPassword"), Some(true), "sa", "secret", None).unwrap_err();
        assert!(err.to_string().contains("Trusted_Connection"));
    }

    #[test]
    fn tc_plus_ad_password() {
        let err =
            validate_auth(Some("ActiveDirectoryPassword"), Some(true), "u", "p", None).unwrap_err();
        assert!(err.to_string().contains("Trusted_Connection"));
    }

    #[test]
    fn tc_plus_ad_integrated() {
        let err =
            validate_auth(Some("ActiveDirectoryIntegrated"), Some(true), "", "", None).unwrap_err();
        assert!(err.to_string().contains("Trusted_Connection"));
    }

    // ---------------------------------------------------------------
    // SqlPassword requires UID + PWD
    // ---------------------------------------------------------------

    #[test]
    fn sqlpassword_no_uid_no_pwd() {
        let err = validate_auth(Some("SqlPassword"), None, "", "", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    #[test]
    fn sqlpassword_uid_no_pwd() {
        let err = validate_auth(Some("SqlPassword"), None, "sa", "", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    #[test]
    fn sqlpassword_pwd_no_uid() {
        let err = validate_auth(Some("SqlPassword"), None, "", "secret", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    // ---------------------------------------------------------------
    // ADPassword requires UID + PWD
    // ---------------------------------------------------------------

    #[test]
    fn ad_password_no_uid_no_pwd() {
        let err = validate_auth(Some("ActiveDirectoryPassword"), None, "", "", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    #[test]
    fn ad_password_uid_no_pwd() {
        let err = validate_auth(Some("ActiveDirectoryPassword"), None, "u", "", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    #[test]
    fn ad_password_pwd_no_uid() {
        let err = validate_auth(Some("ActiveDirectoryPassword"), None, "", "p", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    // ---------------------------------------------------------------
    // ADSPA requires UID + PWD
    // ---------------------------------------------------------------

    #[test]
    fn adspa_no_uid_no_pwd() {
        let err =
            validate_auth(Some("ActiveDirectoryServicePrincipal"), None, "", "", None).unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    #[test]
    fn adspa_uid_no_pwd() {
        let err = validate_auth(
            Some("ActiveDirectoryServicePrincipal"),
            None,
            "cid",
            "",
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    #[test]
    fn adspa_pwd_no_uid() {
        let err = validate_auth(
            Some("ActiveDirectoryServicePrincipal"),
            None,
            "",
            "sec",
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Both User and Password"));
    }

    // ---------------------------------------------------------------
    // ADIntegrated with UID/PWD — validator passes, transformer clears
    // ---------------------------------------------------------------

    #[test]
    fn happy_ad_integrated_with_uid() {
        assert!(validate_auth(Some("ActiveDirectoryIntegrated"), None, "u", "", None).is_ok());
    }

    #[test]
    fn happy_ad_integrated_with_pwd() {
        assert!(validate_auth(Some("ActiveDirectoryIntegrated"), None, "", "p", None).is_ok());
    }

    #[test]
    fn happy_ad_integrated_with_uid_and_pwd() {
        assert!(validate_auth(Some("ActiveDirectoryIntegrated"), None, "u", "p", None).is_ok());
    }

    // ---------------------------------------------------------------
    // Unknown Authentication value
    // ---------------------------------------------------------------

    #[test]
    fn bogus_auth_value() {
        let err = validate_auth(Some("BogusValue"), None, "", "", None).unwrap_err();
        assert!(err.to_string().contains("Unsupported Authentication value"));
        assert!(err.to_string().contains("BogusValue"));
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::connection::client_context::TdsAuthenticationMethod;

/// Maps an ODBC connection-string `Authentication=` value to a [`TdsAuthenticationMethod`].
///
/// Case-insensitive. Returns `None` for unrecognized or empty values.
/// `SqlPassword` collapses to `Password` (same Login7 on the wire).
pub fn auth_method_from_keyword(value: &str) -> Option<TdsAuthenticationMethod> {
    match value.to_lowercase().as_str() {
        "sqlpassword" => Some(TdsAuthenticationMethod::Password),
        "activedirectoryintegrated" => Some(TdsAuthenticationMethod::ActiveDirectoryIntegrated),
        "activedirectorypassword" => Some(TdsAuthenticationMethod::ActiveDirectoryPassword),
        "activedirectoryinteractive" => Some(TdsAuthenticationMethod::ActiveDirectoryInteractive),
        "activedirectorymsi" => Some(TdsAuthenticationMethod::ActiveDirectoryMSI),
        "activedirectoryserviceprincipal" => {
            Some(TdsAuthenticationMethod::ActiveDirectoryServicePrincipal)
        }
        "activedirectorydefault" => Some(TdsAuthenticationMethod::ActiveDirectoryDefault),
        "activedirectorydevicecodeflow" => {
            Some(TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow)
        }
        "activedirectoryworkloadidentity" => {
            Some(TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity)
        }
        _ => None,
    }
}

/// Returns `true` if `value` is a recognized ODBC `Authentication=` keyword (case-insensitive).
/// An empty string is considered recognized (intentional reset).
pub fn is_recognized_keyword(value: &str) -> bool {
    value.is_empty() || auth_method_from_keyword(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognized_keywords_case_insensitive() {
        assert!(is_recognized_keyword("SqlPassword"));
        assert!(is_recognized_keyword("SQLPASSWORD"));
        assert!(is_recognized_keyword("sqlpassword"));
        assert!(is_recognized_keyword("ActiveDirectoryIntegrated"));
        assert!(is_recognized_keyword("activedirectoryintegrated"));
        assert!(is_recognized_keyword(""));
    }

    #[test]
    fn unrecognized_keyword() {
        assert!(!is_recognized_keyword("NotARealAuth"));
        assert!(!is_recognized_keyword("Sql Password"));
    }

    #[test]
    fn sqlpassword_collapses_to_password() {
        assert_eq!(
            auth_method_from_keyword("SqlPassword"),
            Some(TdsAuthenticationMethod::Password)
        );
    }

    #[test]
    fn empty_returns_none() {
        assert_eq!(auth_method_from_keyword(""), None);
    }
}

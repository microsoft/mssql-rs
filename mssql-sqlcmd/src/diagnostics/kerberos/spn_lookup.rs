// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Whether the SPN a login requested is registered in the directory: one
//! read-only search for that exact SPN, with the caller's own credentials,
//! within the time left. Windows binds to its forest's global catalog the
//! way its own tools do; elsewhere, a global catalog of the client's realm
//! is found through DNS and bound to with Kerberos (SASL GSSAPI), signed and
//! sealed. Nothing is changed, and nothing is claimed beyond what was
//! searched: a search that could not run, was refused or did not complete
//! says nothing about the SPN.

use mssql_tds::security::SecurityError;

use super::super::redact::Category;
use super::super::{Certainty, CheckId, ErrorRecord, Field, LocalizedText};

/// How much of the directory a completed search covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The forest, through a global catalog.
    Forest,
    /// One domain, through one of its domain controllers.
    #[cfg_attr(not(unix), allow(dead_code))]
    Domain,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Forest => "forest",
            Self::Domain => "domain",
        }
    }
}

/// What an account that has the SPN is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountKind {
    Computer,
    User,
    ManagedServiceAccount,
    GroupManagedServiceAccount,
    Other,
}

impl AccountKind {
    /// The kind its `objectClass` values give, the most specific first.
    pub(crate) fn of(classes: &[String]) -> Self {
        let has = |class: &str| classes.iter().any(|c| c.eq_ignore_ascii_case(class));
        if has("msDS-GroupManagedServiceAccount") {
            Self::GroupManagedServiceAccount
        } else if has("msDS-ManagedServiceAccount") {
            Self::ManagedServiceAccount
        } else if has("computer") {
            Self::Computer
        } else if has("user") {
            Self::User
        } else {
            Self::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Computer => "computer",
            Self::User => "user",
            Self::ManagedServiceAccount => "managedServiceAccount",
            Self::GroupManagedServiceAccount => "groupManagedServiceAccount",
            Self::Other => "other",
        }
    }
}

/// An account the search found with the SPN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Account {
    /// Its `sAMAccountName`.
    pub(crate) name: String,
    pub(crate) kind: AccountKind,
    /// Its `msDS-SupportedEncryptionTypes`, when the directory returned it.
    pub(crate) encryption_types: Option<u32>,
}

/// Why a lookup said nothing about the SPN.
#[derive(Clone, Debug)]
pub(crate) enum Failure {
    /// An LDAP result code (RFC 4511) or Windows LDAP error, with the
    /// server's or library's words.
    Ldap { code: u32, message: Option<String> },
    /// The Kerberos bind's own failure.
    #[cfg_attr(not(unix), allow(dead_code))]
    Security(SecurityError),
    /// The operating system's error, connecting or reading.
    #[cfg_attr(not(unix), allow(dead_code))]
    Os { code: Option<i32>, message: String },
}

/// What looking the SPN up came to.
#[derive(Clone, Debug)]
pub(crate) enum Lookup {
    /// The search completed: the accounts with the SPN, two at most, for
    /// more than one is all that matters.
    Searched {
        scope: Scope,
        accounts: Vec<Account>,
    },
    /// No directory to search: this computer is in no domain, or no domain
    /// controller of the realm was found.
    NoDirectory(Option<Failure>),
    /// No valid Kerberos credentials to bind with.
    #[cfg_attr(not(unix), allow(dead_code))]
    NoCredentials,
    /// The directory refused the bind or the search.
    Refused(Failure),
    /// The bind or the search did not complete.
    Failed(Failure),
}

/// What a lookup is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Query {
    /// The SPN as the directory holds it: `service/host[:port]`.
    pub(crate) spn: String,
    /// The client's Kerberos realm, whose directory is searched off Windows.
    pub(crate) realm: Option<String>,
    /// When the lookup must be done by.
    pub(crate) deadline: std::time::Instant,
}

/// The SPN `requested` as a directory holds it: a host-based name
/// (`MSSQLSvc@host`) as the Kerberos library turns it into a principal,
/// `MSSQLSvc/host`, and without a realm.
pub(crate) fn directory_form(requested: &str) -> String {
    if requested.contains('/') {
        requested
            .rsplit_once('@')
            .map_or(requested, |(name, _)| name)
            .to_string()
    } else {
        match requested.split_once('@') {
            Some((service, host)) => format!("{service}/{host}"),
            None => requested.to_string(),
        }
    }
}

/// The names RFC 4511 gives the LDAP result codes a lookup can end with.
fn ldap_code_name(code: u32) -> Option<&'static str> {
    Some(match code {
        1 => "operationsError",
        2 => "protocolError",
        3 => "timeLimitExceeded",
        4 => "sizeLimitExceeded",
        7 => "authMethodNotSupported",
        8 => "strongerAuthRequired",
        10 => "referral",
        11 => "adminLimitExceeded",
        13 => "confidentialityRequired",
        14 => "saslBindInProgress",
        32 => "noSuchObject",
        48 => "inappropriateAuthentication",
        49 => "invalidCredentials",
        50 => "insufficientAccessRights",
        51 => "busy",
        52 => "unavailable",
        53 => "unwillingToPerform",
        80 => "other",
        // Windows LDAP's own (winldap.h).
        0x51 => "LDAP_SERVER_DOWN",
        0x52 => "LDAP_LOCAL_ERROR",
        0x53 => "LDAP_ENCODING_ERROR",
        0x54 => "LDAP_DECODING_ERROR",
        0x55 => "LDAP_TIMEOUT",
        0x56 => "LDAP_AUTH_UNKNOWN",
        0x5B => "LDAP_CONNECT_ERROR",
        0x5C => "LDAP_NOT_SUPPORTED",
        _ => return None,
    })
}

/// Whether an LDAP result code is the directory refusing the bind or the
/// search, rather than failing to complete it.
pub(crate) fn refusal(code: u32) -> bool {
    matches!(code, 7 | 8 | 13 | 48 | 49 | 50 | 53 | 0x56)
}

impl Failure {
    /// Its code, by name where it has one.
    fn code_name(&self) -> String {
        match self {
            Self::Ldap { code, .. } => {
                ldap_code_name(*code).map_or_else(|| code.to_string(), str::to_string)
            }
            Self::Security(error) => super::SecurityCodes::of(error)
                .most_specific()
                .unwrap_or_else(|| "securityError".to_string()),
            Self::Os {
                code: Some(code), ..
            } => format!("errno {code}"),
            Self::Os { code: None, .. } => "connectionFailed".to_string(),
        }
    }

    /// Its error-chain records.
    fn records(&self, check: CheckId) -> Vec<ErrorRecord> {
        match self {
            Self::Ldap { code, message } => {
                let mut record = ErrorRecord::new("ldap", check, String::new());
                let name = ldap_code_name(*code);
                record.code = name.map(str::to_string);
                record.native_error = i32::try_from(*code).ok();
                record.message = match message.as_deref().filter(|m| !m.trim().is_empty()) {
                    Some(message) => LocalizedText::literal(message.trim()),
                    None => {
                        let code = name.map_or_else(|| code.to_string(), str::to_string);
                        LocalizedText::new(
                            "diagnose.error.ldap_result",
                            format!("LDAP error {code}"),
                            vec![("code", code)],
                        )
                    }
                };
                vec![record]
            }
            Self::Security(error) => super::security_records(
                error,
                &LocalizedText::new(
                    "diagnose.error.spn_lookup_bind_failed",
                    "The Kerberos bind to the directory failed.",
                    vec![],
                ),
                check,
            ),
            Self::Os { code, message } => {
                let mut record = ErrorRecord::new("os", check, String::new());
                record.os_error = *code;
                record.message = LocalizedText::literal(message.clone());
                vec![record]
            }
        }
    }
}

/// A lookup's result, as the connection attempt reports it.
pub(crate) struct Reported {
    pub(crate) fields: Vec<Field>,
    pub(crate) findings: Vec<(Certainty, LocalizedText)>,
    pub(crate) limitations: Vec<LocalizedText>,
    pub(crate) records: Vec<ErrorRecord>,
    /// Whether the search completed, so that the report says whether the
    /// SPN is registered within the scope searched.
    pub(crate) completed: bool,
}

/// What `lookup` of `searched`, the directory form of the SPN `requested`,
/// came to; `None` when it did not finish within the time limit.
pub(crate) fn report(
    lookup: Option<&Lookup>,
    requested: &str,
    searched: &str,
    check: CheckId,
) -> Reported {
    let mut reported = Reported {
        fields: Vec::new(),
        findings: Vec::new(),
        limitations: Vec::new(),
        records: Vec::new(),
        completed: matches!(lookup, Some(Lookup::Searched { .. })),
    };
    let not_evidence = |id, english: String, code: Option<String>| {
        LocalizedText::new(
            id,
            english,
            code.into_iter().map(|code| ("code", code)).collect(),
        )
    };
    let state = match lookup {
        None => {
            reported.limitations.push(not_evidence(
                "diagnose.limitation.spn_lookup_not_checked",
                "The SPN could not be looked up in the directory within the time limit; this is \
                 not evidence that it is missing or registered more than once."
                    .to_string(),
                None,
            ));
            "notChecked"
        }
        Some(Lookup::NoDirectory(failure)) => {
            reported.limitations.push(not_evidence(
                "diagnose.limitation.spn_lookup_no_directory",
                "No directory was found to look the SPN up in: this computer is in no domain, or \
                 no domain controller of the client's realm could be found or reached."
                    .to_string(),
                None,
            ));
            if let Some(failure) = failure {
                reported.records = failure.records(check);
            }
            "noDirectory"
        }
        Some(Lookup::NoCredentials) => {
            reported.limitations.push(not_evidence(
                "diagnose.limitation.spn_lookup_no_credentials",
                "The SPN was not looked up: the client has no valid Kerberos credentials to \
                 search the directory with."
                    .to_string(),
                None,
            ));
            "noCredentials"
        }
        Some(Lookup::Refused(failure)) => {
            let code = failure.code_name();
            reported.limitations.push(not_evidence(
                "diagnose.limitation.spn_lookup_refused",
                format!(
                    "The directory refused the SPN lookup ({code}); this is not evidence that \
                     the SPN is missing or registered more than once."
                ),
                Some(code),
            ));
            reported.records = failure.records(check);
            "refused"
        }
        Some(Lookup::Failed(failure)) => {
            let code = failure.code_name();
            reported.limitations.push(not_evidence(
                "diagnose.limitation.spn_lookup_failed",
                format!(
                    "The SPN lookup did not complete ({code}); this is not evidence that the SPN \
                     is missing or registered more than once."
                ),
                Some(code),
            ));
            reported.records = failure.records(check);
            "failed"
        }
        Some(Lookup::Searched { scope, accounts }) => {
            searched_report(&mut reported, *scope, accounts, requested, searched)
        }
    };
    reported.fields.insert(0, Field::plain("spnLookup", state));
    if requested != searched {
        reported
            .fields
            .insert(1, Field::identifier("spnSearched", searched, Category::Spn));
    }
    reported
}

fn searched_report(
    reported: &mut Reported,
    scope: Scope,
    accounts: &[Account],
    requested: &str,
    searched: &str,
) -> &'static str {
    reported
        .fields
        .push(Field::plain("spnLookupScope", scope.name()));
    for account in accounts {
        reported.fields.push(Field::identifier(
            "spnAccounts",
            account.name.clone(),
            Category::User,
        ));
    }
    let spn = searched.to_string();
    reported.limitations.push(match scope {
        Scope::Forest => LocalizedText::new(
            "diagnose.limitation.spn_lookup_forest",
            "The SPN was searched in the global catalog of the client's forest: an account in \
             a trusted forest is not searched.",
            vec![],
        ),
        Scope::Domain => LocalizedText::new(
            "diagnose.limitation.spn_lookup_domain",
            "The SPN was searched in the client's domain only: other domains of its forest are \
             not searched.",
            vec![],
        ),
    });
    match accounts {
        [] => {
            reported.findings.push((
                Certainty::Confirmed,
                match scope {
                    Scope::Forest => LocalizedText::new(
                        "diagnose.finding.spn_not_registered_forest",
                        format!("No account in the client's forest has the SPN {spn}."),
                        vec![("spn", spn)],
                    ),
                    Scope::Domain => LocalizedText::new(
                        "diagnose.finding.spn_not_registered_domain",
                        format!("No account in the client's domain has the SPN {spn}."),
                        vec![("spn", spn)],
                    ),
                },
            ));
            // A host-based name's host can be canonicalized before the KDC
            // is asked for it.
            if !requested.contains('/') {
                reported.limitations.push(LocalizedText::new(
                    "diagnose.limitation.spn_lookup_host_canonicalized",
                    "The Kerberos library can canonicalize the host name of a host-based SPN \
                     before asking the KDC for it (dns_canonicalize_hostname and rdns in \
                     krb5.conf), so the KDC may have been asked for another name than the one \
                     searched.",
                    vec![],
                ));
            }
            "notFound"
        }
        [account] => {
            reported
                .fields
                .push(Field::plain("spnAccountType", account.kind.name()));
            match account.encryption_types {
                Some(types) => {
                    let names = supported_encryption_types(types);
                    if names.is_empty() {
                        reported
                            .fields
                            .push(Field::plain("spnAccountEncryptionTypes", "notSet"));
                    }
                    for name in names {
                        reported
                            .fields
                            .push(Field::plain("spnAccountEncryptionTypes", name));
                    }
                }
                None => reported.limitations.push(match scope {
                    Scope::Forest => LocalizedText::new(
                        "diagnose.limitation.spn_account_etypes_not_in_global_catalog",
                        "A global catalog does not hold an account's supported encryption types \
                         (msDS-SupportedEncryptionTypes), so the account's are not shown.",
                        vec![],
                    ),
                    Scope::Domain => LocalizedText::new(
                        "diagnose.limitation.spn_account_etypes_not_returned",
                        "The directory did not return the account's supported encryption types \
                         (msDS-SupportedEncryptionTypes): they are not set, and the KDC uses its \
                         defaults for the account, or the client cannot read them.",
                        vec![],
                    ),
                }),
            }
            let name = account.name.clone();
            reported.findings.push((
                Certainty::Informational,
                LocalizedText::new(
                    "diagnose.finding.spn_registered_once",
                    format!("The SPN {spn} is registered on one account, {name}."),
                    vec![("spn", spn), ("account", name)],
                ),
            ));
            "found"
        }
        _ => {
            reported.findings.push((
                Certainty::Confirmed,
                match scope {
                    Scope::Forest => LocalizedText::new(
                        "diagnose.finding.spn_registered_more_than_once_forest",
                        format!(
                            "The SPN {spn} is registered on more than one account in the \
                             client's forest, so the KDC cannot tell which account's key to \
                             issue its ticket with."
                        ),
                        vec![("spn", spn)],
                    ),
                    Scope::Domain => LocalizedText::new(
                        "diagnose.finding.spn_registered_more_than_once_domain",
                        format!(
                            "The SPN {spn} is registered on more than one account in the \
                             client's domain, so the KDC cannot tell which account's key to \
                             issue its ticket with."
                        ),
                        vec![("spn", spn)],
                    ),
                },
            ));
            "multiple"
        }
    }
}

/// The Kerberos encryption types `msDS-SupportedEncryptionTypes` allows, as
/// IANA names them; its other flags are not encryption types.
fn supported_encryption_types(types: u32) -> Vec<String> {
    [(0x1, 1), (0x2, 3), (0x4, 23), (0x8, 17), (0x10, 18)]
        .into_iter()
        .filter(|(bit, _)| types & bit != 0)
        .map(|(_, etype)| super::credentials::encryption_type_name(etype))
        .collect()
}

/// RFC 4515's escaping of `value` for an LDAP filter's string form: the
/// characters it gives meaning, and NUL.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn filter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '*' => escaped.push_str("\\2a"),
            '(' => escaped.push_str("\\28"),
            ')' => escaped.push_str("\\29"),
            '\\' => escaped.push_str("\\5c"),
            '\0' => escaped.push_str("\\00"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, kind: AccountKind, encryption_types: Option<u32>) -> Account {
        Account {
            name: name.to_string(),
            kind,
            encryption_types,
        }
    }

    fn shown(reported: &Reported) -> Vec<(&str, &str)> {
        reported
            .fields
            .iter()
            .map(|f| (f.key, f.value.as_str()))
            .collect()
    }

    fn ids(texts: &[LocalizedText]) -> Vec<&'static str> {
        texts.iter().filter_map(|t| t.id).collect()
    }

    const SPN: &str = "MSSQLSvc/sql.example.local:1433";

    #[test]
    fn the_spn_is_searched_as_the_directory_holds_it() {
        assert_eq!(directory_form(SPN), SPN);
        assert_eq!(
            directory_form("MSSQLSvc/sql.example.local:1433@EXAMPLE.LOCAL"),
            SPN
        );
        assert_eq!(
            directory_form("MSSQLSvc@sql.example.local"),
            "MSSQLSvc/sql.example.local"
        );
        assert_eq!(filter_value("a*b(c)d\\e"), "a\\2ab\\28c\\29d\\5ce");
    }

    #[test]
    fn one_account_is_shown_with_its_kind_and_encryption_types() {
        let lookup = Lookup::Searched {
            scope: Scope::Forest,
            accounts: vec![account("SQL$", AccountKind::Computer, Some(0x1C | 0x20000))],
        };
        let reported = report(Some(&lookup), SPN, SPN, CheckId::ConnectionAttempt);
        assert_eq!(
            shown(&reported),
            [
                ("spnLookup", "found"),
                ("spnLookupScope", "forest"),
                ("spnAccounts", "SQL$"),
                ("spnAccountType", "computer"),
                ("spnAccountEncryptionTypes", "rc4-hmac"),
                ("spnAccountEncryptionTypes", "aes128-cts-hmac-sha1-96"),
                ("spnAccountEncryptionTypes", "aes256-cts-hmac-sha1-96"),
            ]
        );
        let named = reported
            .fields
            .iter()
            .find(|f| f.key == "spnAccounts")
            .unwrap();
        assert_eq!(named.category, Some(Category::User), "pseudonymized");
        assert_eq!(reported.findings[0].0, Certainty::Informational);
        assert_eq!(
            ids(&reported.limitations),
            ["diagnose.limitation.spn_lookup_forest"]
        );

        // Set to no encryption type, or not returned at all.
        let unset = Lookup::Searched {
            scope: Scope::Domain,
            accounts: vec![account("svc_sql", AccountKind::User, Some(0))],
        };
        let reported = report(Some(&unset), SPN, SPN, CheckId::ConnectionAttempt);
        assert!(shown(&reported).contains(&("spnAccountEncryptionTypes", "notSet")));
        let absent = Lookup::Searched {
            scope: Scope::Forest,
            accounts: vec![account("svc_sql", AccountKind::User, None)],
        };
        let reported = report(Some(&absent), SPN, SPN, CheckId::ConnectionAttempt);
        assert!(
            !shown(&reported)
                .iter()
                .any(|(key, _)| *key == "spnAccountEncryptionTypes")
        );
        assert!(
            ids(&reported.limitations)
                .contains(&"diagnose.limitation.spn_account_etypes_not_in_global_catalog")
        );
        let unread = Lookup::Searched {
            scope: Scope::Domain,
            accounts: vec![account("svc_sql", AccountKind::User, None)],
        };
        let reported = report(Some(&unread), SPN, SPN, CheckId::ConnectionAttempt);
        assert!(
            ids(&reported.limitations)
                .contains(&"diagnose.limitation.spn_account_etypes_not_returned")
        );
    }

    #[test]
    fn none_or_several_accounts_are_confirmed_within_the_scope_searched() {
        let none = Lookup::Searched {
            scope: Scope::Forest,
            accounts: Vec::new(),
        };
        let reported = report(
            Some(&none),
            "MSSQLSvc@sql.example.local",
            "MSSQLSvc/sql.example.local",
            CheckId::ConnectionAttempt,
        );
        assert_eq!(
            shown(&reported),
            [
                ("spnLookup", "notFound"),
                ("spnSearched", "MSSQLSvc/sql.example.local"),
                ("spnLookupScope", "forest"),
            ]
        );
        assert_eq!(reported.findings[0].0, Certainty::Confirmed);
        assert!(
            reported.findings[0]
                .1
                .contains("No account in the client's forest")
        );
        assert_eq!(
            ids(&reported.limitations),
            [
                "diagnose.limitation.spn_lookup_forest",
                "diagnose.limitation.spn_lookup_host_canonicalized",
            ]
        );

        let two = Lookup::Searched {
            scope: Scope::Domain,
            accounts: vec![
                account("SQL$", AccountKind::Computer, None),
                account("svc_sql", AccountKind::User, None),
            ],
        };
        let reported = report(Some(&two), SPN, SPN, CheckId::ConnectionAttempt);
        assert_eq!(
            shown(&reported),
            [
                ("spnLookup", "multiple"),
                ("spnLookupScope", "domain"),
                ("spnAccounts", "SQL$"),
                ("spnAccounts", "svc_sql"),
            ]
        );
        assert_eq!(
            reported
                .findings
                .iter()
                .map(|(c, t)| (*c, t.id))
                .collect::<Vec<_>>(),
            [(
                Certainty::Confirmed,
                Some("diagnose.finding.spn_registered_more_than_once_domain")
            )]
        );
    }

    #[test]
    fn a_lookup_that_said_nothing_claims_nothing() {
        let cases = [
            (
                None,
                "notChecked",
                "diagnose.limitation.spn_lookup_not_checked",
            ),
            (
                Some(Lookup::NoDirectory(None)),
                "noDirectory",
                "diagnose.limitation.spn_lookup_no_directory",
            ),
            (
                Some(Lookup::NoCredentials),
                "noCredentials",
                "diagnose.limitation.spn_lookup_no_credentials",
            ),
            (
                Some(Lookup::Refused(Failure::Ldap {
                    code: 8,
                    message: Some(
                        "00002028: The server requires binds to turn on integrity checking"
                            .to_string(),
                    ),
                })),
                "refused",
                "diagnose.limitation.spn_lookup_refused",
            ),
            (
                Some(Lookup::Failed(Failure::Os {
                    code: Some(111),
                    message: "Connection refused".to_string(),
                })),
                "failed",
                "diagnose.limitation.spn_lookup_failed",
            ),
        ];
        for (lookup, state, limitation) in cases {
            let reported = report(lookup.as_ref(), SPN, SPN, CheckId::ConnectionAttempt);
            assert_eq!(shown(&reported), [("spnLookup", state)]);
            assert!(reported.findings.is_empty(), "{state}");
            assert_eq!(ids(&reported.limitations), [limitation]);
        }
        let refused = Lookup::Refused(Failure::Ldap {
            code: 8,
            message: Some(" 00002028: integrity checking required\n".to_string()),
        });
        let reported = report(Some(&refused), SPN, SPN, CheckId::ConnectionAttempt);
        assert!(reported.limitations[0].contains("(strongerAuthRequired)"));
        assert_eq!(reported.records[0].source, "ldap");
        assert_eq!(
            reported.records[0].code.as_deref(),
            Some("strongerAuthRequired")
        );
        assert_eq!(reported.records[0].native_error, Some(8));
        assert!(
            reported.records[0]
                .message
                .contains("00002028: integrity checking required")
        );
    }

    #[test]
    fn account_kinds_come_from_their_most_specific_class() {
        let classes = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            AccountKind::of(&classes(&[
                "top",
                "person",
                "organizationalPerson",
                "user",
                "computer",
                "msDS-GroupManagedServiceAccount"
            ])),
            AccountKind::GroupManagedServiceAccount
        );
        assert_eq!(
            AccountKind::of(&classes(&[
                "top",
                "person",
                "organizationalPerson",
                "user",
                "computer"
            ])),
            AccountKind::Computer
        );
        assert_eq!(
            AccountKind::of(&classes(&["top", "person", "organizationalPerson", "user"])),
            AccountKind::User
        );
        assert_eq!(AccountKind::of(&[]), AccountKind::Other);
    }
}

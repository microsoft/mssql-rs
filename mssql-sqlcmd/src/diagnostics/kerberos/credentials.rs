// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The caller's own Kerberos credentials, read where its Kerberos library or
//! Windows keeps them, without asking a KDC for anything: whose they are,
//! whether a ticket-granting ticket is valid, expired or absent, and the
//! service ticket for the SPN, with the encryption types each ticket has.
//!
//! Only the caller's own: a credentials cache is the one its Kerberos library
//! uses (`KRB5CCNAME`, else the configured default), and a logon session is
//! the process's. No key is read, and no ticket is requested, renewed or
//! removed. A ticket's encryption types are its own, not every type its
//! account allows.

use std::time::{SystemTime, UNIX_EPOCH};

use super::super::redact::Category;
use super::super::{Certainty, CheckId, ErrorRecord, Field, LocalizedText};
use super::{gss_major_name, hex, krb5_error_name};

/// Where the credentials are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    /// A credentials cache (Linux, macOS).
    #[cfg_attr(not(unix), allow(dead_code))]
    Cache {
        /// Its type as the Kerberos library names it (`FILE`, `DIR`, `KCM`,
        /// `KEYRING`), when known.
        kind: Option<String>,
        /// The file it is, for a `FILE` or `DIR` cache.
        path: Option<String>,
        /// Whether `KRB5CCNAME` named it, rather than the configured default.
        named: bool,
    },
    /// The process's logon session (Windows), which keeps the tickets it
    /// gets, with what Windows said of it.
    #[cfg_attr(not(windows), allow(dead_code))]
    LogonSession(Option<Logon>),
}

/// The Windows logon session the process runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Logon {
    /// Its `SECURITY_LOGON_TYPE`.
    pub(crate) kind: u32,
    /// The package that authenticated it: `Kerberos`, `NTLM`, `CloudAP`...
    pub(crate) package: String,
    /// `DOMAIN\user`.
    pub(crate) account: String,
}

/// `SECURITY_LOGON_TYPE`'s `NewCredentials` (`runas /netonly`).
pub(crate) const NEW_CREDENTIALS_LOGON: u32 = 9;

/// What the ticket-granting ticket, or the cache it would be in, came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// A ticket-granting ticket that had not expired.
    Valid,
    /// One that had.
    Expired,
    /// The cache, or the logon session, held none.
    NoTicket,
    /// There was no cache.
    #[cfg_attr(not(unix), allow(dead_code))]
    NoCache,
    /// The cache, or the logon session's tickets, could not be read.
    Unreadable,
    /// No Kerberos library to read the cache with.
    #[cfg_attr(not(unix), allow(dead_code))]
    Unavailable,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Expired => "expired",
            Self::NoTicket | Self::NoCache => "absent",
            Self::Unreadable => "unreadable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// A ticket, without its key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ticket {
    /// When it ends, in seconds since the Unix epoch.
    pub(crate) ends: Option<i64>,
    /// Whether it had ended when it was read.
    pub(crate) expired: bool,
    /// Its own encryption type: the type of its service's key.
    pub(crate) encryption: Option<i32>,
    /// Its session key's encryption type.
    pub(crate) session_key: Option<i32>,
    /// The version of the service key it is encrypted with.
    pub(crate) key_version: Option<u32>,
}

/// A ticket as a cache or logon session lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) struct Listed {
    /// The principal it is for, `name@REALM`.
    pub(crate) client: String,
    /// Its service's principal, `service/host[:port]@REALM`.
    pub(crate) server: String,
    pub(crate) ticket: Ticket,
}

/// Why the credentials could not be read: the code, and the words of what
/// gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The Kerberos library's error code.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Krb5 { code: i32, message: Option<String> },
    /// GSSAPI's major status and the Kerberos library's error, its minor.
    #[cfg_attr(not(unix), allow(dead_code))]
    Gssapi {
        major: u32,
        minor: i32,
        message: Option<String>,
    },
    /// The NTSTATUS of the Windows LSA call that failed.
    #[cfg_attr(not(windows), allow(dead_code))]
    Lsa { status: i32, call: &'static str },
}

impl Failure {
    /// The code that says the most, as its symbolic name or in hex; a
    /// system error the Kerberos library passed on, as its `errno`.
    fn code_name(&self) -> String {
        match self {
            Self::Krb5 { code, .. } if system_error(*code) => format!("errno {code}"),
            Self::Krb5 { code, .. } => {
                krb5_error_name(*code).map_or_else(|| hex(*code), str::to_string)
            }
            Self::Gssapi { minor, .. } if *minor != 0 => {
                krb5_error_name(*minor).map_or_else(|| hex(*minor), str::to_string)
            }
            Self::Gssapi { major, .. } => {
                gss_major_name(*major).map_or_else(|| hex(*major as i32), str::to_string)
            }
            Self::Lsa { status, .. } => {
                ntstatus_name(*status).map_or_else(|| hex(*status), str::to_string)
            }
        }
    }
}

/// The NTSTATUS values a ticket or logon-session query can end with.
const NTSTATUS: &[(i32, &str)] = &[
    (0xC000_0022_u32 as i32, "STATUS_ACCESS_DENIED"),
    (0xC000_005F_u32 as i32, "STATUS_NO_SUCH_LOGON_SESSION"),
    (0xC000_009A_u32 as i32, "STATUS_INSUFFICIENT_RESOURCES"),
    (0xC000_00BB_u32 as i32, "STATUS_NOT_SUPPORTED"),
    (0xC000_00FE_u32 as i32, "STATUS_NO_SUCH_PACKAGE"),
];

fn ntstatus_name(status: i32) -> Option<&'static str> {
    NTSTATUS
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, name)| *name)
}

/// The caller's Kerberos credentials, as they were read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) store: Store,
    pub(crate) state: State,
    /// Whose they are: the cache's principal or, on Windows, the
    /// ticket-granting ticket's client.
    pub(crate) principal: Option<String>,
    pub(crate) tgt: Option<Ticket>,
    /// Whether the reader lists the tickets, rather than reading only the
    /// default credentials' principal and lifetime.
    pub(crate) lists_tickets: bool,
    /// The service ticket for the SPN, when the tickets were listed and an
    /// SPN was known: `Some(None)` when none was found.
    pub(crate) service_ticket: Option<Option<Ticket>>,
    /// Whether, with none found for a host-based SPN (`service@host`), a
    /// ticket for the same service on another host name was: the Kerberos
    /// library can canonicalize such an SPN's host name before asking.
    pub(crate) service_ticket_for_another_host: bool,
    pub(crate) failure: Option<Failure>,
}

impl Credentials {
    /// Nothing read from `store` but `state`.
    pub(crate) fn new(store: Store, state: State) -> Self {
        Self {
            store,
            state,
            principal: None,
            tgt: None,
            lists_tickets: false,
            service_ticket: None,
            service_ticket_for_another_host: false,
            failure: None,
        }
    }

    /// From the tickets listed: the ticket-granting ticket for the client's
    /// own realm, else any, and the service ticket for `spn`. Where several
    /// match, the one that ends last.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    pub(crate) fn read_tickets(&mut self, tickets: &[Listed], spn: Option<&str>) {
        let realm = self
            .principal
            .as_deref()
            .or_else(|| tickets.first().map(|t| t.client.as_str()))
            .and_then(|principal| principal.rsplit_once('@'))
            .map(|(_, realm)| realm.to_string());
        let own_realm = |t: &Listed| {
            tgs_realm(&t.server)
                .zip(realm.as_deref())
                .is_some_and(|(granted, own)| granted.eq_ignore_ascii_case(own))
        };
        let tgt =
            last(tickets, own_realm).or_else(|| last(tickets, |t| tgs_realm(&t.server).is_some()));
        match tgt {
            Some(tgt) => {
                if self.principal.is_none() {
                    self.principal = Some(tgt.client.clone());
                }
                self.tgt = Some(tgt.ticket);
                self.state = if tgt.ticket.expired {
                    State::Expired
                } else {
                    State::Valid
                };
            }
            None => self.state = State::NoTicket,
        }
        if let Some(spn) = spn {
            let found = last(tickets, |t| names_spn(&t.server, spn)).map(|t| t.ticket);
            self.service_ticket_for_another_host = found.is_none()
                && spn.split_once('@').is_some_and(|(service, host)| {
                    !service.contains('/')
                        && tickets.iter().any(|t| {
                            let name = t.server.rsplit_once('@').map_or(&*t.server, |(n, _)| n);
                            name.split_once('/').is_some_and(|(other, other_host)| {
                                other.eq_ignore_ascii_case(service)
                                    && !other_host.eq_ignore_ascii_case(host)
                            })
                        })
                });
            self.service_ticket = Some(found);
        }
    }

    /// The check's fields. `kerberosTicketCache` keeps what it reported
    /// before the credentials were read: whether a cache was `found` or
    /// `notFound`, `unverified` when it could not be read, and
    /// `notApplicable` on Windows.
    pub(crate) fn fields(&self) -> Vec<Field> {
        let mut fields = Vec::new();
        match &self.store {
            Store::Cache { kind, path, named } => {
                fields.push(Field::plain(
                    "kerberosTicketCache",
                    match self.state {
                        State::NoCache => "notFound",
                        State::Unreadable | State::Unavailable => "unverified",
                        State::Valid | State::Expired | State::NoTicket => "found",
                    },
                ));
                fields.push(Field::plain(
                    "kerberosCredentialsSource",
                    if *named { "KRB5CCNAME" } else { "defaultCache" },
                ));
                if let Some(kind) = kind {
                    fields.push(Field::plain("kerberosCacheType", kind.clone()));
                }
                if let Some(path) = path {
                    fields.push(Field::identifier(
                        "kerberosCache",
                        path.clone(),
                        Category::Path,
                    ));
                }
            }
            Store::LogonSession(logon) => {
                fields.push(Field::plain("kerberosTicketCache", "notApplicable"));
                fields.push(Field::plain("kerberosCredentialsSource", "logonSession"));
                if let Some(logon) = logon {
                    fields.push(Field::plain("logonType", logon_type_name(logon.kind)));
                    fields.push(Field::plain("logonPackage", logon.package.clone()));
                    if !logon.account.is_empty() {
                        fields.push(Field::identifier(
                            "logonAccount",
                            logon.account.clone(),
                            Category::User,
                        ));
                    }
                }
            }
        }
        fields.push(Field::plain("ticketGrantingTicket", self.state.name()));
        if let Some(principal) = &self.principal {
            fields.push(Field::identifier(
                "kerberosPrincipal",
                principal.clone(),
                Category::User,
            ));
        }
        if let Some(tgt) = &self.tgt {
            if let Some(ends) = tgt.ends {
                fields.push(Field::plain("tgtEndTime", utc_text(ends)));
            }
            encryption_fields(&mut fields, tgt, "tgtEncryptionType", "tgtSessionKeyType");
        }
        if let Some(service) = &self.service_ticket {
            fields.push(Field::plain(
                "serviceTicket",
                if service.is_some() {
                    "found"
                } else {
                    "notFound"
                },
            ));
            if let Some(ticket) = service {
                encryption_fields(
                    &mut fields,
                    ticket,
                    "serviceTicketEncryptionType",
                    "serviceTicketSessionKeyType",
                );
                if let Some(version) = ticket.key_version {
                    fields.push(Field::plain("serviceTicketKeyVersion", version.to_string()));
                }
            }
        }
        fields
    }

    /// What the credentials could not show.
    pub(crate) fn limitations(&self) -> Vec<LocalizedText> {
        let mut limitations = Vec::new();
        let logon_session = matches!(self.store, Store::LogonSession(_));
        match self.state {
            State::Unreadable => {
                let code = self
                    .failure
                    .as_ref()
                    .map_or_else(|| "unknown".to_string(), Failure::code_name);
                limitations.push(LocalizedText::new(
                    "diagnose.limitation.kerberos_credentials_unreadable",
                    format!(
                        "The Kerberos credentials could not be read ({code}), so whether a \
                         valid ticket was available is not known."
                    ),
                    vec![("code", code)],
                ));
            }
            State::Unavailable => limitations.push(LocalizedText::new(
                "diagnose.limitation.kerberos_library_unavailable",
                "The Kerberos library could not be loaded, so the credentials cache was not read.",
                vec![],
            )),
            State::Valid | State::Expired | State::NoTicket | State::NoCache => {
                if !self.lists_tickets && !logon_session {
                    limitations.push(LocalizedText::new(
                        "diagnose.limitation.kerberos_tickets_not_listed",
                        "On this platform only the default Kerberos credentials' principal and \
                         lifetime are read, not the cache's tickets: a ticket-granting ticket is \
                         not told apart from a service ticket, and encryption types are not read.",
                        vec![],
                    ));
                }
            }
        }
        if logon_session && matches!(self.state, State::NoTicket | State::Expired) {
            limitations.push(LocalizedText::new(
                "diagnose.limitation.kerberos_tickets_on_demand",
                "Windows requests Kerberos tickets when they are needed, with the logon \
                 session's credentials: a logon session without a valid ticket-granting ticket \
                 cached can still get one.",
                vec![],
            ));
        }
        if let Store::LogonSession(Some(logon)) = &self.store
            && logon.kind == NEW_CREDENTIALS_LOGON
        {
            limitations.push(LocalizedText::new(
                "diagnose.limitation.kerberos_new_credentials_logon",
                "The process runs in a NewCredentials logon session (runas /netonly): the logon \
                 account shown is the local one, and network logins use the credentials given \
                 for the network.",
                vec![],
            ));
        }
        if self.service_ticket_for_another_host {
            limitations.push(LocalizedText::new(
                "diagnose.limitation.kerberos_spn_host_canonicalized",
                "The cache holds a ticket for the SPN's service on another host name: the \
                 Kerberos library can canonicalize a host-based SPN's host name \
                 (dns_canonicalize_hostname and rdns in krb5.conf), so the SPN's service ticket \
                 may be that one.",
                vec![],
            ));
        }
        let encryption_shown = self
            .tgt
            .iter()
            .chain(self.service_ticket.iter().flatten())
            .any(|t| t.encryption.is_some() || t.session_key.is_some());
        if encryption_shown {
            limitations.push(LocalizedText::new(
                "diagnose.limitation.kerberos_etypes_of_tickets",
                "The encryption types shown are those of the tickets found; they do not show \
                 every type the account, the service or the KDC allows.",
                vec![],
            ));
        }
        limitations
    }

    /// The error-chain records of a read that failed, in the words of what
    /// failed: the Kerberos library's, or GSSAPI's then the Kerberos
    /// library's, or the Windows LSA call's.
    pub(crate) fn records(&self, check: CheckId) -> Vec<ErrorRecord> {
        let Some(failure) = &self.failure else {
            return Vec::new();
        };
        match failure {
            Failure::Krb5 { code, message } => vec![kerberos_record(check, *code, message)],
            Failure::Gssapi {
                major,
                minor,
                message,
            } => {
                let mut outer = ErrorRecord::new("gssapi", check, String::new());
                outer.message = LocalizedText::new(
                    "diagnose.error.kerberos_credentials_unreadable",
                    "The default Kerberos credentials could not be read.",
                    vec![],
                );
                outer.code = gss_major_name(*major).map(str::to_string);
                outer.native_error = Some(*major as i32);
                let mut records = vec![outer];
                if *minor != 0 {
                    records.push(kerberos_record(check, *minor, message));
                }
                records
            }
            Failure::Lsa { status, call } => {
                let mut record = ErrorRecord::new("lsa", check, String::new());
                record.message = LocalizedText::new(
                    "diagnose.error.lsa_call_failed",
                    format!("Windows did not complete {call}."),
                    vec![("call", (*call).to_string())],
                );
                record.code = ntstatus_name(*status).map(str::to_string);
                record.native_error = Some(*status);
                vec![record]
            }
        }
    }

    /// The finding when a credentials cache held no valid ticket-granting
    /// ticket, so a login had no Kerberos credentials to present; `None` for
    /// a logon session, whose tickets Windows gets when needed, or when a
    /// valid service ticket for the SPN was cached. Confirmed where the
    /// tickets are listed or there was no cache; suspected where only the
    /// default credentials were read, which cannot show a service ticket.
    pub(crate) fn missing_ticket_finding(&self) -> Option<(Certainty, LocalizedText)> {
        let Store::Cache { named, .. } = &self.store else {
            return None;
        };
        if matches!(self.service_ticket, Some(Some(ticket)) if !ticket.expired) {
            return None;
        }
        let text = match self.state {
            State::NoCache if *named => LocalizedText::new(
                "diagnose.finding.kerberos_named_cache_not_found",
                "The Kerberos credentials cache KRB5CCNAME names does not exist, so the \
                 integrated login had no Kerberos credentials to present.",
                vec![],
            ),
            State::NoCache => LocalizedText::new(
                "diagnose.finding.kerberos_cache_not_found",
                "This user has no Kerberos credentials cache, so the integrated login had no \
                 Kerberos credentials to present.",
                vec![],
            ),
            State::NoTicket => LocalizedText::new(
                "diagnose.finding.kerberos_no_tgt",
                "This user's Kerberos credentials cache holds no ticket-granting ticket, so the \
                 integrated login had no Kerberos credentials to present.",
                vec![],
            ),
            State::Expired => match self.tgt.and_then(|t| t.ends) {
                Some(ends) => {
                    let ends = utc_text(ends);
                    LocalizedText::new(
                        "diagnose.finding.kerberos_tgt_expired_at",
                        format!(
                            "The ticket-granting ticket in this user's Kerberos credentials \
                             cache expired at {ends}, so the integrated login had no valid \
                             Kerberos credentials to present."
                        ),
                        vec![("ends", ends)],
                    )
                }
                None => LocalizedText::new(
                    "diagnose.finding.kerberos_tgt_expired",
                    "The ticket-granting ticket in this user's Kerberos credentials cache had \
                     expired, so the integrated login had no valid Kerberos credentials to \
                     present.",
                    vec![],
                ),
            },
            State::Valid | State::Unreadable | State::Unavailable => return None,
        };
        let certainty = if self.lists_tickets || self.state == State::NoCache {
            Certainty::Confirmed
        } else {
            Certainty::Suspected
        };
        Some((certainty, text))
    }

    /// The finding when the service ticket for the SPN is encrypted with a
    /// type Kerberos has deprecated (RFC 6649, RFC 8429): DES, triple DES or
    /// RC4.
    pub(crate) fn deprecated_encryption_finding(&self) -> Option<LocalizedText> {
        let encryption = self
            .service_ticket
            .flatten()
            .and_then(|t| t.encryption)
            .filter(|etype| matches!(etype, 1 | 2 | 3 | 16 | 23 | 24))?;
        let encryption = encryption_type_name(encryption);
        Some(LocalizedText::new(
            "diagnose.finding.kerberos_service_ticket_deprecated_etype",
            format!(
                "The service ticket for the SPN is encrypted with {encryption}, an encryption \
                 type Kerberos has deprecated."
            ),
            vec![("encryption", encryption)],
        ))
    }
}

/// The fields when the credentials could not be read within the time
/// limit: a logon session's (Windows) or a cache's.
pub(crate) fn not_checked_fields(logon_session: bool) -> Vec<Field> {
    vec![
        Field::plain(
            "kerberosTicketCache",
            if logon_session {
                "notApplicable"
            } else {
                "notChecked"
            },
        ),
        Field::plain("ticketGrantingTicket", "notChecked"),
    ]
}

/// The limitation when the credentials could not be read within the time
/// limit.
pub(crate) fn not_checked_limitation() -> LocalizedText {
    LocalizedText::new(
        "diagnose.limitation.kerberos_cache_not_checked",
        "The Kerberos credentials could not be read within the time limit.",
        vec![],
    )
}

/// The ticket among `tickets` that `matching` takes and that ends last.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn last(tickets: &[Listed], matching: impl Fn(&Listed) -> bool) -> Option<&Listed> {
    tickets
        .iter()
        .filter(|t| matching(t))
        .max_by_key(|t| t.ticket.ends.unwrap_or(i64::MIN))
}

fn encryption_fields(
    fields: &mut Vec<Field>,
    ticket: &Ticket,
    encryption: &'static str,
    session_key: &'static str,
) {
    if let Some(etype) = ticket.encryption {
        fields.push(Field::plain(encryption, encryption_type_name(etype)));
    }
    if let Some(etype) = ticket.session_key {
        fields.push(Field::plain(session_key, encryption_type_name(etype)));
    }
}

fn kerberos_record(check: CheckId, code: i32, message: &Option<String>) -> ErrorRecord {
    let mut record = ErrorRecord::new("kerberos", check, String::new());
    if system_error(code) {
        record.os_error = Some(code);
    } else {
        record.code = krb5_error_name(code).map(str::to_string);
        record.native_error = Some(code);
    }
    record.message = match message.as_deref().filter(|m| !m.is_empty()) {
        Some(message) => LocalizedText::literal(message),
        None => {
            let code = krb5_error_name(code).map_or_else(|| hex(code), str::to_string);
            LocalizedText::new(
                "diagnose.error.kerberos_status",
                format!("Kerberos error {code}"),
                vec![("code", code)],
            )
        }
    };
    record
}

/// Whether a Kerberos library code is a system error (`errno`) it passed
/// on, not one of its own error tables, whose codes are far from zero.
fn system_error(code: i32) -> bool {
    (1..0x1_0000).contains(&code)
}

/// Whether a Kerberos library code from reading a cache's principal means
/// there is no cache.
#[cfg(unix)]
pub(crate) fn no_cache(code: i32) -> bool {
    matches!(code, super::KRB5_FCC_NOFILE | super::KRB5_CC_NOTFOUND)
}

/// The realm a ticket-granting service principal (`krbtgt/REALM@...`)
/// grants tickets for.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn tgs_realm(server: &str) -> Option<&str> {
    let name = server.rsplit_once('@').map_or(server, |(name, _)| name);
    let (service, realm) = name.split_once('/')?;
    (service.eq_ignore_ascii_case("krbtgt") && !realm.is_empty()).then_some(realm)
}

/// Whether `server`, a ticket's service principal, is the one `spn` names:
/// `service/host[:port]` as it is, or a host-based `service@host` as the
/// Kerberos library turns it into a principal, `service/host`. Realms are
/// ignored, and case.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) fn names_spn(server: &str, spn: &str) -> bool {
    let server = server.rsplit_once('@').map_or(server, |(name, _)| name);
    let spn = if spn.contains('/') {
        spn.rsplit_once('@')
            .map_or(spn, |(name, _)| name)
            .to_string()
    } else {
        match spn.split_once('@') {
            Some((service, host)) => format!("{service}/{host}"),
            None => spn.to_string(),
        }
    };
    server.eq_ignore_ascii_case(&spn)
}

/// A Kerberos encryption type as IANA names it, else its number.
pub(crate) fn encryption_type_name(etype: i32) -> String {
    match etype {
        1 => "des-cbc-crc",
        2 => "des-cbc-md4",
        3 => "des-cbc-md5",
        16 => "des3-cbc-sha1-kd",
        17 => "aes128-cts-hmac-sha1-96",
        18 => "aes256-cts-hmac-sha1-96",
        19 => "aes128-cts-hmac-sha256-128",
        20 => "aes256-cts-hmac-sha384-192",
        23 => "rc4-hmac",
        24 => "rc4-hmac-exp",
        25 => "camellia128-cts-cmac",
        26 => "camellia256-cts-cmac",
        other => return other.to_string(),
    }
    .to_string()
}

/// A `SECURITY_LOGON_TYPE`'s name, else its number.
fn logon_type_name(kind: u32) -> String {
    match kind {
        2 => "interactive",
        3 => "network",
        4 => "batch",
        5 => "service",
        6 => "proxy",
        7 => "unlock",
        8 => "networkCleartext",
        NEW_CREDENTIALS_LOGON => "newCredentials",
        10 => "remoteInteractive",
        11 => "cachedInteractive",
        12 => "cachedRemoteInteractive",
        13 => "cachedUnlock",
        other => return other.to_string(),
    }
    .to_string()
}

/// The time now, in seconds since the Unix epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `seconds` since the Unix epoch as an RFC 3339 UTC time,
/// `2026-11-20T08:15:00Z`.
pub(crate) fn utc_text(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Days to a civil date: Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        time / 60 % 60,
        time % 60
    )
}

/// A Kerberos ticket's own encryption type and the version of the service
/// key it is encrypted with, from its DER encoding (RFC 4120 5.3):
/// `Ticket ::= [APPLICATION 1] SEQUENCE { tkt-vno [0], realm [1], sname [2],
/// enc-part [3] EncryptedData }` and `EncryptedData ::= SEQUENCE { etype [0]
/// Int32, kvno [1] UInt32 OPTIONAL, cipher [2] OCTET STRING }`. The cipher
/// is not read.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn ticket_encryption(der: &[u8]) -> Option<(i32, Option<u32>)> {
    let (ticket, _) = element(der, 0x61)?;
    let (fields, _) = element(ticket, 0x30)?;
    let (encrypted, _) = element(field(fields, 0xA3)?, 0x30)?;
    let etype = i32::try_from(integer(field(encrypted, 0xA0)?)?).ok()?;
    let key_version = field(encrypted, 0xA1)
        .and_then(integer)
        .and_then(|v| u32::try_from(v).ok());
    Some((etype, key_version))
}

/// The content of the DER element at the start of `input` when its tag is
/// `tag`, and what follows the element.
fn element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&found, rest) = input.split_first()?;
    if found != tag {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        let length = bytes
            .iter()
            .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
        (length, rest)
    };
    (rest.len() >= length).then(|| rest.split_at(length))
}

/// The content of the context-specific field `tag` among a SEQUENCE's.
fn field(mut fields: &[u8], tag: u8) -> Option<&[u8]> {
    while let Some(&found) = fields.first() {
        let (content, rest) = element(fields, found)?;
        if found == tag {
            return Some(content);
        }
        fields = rest;
    }
    None
}

/// The DER INTEGER at the start of `input`, when it fits 64 bits.
fn integer(input: &[u8]) -> Option<i64> {
    let (bytes, _) = element(input, 0x02)?;
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let sign = if bytes[0] & 0x80 == 0 { 0 } else { -1 };
    Some(
        bytes
            .iter()
            .fold(sign, |value: i64, byte| (value << 8) | i64::from(*byte)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if content.len() < 0x80 {
            out.push(content.len() as u8);
        } else {
            let length = (content.len() as u32).to_be_bytes();
            let skip = length.iter().take_while(|b| **b == 0).count();
            out.push(0x80 | (4 - skip) as u8);
            out.extend_from_slice(&length[skip..]);
        }
        out.extend_from_slice(content);
        out
    }

    fn int(value: i64) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        let mut start = 0;
        while start < 7
            && ((bytes[start] == 0 && bytes[start + 1] & 0x80 == 0)
                || (bytes[start] == 0xFF && bytes[start + 1] & 0x80 != 0))
        {
            start += 1;
        }
        der(0x02, &bytes[start..])
    }

    /// A ticket as a KDC encodes it, with a cipher of `cipher_len` bytes.
    fn ticket(etype: i64, kvno: Option<i64>, cipher_len: usize) -> Vec<u8> {
        let mut encrypted = der(0xA0, &int(etype));
        if let Some(kvno) = kvno {
            encrypted.extend(der(0xA1, &int(kvno)));
        }
        encrypted.extend(der(0xA2, &der(0x04, &vec![0x5A; cipher_len])));
        let mut fields = der(0xA0, &int(5));
        fields.extend(der(0xA1, &der(0x1B, b"EXAMPLE.LOCAL")));
        fields.extend(der(0xA2, &der(0x30, b"")));
        fields.extend(der(0xA3, &der(0x30, &encrypted)));
        der(0x61, &der(0x30, &fields))
    }

    #[test]
    fn a_tickets_encryption_type_and_key_version_are_read_from_its_encoding() {
        assert_eq!(
            ticket_encryption(&ticket(18, Some(2), 16)),
            Some((18, Some(2)))
        );
        assert_eq!(ticket_encryption(&ticket(23, None, 16)), Some((23, None)));
        // A cipher long enough for a multi-byte length, and a key version
        // that UInt32 encodes with a leading zero.
        assert_eq!(
            ticket_encryption(&ticket(17, Some(0xFFFF_FFFF), 300)),
            Some((17, Some(0xFFFF_FFFF)))
        );
        // Windows numbers some types below zero.
        assert_eq!(
            ticket_encryption(&ticket(-128, None, 8)),
            Some((-128, None))
        );
    }

    #[test]
    fn a_malformed_ticket_has_no_encryption_type() {
        let good = ticket(18, Some(2), 16);
        for end in 0..good.len() {
            assert_eq!(ticket_encryption(&good[..end]), None, "cut at {end}");
        }
        assert_eq!(ticket_encryption(&der(0x30, &good)), None);
        assert_eq!(
            ticket_encryption(&[0x61, 0x84, 0xFF, 0xFF, 0xFF, 0xFF]),
            None
        );
    }

    #[test]
    fn times_are_shown_in_utc() {
        assert_eq!(utc_text(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_text(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_text(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(utc_text(2_147_483_648), "2038-01-19T03:14:08Z");
        assert_eq!(utc_text(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn a_service_ticket_is_matched_to_the_spn_in_either_form() {
        let server = "MSSQLSvc/sql.example.local:1433@EXAMPLE.LOCAL";
        assert!(names_spn(server, "MSSQLSvc/sql.example.local:1433"));
        assert!(names_spn(
            server,
            "mssqlsvc/SQL.example.local:1433@EXAMPLE.LOCAL"
        ));
        assert!(!names_spn(server, "MSSQLSvc/sql.example.local:1434"));
        assert!(!names_spn(server, "MSSQLSvc@sql.example.local"));
        // A host-based name becomes `service/host`, without a port.
        let host_based = "MSSQLSvc/sql.example.local@EXAMPLE.LOCAL";
        assert!(names_spn(host_based, "MSSQLSvc@sql.example.local"));
        assert!(!names_spn(host_based, "MSSQLSvc@sql"));
    }

    fn listed(server: &str, ends: i64, expired: bool, etype: i32) -> Listed {
        Listed {
            client: "testuser@EXAMPLE.LOCAL".to_string(),
            server: server.to_string(),
            ticket: Ticket {
                ends: Some(ends),
                expired,
                encryption: Some(etype),
                session_key: Some(etype),
                key_version: None,
            },
        }
    }

    /// A FILE cache as MIT krb5's library reads it, listing its tickets.
    fn cache(named: bool) -> Credentials {
        let mut credentials = Credentials::new(
            Store::Cache {
                kind: Some("FILE".to_string()),
                path: Some("/tmp/krb5cc_1000".to_string()),
                named,
            },
            State::NoTicket,
        );
        credentials.lists_tickets = true;
        credentials
    }

    #[test]
    fn the_tgt_is_the_latest_for_the_clients_own_realm() {
        let mut credentials = cache(false);
        credentials.principal = Some("testuser@EXAMPLE.LOCAL".to_string());
        credentials.read_tickets(
            &[
                listed("krbtgt/OTHER.LOCAL@EXAMPLE.LOCAL", 300, false, 23),
                listed("krbtgt/EXAMPLE.LOCAL@EXAMPLE.LOCAL", 100, true, 18),
                listed("krbtgt/EXAMPLE.LOCAL@EXAMPLE.LOCAL", 200, false, 17),
                listed("MSSQLSvc/sql.example.local@EXAMPLE.LOCAL", 150, false, 18),
            ],
            Some("MSSQLSvc@sql.example.local"),
        );
        assert_eq!(credentials.state, State::Valid);
        assert_eq!(credentials.tgt.and_then(|t| t.encryption), Some(17));
        assert_eq!(
            credentials.service_ticket.flatten().and_then(|t| t.ends),
            Some(150)
        );

        let mut expired = cache(false);
        expired.read_tickets(
            &[listed("krbtgt/EXAMPLE.LOCAL@EXAMPLE.LOCAL", 100, true, 18)],
            None,
        );
        assert_eq!(expired.state, State::Expired);
        assert_eq!(
            expired.principal.as_deref(),
            Some("testuser@EXAMPLE.LOCAL"),
            "without a cache principal, the TGT's client"
        );
        assert_eq!(expired.service_ticket, None, "no SPN, no service ticket");

        let mut none = cache(false);
        none.read_tickets(
            &[listed("MSSQLSvc/sql:1433@EXAMPLE.LOCAL", 150, false, 18)],
            Some("MSSQLSvc/sql:1433"),
        );
        assert_eq!(none.state, State::NoTicket);
        assert!(none.service_ticket.flatten().is_some());
    }

    fn value<'a>(fields: &'a [Field], key: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }

    #[test]
    fn a_caches_fields_keep_the_ticket_cache_state_it_had() {
        for (state, cache_state, credentials) in [
            (State::Valid, "found", "valid"),
            (State::Expired, "found", "expired"),
            (State::NoTicket, "found", "absent"),
            (State::NoCache, "notFound", "absent"),
            (State::Unreadable, "unverified", "unreadable"),
            (State::Unavailable, "unverified", "unavailable"),
        ] {
            let mut read = cache(false);
            read.state = state;
            let fields = read.fields();
            assert_eq!(
                value(&fields, "kerberosTicketCache"),
                Some(cache_state),
                "{state:?}"
            );
            assert_eq!(value(&fields, "ticketGrantingTicket"), Some(credentials));
            assert_eq!(
                value(&fields, "kerberosCredentialsSource"),
                Some("defaultCache")
            );
            assert_eq!(value(&fields, "kerberosCacheType"), Some("FILE"));
        }
        let fields = cache(true).fields();
        assert_eq!(
            value(&fields, "kerberosCredentialsSource"),
            Some("KRB5CCNAME")
        );
        let path = fields.iter().find(|f| f.key == "kerberosCache").unwrap();
        assert_eq!(path.category, Some(Category::Path));
    }

    #[test]
    fn a_logon_sessions_fields_say_what_logged_it_on() {
        let mut read = Credentials::new(
            Store::LogonSession(Some(Logon {
                kind: 2,
                package: "Kerberos".to_string(),
                account: r"CONTOSO\alice".to_string(),
            })),
            State::Valid,
        );
        read.principal = Some("alice@CONTOSO.COM".to_string());
        read.tgt = Some(Ticket {
            ends: Some(1_700_000_000),
            expired: false,
            encryption: Some(18),
            session_key: Some(18),
            key_version: None,
        });
        read.service_ticket = Some(Some(Ticket {
            encryption: Some(23),
            session_key: Some(18),
            key_version: Some(4),
            ..Ticket::default()
        }));
        let fields = read.fields();
        let shown: Vec<(&str, &str)> = fields.iter().map(|f| (f.key, f.value.as_str())).collect();
        assert_eq!(
            shown,
            [
                ("kerberosTicketCache", "notApplicable"),
                ("kerberosCredentialsSource", "logonSession"),
                ("logonType", "interactive"),
                ("logonPackage", "Kerberos"),
                ("logonAccount", r"CONTOSO\alice"),
                ("ticketGrantingTicket", "valid"),
                ("kerberosPrincipal", "alice@CONTOSO.COM"),
                ("tgtEndTime", "2023-11-14T22:13:20Z"),
                ("tgtEncryptionType", "aes256-cts-hmac-sha1-96"),
                ("tgtSessionKeyType", "aes256-cts-hmac-sha1-96"),
                ("serviceTicket", "found"),
                ("serviceTicketEncryptionType", "rc4-hmac"),
                ("serviceTicketSessionKeyType", "aes256-cts-hmac-sha1-96"),
                ("serviceTicketKeyVersion", "4"),
            ]
        );
        for key in ["logonAccount", "kerberosPrincipal"] {
            let field = fields.iter().find(|f| f.key == key).unwrap();
            assert_eq!(field.category, Some(Category::User), "{key}");
        }
    }

    fn ids(texts: &[LocalizedText]) -> Vec<&'static str> {
        texts.iter().filter_map(|t| t.id).collect()
    }

    #[test]
    fn what_could_not_be_read_is_a_limitation() {
        let mut unreadable = cache(false);
        unreadable.state = State::Unreadable;
        unreadable.failure = Some(Failure::Krb5 {
            code: super::super::KRB5_FCC_PERM,
            message: None,
        });
        let limitations = unreadable.limitations();
        assert_eq!(
            ids(&limitations),
            ["diagnose.limitation.kerberos_credentials_unreadable"]
        );
        assert!(limitations[0].contains("(KRB5_FCC_PERM)"));

        let mut missing = cache(false);
        missing.state = State::Unavailable;
        assert_eq!(
            ids(&missing.limitations()),
            ["diagnose.limitation.kerberos_library_unavailable"]
        );

        let mut unlisted = cache(false);
        unlisted.state = State::Valid;
        unlisted.lists_tickets = false;
        assert_eq!(
            ids(&unlisted.limitations()),
            ["diagnose.limitation.kerberos_tickets_not_listed"]
        );

        let mut windows = Credentials::new(
            Store::LogonSession(Some(Logon {
                kind: NEW_CREDENTIALS_LOGON,
                package: "Negotiate".to_string(),
                account: String::new(),
            })),
            State::NoTicket,
        );
        windows.lists_tickets = true;
        assert_eq!(
            ids(&windows.limitations()),
            [
                "diagnose.limitation.kerberos_tickets_on_demand",
                "diagnose.limitation.kerberos_new_credentials_logon",
            ]
        );
    }

    #[test]
    fn encryption_types_and_canonicalized_hosts_carry_their_limits() {
        let tgt = listed("krbtgt/EXAMPLE.LOCAL@EXAMPLE.LOCAL", 200, false, 18);
        let mut read = cache(false);
        read.read_tickets(std::slice::from_ref(&tgt), Some("MSSQLSvc@sql"));
        assert_eq!(read.service_ticket, Some(None));
        assert_eq!(
            ids(&read.limitations()),
            ["diagnose.limitation.kerberos_etypes_of_tickets"],
            "no ticket for the service at all: nothing was canonicalized"
        );
        // The library asked for the host's canonical name.
        let canonical = listed("MSSQLSvc/sql.example.local@EXAMPLE.LOCAL", 200, false, 18);
        read.read_tickets(&[tgt, canonical], Some("MSSQLSvc@sql"));
        assert_eq!(read.service_ticket, Some(None));
        assert_eq!(
            ids(&read.limitations()),
            [
                "diagnose.limitation.kerberos_spn_host_canonicalized",
                "diagnose.limitation.kerberos_etypes_of_tickets",
            ]
        );
        let mut ported = cache(false);
        ported.read_tickets(&[], Some("MSSQLSvc/sql:1433"));
        assert!(ported.limitations().is_empty());
    }

    #[test]
    fn a_cache_without_a_valid_tgt_is_a_finding_a_logon_session_is_not() {
        let finding = |state: State, named: bool, listed: bool| {
            let mut read = cache(named);
            read.state = state;
            read.lists_tickets = listed;
            read.missing_ticket_finding()
                .map(|(certainty, text)| (certainty, text.id.unwrap()))
        };
        assert_eq!(
            finding(State::NoCache, true, true),
            Some((
                Certainty::Confirmed,
                "diagnose.finding.kerberos_named_cache_not_found"
            ))
        );
        assert_eq!(
            finding(State::NoCache, false, true),
            Some((
                Certainty::Confirmed,
                "diagnose.finding.kerberos_cache_not_found"
            ))
        );
        assert_eq!(
            finding(State::NoTicket, false, false),
            Some((Certainty::Suspected, "diagnose.finding.kerberos_no_tgt")),
            "a service ticket cannot be ruled out where none were listed"
        );
        assert_eq!(
            finding(State::NoCache, false, false),
            Some((
                Certainty::Confirmed,
                "diagnose.finding.kerberos_cache_not_found"
            )),
            "no cache holds no service ticket either"
        );
        assert_eq!(
            finding(State::Expired, false, true),
            Some((
                Certainty::Confirmed,
                "diagnose.finding.kerberos_tgt_expired"
            ))
        );
        for state in [State::Valid, State::Unreadable, State::Unavailable] {
            assert_eq!(finding(state, false, true), None, "{state:?}");
        }

        let mut expired = cache(false);
        expired.read_tickets(
            &[listed(
                "krbtgt/EXAMPLE.LOCAL@EXAMPLE.LOCAL",
                1_700_000_000,
                true,
                18,
            )],
            None,
        );
        let (_, text) = expired.missing_ticket_finding().unwrap();
        assert!(text.contains("expired at 2023-11-14T22:13:20Z"), "{text:?}");

        // A valid service ticket for the SPN serves without a TGT.
        let mut served = cache(false);
        served.read_tickets(
            &[listed("MSSQLSvc/sql:1433@EXAMPLE.LOCAL", 200, false, 18)],
            Some("MSSQLSvc/sql:1433"),
        );
        assert_eq!(served.missing_ticket_finding(), None);

        let windows = Credentials::new(Store::LogonSession(None), State::NoTicket);
        assert_eq!(windows.missing_ticket_finding(), None);
    }

    #[test]
    fn a_service_ticket_with_a_deprecated_type_is_a_finding() {
        let with = |etype: i32| {
            let mut read = cache(false);
            read.read_tickets(
                &[listed("MSSQLSvc/sql:1433@EXAMPLE.LOCAL", 200, false, etype)],
                Some("MSSQLSvc/sql:1433"),
            );
            read.deprecated_encryption_finding()
        };
        let rc4 = with(23).unwrap();
        assert!(rc4.contains("encrypted with rc4-hmac"), "{rc4:?}");
        assert!(with(3).is_some());
        assert_eq!(with(18), None);
        assert_eq!(with(17), None);
    }

    #[test]
    fn a_failed_read_keeps_its_codes() {
        let mut read = cache(false);
        read.state = State::Unreadable;
        read.failure = Some(Failure::Krb5 {
            code: super::super::KRB5_FCC_PERM,
            message: Some(
                "Credentials cache permissions incorrect (filename: /tmp/cc)".to_string(),
            ),
        });
        let records = read.records(CheckId::ConnectionAttempt);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, "kerberos");
        assert_eq!(records[0].code.as_deref(), Some("KRB5_FCC_PERM"));
        assert_eq!(records[0].native_error, Some(super::super::KRB5_FCC_PERM));
        assert!(records[0].message.contains("(filename: /tmp/cc)"));

        // A system error the library passed on is the operating system's.
        read.failure = Some(Failure::Krb5 {
            code: 1,
            message: Some("Operation not permitted".to_string()),
        });
        let records = read.records(CheckId::ConnectionAttempt);
        assert_eq!(
            (
                records[0].os_error,
                records[0].native_error,
                records[0].code.as_deref()
            ),
            (Some(1), None, None)
        );
        assert!(read.limitations()[0].contains("(errno 1)"));

        read.failure = Some(Failure::Gssapi {
            major: 7 << 16,
            minor: super::super::KRB5_FCC_NOFILE,
            message: None,
        });
        let codes: Vec<_> = read
            .records(CheckId::ConnectionAttempt)
            .iter()
            .map(|r| (r.source, r.code.clone()))
            .collect();
        assert_eq!(
            codes,
            [
                ("gssapi", Some("GSS_S_NO_CRED".to_string())),
                ("kerberos", Some("KRB5_FCC_NOFILE".to_string()))
            ]
        );

        read.failure = Some(Failure::Lsa {
            status: 0xC000_005F_u32 as i32,
            call: "LsaCallAuthenticationPackage",
        });
        let records = read.records(CheckId::ConnectionAttempt);
        assert_eq!(records[0].source, "lsa");
        assert_eq!(
            records[0].code.as_deref(),
            Some("STATUS_NO_SUCH_LOGON_SESSION")
        );
        assert!(read.limitations()[0].contains("(STATUS_NO_SUCH_LOGON_SESSION)"));

        assert!(cache(false).records(CheckId::ConnectionAttempt).is_empty());
    }
}

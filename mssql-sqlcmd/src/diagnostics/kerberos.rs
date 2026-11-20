// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Kerberos and SSPI evidence for integrated authentication (`-E`): what a
//! security package's failure codes establish, and what the login's security
//! context reported negotiating.
//!
//! The codes are kept as numbers and named here on every platform, so a
//! report means the same wherever it is read: Windows SSPI's
//! `SECURITY_STATUS` (`winerror.h`), GSSAPI's major status (RFC 2744), and
//! the Kerberos library's error code that GSSAPI returns as its minor status
//! (MIT krb5's `krb5.h`). A code establishes only what it says: a generic
//! failure, a key mismatch or SQL Server's 18452 do not establish one cause.

use mssql_tds::connection::connect_stage;
use mssql_tds::security::SecurityError;

use super::stages::Step;
use super::{CheckId, Coverage, Domain, ErrorRecord, LocalizedText};

#[cfg_attr(not(unix), allow(dead_code))]
mod ber;
#[cfg(unix)]
mod ccache;
mod credentials;
#[cfg(unix)]
mod ldap;
#[cfg(windows)]
mod lsa;
mod spn_lookup;
#[cfg(windows)]
mod wldap;

pub(super) use credentials::{Credentials, not_checked_fields, not_checked_limitation};
#[cfg(test)]
pub(super) use credentials::{Failure, State, Store, Ticket};
#[cfg(test)]
pub(super) use spn_lookup::{Account, AccountKind, Failure as SpnFailure, Scope};
pub(super) use spn_lookup::{
    Lookup as SpnLookup, Query as SpnQuery, Reported as SpnReported, directory_form,
    report as spn_report,
};

/// How the SPN is looked up in the directory on this platform.
pub(super) fn spn_directory() -> Option<fn(SpnQuery) -> SpnLookup> {
    #[cfg(windows)]
    return Some(wldap::lookup);
    #[cfg(unix)]
    return Some(ldap::lookup);
    #[cfg(not(any(windows, unix)))]
    None
}

/// How the caller's own Kerberos credentials are read; tests read their own.
#[derive(Clone, Copy)]
pub(super) struct CredentialsLookup {
    /// Whether they are a logon session's (Windows), not a cache's.
    pub(super) logon_session: bool,
    /// Reads them, finding the service ticket for the SPN given.
    pub(super) read: fn(Option<String>) -> Credentials,
}

impl CredentialsLookup {
    /// This platform's.
    pub(super) fn platform() -> Option<Self> {
        #[cfg(windows)]
        return Some(Self {
            logon_session: true,
            read: lsa::read,
        });
        #[cfg(unix)]
        return Some(Self {
            logon_session: false,
            read: ccache::read,
        });
        #[cfg(not(any(windows, unix)))]
        None
    }
}

/// The specialist domain of a Kerberos signal.
const KERBEROS_DOMAIN: &str = "security.authentication.kerberos";

// SECURITY_STATUS values, as `winerror.h` defines them.
const SEC_E_TARGET_UNKNOWN: i32 = 0x8009_0303_u32 as i32;
const SEC_E_NO_CREDENTIALS: i32 = 0x8009_030E_u32 as i32;
const SEC_E_NO_AUTHENTICATING_AUTHORITY: i32 = 0x8009_0311_u32 as i32;
const SEC_E_WRONG_PRINCIPAL: i32 = 0x8009_0322_u32 as i32;
const SEC_E_TIME_SKEW: i32 = 0x8009_0324_u32 as i32;
const SEC_E_KDC_UNKNOWN_ETYPE: i32 = 0x8009_0342_u32 as i32;

/// The SECURITY_STATUS values a failed integrated login can end with.
const SSPI_STATUS: &[(i32, &str)] = &[
    (0x8009_0300_u32 as i32, "SEC_E_INSUFFICIENT_MEMORY"),
    (0x8009_0301_u32 as i32, "SEC_E_INVALID_HANDLE"),
    (0x8009_0302_u32 as i32, "SEC_E_UNSUPPORTED_FUNCTION"),
    (SEC_E_TARGET_UNKNOWN, "SEC_E_TARGET_UNKNOWN"),
    (0x8009_0304_u32 as i32, "SEC_E_INTERNAL_ERROR"),
    (0x8009_0305_u32 as i32, "SEC_E_SECPKG_NOT_FOUND"),
    (0x8009_0308_u32 as i32, "SEC_E_INVALID_TOKEN"),
    (0x8009_030C_u32 as i32, "SEC_E_LOGON_DENIED"),
    (0x8009_030D_u32 as i32, "SEC_E_UNKNOWN_CREDENTIALS"),
    (SEC_E_NO_CREDENTIALS, "SEC_E_NO_CREDENTIALS"),
    (0x8009_030F_u32 as i32, "SEC_E_MESSAGE_ALTERED"),
    (0x8009_0310_u32 as i32, "SEC_E_OUT_OF_SEQUENCE"),
    (
        SEC_E_NO_AUTHENTICATING_AUTHORITY,
        "SEC_E_NO_AUTHENTICATING_AUTHORITY",
    ),
    (0x8009_0317_u32 as i32, "SEC_E_CONTEXT_EXPIRED"),
    (0x8009_0320_u32 as i32, "SEC_E_INCOMPLETE_CREDENTIALS"),
    (SEC_E_WRONG_PRINCIPAL, "SEC_E_WRONG_PRINCIPAL"),
    (SEC_E_TIME_SKEW, "SEC_E_TIME_SKEW"),
    (0x8009_0331_u32 as i32, "SEC_E_ALGORITHM_MISMATCH"),
    (0x8009_0334_u32 as i32, "SEC_E_NO_TGT_REPLY"),
    (0x8009_0337_u32 as i32, "SEC_E_CRYPTO_SYSTEM_INVALID"),
    (0x8009_0338_u32 as i32, "SEC_E_MAX_REFERRALS_EXCEEDED"),
    (0x8009_033A_u32 as i32, "SEC_E_STRONG_CRYPTO_NOT_SUPPORTED"),
    (0x8009_033B_u32 as i32, "SEC_E_TOO_MANY_PRINCIPALS"),
    (0x8009_0340_u32 as i32, "SEC_E_KDC_INVALID_REQUEST"),
    (0x8009_0341_u32 as i32, "SEC_E_KDC_UNABLE_TO_REFER"),
    (SEC_E_KDC_UNKNOWN_ETYPE, "SEC_E_KDC_UNKNOWN_ETYPE"),
    (0x8009_0346_u32 as i32, "SEC_E_BAD_BINDINGS"),
    (0x8009_0347_u32 as i32, "SEC_E_MULTIPLE_ACCOUNTS"),
    (0x8009_0348_u32 as i32, "SEC_E_NO_KERB_KEY"),
    (0x8009_0350_u32 as i32, "SEC_E_DOWNGRADE_DETECTED"),
    (0x8009_035E_u32 as i32, "SEC_E_DELEGATION_POLICY"),
    (0x8009_035F_u32 as i32, "SEC_E_POLICY_NLTM_ONLY"),
];

// MIT krb5 error codes (com_err table `krb5`, and `k5e1` for KCM), as
// `krb5.h` defines them.
const KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN: i32 = -1_765_328_377;
const KRB5KDC_ERR_PRINCIPAL_NOT_UNIQUE: i32 = -1_765_328_376;
const KRB5KDC_ERR_ETYPE_NOSUPP: i32 = -1_765_328_370;
const KRB5KRB_AP_ERR_BAD_INTEGRITY: i32 = -1_765_328_353;
const KRB5KRB_AP_ERR_TKT_EXPIRED: i32 = -1_765_328_352;
const KRB5KRB_AP_ERR_SKEW: i32 = -1_765_328_347;
const KRB5KRB_AP_ERR_MODIFIED: i32 = -1_765_328_343;
const KRB5KRB_AP_ERR_BADKEYVER: i32 = -1_765_328_340;
const KRB5KRB_AP_ERR_NOKEY: i32 = -1_765_328_339;
const KRB5KRB_AP_ERR_MUT_FAIL: i32 = -1_765_328_338;
const KRB5_CC_BADNAME: i32 = -1_765_328_245;
const KRB5_CC_UNKNOWN_TYPE: i32 = -1_765_328_244;
const KRB5_CC_NOTFOUND: i32 = -1_765_328_243;
const KRB5_KDCREP_SKEW: i32 = -1_765_328_236;
const KRB5_PROG_ETYPE_NOSUPP: i32 = -1_765_328_234;
const KRB5_REALM_UNKNOWN: i32 = -1_765_328_230;
const KRB5_KDC_UNREACH: i32 = -1_765_328_228;
const KRB5_CC_IO: i32 = -1_765_328_191;
const KRB5_FCC_PERM: i32 = -1_765_328_190;
const KRB5_FCC_NOFILE: i32 = -1_765_328_189;
const KRB5_CC_FORMAT: i32 = -1_765_328_185;
const KRB5_CC_NOT_KTYPE: i32 = -1_765_328_184;
const KRB5_REALM_CANT_RESOLVE: i32 = -1_765_328_164;
const KRB5_CONFIG_NODEFREALM: i32 = -1_765_328_160;
const KRB5_CC_NOSUPP: i32 = -1_765_328_137;
const KRB5_KCM_MALFORMED_REPLY: i32 = -1_750_600_184;
const KRB5_KCM_RPC_ERROR: i32 = -1_750_600_183;
const KRB5_KCM_REPLY_TOO_BIG: i32 = -1_750_600_182;
const KRB5_KCM_NO_SERVER: i32 = -1_750_600_181;

/// The Kerberos library's error codes a failed login can end with.
const KRB5_ERRORS: &[(i32, &str)] = &[
    (-1_765_328_378, "KRB5KDC_ERR_C_PRINCIPAL_UNKNOWN"),
    (
        KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN,
        "KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN",
    ),
    (
        KRB5KDC_ERR_PRINCIPAL_NOT_UNIQUE,
        "KRB5KDC_ERR_PRINCIPAL_NOT_UNIQUE",
    ),
    (-1_765_328_372, "KRB5KDC_ERR_POLICY"),
    (KRB5KDC_ERR_ETYPE_NOSUPP, "KRB5KDC_ERR_ETYPE_NOSUPP"),
    (-1_765_328_366, "KRB5KDC_ERR_CLIENT_REVOKED"),
    (-1_765_328_365, "KRB5KDC_ERR_SERVICE_REVOKED"),
    (-1_765_328_361, "KRB5KDC_ERR_KEY_EXP"),
    (-1_765_328_360, "KRB5KDC_ERR_PREAUTH_FAILED"),
    (-1_765_328_355, "KRB5KDC_ERR_SVC_UNAVAILABLE"),
    (-1_765_328_316, "KRB5KDC_ERR_WRONG_REALM"),
    (KRB5KRB_AP_ERR_BAD_INTEGRITY, "KRB5KRB_AP_ERR_BAD_INTEGRITY"),
    (KRB5KRB_AP_ERR_TKT_EXPIRED, "KRB5KRB_AP_ERR_TKT_EXPIRED"),
    (-1_765_328_351, "KRB5KRB_AP_ERR_TKT_NYV"),
    (-1_765_328_349, "KRB5KRB_AP_ERR_NOT_US"),
    (KRB5KRB_AP_ERR_SKEW, "KRB5KRB_AP_ERR_SKEW"),
    (KRB5KRB_AP_ERR_MODIFIED, "KRB5KRB_AP_ERR_MODIFIED"),
    (KRB5KRB_AP_ERR_BADKEYVER, "KRB5KRB_AP_ERR_BADKEYVER"),
    (KRB5KRB_AP_ERR_NOKEY, "KRB5KRB_AP_ERR_NOKEY"),
    (KRB5KRB_AP_ERR_MUT_FAIL, "KRB5KRB_AP_ERR_MUT_FAIL"),
    (-1_765_328_324, "KRB5KRB_ERR_GENERIC"),
    (-1_765_328_248, "KRB5_CONFIG_BADFORMAT"),
    (KRB5_CC_BADNAME, "KRB5_CC_BADNAME"),
    (KRB5_CC_UNKNOWN_TYPE, "KRB5_CC_UNKNOWN_TYPE"),
    (KRB5_CC_NOTFOUND, "KRB5_CC_NOTFOUND"),
    (-1_765_328_241, "KRB5_NO_TKT_SUPPLIED"),
    (KRB5_KDCREP_SKEW, "KRB5_KDCREP_SKEW"),
    (KRB5_PROG_ETYPE_NOSUPP, "KRB5_PROG_ETYPE_NOSUPP"),
    (KRB5_REALM_UNKNOWN, "KRB5_REALM_UNKNOWN"),
    (KRB5_KDC_UNREACH, "KRB5_KDC_UNREACH"),
    (-1_765_328_203, "KRB5_KT_NOTFOUND"),
    (-1_765_328_196, "KRB5_BAD_ENCTYPE"),
    (KRB5_CC_IO, "KRB5_CC_IO"),
    (KRB5_FCC_PERM, "KRB5_FCC_PERM"),
    (KRB5_FCC_NOFILE, "KRB5_FCC_NOFILE"),
    (-1_765_328_188, "KRB5_FCC_INTERNAL"),
    (-1_765_328_186, "KRB5_CC_NOMEM"),
    (KRB5_CC_FORMAT, "KRB5_CC_FORMAT"),
    (KRB5_CC_NOT_KTYPE, "KRB5_CC_NOT_KTYPE"),
    (-1_765_328_167, "KRB5_ERR_HOST_REALM_UNKNOWN"),
    (KRB5_REALM_CANT_RESOLVE, "KRB5_REALM_CANT_RESOLVE"),
    (-1_765_328_163, "KRB5_TKT_NOT_FORWARDABLE"),
    (KRB5_CONFIG_NODEFREALM, "KRB5_CONFIG_NODEFREALM"),
    (-1_765_328_138, "KRB5_CC_READONLY"),
    (KRB5_CC_NOSUPP, "KRB5_CC_NOSUPP"),
    (KRB5_KCM_MALFORMED_REPLY, "KRB5_KCM_MALFORMED_REPLY"),
    (KRB5_KCM_RPC_ERROR, "KRB5_KCM_RPC_ERROR"),
    (KRB5_KCM_REPLY_TOO_BIG, "KRB5_KCM_REPLY_TOO_BIG"),
    (KRB5_KCM_NO_SERVER, "KRB5_KCM_NO_SERVER"),
];

// GSSAPI routine errors (RFC 2744), in bits 16-23 of the major status.
const GSS_S_NO_CRED: u32 = 7;
const GSS_S_CREDENTIALS_EXPIRED: u32 = 11;

/// The status codes a security-package failure carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SecurityCodes {
    /// Windows SSPI's SECURITY_STATUS.
    pub(super) sspi: Option<i32>,
    /// GSSAPI's major status.
    pub(super) gss_major: Option<u32>,
    /// GSSAPI's minor status: with Kerberos, the Kerberos library's error.
    pub(super) gss_minor: Option<i32>,
}

impl SecurityCodes {
    pub(super) fn of(error: &SecurityError) -> Self {
        match error {
            #[cfg(unix)]
            SecurityError::GssapiError { major, minor, .. } => Self {
                gss_major: Some(*major),
                gss_minor: (*minor != 0).then_some(*minor as i32),
                ..Self::default()
            },
            #[cfg(windows)]
            SecurityError::SspiError { status, .. } => Self {
                sspi: Some(*status),
                ..Self::default()
            },
            #[cfg(windows)]
            SecurityError::InitContextFailed { code, .. }
            | SecurityError::AcquireCredentialsFailed { code, .. }
                if *code != 0 =>
            {
                Self {
                    sspi: Some(*code as i32),
                    ..Self::default()
                }
            }
            // The SSPI context reports SEC_E_NO_CREDENTIALS as this variant.
            #[cfg(windows)]
            SecurityError::NoCredentials => Self {
                sspi: Some(SEC_E_NO_CREDENTIALS),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }

    /// The code that says the most: the Kerberos library's, then GSSAPI's
    /// or SSPI's, as its symbolic name or, when it has none, in hex.
    pub(super) fn most_specific(self) -> Option<String> {
        if let Some(minor) = self.gss_minor {
            return Some(krb5_error_name(minor).map_or_else(|| hex(minor), str::to_string));
        }
        if let Some(status) = self.sspi {
            return Some(sspi_status_name(status).map_or_else(|| hex(status), str::to_string));
        }
        self.gss_major
            .map(|major| gss_major_name(major).map_or_else(|| hex(major as i32), str::to_string))
    }
}

fn hex(code: i32) -> String {
    format!("0x{:08X}", code as u32)
}

/// The symbolic name of a SECURITY_STATUS.
pub(super) fn sspi_status_name(status: i32) -> Option<&'static str> {
    SSPI_STATUS
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, name)| *name)
}

/// The symbolic name of a Kerberos library error code.
pub(super) fn krb5_error_name(code: i32) -> Option<&'static str> {
    KRB5_ERRORS
        .iter()
        .find(|(value, _)| *value == code)
        .map(|(_, name)| *name)
}

/// The symbolic name of a GSSAPI major status's error: its routine error,
/// otherwise its calling error.
pub(super) fn gss_major_name(major: u32) -> Option<&'static str> {
    const ROUTINE: [&str; 18] = [
        "GSS_S_BAD_MECH",
        "GSS_S_BAD_NAME",
        "GSS_S_BAD_NAMETYPE",
        "GSS_S_BAD_BINDINGS",
        "GSS_S_BAD_STATUS",
        "GSS_S_BAD_SIG",
        "GSS_S_NO_CRED",
        "GSS_S_NO_CONTEXT",
        "GSS_S_DEFECTIVE_TOKEN",
        "GSS_S_DEFECTIVE_CREDENTIAL",
        "GSS_S_CREDENTIALS_EXPIRED",
        "GSS_S_CONTEXT_EXPIRED",
        "GSS_S_FAILURE",
        "GSS_S_BAD_QOP",
        "GSS_S_UNAUTHORIZED",
        "GSS_S_UNAVAILABLE",
        "GSS_S_DUPLICATE_ELEMENT",
        "GSS_S_NAME_NOT_MN",
    ];
    const CALLING: [&str; 3] = [
        "GSS_S_CALL_INACCESSIBLE_READ",
        "GSS_S_CALL_INACCESSIBLE_WRITE",
        "GSS_S_CALL_BAD_STRUCTURE",
    ];
    let routine = (major >> 16) & 0xff;
    let calling = major >> 24;
    if routine != 0 {
        ROUTINE.get(routine as usize - 1).copied()
    } else if calling != 0 {
        CALLING.get(calling as usize - 1).copied()
    } else {
        None
    }
}

/// What a security-package failure's codes establish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SecuritySignal {
    /// The KDC has no single account for the SPN.
    ServicePrincipalUnknown,
    /// The KDC has more than one account for the SPN.
    ServicePrincipalNotUnique,
    /// SSPI found the target unknown or unreachable.
    TargetUnknown,
    /// Two clocks differ by more than Kerberos allows.
    ClockSkew,
    /// No encryption type is supported on both sides.
    NoCommonEncryptionType,
    /// The client's credentials use an encryption type its configuration
    /// does not allow for the request.
    CredentialsEncryptionTypeNotAllowed,
    /// The client had no credentials.
    NoCredentials,
    /// The client's credentials had expired.
    CredentialsExpired,
    /// The client's credential cache could not be read.
    CacheUnreadable,
    /// No KDC for the realm could be reached.
    KdcUnreachable,
    /// The client's configuration names no KDC or realm for the request.
    RealmUnknown,
    /// A key did not match: the service could not read the ticket, or the
    /// client could not verify the service's answer.
    KeyMismatch,
    /// The Kerberos library could not be loaded.
    LibraryMissing,
}

impl SecuritySignal {
    /// What `error`'s codes establish, or `None` for a failure they do not
    /// explain.
    pub(super) fn of(error: &SecurityError) -> Option<Self> {
        if matches!(error, SecurityError::LoadLibraryFailed(_)) {
            return Some(Self::LibraryMissing);
        }
        Self::from_codes(SecurityCodes::of(error))
    }

    /// The signal of `codes`: the Kerberos library's error says the most,
    /// then SSPI's or GSSAPI's status.
    pub(super) fn from_codes(codes: SecurityCodes) -> Option<Self> {
        codes
            .gss_minor
            .and_then(Self::from_krb5)
            .or_else(|| codes.sspi.and_then(Self::from_sspi))
            .or_else(|| codes.gss_major.and_then(Self::from_gss_major))
    }

    fn from_krb5(code: i32) -> Option<Self> {
        Some(match code {
            KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN => Self::ServicePrincipalUnknown,
            KRB5KDC_ERR_PRINCIPAL_NOT_UNIQUE => Self::ServicePrincipalNotUnique,
            KRB5KRB_AP_ERR_SKEW | KRB5_KDCREP_SKEW => Self::ClockSkew,
            KRB5KDC_ERR_ETYPE_NOSUPP | KRB5_PROG_ETYPE_NOSUPP => Self::NoCommonEncryptionType,
            KRB5_CC_NOT_KTYPE => Self::CredentialsEncryptionTypeNotAllowed,
            KRB5_FCC_NOFILE | KRB5_CC_NOTFOUND => Self::NoCredentials,
            KRB5KRB_AP_ERR_TKT_EXPIRED => Self::CredentialsExpired,
            KRB5_CC_IO
            | KRB5_FCC_PERM
            | KRB5_CC_FORMAT
            | KRB5_CC_NOSUPP
            | KRB5_CC_BADNAME
            | KRB5_CC_UNKNOWN_TYPE
            | KRB5_KCM_NO_SERVER
            | KRB5_KCM_RPC_ERROR
            | KRB5_KCM_MALFORMED_REPLY
            | KRB5_KCM_REPLY_TOO_BIG => Self::CacheUnreadable,
            KRB5_KDC_UNREACH | KRB5_REALM_CANT_RESOLVE => Self::KdcUnreachable,
            KRB5_REALM_UNKNOWN | KRB5_CONFIG_NODEFREALM => Self::RealmUnknown,
            KRB5KRB_AP_ERR_MODIFIED
            | KRB5KRB_AP_ERR_BAD_INTEGRITY
            | KRB5KRB_AP_ERR_BADKEYVER
            | KRB5KRB_AP_ERR_NOKEY
            | KRB5KRB_AP_ERR_MUT_FAIL => Self::KeyMismatch,
            _ => return None,
        })
    }

    fn from_sspi(status: i32) -> Option<Self> {
        Some(match status {
            SEC_E_TARGET_UNKNOWN => Self::TargetUnknown,
            SEC_E_TIME_SKEW => Self::ClockSkew,
            SEC_E_KDC_UNKNOWN_ETYPE => Self::NoCommonEncryptionType,
            SEC_E_NO_CREDENTIALS => Self::NoCredentials,
            SEC_E_NO_AUTHENTICATING_AUTHORITY => Self::KdcUnreachable,
            SEC_E_WRONG_PRINCIPAL => Self::KeyMismatch,
            _ => return None,
        })
    }

    fn from_gss_major(major: u32) -> Option<Self> {
        match (major >> 16) & 0xff {
            GSS_S_NO_CRED => Some(Self::NoCredentials),
            GSS_S_CREDENTIALS_EXPIRED => Some(Self::CredentialsExpired),
            _ => None,
        }
    }

    /// How conclusively the signal explains the failed attempt, and the
    /// specialist domain it points to: Kerberos's, unless SSPI reported no
    /// credentials, which is the selected package's (`auth_domain`).
    pub(super) fn coverage(
        self,
        codes: SecurityCodes,
        auth_domain: &'static str,
    ) -> (Coverage, Domain) {
        match self {
            Self::KdcUnreachable => (
                Coverage::Diagnosed,
                Domain::undetermined(&[KERBEROS_DOMAIN, "connectivity.network"]),
            ),
            Self::KeyMismatch => (Coverage::Inconclusive, Domain::specific(KERBEROS_DOMAIN)),
            Self::LibraryMissing => (Coverage::Diagnosed, Domain::specific("client.driver")),
            Self::NoCredentials if codes.sspi.is_some() => {
                (Coverage::Diagnosed, Domain::specific(auth_domain))
            }
            _ => (Coverage::Diagnosed, Domain::specific(KERBEROS_DOMAIN)),
        }
    }

    /// Whether how the SPN is registered can explain the failure, so that
    /// looking it up in the directory says something about it.
    pub(super) fn spn_related(self) -> bool {
        matches!(
            self,
            Self::ServicePrincipalUnknown
                | Self::ServicePrincipalNotUnique
                | Self::TargetUnknown
                | Self::KeyMismatch
                | Self::NoCommonEncryptionType
        )
    }

    /// The finding: what the signal establishes, with `code`, the code that
    /// gave it, and no more.
    pub(super) fn finding(self, code: &str) -> LocalizedText {
        let (id, english) = match self {
            Self::ServicePrincipalUnknown => (
                "diagnose.finding.kerberos_spn_unknown",
                format!(
                    "The KDC has no single account for the SPN the client requested ({code}): no \
                     account has it, more than one does, or the service registered another SPN; \
                     this signal alone does not say which."
                ),
            ),
            Self::ServicePrincipalNotUnique => (
                "diagnose.finding.kerberos_spn_not_unique",
                format!(
                    "The KDC reported more than one account for the SPN the client requested \
                     ({code})."
                ),
            ),
            Self::TargetUnknown => (
                "diagnose.finding.sspi_target_unknown",
                format!(
                    "Windows reported the SPN's target unknown or unreachable ({code}); this \
                     signal alone does not establish whether the SPN is missing, registered on \
                     more than one account, or not reachable."
                ),
            ),
            Self::ClockSkew => (
                "diagnose.finding.kerberos_clock_skew",
                format!(
                    "Kerberos refused the request because two clocks differ by more than it \
                     allows ({code}); which clock is off is not established."
                ),
            ),
            Self::NoCommonEncryptionType => (
                "diagnose.finding.kerberos_no_common_etype",
                format!(
                    "No encryption type is supported by both the client and the KDC or the \
                     service account ({code})."
                ),
            ),
            Self::CredentialsEncryptionTypeNotAllowed => (
                "diagnose.finding.kerberos_credentials_etype_not_allowed",
                format!(
                    "The client's Kerberos credentials use an encryption type that its Kerberos \
                     configuration does not allow for this request ({code})."
                ),
            ),
            Self::NoCredentials => (
                "diagnose.finding.integrated_no_credentials",
                format!("The client had no credentials for integrated authentication ({code})."),
            ),
            Self::CredentialsExpired => (
                "diagnose.finding.kerberos_credentials_expired",
                format!("The client's Kerberos credentials had expired ({code})."),
            ),
            Self::CacheUnreadable => (
                "diagnose.finding.kerberos_cache_unreadable",
                format!("The client's Kerberos credential cache could not be read ({code})."),
            ),
            Self::KdcUnreachable => (
                "diagnose.finding.kerberos_kdc_unreachable",
                format!("The client could not reach a KDC for the realm ({code})."),
            ),
            Self::RealmUnknown => (
                "diagnose.finding.kerberos_realm_unknown",
                format!(
                    "The client's Kerberos configuration names no KDC or realm for the request \
                     ({code})."
                ),
            ),
            Self::KeyMismatch => (
                "diagnose.finding.kerberos_key_mismatch",
                format!(
                    "A Kerberos key did not match ({code}): the SPN may be registered on an \
                     account other than the one SQL Server runs as, or that account's key may \
                     have changed; this signal alone does not establish which."
                ),
            ),
            Self::LibraryMissing => (
                "diagnose.finding.kerberos_library_missing",
                format!(
                    "The Kerberos library could not be loaded, so integrated authentication \
                     could not start ({code})."
                ),
            ),
        };
        LocalizedText::new(id, english, vec![("code", code.to_string())])
    }
}

/// The limitation of a failure whose codes establish nothing specific.
pub(super) fn generic_failure_limitation() -> LocalizedText {
    LocalizedText::new(
        "diagnose.limitation.security_failure_generic",
        "A generic SSPI or GSSAPI failure, without a more specific status, does not establish a \
         missing or duplicate SPN or any other single cause.",
        vec![],
    )
}

/// The limitation of SQL Server's 18452 with integrated authentication.
pub(super) fn login_18452_limitation() -> LocalizedText {
    LocalizedText::new(
        "diagnose.limitation.login_18452_not_one_cause",
        "SQL Server's error 18452 alone does not establish one cause: the login may come from a \
         domain the server does not trust, or Kerberos may have failed on the server.",
        vec![],
    )
}

/// The error-chain records of a security-package failure, outer first: the
/// package's own status (SSPI's, or GSSAPI's major one), worded as `text`,
/// then the Kerberos library's error GSSAPI gives as its minor status, in the
/// library's words. A failure with no status is one client record.
pub(super) fn security_records(
    error: &SecurityError,
    text: &LocalizedText,
    check: CheckId,
) -> Vec<ErrorRecord> {
    let codes = SecurityCodes::of(error);
    let mut records = Vec::new();
    let coded = |source: &'static str, code: Option<String>, number: i32| {
        let mut record = ErrorRecord::new(source, check, String::new());
        record.message = text.clone();
        record.code = code;
        record.native_error = Some(number);
        record
    };
    if let Some(status) = codes.sspi {
        records.push(coded(
            "sspi",
            sspi_status_name(status).map(str::to_string),
            status,
        ));
    }
    if let Some(major) = codes.gss_major {
        records.push(coded(
            "gssapi",
            gss_major_name(major).map(str::to_string),
            major as i32,
        ));
        if let Some(minor) = codes.gss_minor {
            let mut inner = coded(
                "kerberos",
                krb5_error_name(minor).map(str::to_string),
                minor,
            );
            inner.message = minor_text(&error.to_string()).map_or_else(
                || {
                    let code = krb5_error_name(minor).map_or_else(|| hex(minor), str::to_string);
                    LocalizedText::new(
                        "diagnose.error.kerberos_status",
                        format!("Kerberos error {code}"),
                        vec![("code", code.clone())],
                    )
                },
                LocalizedText::literal,
            );
            records.push(inner);
        }
    }
    if records.is_empty() {
        let mut client = ErrorRecord::new("client", check, String::new());
        client.message = text.clone();
        client.code = client_code(error).map(str::to_string);
        records.push(client);
    }
    records
}

/// The GSSAPI library's words for the minor status, from the message
/// `mssql-tds` builds (`...; Minor: <text>`).
fn minor_text(message: &str) -> Option<String> {
    message
        .rsplit_once("Minor: ")
        .map(|(_, minor)| minor.trim().to_string())
        .filter(|minor| !minor.is_empty())
}

/// A stable code for a failure without a status.
fn client_code(error: &SecurityError) -> Option<&'static str> {
    Some(match error {
        SecurityError::LoadLibraryFailed(_) => "securityLibraryNotLoaded",
        SecurityError::NoCredentials => "noCredentials",
        SecurityError::CredentialsExpired => "credentialsExpired",
        SecurityError::TargetUnknown(_) => "targetUnknown",
        SecurityError::InvalidSpnFormat(_) => "invalidSpn",
        SecurityError::InvalidToken => "invalidToken",
        SecurityError::Timeout => "timedOut",
        SecurityError::AuthenticationDenied(_) => "authenticationDenied",
        SecurityError::NotSupported(_) => "notSupported",
        _ => return None,
    })
}

/// What the diagnostic client's security context reported at login, from
/// its connect stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LoginSecurity {
    /// The package asked for: `Negotiate`, `Kerberos`, ...
    pub(super) package: String,
    /// The package negotiated, and whether the negotiation completed.
    pub(super) negotiated: Option<(String, bool)>,
    /// Whether the context retried without an SPN.
    pub(super) retried_without_spn: bool,
}

impl LoginSecurity {
    /// What a login stage recorded, when it made a security context.
    pub(super) fn from_step(step: &Step) -> Option<Self> {
        let package = step.detail(connect_stage::PACKAGE)?.to_string();
        let negotiated = step
            .detail(connect_stage::NEGOTIATED_PACKAGE)
            .filter(|name| !name.is_empty())
            .map(|name| {
                (
                    name.to_string(),
                    step.detail(connect_stage::NEGOTIATION_COMPLETE) == Some("true"),
                )
            });
        Some(Self {
            package,
            negotiated,
            retried_without_spn: step.detail(connect_stage::SPN_RETRY) == Some("true"),
        })
    }

    /// Whether Negotiate chose NTLM.
    pub(super) fn negotiate_chose_ntlm(&self) -> bool {
        self.package.eq_ignore_ascii_case("Negotiate")
            && self
                .negotiated
                .as_ref()
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("NTLM"))
    }
}

/// The finding when Negotiate chose NTLM for the login.
pub(super) fn negotiate_chose_ntlm_finding() -> LocalizedText {
    LocalizedText::new(
        "diagnose.finding.negotiate_chose_ntlm",
        "Negotiate chose NTLM, not Kerberos, for this login.",
        vec![],
    )
}

/// The finding when the client retried without an SPN.
pub(super) fn retried_without_spn_finding() -> LocalizedText {
    LocalizedText::new(
        "diagnose.finding.retried_without_spn",
        "The client retried without an SPN, which it does only for a connection to this \
         computer, so that Negotiate can use NTLM.",
        vec![],
    )
}

/// The finding when SQL Server reports NTLM for an integrated login.
pub(super) fn server_reports_ntlm_finding() -> LocalizedText {
    LocalizedText::new(
        "diagnose.finding.server_reports_ntlm",
        "SQL Server reports that this login used NTLM, not Kerberos.",
        vec![],
    )
}

/// Why Negotiate chose NTLM is not reported.
pub(super) fn ntlm_reason_limitation() -> LocalizedText {
    LocalizedText::new(
        "diagnose.limitation.ntlm_reason_not_reported",
        "Windows does not report why Negotiate chose NTLM: a KDC that could not be reached, an \
         SPN the KDC does not know, or a connection to this computer can each lead to it.",
        vec![],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_are_named_as_their_headers_name_them() {
        assert_eq!(
            sspi_status_name(0x8009_0303_u32 as i32),
            Some("SEC_E_TARGET_UNKNOWN")
        );
        assert_eq!(
            sspi_status_name(0x8009_0342_u32 as i32),
            Some("SEC_E_KDC_UNKNOWN_ETYPE")
        );
        assert_eq!(sspi_status_name(0x8009_0399_u32 as i32), None);
        assert_eq!(
            krb5_error_name(-1_765_328_377),
            Some("KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN")
        );
        assert_eq!(krb5_error_name(-1_750_600_181), Some("KRB5_KCM_NO_SERVER"));
        assert_eq!(krb5_error_name(1), None);
        assert_eq!(gss_major_name(13 << 16), Some("GSS_S_FAILURE"));
        assert_eq!(gss_major_name(7 << 16), Some("GSS_S_NO_CRED"));
        // A routine error wins over a calling error and supplementary bits.
        assert_eq!(
            gss_major_name((1 << 24) | (9 << 16) | 1),
            Some("GSS_S_DEFECTIVE_TOKEN")
        );
        assert_eq!(
            gss_major_name(1 << 24),
            Some("GSS_S_CALL_INACCESSIBLE_READ")
        );
        assert_eq!(gss_major_name(1), None);
        assert_eq!(gss_major_name(30 << 16), None);
    }

    #[test]
    fn the_kerberos_code_says_more_than_the_gssapi_status() {
        let codes = |major: u32, minor: i32| SecurityCodes {
            gss_major: Some(major << 16),
            gss_minor: Some(minor),
            ..SecurityCodes::default()
        };
        let signal = |c| SecuritySignal::from_codes(c);
        assert_eq!(
            signal(codes(13, -1_765_328_377)),
            Some(SecuritySignal::ServicePrincipalUnknown)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_376)),
            Some(SecuritySignal::ServicePrincipalNotUnique)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_347)),
            Some(SecuritySignal::ClockSkew)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_236)),
            Some(SecuritySignal::ClockSkew)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_370)),
            Some(SecuritySignal::NoCommonEncryptionType)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_184)),
            Some(SecuritySignal::CredentialsEncryptionTypeNotAllowed)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_343)),
            Some(SecuritySignal::KeyMismatch)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_228)),
            Some(SecuritySignal::KdcUnreachable)
        );
        assert_eq!(
            signal(codes(13, -1_765_328_230)),
            Some(SecuritySignal::RealmUnknown)
        );
        // No credentials, from GSSAPI, explained by the cache the library
        // could not reach: the cache, not the credentials, is what failed.
        assert_eq!(
            signal(codes(7, -1_765_328_189)),
            Some(SecuritySignal::NoCredentials)
        );
        assert_eq!(
            signal(codes(7, -1_750_600_181)),
            Some(SecuritySignal::CacheUnreadable)
        );
        assert_eq!(
            signal(codes(7, 0x1234)),
            Some(SecuritySignal::NoCredentials)
        );
        assert_eq!(
            signal(codes(11, 0x1234)),
            Some(SecuritySignal::CredentialsExpired)
        );
        // A failure no code explains establishes nothing.
        assert_eq!(signal(codes(13, 0x1234)), None);
        assert_eq!(signal(codes(13, -1_765_328_324)), None);
    }

    #[test]
    fn sspi_statuses_give_their_signal() {
        let signal = |status: u32| {
            SecuritySignal::from_codes(SecurityCodes {
                sspi: Some(status as i32),
                ..SecurityCodes::default()
            })
        };
        assert_eq!(signal(0x8009_0303), Some(SecuritySignal::TargetUnknown));
        assert_eq!(signal(0x8009_0324), Some(SecuritySignal::ClockSkew));
        assert_eq!(
            signal(0x8009_0342),
            Some(SecuritySignal::NoCommonEncryptionType)
        );
        assert_eq!(signal(0x8009_030E), Some(SecuritySignal::NoCredentials));
        assert_eq!(signal(0x8009_0311), Some(SecuritySignal::KdcUnreachable));
        assert_eq!(signal(0x8009_0322), Some(SecuritySignal::KeyMismatch));
        // Generic: internal error, logon denied, invalid token.
        for generic in [0x8009_0304, 0x8009_030C, 0x8009_0308] {
            assert_eq!(signal(generic), None, "{generic:#x}");
        }
    }

    /// A signal that leaves the cause open never reads as diagnosed, and the
    /// network is a candidate only where the KDC could not be reached; SSPI's
    /// missing credentials are Windows authentication's, not Kerberos's.
    #[test]
    fn only_specific_signals_are_diagnosed() {
        let none = SecurityCodes::default();
        let windows = "security.authentication.windows";
        let (coverage, domain) = SecuritySignal::KeyMismatch.coverage(none, windows);
        assert_eq!(coverage, Coverage::Inconclusive);
        assert_eq!(domain, Domain::specific(KERBEROS_DOMAIN));
        let (coverage, domain) = SecuritySignal::KdcUnreachable.coverage(none, windows);
        assert_eq!(coverage, Coverage::Diagnosed);
        assert_eq!(
            domain,
            Domain::undetermined(&[KERBEROS_DOMAIN, "connectivity.network"])
        );
        let (coverage, domain) = SecuritySignal::ServicePrincipalUnknown.coverage(none, windows);
        assert_eq!(coverage, Coverage::Diagnosed);
        assert_eq!(domain, Domain::specific(KERBEROS_DOMAIN));
        let sspi = SecurityCodes {
            sspi: Some(SEC_E_NO_CREDENTIALS),
            ..SecurityCodes::default()
        };
        let (_, domain) = SecuritySignal::NoCredentials.coverage(sspi, windows);
        assert_eq!(domain, Domain::specific(windows));
        let (_, domain) = SecuritySignal::NoCredentials.coverage(none, windows);
        assert_eq!(domain, Domain::specific(KERBEROS_DOMAIN));
    }

    /// Every signal's finding carries the code and no more: the SPN finding
    /// does not choose between a missing and a duplicate SPN.
    #[test]
    fn findings_name_the_code_and_claim_no_single_cause() {
        let text = SecuritySignal::ServicePrincipalUnknown
            .finding("KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN")
            .render("en-US");
        assert!(text.contains("(KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN)"), "{text}");
        assert!(text.contains("more than one does"), "{text}");
        assert!(text.contains("does not say which"), "{text}");
        let text = SecuritySignal::KeyMismatch
            .finding("KRB5KRB_AP_ERR_MODIFIED")
            .render("en-US");
        assert!(text.contains("does not establish which"), "{text}");
        let text = SecuritySignal::ClockSkew
            .finding("SEC_E_TIME_SKEW")
            .render("en-US");
        assert!(
            text.contains("which clock is off is not established"),
            "{text}"
        );
    }

    #[test]
    fn the_minor_status_text_is_the_librarys() {
        assert_eq!(
            minor_text(
                "GSSAPI error (major=0x000D0000, minor=0x96C73A07): Major: Unspecified GSS \
                 failure; Minor: Server not found in Kerberos database"
            )
            .as_deref(),
            Some("Server not found in Kerberos database")
        );
        assert_eq!(minor_text("Major: Unspecified GSS failure"), None);
        assert_eq!(minor_text("Major: x; Minor: "), None);
    }

    /// A GSSAPI failure is two records, outer first: GSSAPI's status in the
    /// client's words, then the Kerberos library's error in its own.
    #[cfg(unix)]
    #[test]
    fn a_gssapi_failure_keeps_both_codes() {
        let error = SecurityError::GssapiError {
            major: 13 << 16,
            minor: (-1_765_328_377_i32) as u32,
            message: "Major: Unspecified GSS failure; Minor: Server not found in Kerberos database"
                .to_string(),
        };
        let text = LocalizedText::literal(error.to_string());
        let records = security_records(&error, &text, CheckId::ConnectionAttempt);
        assert_eq!(records.len(), 2, "{records:#?}");
        assert_eq!(records[0].source, "gssapi");
        assert_eq!(records[0].code.as_deref(), Some("GSS_S_FAILURE"));
        assert_eq!(records[0].native_error, Some(13 << 16));
        assert_eq!(records[0].message, text);
        assert_eq!(records[1].source, "kerberos");
        assert_eq!(
            records[1].code.as_deref(),
            Some("KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN")
        );
        assert_eq!(records[1].native_error, Some(-1_765_328_377));
        assert_eq!(
            records[1].message.render("en-US"),
            "Server not found in Kerberos database"
        );
        assert_eq!(
            SecuritySignal::of(&error),
            Some(SecuritySignal::ServicePrincipalUnknown)
        );
        assert_eq!(
            SecurityCodes::of(&error).most_specific().as_deref(),
            Some("KRB5KDC_ERR_S_PRINCIPAL_UNKNOWN")
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_sspi_failure_keeps_its_status() {
        let error = SecurityError::InitContextFailed {
            code: 0x8009_0303,
            message: "Target unknown (SPN not found)".to_string(),
        };
        let text = LocalizedText::literal(error.to_string());
        let records = security_records(&error, &text, CheckId::ConnectionAttempt);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].source, "sspi");
        assert_eq!(records[0].code.as_deref(), Some("SEC_E_TARGET_UNKNOWN"));
        assert_eq!(records[0].native_error, Some(0x8009_0303_u32 as i32));
        assert_eq!(
            SecuritySignal::of(&error),
            Some(SecuritySignal::TargetUnknown)
        );
        // No credentials keeps its status, which the SSPI context maps away.
        let records = security_records(
            &SecurityError::NoCredentials,
            &text,
            CheckId::ConnectionAttempt,
        );
        assert_eq!(records[0].code.as_deref(), Some("SEC_E_NO_CREDENTIALS"));
    }

    #[test]
    fn a_failure_without_a_status_is_one_client_record() {
        let error = SecurityError::LoadLibraryFailed("libgssapi_krb5.so".to_string());
        let text = LocalizedText::literal(error.to_string());
        let records = security_records(&error, &text, CheckId::ConnectionAttempt);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, "client");
        assert_eq!(records[0].code.as_deref(), Some("securityLibraryNotLoaded"));
        assert_eq!(
            SecuritySignal::of(&error),
            Some(SecuritySignal::LibraryMissing)
        );
        let generic = SecurityError::InternalError("boom".to_string());
        assert_eq!(SecuritySignal::of(&generic), None);
        assert_eq!(
            security_records(&generic, &text, CheckId::ConnectionAttempt)[0].code,
            None
        );
    }
}

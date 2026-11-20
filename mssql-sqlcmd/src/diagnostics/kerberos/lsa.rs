// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The process's own logon session, through the LSA (Windows): what logged
//! it on, and the Kerberos tickets Windows keeps for it, listed as `klist`
//! lists them. Neither needs a privilege, and neither asks a KDC for a
//! ticket or reads a key.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows_sys::Win32::Security::Authentication::Identity::{
    KERB_QUERY_TKT_CACHE_EX2_RESPONSE, KERB_QUERY_TKT_CACHE_REQUEST, KERB_TICKET_CACHE_INFO_EX2,
    KerbQueryTicketCacheEx2Message, LSA_STRING, LSA_UNICODE_STRING, LsaCallAuthenticationPackage,
    LsaConnectUntrusted, LsaDeregisterLogonProcess, LsaFreeReturnBuffer, LsaGetLogonSessionData,
    LsaLookupAuthenticationPackage, MICROSOFT_KERBEROS_NAME_A, SECURITY_LOGON_SESSION_DATA,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::credentials::{Credentials, Failure, Listed, Logon, State, Store, Ticket, unix_now};

/// The logon session's credentials, and the service ticket for `spn`.
pub(super) fn read(spn: Option<String>) -> Credentials {
    let mut credentials = Credentials::new(Store::LogonSession(logon_session()), State::Unreadable);
    credentials.lists_tickets = true;
    match tickets() {
        Ok(tickets) => credentials.read_tickets(&tickets, spn.as_deref()),
        Err(failure) => credentials.failure = Some(failure),
    }
    credentials
}

/// What logged the process's logon session on, when Windows says.
fn logon_session() -> Option<Logon> {
    let id = logon_id()?;
    let mut data: *mut SECURITY_LOGON_SESSION_DATA = null_mut();
    // SAFETY: `id` is the process's own logon session; the LSA allocates
    // `data`.
    let status = unsafe { LsaGetLogonSessionData(&id, &mut data) };
    if status != 0 || data.is_null() {
        return None;
    }
    // SAFETY: the LSA returned a SECURITY_LOGON_SESSION_DATA, freed below.
    let session = unsafe { &*data };
    let (domain, user) = (text(&session.LogonDomain), text(&session.UserName));
    let logon = Logon {
        kind: session.LogonType,
        package: text(&session.AuthenticationPackage),
        account: match (domain.is_empty(), user.is_empty()) {
            (_, true) => String::new(),
            (true, false) => user,
            (false, false) => format!(r"{domain}\{user}"),
        },
    };
    // SAFETY: `data` came from LsaGetLogonSessionData.
    unsafe { LsaFreeReturnBuffer(data.cast()) };
    Some(logon)
}

/// The process token's logon session.
fn logon_id() -> Option<LUID> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the current process's pseudo-handle; `token` receives a
    // handle, closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // SAFETY: TOKEN_STATISTICS is plain data.
    let mut statistics: TOKEN_STATISTICS = unsafe { std::mem::zeroed() };
    let mut length = 0u32;
    // SAFETY: `statistics` is as large as the size given.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenStatistics,
            (&raw mut statistics).cast(),
            size_of::<TOKEN_STATISTICS>() as u32,
            &mut length,
        )
    };
    // SAFETY: `token` came from OpenProcessToken.
    unsafe { CloseHandle(token) };
    (read != 0).then_some(statistics.AuthenticationId)
}

/// A connection to the LSA, closed when dropped.
struct Lsa(HANDLE);

impl Drop for Lsa {
    fn drop(&mut self) {
        // SAFETY: the handle came from LsaConnectUntrusted.
        unsafe { LsaDeregisterLogonProcess(self.0) };
    }
}

/// A buffer the LSA returned, freed when dropped.
struct Returned(*mut c_void);

impl Drop for Returned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the buffer came from LsaCallAuthenticationPackage.
            unsafe { LsaFreeReturnBuffer(self.0) };
        }
    }
}

/// The Kerberos tickets cached in the process's logon session.
fn tickets() -> Result<Vec<Listed>, Failure> {
    let failed = |status: i32, call: &'static str| Failure::Lsa { status, call };
    let mut handle: HANDLE = null_mut();
    // SAFETY: `handle` receives a connection, closed by `Lsa`.
    let status = unsafe { LsaConnectUntrusted(&mut handle) };
    if status != 0 {
        return Err(failed(status, "LsaConnectUntrusted"));
    }
    let lsa = Lsa(handle);
    let name = LSA_STRING {
        Length: 8,
        MaximumLength: 9,
        Buffer: MICROSOFT_KERBEROS_NAME_A.cast_mut(),
    };
    let mut package = 0u32;
    // SAFETY: `name` holds the package's name, which the LSA only reads.
    let status = unsafe { LsaLookupAuthenticationPackage(lsa.0, &name, &mut package) };
    if status != 0 {
        return Err(failed(status, "LsaLookupAuthenticationPackage"));
    }
    // A zero logon session is the caller's own.
    let request = KERB_QUERY_TKT_CACHE_REQUEST {
        MessageType: KerbQueryTicketCacheEx2Message,
        LogonId: LUID {
            LowPart: 0,
            HighPart: 0,
        },
    };
    let mut response: *mut c_void = null_mut();
    let mut length = 0u32;
    let mut protocol = 0i32;
    // SAFETY: the request is as large as the size given; the LSA allocates
    // the response, freed by `Returned`.
    let status = unsafe {
        LsaCallAuthenticationPackage(
            lsa.0,
            package,
            (&raw const request).cast(),
            size_of::<KERB_QUERY_TKT_CACHE_REQUEST>() as u32,
            &mut response,
            &mut length,
            &mut protocol,
        )
    };
    let response = Returned(response);
    if status != 0 {
        return Err(failed(status, "LsaCallAuthenticationPackage"));
    }
    if protocol != 0 {
        return Err(failed(protocol, "KerbQueryTicketCacheEx2Message"));
    }
    if response.0.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: the LSA returned `length` bytes of a
    // KERB_QUERY_TKT_CACHE_EX2_RESPONSE, whose strings point into them.
    Ok(unsafe { listed(response.0.cast(), length as usize, unix_now()) })
}

/// The tickets a KERB_QUERY_TKT_CACHE_EX2_RESPONSE of `length` bytes lists,
/// as many as fit in it.
///
/// # Safety
///
/// `response` points to `length` readable bytes holding the response, and
/// its strings to readable UTF-16.
unsafe fn listed(
    response: *const KERB_QUERY_TKT_CACHE_EX2_RESPONSE,
    length: usize,
    now: i64,
) -> Vec<Listed> {
    let offset = std::mem::offset_of!(KERB_QUERY_TKT_CACHE_EX2_RESPONSE, Tickets);
    if length < offset {
        return Vec::new();
    }
    // SAFETY: the header is within `length`.
    let count = unsafe { (*response).CountOfTickets } as usize;
    let count = count.min((length - offset) / size_of::<KERB_TICKET_CACHE_INFO_EX2>());
    // SAFETY: the array starts at `offset`, within `length`.
    let first = unsafe { response.cast::<u8>().add(offset) }.cast::<KERB_TICKET_CACHE_INFO_EX2>();
    (0..count)
        .map(|index| {
            // SAFETY: `count` entries fit in `length`.
            let info = unsafe { &*first.add(index) };
            let ends = unix_seconds(info.EndTime);
            Listed {
                client: format!("{}@{}", text(&info.ClientName), text(&info.ClientRealm)),
                server: format!("{}@{}", text(&info.ServerName), text(&info.ServerRealm)),
                ticket: Ticket {
                    ends,
                    expired: ends.is_some_and(|ends| ends <= now),
                    encryption: Some(info.EncryptionType),
                    session_key: Some(info.SessionKeyType as i32),
                    key_version: None,
                },
            }
        })
        .collect()
}

/// A Windows time (100 ns since 1601) in seconds since the Unix epoch;
/// `None` for none, or never.
fn unix_seconds(time: i64) -> Option<i64> {
    (time > 0 && time != i64::MAX).then(|| time / 10_000_000 - 11_644_473_600)
}

/// An LSA string's text.
fn text(string: &LSA_UNICODE_STRING) -> String {
    if string.Buffer.is_null() || string.Length == 0 {
        return String::new();
    }
    // SAFETY: the LSA's string holds `Length` bytes of UTF-16.
    let units =
        unsafe { std::slice::from_raw_parts(string.Buffer, usize::from(string.Length / 2)) };
    String::from_utf16_lossy(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(units: &mut [u16]) -> LSA_UNICODE_STRING {
        LSA_UNICODE_STRING {
            Length: (units.len() * 2) as u16,
            MaximumLength: (units.len() * 2) as u16,
            Buffer: units.as_mut_ptr(),
        }
    }

    #[test]
    fn a_ticket_cache_response_lists_what_fits_in_it() {
        let mut names: Vec<Vec<u16>> =
            ["alice", "CONTOSO.COM", "krbtgt/CONTOSO.COM", "CONTOSO.COM"]
                .iter()
                .map(|s| s.encode_utf16().collect())
                .collect();
        let [client, client_realm, server, server_realm] = &mut names[..] else {
            unreachable!()
        };
        let info = KERB_TICKET_CACHE_INFO_EX2 {
            ClientName: string(client),
            ClientRealm: string(client_realm),
            ServerName: string(server),
            ServerRealm: string(server_realm),
            StartTime: 0,
            // 2023-11-14T22:13:20Z
            EndTime: (1_700_000_000 + 11_644_473_600) * 10_000_000,
            RenewTime: 0,
            EncryptionType: 18,
            TicketFlags: 0,
            SessionKeyType: 17,
            BranchId: 0,
        };
        let response = KERB_QUERY_TKT_CACHE_EX2_RESPONSE {
            MessageType: KerbQueryTicketCacheEx2Message,
            CountOfTickets: 3,
            Tickets: [info],
        };
        // The response says three, but holds one.
        let length = size_of::<KERB_QUERY_TKT_CACHE_EX2_RESPONSE>();
        // SAFETY: `response` is `length` bytes, its strings live in `names`.
        let tickets = unsafe { listed(&response, length, 1_700_000_000) };
        assert_eq!(
            tickets,
            [Listed {
                client: "alice@CONTOSO.COM".to_string(),
                server: "krbtgt/CONTOSO.COM@CONTOSO.COM".to_string(),
                ticket: Ticket {
                    ends: Some(1_700_000_000),
                    expired: true,
                    encryption: Some(18),
                    session_key: Some(17),
                    key_version: None,
                },
            }]
        );
        // SAFETY: as above.
        assert!(unsafe { listed(&response, 4, 0) }.is_empty());
    }

    #[test]
    fn windows_times_without_an_end_have_none() {
        assert_eq!(unix_seconds(116_444_736_000_000_000), Some(0));
        assert_eq!(unix_seconds(0), None);
        assert_eq!(unix_seconds(i64::MAX), None);
    }

    /// The process's own logon session and its tickets are read live. It
    /// reads this computer's credentials, so it runs only when asked for, and
    /// shows none of them when it fails.
    #[test]
    #[ignore = "Reads this computer's logon session - set KERBEROS_TEST=1 to run"]
    fn kerberos_lsa_reads_the_processs_own_logon_session() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let credentials = read(None);
        let Store::LogonSession(logon) = &credentials.store else {
            unreachable!("the LSA reads a logon session");
        };
        let logon = logon
            .as_ref()
            .expect("Windows describes the process's logon session");
        assert!(!logon.package.is_empty(), "no authentication package");
        assert!(credentials.lists_tickets);
        // A failure holds an NTSTATUS and the call's name, nothing else.
        assert_eq!(credentials.failure, None);
    }
}

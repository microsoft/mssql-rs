// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The SPN looked up in the forest's global catalog through Windows' own
//! LDAP library (`wldap32.dll`, loaded at run time from System32): a
//! serverless bind that finds a global catalog as Windows' tools do, with
//! the logon session's own credentials (Negotiate), signed and sealed.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::Instant;

use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use super::spn_lookup::{
    Account, AccountKind, Failure, Lookup, Query, Scope, filter_value, refusal,
};

type Ldap = *mut c_void;
type Message = *mut c_void;

/// `struct l_timeval`.
#[repr(C)]
struct Timeval {
    seconds: i32,
    microseconds: i32,
}

/// `SEC_WINNT_AUTH_IDENTITY_W`: explicit credentials, which only a test
/// binds with.
#[repr(C)]
#[cfg_attr(not(test), allow(dead_code))]
struct AuthIdentity {
    user: *const u16,
    user_length: u32,
    domain: *const u16,
    domain_length: u32,
    password: *const u16,
    password_length: u32,
    flags: u32,
}

// winldap.h
const LDAP_GC_PORT: u32 = 3268;
const LDAP_SCOPE_SUBTREE: u32 = 2;
const LDAP_AUTH_NEGOTIATE: u32 = 0x0486;
const LDAP_OPT_REFERRALS: i32 = 0x08;
const LDAP_OPT_PROTOCOL_VERSION: i32 = 0x11;
const LDAP_OPT_SERVER_ERROR: i32 = 0x33;
const LDAP_OPT_SIGN: i32 = 0x95;
const LDAP_OPT_ENCRYPT: i32 = 0x96;
const LDAP_OPT_AREC_EXCLUSIVE: i32 = 0x98;
/// winldap.h's `((void *) 1)`: a flag the option takes in place of a pointer.
const LDAP_OPT_ON: *const c_void = std::ptr::without_provenance(1);
const LDAP_OPT_OFF: *const c_void = null();
const LDAP_SIZELIMIT_EXCEEDED: u32 = 0x04;
const LDAP_NOT_SUPPORTED: u32 = 0x5C;

/// The attributes read from an account.
const ATTRIBUTES: [&str; 3] = [
    "sAMAccountName",
    "objectClass",
    "msDS-SupportedEncryptionTypes",
];

/// `wldap32.dll`'s functions.
macro_rules! library {
    ($($field:ident: $symbol:literal => fn($($argument:ty),*) -> $result:ty;)*) => {
        struct Library {
            $($field: unsafe extern "C" fn($($argument),*) -> $result,)*
        }

        impl Library {
            fn load() -> Option<Self> {
                let name: Vec<u16> = "wldap32.dll\0".encode_utf16().collect();
                // SAFETY: a NUL-terminated name, from System32 only; the
                // module stays loaded for the process's life.
                let module = unsafe {
                    LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32)
                };
                if module.is_null() {
                    return None;
                }
                Some(Self {
                    $($field: {
                        // SAFETY: a loaded module and a NUL-terminated name.
                        let address =
                            unsafe { GetProcAddress(module, $symbol.as_ptr().cast()) }?;
                        // SAFETY: the export is wldap32's function of this
                        // signature.
                        unsafe {
                            std::mem::transmute::<
                                unsafe extern "system" fn() -> isize,
                                unsafe extern "C" fn($($argument),*) -> $result,
                            >(address)
                        }
                    },)*
                })
            }
        }
    };
}

library! {
    init: c"ldap_initW" => fn(*const u16, u32) -> Ldap;
    set_option: c"ldap_set_optionW" => fn(Ldap, i32, *const c_void) -> u32;
    get_option: c"ldap_get_optionW" => fn(Ldap, i32, *mut c_void) -> u32;
    connect: c"ldap_connect" => fn(Ldap, *mut Timeval) -> u32;
    bind: c"ldap_bind_sW" => fn(Ldap, *const u16, *const c_void, u32) -> u32;
    search: c"ldap_search_ext_sW" => fn(
        Ldap,
        *const u16,
        u32,
        *const u16,
        *const *const u16,
        u32,
        *mut c_void,
        *mut c_void,
        *mut Timeval,
        u32,
        *mut Message
    ) -> u32;
    first_entry: c"ldap_first_entry" => fn(Ldap, Message) -> Message;
    next_entry: c"ldap_next_entry" => fn(Ldap, Message) -> Message;
    get_values: c"ldap_get_valuesW" => fn(Ldap, Message, *const u16) -> *mut *mut u16;
    value_free: c"ldap_value_freeW" => fn(*mut *mut u16) -> u32;
    message_free: c"ldap_msgfree" => fn(Message) -> u32;
    unbind: c"ldap_unbind" => fn(Ldap) -> u32;
    memory_free: c"ldap_memfreeW" => fn(*mut u16) -> ();
    last_error: c"LdapGetLastError" => fn() -> u32;
    error_text: c"ldap_err2stringW" => fn(u32) -> *const u16;
}

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY.get_or_init(Library::load).as_ref()
}

/// Looks the SPN up in the forest's global catalog.
pub(super) fn lookup(query: Query) -> Lookup {
    search(None, None, &query.spn, query.deadline)
}

/// Looks `spn` up on `server` (a host and its global catalog port), or on
/// the forest's global catalog as Windows finds it, bound as `identity` or
/// the logon session.
fn search(
    server: Option<(&str, u32)>,
    identity: Option<&AuthIdentity>,
    spn: &str,
    deadline: Instant,
) -> Lookup {
    let Some(library) = library() else {
        return Lookup::Failed(Failure::Ldap {
            code: LDAP_NOT_SUPPORTED,
            message: Some("wldap32.dll could not be loaded".to_string()),
        });
    };
    let host = server.map(|(host, _)| wide(host));
    let port = server.map_or(LDAP_GC_PORT, |(_, port)| port);
    // SAFETY: a NUL-terminated host, or none to locate a global catalog.
    let ld = unsafe { (library.init)(host.as_ref().map_or(null(), |h| h.as_ptr()), port) };
    if ld.is_null() {
        // SAFETY: no preconditions.
        let code = unsafe { (library.last_error)() };
        return Lookup::NoDirectory(Some(failure(library, None, code)));
    }
    let session = Session { library, ld };
    let version: u32 = 3;
    session.set(LDAP_OPT_PROTOCOL_VERSION, (&raw const version).cast());
    session.set(LDAP_OPT_SIGN, LDAP_OPT_ON);
    session.set(LDAP_OPT_ENCRYPT, LDAP_OPT_ON);
    session.set(LDAP_OPT_REFERRALS, LDAP_OPT_OFF);
    if server.is_some() {
        session.set(LDAP_OPT_AREC_EXCLUSIVE, LDAP_OPT_ON);
    }
    let mut timeout = timeval(deadline);
    // SAFETY: a live session.
    let code = unsafe { (library.connect)(ld, &mut timeout) };
    if code != 0 {
        return Lookup::NoDirectory(Some(failure(library, Some(ld), code)));
    }
    let credentials = identity.map_or(null(), |identity| (identity as *const AuthIdentity).cast());
    // SAFETY: a live session; no DN, and the logon session's credentials
    // or a test's.
    let code = unsafe { (library.bind)(ld, null(), credentials, LDAP_AUTH_NEGOTIATE) };
    if code != 0 {
        return refused_or_failed(failure(library, Some(ld), code), code);
    }
    let base = wide("");
    let filter = wide(&format!("(servicePrincipalName={})", filter_value(spn)));
    let names: Vec<Vec<u16>> = ATTRIBUTES.iter().map(|name| wide(name)).collect();
    let mut attributes: Vec<*const u16> = names.iter().map(|name| name.as_ptr()).collect();
    attributes.push(null());
    let mut timeout = timeval(deadline);
    let mut result: Message = null_mut();
    // SAFETY: a live, bound session; NUL-terminated strings and a
    // NULL-terminated list that outlive the call; `result` receives the
    // answer, freed by `Answer`.
    let code = unsafe {
        (library.search)(
            ld,
            base.as_ptr(),
            LDAP_SCOPE_SUBTREE,
            filter.as_ptr(),
            attributes.as_ptr(),
            0,
            null_mut(),
            null_mut(),
            &mut timeout,
            2, // more than one is all that matters
            &mut result,
        )
    };
    let answer = Answer { library, result };
    let accounts = answer.accounts(ld);
    match code {
        0 => Lookup::Searched {
            scope: Scope::Forest,
            accounts,
        },
        LDAP_SIZELIMIT_EXCEEDED if accounts.len() >= 2 => Lookup::Searched {
            scope: Scope::Forest,
            accounts,
        },
        code => refused_or_failed(failure(library, Some(ld), code), code),
    }
}

/// A session, unbound when dropped.
struct Session {
    library: &'static Library,
    ld: Ldap,
}

impl Session {
    fn set(&self, option: i32, value: *const c_void) {
        // SAFETY: a live session; `value` is what the option takes.
        unsafe { (self.library.set_option)(self.ld, option, value) };
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session came from ldap_initW.
        unsafe { (self.library.unbind)(self.ld) };
    }
}

/// A search's answer, freed when dropped.
struct Answer {
    library: &'static Library,
    result: Message,
}

impl Answer {
    fn accounts(&self, ld: Ldap) -> Vec<Account> {
        let mut accounts = Vec::new();
        if self.result.is_null() {
            return accounts;
        }
        // SAFETY: an answer of this session.
        let mut entry = unsafe { (self.library.first_entry)(ld, self.result) };
        while !entry.is_null() && accounts.len() < 2 {
            let values = |name: &str| self.values(ld, entry, name);
            let encryption_types = values("msDS-SupportedEncryptionTypes")
                .first()
                .and_then(|value| value.trim().parse::<i64>().ok())
                .map(|value| value as u32);
            accounts.push(Account {
                name: values("sAMAccountName")
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
                kind: AccountKind::of(&values("objectClass")),
                encryption_types,
            });
            // SAFETY: an entry of this answer.
            entry = unsafe { (self.library.next_entry)(ld, entry) };
        }
        accounts
    }

    fn values(&self, ld: Ldap, entry: Message, attribute: &str) -> Vec<String> {
        let name = wide(attribute);
        // SAFETY: an entry of this answer and a NUL-terminated name.
        let values = unsafe { (self.library.get_values)(ld, entry, name.as_ptr()) };
        let mut read = Vec::new();
        if values.is_null() {
            return read;
        }
        for at in 0..1024 {
            // SAFETY: a NULL-terminated list of NUL-terminated strings.
            let value = unsafe { *values.add(at) };
            if value.is_null() {
                break;
            }
            // SAFETY: as above.
            read.push(unsafe { text(value) });
        }
        // SAFETY: the list came from ldap_get_valuesW.
        unsafe { (self.library.value_free)(values) };
        read
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if !self.result.is_null() {
            // SAFETY: the answer came from ldap_search_ext_sW.
            unsafe { (self.library.message_free)(self.result) };
        }
    }
}

/// An LDAP error with the server's words, else the library's.
fn failure(library: &Library, ld: Option<Ldap>, code: u32) -> Failure {
    let server = ld.and_then(|ld| {
        let mut words: *mut u16 = null_mut();
        // SAFETY: a live session; `words` receives a string the library
        // allocates, freed below.
        let read =
            unsafe { (library.get_option)(ld, LDAP_OPT_SERVER_ERROR, (&raw mut words).cast()) };
        if read != 0 || words.is_null() {
            return None;
        }
        // SAFETY: a NUL-terminated string from the library.
        let text = unsafe { text(words) };
        // SAFETY: as above.
        unsafe { (library.memory_free)(words) };
        Some(text).filter(|text| !text.trim().is_empty())
    });
    let message = server.or_else(|| {
        // SAFETY: a static string the library keeps, or null.
        let words = unsafe { (library.error_text)(code) };
        // SAFETY: as above.
        (!words.is_null()).then(|| unsafe { text(words) })
    });
    Failure::Ldap { code, message }
}

fn refused_or_failed(failure: Failure, code: u32) -> Lookup {
    if refusal(code) {
        Lookup::Refused(failure)
    } else {
        Lookup::Failed(failure)
    }
}

/// The time left before `deadline`, in whole seconds, one at least.
fn timeval(deadline: Instant) -> Timeval {
    let left = deadline.saturating_duration_since(Instant::now()).as_secs();
    Timeval {
        seconds: i32::try_from(left).unwrap_or(i32::MAX).max(1),
        microseconds: 0,
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated wide string's text, up to 64 KiB of it.
///
/// # Safety
///
/// `words` points to a NUL-terminated wide string.
unsafe fn text(words: *const u16) -> String {
    let mut length = 0;
    // SAFETY: up to the terminator.
    while length < 32_768 && unsafe { *words.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `length` units before the terminator.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(words, length) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPN looked up live in the Kerberos test environment's global
    /// catalog, through its published port with a test user's explicit
    /// credentials, as this computer is in no domain:
    /// `KERBEROS_TEST_LDAP_SERVER=127.0.0.1:13268`,
    /// `KERBEROS_TEST_LDAP_USER=EXAMPLE\testuser` and
    /// `KERBEROS_TEST_USER_PASSWORD`.
    #[test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    fn kerberos_spn_lookup_searches_the_global_catalog_through_wldap32() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let variable = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let server = variable("KERBEROS_TEST_LDAP_SERVER");
        let (host, port) = server.rsplit_once(':').expect("host:port");
        let user = variable("KERBEROS_TEST_LDAP_USER");
        let (domain, user) = user.split_once('\\').expect("DOMAIN\\user");
        let password = variable("KERBEROS_TEST_USER_PASSWORD");
        let [user, domain, password] = [user, domain, password.as_str()]
            .map(|value| value.encode_utf16().collect::<Vec<u16>>());
        let identity = AuthIdentity {
            user: user.as_ptr(),
            user_length: user.len() as u32,
            domain: domain.as_ptr(),
            domain_length: domain.len() as u32,
            password: password.as_ptr(),
            password_length: password.len() as u32,
            flags: 2, // SEC_WINNT_AUTH_IDENTITY_UNICODE
        };
        let port: u32 = port.parse().unwrap();
        let deadline = || Instant::now() + std::time::Duration::from_secs(10);
        match search(
            Some((host, port)),
            Some(&identity),
            "MSSQLSvc/sql.example.local:1433",
            deadline(),
        ) {
            Lookup::Searched { scope, accounts } => {
                assert_eq!(scope, Scope::Forest);
                assert_eq!(accounts.len(), 1, "{accounts:?}");
                assert_eq!(accounts[0].name, "SQL$");
                assert_eq!(accounts[0].kind, AccountKind::Computer);
            }
            other => panic!("{other:?}"),
        }
        match search(
            Some((host, port)),
            Some(&identity),
            "MSSQLSvc/nobody.example.local:1433",
            deadline(),
        ) {
            Lookup::Searched { accounts, .. } => assert!(accounts.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    /// Outside a domain, no global catalog is found to search.
    #[test]
    #[ignore = "Requires a computer in no domain - set KERBEROS_TEST=1 to run"]
    fn kerberos_spn_lookup_outside_a_domain_finds_no_directory() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let looked_up = lookup(Query {
            spn: "MSSQLSvc/sql.example.local:1433".to_string(),
            realm: None,
            deadline: Instant::now() + std::time::Duration::from_secs(10),
        });
        assert!(matches!(looked_up, Lookup::NoDirectory(_)), "{looked_up:?}");
    }
}

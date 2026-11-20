// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The caller's Kerberos credentials cache (Linux, macOS): the one its
//! Kerberos library uses, `KRB5CCNAME` or else the configured default.
//!
//! On Linux, MIT krb5's own library (`libkrb5.so.3`, loaded at run time)
//! gives the cache's type, name and principal and lists its tickets. A
//! ticket entry is MIT's `krb5_creds`, read as MIT lays it out on Linux;
//! its session key is never read. Elsewhere (macOS, whose Kerberos library
//! is another implementation), and where MIT's library cannot be loaded,
//! GSSAPI gives the default credentials' principal and lifetime only.

use mssql_tds::security::SecurityError;

use super::credentials::{Credentials, Failure, State, Store, Ticket, no_cache, unix_now};
use super::{GSS_S_CREDENTIALS_EXPIRED, GSS_S_NO_CRED, KRB5KRB_AP_ERR_TKT_EXPIRED, minor_text};

/// GSSAPI's Kerberos errors for a cache without a ticket-granting ticket
/// (MIT's `k5g` table: `KG_TGT_MISSING`, `KG_EMPTY_CCACHE`).
const KG_TGT_MISSING: i32 = 39_756_034;
const KG_EMPTY_CCACHE: i32 = 39_756_044;

/// The caller's credentials, and the service ticket for `spn` where the
/// tickets are listed.
pub(super) fn read(spn: Option<String>) -> Credentials {
    let named = std::env::var("KRB5CCNAME")
        .ok()
        .filter(|name| !name.is_empty());
    #[cfg(target_os = "linux")]
    if let Some(credentials) = mit::read(spn.as_deref(), named.as_deref()) {
        return credentials;
    }
    // Without MIT's library the tickets are not listed: no service ticket
    // to find.
    let _ = spn;
    read_default(named.as_deref())
}

/// The cache `KRB5CCNAME` names, as its name alone shows it, or the
/// configured default, whose type and file the library has to tell.
fn named_store(named: Option<&str>) -> Store {
    let (kind, path) = named.map_or((None, None), |name| {
        let (kind, residual) = split_name(name);
        let path = cache_path(&kind, residual);
        (Some(kind), path)
    });
    Store::Cache {
        kind,
        path,
        named: named.is_some(),
    }
}

/// The default credentials as GSSAPI finds them: their principal and
/// lifetime. The cache's type and file are known only from `KRB5CCNAME`.
fn read_default(named: Option<&str>) -> Credentials {
    let mut credentials = Credentials::new(named_store(named), State::Unreadable);
    match mssql_tds::security::unix::default_credential() {
        Ok(credential) => {
            let lifetime = credential.lifetime_seconds;
            let expired = lifetime == 0;
            credentials.principal = Some(credential.principal);
            credentials.tgt = Some(Ticket {
                ends: (!expired && lifetime != u32::MAX)
                    .then(|| unix_now().saturating_add(i64::from(lifetime))),
                expired,
                ..Ticket::default()
            });
            credentials.state = if expired {
                State::Expired
            } else {
                State::Valid
            };
        }
        Err(SecurityError::LoadLibraryFailed(_)) => credentials.state = State::Unavailable,
        Err(SecurityError::GssapiError {
            major,
            minor,
            message,
        }) => {
            let minor = minor as i32;
            let routine = (major >> 16) & 0xFF;
            if no_cache(minor) {
                credentials.state = State::NoCache;
            } else if routine == GSS_S_NO_CRED || matches!(minor, KG_TGT_MISSING | KG_EMPTY_CCACHE)
            {
                credentials.state = State::NoTicket;
            } else if routine == GSS_S_CREDENTIALS_EXPIRED || minor == KRB5KRB_AP_ERR_TKT_EXPIRED {
                credentials.state = State::Expired;
                credentials.tgt = Some(Ticket {
                    expired: true,
                    ..Ticket::default()
                });
            } else {
                credentials.failure = Some(Failure::Gssapi {
                    major,
                    minor,
                    message: minor_text(&message),
                });
            }
        }
        Err(_) => {}
    }
    credentials
}

/// A cache name's type and residual: `TYPE:residual`, or a path, which is a
/// `FILE` cache.
fn split_name(name: &str) -> (String, &str) {
    match name.split_once(':') {
        Some((kind, residual)) if !name.starts_with('/') => (kind.to_ascii_uppercase(), residual),
        _ => ("FILE".to_string(), name),
    }
}

/// The file a `FILE` or `DIR` cache's residual names. A `DIR` cache's own
/// residual is its subsidiary file after a `:`.
fn cache_path(kind: &str, residual: &str) -> Option<String> {
    let path = residual.strip_prefix(':').unwrap_or(residual);
    (matches!(kind, "FILE" | "DIR") && path.starts_with('/')).then(|| path.to_string())
}

#[cfg(target_os = "linux")]
mod mit {
    use std::ffi::{CStr, c_char, c_uint, c_void};
    use std::ptr::null_mut;
    use std::sync::OnceLock;

    use super::super::credentials::{
        Credentials, Failure, Listed, State, Store, Ticket, no_cache, ticket_encryption, unix_now,
    };
    use super::{cache_path, named_store};

    type Code = i32;
    type Context = *mut c_void;
    type Ccache = *mut c_void;
    type Principal = *mut c_void;
    type Cursor = *mut c_void;

    /// `KRB5_CC_END`: the cache has no more entries.
    const KRB5_CC_END: Code = -1_765_328_242;

    // MIT krb5's `krb5_creds` and the structures it holds (`krb5.h`).
    #[repr(C)]
    struct Keyblock {
        magic: i32,
        enctype: i32,
        length: c_uint,
        contents: *mut u8,
    }

    #[repr(C)]
    struct Times {
        authtime: i32,
        starttime: i32,
        endtime: i32,
        renew_till: i32,
    }

    #[repr(C)]
    struct Data {
        magic: i32,
        length: c_uint,
        data: *mut c_char,
    }

    #[repr(C)]
    struct Creds {
        magic: i32,
        client: Principal,
        server: Principal,
        keyblock: Keyblock,
        times: Times,
        is_skey: c_uint,
        ticket_flags: i32,
        addresses: *mut *mut c_void,
        ticket: Data,
        second_ticket: Data,
        authdata: *mut *mut c_void,
    }

    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<Creds>() == 120);

    /// MIT krb5's functions, resolved from `libkrb5.so.3`.
    macro_rules! library {
        ($($field:ident: $symbol:literal => fn($($argument:ty),*) $(-> $result:ty)?;)*) => {
            struct Library {
                $($field: unsafe extern "C" fn($($argument),*) $(-> $result)?,)*
            }

            impl Library {
                fn load(handle: *mut c_void) -> Option<Self> {
                    Some(Self {
                        $($field: {
                            // SAFETY: `handle` is an open library and the name
                            // a C string.
                            let address = unsafe { libc::dlsym(handle, $symbol.as_ptr()) };
                            if address.is_null() {
                                return None;
                            }
                            // SAFETY: the symbol is MIT krb5's function of
                            // this signature.
                            unsafe {
                                std::mem::transmute::<
                                    *mut c_void,
                                    unsafe extern "C" fn($($argument),*) $(-> $result)?,
                                >(address)
                            }
                        },)*
                    })
                }
            }
        };
    }

    library! {
        init_context: c"krb5_init_context" => fn(*mut Context) -> Code;
        free_context: c"krb5_free_context" => fn(Context);
        get_error_message: c"krb5_get_error_message" => fn(Context, Code) -> *const c_char;
        free_error_message: c"krb5_free_error_message" => fn(Context, *const c_char);
        cc_default: c"krb5_cc_default" => fn(Context, *mut Ccache) -> Code;
        cc_close: c"krb5_cc_close" => fn(Context, Ccache) -> Code;
        cc_get_type: c"krb5_cc_get_type" => fn(Context, Ccache) -> *const c_char;
        cc_get_name: c"krb5_cc_get_name" => fn(Context, Ccache) -> *const c_char;
        cc_get_principal: c"krb5_cc_get_principal" => fn(Context, Ccache, *mut Principal) -> Code;
        cc_start_seq_get: c"krb5_cc_start_seq_get" => fn(Context, Ccache, *mut Cursor) -> Code;
        cc_next_cred: c"krb5_cc_next_cred" => fn(Context, Ccache, *mut Cursor, *mut Creds) -> Code;
        cc_end_seq_get: c"krb5_cc_end_seq_get" => fn(Context, Ccache, *mut Cursor) -> Code;
        free_cred_contents: c"krb5_free_cred_contents" => fn(Context, *mut Creds);
        free_principal: c"krb5_free_principal" => fn(Context, Principal);
        unparse_name: c"krb5_unparse_name" => fn(Context, Principal, *mut *mut c_char) -> Code;
        free_unparsed_name: c"krb5_free_unparsed_name" => fn(Context, *mut c_char);
    }

    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();

    /// MIT krb5's library, loaded once, or `None` where it is not installed.
    fn library() -> Option<&'static Library> {
        LIBRARY
            .get_or_init(|| {
                // SAFETY: a constant library name; the handle stays open for
                // the process's life.
                let handle = unsafe {
                    libc::dlopen(c"libkrb5.so.3".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
                };
                if handle.is_null() {
                    return None;
                }
                let library = Library::load(handle);
                if library.is_none() {
                    // SAFETY: `handle` was opened above and nothing from it
                    // is kept.
                    unsafe { libc::dlclose(handle) };
                }
                library
            })
            .as_ref()
    }

    /// The default cache's credentials, or `None` where MIT krb5's library
    /// is not installed; `named`, the cache `KRB5CCNAME` names.
    pub(super) fn read(spn: Option<&str>, named: Option<&str>) -> Option<Credentials> {
        let library = library()?;
        // Until the library resolves the cache, its name says what it is.
        let mut credentials = Credentials::new(named_store(named), State::Unreadable);
        credentials.lists_tickets = true;
        let mut context: Context = null_mut();
        // SAFETY: `context` receives a new context or stays null.
        let code = unsafe { (library.init_context)(&mut context) };
        if code != 0 {
            credentials.failure = Some(Failure::Krb5 {
                code,
                message: None,
            });
            return Some(credentials);
        }
        let krb5 = Krb5 { library, context };
        let mut raw: Ccache = null_mut();
        // SAFETY: a live context; `raw` receives the default cache.
        let code = unsafe { (library.cc_default)(context, &mut raw) };
        if code != 0 {
            // A DIR collection whose directory does not exist fails here.
            if no_cache(code) {
                credentials.state = State::NoCache;
            } else {
                credentials.failure = Some(krb5.failure(code));
            }
            return Some(credentials);
        }
        let cache = Cache { krb5: &krb5, raw };
        let kind = cache.text(library.cc_get_type);
        let path = kind
            .as_deref()
            .zip(cache.text(library.cc_get_name))
            .and_then(|(kind, residual)| cache_path(kind, &residual));
        credentials.store = Store::Cache {
            kind,
            path,
            named: named.is_some(),
        };
        let mut principal: Principal = null_mut();
        // SAFETY: a live context and cache; `principal` receives a new one.
        let code = unsafe { (library.cc_get_principal)(context, raw, &mut principal) };
        if code != 0 {
            if no_cache(code) {
                credentials.state = State::NoCache;
            } else {
                credentials.failure = Some(krb5.failure(code));
            }
            return Some(credentials);
        }
        credentials.principal = krb5.name(principal);
        // SAFETY: `principal` came from krb5_cc_get_principal.
        unsafe { (library.free_principal)(context, principal) };
        match cache.tickets() {
            Ok(tickets) => credentials.read_tickets(&tickets, spn),
            Err(code) => credentials.failure = Some(krb5.failure(code)),
        }
        Some(credentials)
    }

    /// A krb5 context, freed when dropped.
    struct Krb5 {
        library: &'static Library,
        context: Context,
    }

    impl Krb5 {
        fn failure(&self, code: Code) -> Failure {
            // SAFETY: a live context; the message is freed below.
            let text = unsafe { (self.library.get_error_message)(self.context, code) };
            let message = (!text.is_null()).then(|| {
                // SAFETY: krb5 returns a C string.
                let message = unsafe { CStr::from_ptr(text) }
                    .to_string_lossy()
                    .into_owned();
                // SAFETY: `text` came from krb5_get_error_message.
                unsafe { (self.library.free_error_message)(self.context, text) };
                message
            });
            Failure::Krb5 { code, message }
        }

        /// A principal's name, `name@REALM`.
        fn name(&self, principal: Principal) -> Option<String> {
            let mut name: *mut c_char = null_mut();
            // SAFETY: a live context and principal; `name` receives a new
            // string.
            let code = unsafe { (self.library.unparse_name)(self.context, principal, &mut name) };
            if code != 0 || name.is_null() {
                return None;
            }
            // SAFETY: krb5 returns a C string.
            let text = unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: `name` came from krb5_unparse_name.
            unsafe { (self.library.free_unparsed_name)(self.context, name) };
            Some(text)
        }
    }

    impl Drop for Krb5 {
        fn drop(&mut self) {
            // SAFETY: the context came from krb5_init_context.
            unsafe { (self.library.free_context)(self.context) };
        }
    }

    /// A credentials cache, closed when dropped.
    struct Cache<'a> {
        krb5: &'a Krb5,
        raw: Ccache,
    }

    impl Cache<'_> {
        /// A string the library keeps for the cache: its type or name.
        fn text(
            &self,
            get: unsafe extern "C" fn(Context, Ccache) -> *const c_char,
        ) -> Option<String> {
            // SAFETY: a live context and cache; the string is the library's.
            let text = unsafe { get(self.krb5.context, self.raw) };
            // SAFETY: krb5 returns a C string it owns.
            (!text.is_null()).then(|| {
                unsafe { CStr::from_ptr(text) }
                    .to_string_lossy()
                    .into_owned()
            })
        }

        /// The tickets in the cache, without its configuration entries, or
        /// the code that stopped the listing.
        fn tickets(&self) -> Result<Vec<Listed>, Code> {
            let Krb5 { library, context } = *self.krb5;
            let mut cursor: Cursor = null_mut();
            // SAFETY: a live context and cache; `cursor` receives a new one.
            let code = unsafe { (library.cc_start_seq_get)(context, self.raw, &mut cursor) };
            if code != 0 {
                return Err(code);
            }
            let now = unix_now();
            let mut tickets = Vec::new();
            let ended = loop {
                // SAFETY: every field of `krb5_creds` may be zero.
                let mut creds: Creds = unsafe { std::mem::zeroed() };
                // SAFETY: a live cursor; krb5 fills `creds`.
                let code =
                    unsafe { (library.cc_next_cred)(context, self.raw, &mut cursor, &mut creds) };
                if code == KRB5_CC_END {
                    break Ok(());
                }
                if code != 0 {
                    break Err(code);
                }
                if let Some(listed) = self.listed(&creds, now) {
                    tickets.push(listed);
                }
                // SAFETY: `creds` was filled by krb5_cc_next_cred.
                unsafe { (library.free_cred_contents)(context, &mut creds) };
            };
            // SAFETY: the cursor came from krb5_cc_start_seq_get.
            unsafe { (library.cc_end_seq_get)(context, self.raw, &mut cursor) };
            ended.map(|()| tickets)
        }

        fn listed(&self, creds: &Creds, now: i64) -> Option<Listed> {
            let server = self.krb5.name(creds.server)?;
            if server.starts_with("X-CACHECONF:") {
                return None;
            }
            // MIT reads timestamps as unsigned, past 2038.
            let ends = (creds.times.endtime != 0).then(|| i64::from(creds.times.endtime as u32));
            let encoded = (!creds.ticket.data.is_null() && creds.ticket.length > 0).then(|| {
                // SAFETY: krb5 holds `length` bytes of the encoded ticket.
                unsafe {
                    std::slice::from_raw_parts(
                        creds.ticket.data.cast::<u8>(),
                        creds.ticket.length as usize,
                    )
                }
            });
            let (encryption, key_version) = encoded
                .and_then(ticket_encryption)
                .map_or((None, None), |(etype, kvno)| (Some(etype), kvno));
            Some(Listed {
                client: self.krb5.name(creds.client).unwrap_or_default(),
                server,
                ticket: Ticket {
                    ends,
                    expired: ends.is_some_and(|ends| ends <= now),
                    encryption,
                    session_key: Some(creds.keyblock.enctype),
                    key_version,
                },
            })
        }
    }

    impl Drop for Cache<'_> {
        fn drop(&mut self) {
            // SAFETY: the cache came from krb5_cc_default.
            unsafe { (self.krb5.library.cc_close)(self.krb5.context, self.raw) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GSSAPI, which macOS reads with, finds the same default credentials
    /// that MIT krb5's own library lists, whatever state they are in.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    fn kerberos_gssapi_finds_the_credentials_mit_lists() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let named = std::env::var("KRB5CCNAME").ok();
        let listed = mit::read(None, named.as_deref()).expect("MIT krb5 is installed");
        let found = read_default(named.as_deref());
        // Without a ticket-granting ticket, GSSAPI still finds another
        // ticket (`kinit -S`) valid: it does not tell them apart.
        if listed.state == State::NoTicket && found.state == State::Valid {
            return;
        }
        assert_eq!(found.state, listed.state, "{found:#?}\n{listed:#?}");
        if listed.state == State::Valid {
            assert_eq!(found.principal, listed.principal);
            let ends = |c: &Credentials| c.tgt.and_then(|t| t.ends).unwrap();
            assert!((ends(&found) - ends(&listed)).abs() <= 2, "{found:#?}");
        }
    }

    #[test]
    fn a_cache_name_gives_its_type_and_file() {
        assert_eq!(
            split_name("FILE:/tmp/krb5cc_0"),
            ("FILE".to_string(), "/tmp/krb5cc_0")
        );
        assert_eq!(
            split_name("/tmp/krb5cc_0"),
            ("FILE".to_string(), "/tmp/krb5cc_0")
        );
        assert_eq!(split_name("kcm:1000"), ("KCM".to_string(), "1000"));
        assert_eq!(
            cache_path("FILE", "/tmp/krb5cc_0").as_deref(),
            Some("/tmp/krb5cc_0")
        );
        assert_eq!(
            cache_path("DIR", ":/run/user/1000/krb5cc/tkt1").as_deref(),
            Some("/run/user/1000/krb5cc/tkt1")
        );
        assert_eq!(cache_path("KCM", "1000"), None);
        assert_eq!(cache_path("KEYRING", "persistent:1000"), None);
        assert_eq!(cache_path("FILE", "relative"), None);
    }
}

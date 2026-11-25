// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A minimal LDAP client (RFC 4511) for one read-only search, off Windows:
//! the realm's directory servers found through DNS (RFC 2782), a SASL
//! GSSAPI bind (RFC 4752) with the user's own Kerberos credentials and the
//! security layer the server offers (sealed, else signed), then the search.
//! Every read and write is bounded by the lookup's deadline.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mssql_tds::security::{GssapiContext, SecurityContext, SecurityError};

use super::ber;
use super::spn_lookup::{Account, AccountKind, Failure, Lookup, Query, Scope, refusal};

/// The most a message, or a protected buffer, may take: far more than one
/// search's answer needs.
const MAX_MESSAGE: usize = 16 << 20;

/// The most servers of each kind tried.
const SERVERS_PER_KIND: usize = 3;

/// The attributes read from an account.
const ATTRIBUTES: [&str; 3] = [
    "sAMAccountName",
    "objectClass",
    "msDS-SupportedEncryptionTypes",
];

/// A directory server, and how much of the directory it searches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Server {
    host: String,
    port: u16,
    scope: Scope,
    /// The search base: the whole forest (`""`) on a global catalog, the
    /// domain's naming context on a domain controller.
    base: String,
}

/// Looks the SPN up in the directory of the client's realm.
pub(super) fn lookup(query: Query) -> Lookup {
    let Some(realm) = query.realm.as_deref() else {
        return Lookup::NoCredentials;
    };
    let servers = directory_servers(realm);
    let mut unreachable = None;
    for server in &servers {
        if Instant::now() >= query.deadline {
            break;
        }
        match Connection::open(server, query.deadline) {
            Ok(mut connection) => return connection.lookup(server, &query.spn),
            Err(failure) => unreachable = Some(failure),
        }
    }
    Lookup::NoDirectory(unreachable)
}

/// The realm's global catalogs, then its domain controllers, as DNS lists
/// them: Active Directory's own records, which no other directory has, so
/// that an empty answer means that no account has the SPN.
fn directory_servers(realm: &str) -> Vec<Server> {
    let domain = realm.to_ascii_lowercase();
    let naming_context = domain
        .split('.')
        .filter(|label| !label.is_empty())
        .map(|label| format!("DC={label}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut servers = Vec::new();
    for (service, scope, base) in [
        ("_gc._tcp", Scope::Forest, String::new()),
        ("_ldap._tcp.dc._msdcs", Scope::Domain, naming_context),
    ] {
        for record in srv(&format!("{service}.{domain}"))
            .into_iter()
            .take(SERVERS_PER_KIND)
        {
            servers.push(Server {
                host: record.target,
                port: record.port,
                scope,
                base: base.clone(),
            });
        }
    }
    servers
}

/// An SRV record (RFC 2782).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Srv {
    priority: u16,
    weight: u16,
    port: u16,
    target: String,
}

/// The SRV records DNS has for `name`, in the order to try them: by
/// priority, then the heaviest first.
fn srv(name: &str) -> Vec<Srv> {
    let (Some(query), Ok(name)) = (res_query(), std::ffi::CString::new(name)) else {
        return Vec::new();
    };
    let mut answer = vec![0u8; 4096];
    loop {
        // SAFETY: `answer` holds `len` writable bytes and `name` is a C string.
        let length = unsafe {
            query(
                name.as_ptr(),
                1,  // C_IN
                33, // T_SRV
                answer.as_mut_ptr(),
                c_int::try_from(answer.len()).unwrap_or(c_int::MAX),
            )
        };
        let Ok(length) = usize::try_from(length) else {
            return Vec::new();
        };
        if length > answer.len() && answer.len() < 65_536 {
            answer.resize(65_536, 0);
            continue;
        }
        return srv_records(&answer[..length.min(answer.len())]).unwrap_or_default();
    }
}

type ResQuery = unsafe extern "C" fn(*const c_char, c_int, c_int, *mut u8, c_int) -> c_int;

/// The resolver's `res_query`: libc's own (glibc 2.34 and later, musl), else
/// the resolver library's, loaded at run time.
fn res_query() -> Option<ResQuery> {
    static QUERY: OnceLock<Option<ResQuery>> = OnceLock::new();
    *QUERY.get_or_init(|| {
        const NAMES: [&CStr; 3] = [c"res_query", c"__res_query", c"res_9_query"];
        #[cfg(target_os = "macos")]
        const LIBRARY: &CStr = c"/usr/lib/libresolv.9.dylib";
        #[cfg(not(target_os = "macos"))]
        const LIBRARY: &CStr = c"libresolv.so.2";
        let find = |handle: *mut c_void| {
            NAMES.iter().find_map(|name| {
                // SAFETY: a valid handle and a C string.
                let symbol = unsafe { libc::dlsym(handle, name.as_ptr()) };
                // SAFETY: the resolver's res_query, of this signature.
                (!symbol.is_null())
                    .then(|| unsafe { std::mem::transmute::<*mut c_void, ResQuery>(symbol) })
            })
        };
        find(libc::RTLD_DEFAULT).or_else(|| {
            // SAFETY: a constant library name; the handle stays open for
            // the process's life.
            let handle =
                unsafe { libc::dlopen(LIBRARY.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            (!handle.is_null()).then(|| find(handle)).flatten()
        })
    })
}

/// The SRV records of a DNS answer (RFC 1035 4.1), in the order to try them.
fn srv_records(message: &[u8]) -> Option<Vec<Srv>> {
    let count = |at: usize| {
        Some(u16::from_be_bytes([
            *message.get(at)?,
            *message.get(at + 1)?,
        ]))
    };
    let questions = count(4)?;
    let answers = count(6)?;
    let mut at = 12;
    for _ in 0..questions {
        at = skip_name(message, at)? + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        at = skip_name(message, at)?;
        let kind = count(at)?;
        let length = usize::from(count(at + 8)?);
        let data = at + 10;
        if message.len() < data + length {
            return None;
        }
        if kind == 33 && length >= 7 {
            let target = read_name(message, data + 6)?;
            if !target.is_empty() {
                records.push(Srv {
                    priority: count(data)?,
                    weight: count(data + 2)?,
                    port: count(data + 4)?,
                    target,
                });
            }
        }
        at = data + length;
    }
    records.sort_by_key(|r| (r.priority, std::cmp::Reverse(r.weight)));
    Some(records)
}

/// Where the name at `at` ends.
fn skip_name(message: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let length = *message.get(at)?;
        match length {
            0 => return Some(at + 1),
            l if l & 0xC0 == 0xC0 => return Some(at + 2),
            l => at += 1 + usize::from(l),
        }
    }
}

/// The name at `at`, following compression pointers, without the root's
/// trailing dot.
fn read_name(message: &[u8], mut at: usize) -> Option<String> {
    let mut labels: Vec<String> = Vec::new();
    for _ in 0..128 {
        let length = *message.get(at)?;
        match length {
            0 => return Some(labels.join(".")),
            l if l & 0xC0 == 0xC0 => {
                at = (usize::from(l & 0x3F) << 8) | usize::from(*message.get(at + 1)?);
            }
            l => {
                let label = message.get(at + 1..at + 1 + usize::from(l))?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                at += 1 + usize::from(l);
            }
        }
    }
    None
}

/// An LDAP message's operation, as far as the lookup reads it.
#[derive(Debug, PartialEq, Eq)]
enum Operation {
    Bind {
        result: LdapResult,
        server_credentials: Option<Vec<u8>>,
    },
    Entry(Vec<(String, Vec<String>)>),
    Done(LdapResult),
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LdapResult {
    code: u32,
    message: String,
}

impl LdapResult {
    fn failure(self) -> Failure {
        Failure::Ldap {
            code: self.code,
            message: Some(self.message),
        }
    }
}

/// An LDAP message's id and operation.
fn parse_message(pdu: &[u8]) -> Option<(i64, Operation)> {
    let (content, _) = ber::element(pdu, 0x30)?;
    let id = ber::integer(content)?;
    let (_, _, rest) = ber::next(content)?;
    let (tag, operation, _) = ber::next(rest)?;
    let operation = match tag {
        0x61 => {
            let (result, rest) = ldap_result(operation)?;
            Operation::Bind {
                result,
                server_credentials: ber::field(rest, 0x87).map(<[u8]>::to_vec),
            }
        }
        0x64 => Operation::Entry(attributes(operation)?),
        0x65 => Operation::Done(ldap_result(operation)?.0),
        _ => Operation::Other,
    };
    Some((id, operation))
}

/// An LDAPResult's code and diagnostic message, and what follows them.
fn ldap_result(operation: &[u8]) -> Option<(LdapResult, &[u8])> {
    let code = u32::try_from(ber::tagged_integer(operation, 0x0A)?).ok()?;
    let (_, _, rest) = ber::next(operation)?;
    let (_, rest) = ber::element(rest, 0x04)?;
    let (message, rest) = ber::element(rest, 0x04)?;
    Some((
        LdapResult {
            code,
            message: String::from_utf8_lossy(message).into_owned(),
        },
        rest,
    ))
}

/// A search entry's attributes and their values.
fn attributes(entry: &[u8]) -> Option<Vec<(String, Vec<String>)>> {
    let (_, rest) = ber::element(entry, 0x04)?;
    let (mut list, _) = ber::element(rest, 0x30)?;
    let mut attributes = Vec::new();
    while !list.is_empty() {
        let (attribute, rest) = ber::element(list, 0x30)?;
        let (name, values) = ber::element(attribute, 0x04)?;
        let (mut values, _) = ber::element(values, 0x31)?;
        let mut read = Vec::new();
        while !values.is_empty() {
            let (value, rest) = ber::element(values, 0x04)?;
            read.push(String::from_utf8_lossy(value).into_owned());
            values = rest;
        }
        attributes.push((String::from_utf8_lossy(name).into_owned(), read));
        list = rest;
    }
    Some(attributes)
}

/// An account from a search entry's attributes.
fn account(attributes: &[(String, Vec<String>)]) -> Account {
    let values = |name: &str| {
        attributes
            .iter()
            .find(|(attribute, _)| attribute.eq_ignore_ascii_case(name))
            .map_or(&[][..], |(_, values)| values.as_slice())
    };
    Account {
        name: values("sAMAccountName")
            .first()
            .cloned()
            .unwrap_or_default(),
        kind: AccountKind::of(values("objectClass")),
        // A signed 32-bit number in the directory.
        encryption_types: values("msDS-SupportedEncryptionTypes")
            .first()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .map(|value| value as u32),
    }
}

/// A SASL bind request with GSSAPI's `credentials`.
fn bind_request(credentials: &[u8]) -> Vec<u8> {
    let mut sasl = ber::tlv(0x04, b"GSSAPI");
    sasl.extend(ber::tlv(0x04, credentials));
    let mut content = ber::number(0x02, 3);
    content.extend(ber::tlv(0x04, b""));
    content.extend(ber::tlv(0xA3, &sasl));
    ber::tlv(0x60, &content)
}

/// A subtree search of `base` for the accounts whose `attribute` is
/// `value`, two at most, within `seconds`.
fn search_request(base: &str, attribute: &str, value: &str, seconds: i64) -> Vec<u8> {
    let mut content = ber::tlv(0x04, base.as_bytes());
    content.extend(ber::number(0x0A, 2)); // wholeSubtree
    content.extend(ber::number(0x0A, 0)); // neverDerefAliases
    content.extend(ber::number(0x02, 2)); // more than one is all that matters
    content.extend(ber::number(0x02, seconds));
    content.extend(ber::tlv(0x01, &[0x00])); // typesOnly: FALSE
    let mut assertion = ber::tlv(0x04, attribute.as_bytes());
    assertion.extend(ber::tlv(0x04, value.as_bytes()));
    content.extend(ber::tlv(0xA3, &assertion)); // equalityMatch
    let attributes: Vec<u8> = ATTRIBUTES
        .iter()
        .flat_map(|attribute| ber::tlv(0x04, attribute.as_bytes()))
        .collect();
    content.extend(ber::tlv(0x30, &attributes));
    ber::tlv(0x63, &content)
}

/// The security layer RFC 4752 lets the client choose from the server's
/// offer: confidentiality, else integrity; and the reply that chooses it,
/// with the largest buffer the client takes (64 KiB) and no authorization
/// identity. An offer of neither is refused: an answer that is not signed
/// could be anyone's.
fn choose_layer(offer: &[u8]) -> Option<(bool, [u8; 4])> {
    let [layers, _, _, _] = offer else {
        return None;
    };
    let (chosen, seal) = if layers & 4 != 0 {
        (4, true)
    } else if layers & 2 != 0 {
        (2, false)
    } else {
        return None;
    };
    Some((seal, [chosen, 0x01, 0x00, 0x00]))
}

/// The security layer a bind established: messages are signed, and sealed
/// when `seal`.
struct Layer {
    context: GssapiContext,
    seal: bool,
}

/// One connection to a directory server.
struct Connection {
    stream: TcpStream,
    deadline: Instant,
    /// The bind's security layer, once established; the bind's own
    /// messages go without one.
    layer: Option<Layer>,
    /// What was read (and unwrapped) but not yet taken as a message.
    received: Vec<u8>,
    next_id: i64,
}

impl Connection {
    fn open(server: &Server, deadline: Instant) -> Result<Self, Failure> {
        let addresses = (server.host.as_str(), server.port)
            .to_socket_addrs()
            .map_err(os_failure)?;
        let mut last = None;
        for address in addresses {
            let Some(left) = left(deadline) else {
                break;
            };
            match TcpStream::connect_timeout(&address, left) {
                Ok(stream) => {
                    return Ok(Self {
                        stream,
                        deadline,
                        layer: None,
                        received: Vec::new(),
                        next_id: 1,
                    });
                }
                Err(error) => last = Some(os_failure(error)),
            }
        }
        Err(last.unwrap_or_else(|| os_failure(timed_out())))
    }

    /// Binds, then searches for the SPN, and says what it came to.
    fn lookup(&mut self, server: &Server, spn: &str) -> Lookup {
        let looked_up = self
            .bind(&server.host)
            .and_then(|()| self.search(server, "servicePrincipalName", spn));
        let _ = self.send(&[0x42, 0x00]); // UnbindRequest, as far as time allows
        looked_up.unwrap_or_else(|lookup| lookup)
    }

    /// A SASL GSSAPI bind, as RFC 4752 runs it. It succeeds only once the
    /// server proved itself (mutual authentication) and a signed or sealed
    /// layer was agreed, so that the search's answer is the server's.
    fn bind(&mut self, host: &str) -> Result<(), Lookup> {
        let mut context = GssapiContext::for_ldap(host).map_err(security_failure)?;
        let mut token = context.generate_token(None).map_err(security_failure)?.data;
        for _ in 0..8 {
            let id = self.send(&bind_request(&token)).map_err(Lookup::Failed)?;
            let (result, credentials) = match self.receive(id).map_err(Lookup::Failed)? {
                Operation::Bind {
                    result,
                    server_credentials,
                } => (result, server_credentials.unwrap_or_default()),
                _ => return Err(Lookup::Failed(protocol_failure())),
            };
            match result.code {
                // Success before then is not trusted.
                0 => return Err(Lookup::Failed(protocol_failure())),
                14 if !context.is_complete() => {
                    token = context
                        .generate_token(Some(&credentials))
                        .map_err(security_failure)?
                        .data;
                }
                14 => {
                    let (offer, _) = context.unwrap(&credentials).map_err(security_failure)?;
                    let (seal, reply) =
                        choose_layer(&offer).ok_or_else(|| Lookup::Failed(protocol_failure()))?;
                    let (wrapped, _) = context.wrap(false, &reply).map_err(security_failure)?;
                    let id = self.send(&bind_request(&wrapped)).map_err(Lookup::Failed)?;
                    match self.receive(id).map_err(Lookup::Failed)? {
                        Operation::Bind { result, .. } if result.code == 0 => {
                            self.layer = Some(Layer { context, seal });
                            return Ok(());
                        }
                        Operation::Bind { result, .. } => return Err(ldap_failure(result)),
                        _ => return Err(Lookup::Failed(protocol_failure())),
                    }
                }
                _ => return Err(ldap_failure(result)),
            }
        }
        Err(Lookup::Failed(protocol_failure()))
    }

    /// Searches for the accounts whose `attribute` is `value`.
    fn search(&mut self, server: &Server, attribute: &str, value: &str) -> Result<Lookup, Lookup> {
        let seconds = left(self.deadline)
            .map_or(1, |left| i64::try_from(left.as_secs()).unwrap_or(i64::MAX))
            .clamp(1, i64::from(i32::MAX));
        let id = self
            .send(&search_request(&server.base, attribute, value, seconds))
            .map_err(Lookup::Failed)?;
        let mut accounts = Vec::new();
        loop {
            match self.receive(id).map_err(Lookup::Failed)? {
                Operation::Entry(attributes) => {
                    accounts.push(account(&attributes));
                    // More than one is all that matters: the size limit's
                    // end, or a server that ignores it, is not read.
                    if accounts.len() == 2 {
                        return Ok(Lookup::Searched {
                            scope: server.scope,
                            accounts,
                        });
                    }
                }
                Operation::Done(result) => {
                    return match result.code {
                        0 => Ok(Lookup::Searched {
                            scope: server.scope,
                            accounts,
                        }),
                        _ => Err(ldap_failure(result)),
                    };
                }
                // Continuation references point outside what is searched.
                Operation::Other | Operation::Bind { .. } => {}
            }
        }
    }

    /// Sends `operation` as a message, protected when a layer was
    /// established; its id.
    fn send(&mut self, operation: &[u8]) -> Result<i64, Failure> {
        let id = self.next_id;
        self.next_id += 1;
        let mut content = ber::number(0x02, id);
        content.extend_from_slice(operation);
        let message = ber::tlv(0x30, &content);
        let bytes = match &mut self.layer {
            None => message,
            Some(layer) => {
                let (token, _) = layer
                    .context
                    .wrap(layer.seal, &message)
                    .map_err(Failure::Security)?;
                let mut framed = u32::try_from(token.len())
                    .map_err(|_| protocol_failure())?
                    .to_be_bytes()
                    .to_vec();
                framed.extend(token);
                framed
            }
        };
        let left = left(self.deadline).ok_or_else(|| os_failure(timed_out()))?;
        self.stream
            .set_write_timeout(Some(left))
            .and_then(|()| self.stream.write_all(&bytes))
            .map_err(os_failure)?;
        Ok(id)
    }

    /// The next message answering `id`; others, such as a notice of
    /// disconnection (id 0), are passed over.
    fn receive(&mut self, id: i64) -> Result<Operation, Failure> {
        loop {
            let length = ber::framed_length(&self.received).map_err(|()| protocol_failure())?;
            match length {
                Some(length) if length > MAX_MESSAGE => return Err(protocol_failure()),
                Some(length) if self.received.len() >= length => {
                    let message: Vec<u8> = self.received.drain(..length).collect();
                    let (found, operation) =
                        parse_message(&message).ok_or_else(protocol_failure)?;
                    if found == id {
                        return Ok(operation);
                    }
                }
                _ => self.fill()?,
            }
        }
    }

    /// Reads more of the stream: a protected buffer, unwrapped, or what the
    /// socket has.
    fn fill(&mut self) -> Result<(), Failure> {
        let more = match &mut self.layer {
            Some(layer) => {
                let mut length = [0u8; 4];
                read_exact(&mut self.stream, self.deadline, &mut length)?;
                let length = u32::from_be_bytes(length) as usize;
                if length > MAX_MESSAGE {
                    return Err(protocol_failure());
                }
                let mut token = vec![0u8; length];
                read_exact(&mut self.stream, self.deadline, &mut token)?;
                layer.context.unwrap(&token).map_err(Failure::Security)?.0
            }
            None => {
                let mut buffer = vec![0u8; 16 << 10];
                let read = read(&mut self.stream, self.deadline, &mut buffer)?;
                buffer.truncate(read);
                buffer
            }
        };
        if self.received.len() + more.len() > MAX_MESSAGE {
            return Err(protocol_failure());
        }
        self.received.extend(more);
        Ok(())
    }
}

/// Reads what `stream` has by `deadline`; a closed stream is a failure.
fn read(stream: &mut TcpStream, deadline: Instant, buffer: &mut [u8]) -> Result<usize, Failure> {
    let left = left(deadline).ok_or_else(|| os_failure(timed_out()))?;
    stream.set_read_timeout(Some(left)).map_err(os_failure)?;
    match stream.read(buffer).map_err(os_failure)? {
        0 => Err(closed()),
        read => Ok(read),
    }
}

fn read_exact(stream: &mut TcpStream, deadline: Instant, buffer: &mut [u8]) -> Result<(), Failure> {
    let mut at = 0;
    while at < buffer.len() {
        at += read(stream, deadline, &mut buffer[at..])?;
    }
    Ok(())
}

/// The time left before `deadline`, if any.
fn left(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}

fn timed_out() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::TimedOut)
}

fn os_failure(error: std::io::Error) -> Failure {
    Failure::Os {
        code: error.raw_os_error(),
        message: error.to_string(),
    }
}

/// The server closed the connection: LDAP clients' `LDAP_SERVER_DOWN`, as
/// Windows LDAP reports the same.
fn closed() -> Failure {
    Failure::Ldap {
        code: 0x51,
        message: None,
    }
}

/// A message that does not decode as LDAP: LDAP's own protocolError.
fn protocol_failure() -> Failure {
    Failure::Ldap {
        code: 2,
        message: None,
    }
}

fn ldap_failure(result: LdapResult) -> Lookup {
    if refusal(result.code) {
        Lookup::Refused(result.failure())
    } else {
        Lookup::Failed(result.failure())
    }
}

/// A GSSAPI failure binding: no credentials to bind with, or the bind's own
/// failure.
fn security_failure(error: SecurityError) -> Lookup {
    match &error {
        SecurityError::GssapiError { major, .. } if (major >> 16) & 0xFF == 7 => {
            Lookup::NoCredentials
        }
        SecurityError::NoCredentials | SecurityError::CredentialsExpired => Lookup::NoCredentials,
        _ => Lookup::Failed(Failure::Security(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DNS answer for `_gc._tcp.example.local` with `records` (priority,
    /// weight, port, target), the targets compressed against the question
    /// where they can be.
    fn answer(records: &[(u16, u16, u16, &str)]) -> Vec<u8> {
        let mut message = vec![0x12, 0x34, 0x81, 0x80, 0, 1];
        message.extend((records.len() as u16).to_be_bytes());
        message.extend([0, 0, 0, 0]);
        let name = |name: &str| {
            let mut encoded = Vec::new();
            for label in name.split('.').filter(|label| !label.is_empty()) {
                encoded.push(label.len() as u8);
                encoded.extend(label.as_bytes());
            }
            encoded.push(0);
            encoded
        };
        message.extend(name("_gc._tcp.example.local"));
        message.extend([0, 33, 0, 1]);
        for (priority, weight, port, target) in records {
            message.extend([0xC0, 12, 0, 33, 0, 1, 0, 0, 0x0E, 0x10]);
            // `dc.example.local` points into the question's `example.local`.
            let target = match target.strip_suffix(".example.local") {
                Some(host) => {
                    let mut encoded = vec![host.len() as u8];
                    encoded.extend(host.as_bytes());
                    encoded.extend([0xC0, 12 + 9]);
                    encoded
                }
                None => name(target),
            };
            message.extend(((6 + target.len()) as u16).to_be_bytes());
            message.extend(priority.to_be_bytes());
            message.extend(weight.to_be_bytes());
            message.extend(port.to_be_bytes());
            message.extend(target);
        }
        message
    }

    #[test]
    fn srv_records_are_read_in_the_order_to_try_them() {
        let message = answer(&[
            (10, 5, 3268, "dc2.example.local"),
            (0, 10, 3268, "dc1.example.local"),
            (0, 90, 3268, "dc3.other.example"),
            (0, 0, 0, "."),
        ]);
        let targets: Vec<(String, u16)> = srv_records(&message)
            .unwrap()
            .into_iter()
            .map(|r| (r.target, r.port))
            .collect();
        assert_eq!(
            targets,
            [
                ("dc3.other.example".to_string(), 3268),
                ("dc1.example.local".to_string(), 3268),
                ("dc2.example.local".to_string(), 3268),
            ],
            "no service ('.') is not a server"
        );
        for end in 0..message.len() {
            let _ = srv_records(&message[..end]);
        }
        // A target that points to itself is not followed forever.
        let mut looped = vec![0, 0, 0x81, 0x80, 0, 0, 0, 1, 0, 0, 0, 0];
        looped.extend([
            0xC0, 12, 0, 33, 0, 1, 0, 0, 0, 60, 0, 8, 0, 0, 0, 0, 0x0C, 0x8C,
        ]);
        looped.extend([0xC0, 30]);
        assert_eq!(srv_records(&looped), None);
    }

    fn result(tag: u8, code: i64, message: &str, extra: &[u8]) -> Vec<u8> {
        let mut content = ber::number(0x0A, code);
        content.extend(ber::tlv(0x04, b""));
        content.extend(ber::tlv(0x04, message.as_bytes()));
        content.extend_from_slice(extra);
        ber::tlv(tag, &content)
    }

    fn message(id: i64, operation: &[u8]) -> Vec<u8> {
        let mut content = ber::number(0x02, id);
        content.extend_from_slice(operation);
        ber::tlv(0x30, &content)
    }

    #[test]
    fn responses_are_read_as_a_server_sends_them() {
        let bind = message(1, &result(0x61, 14, "", &ber::tlv(0x87, b"token")));
        assert_eq!(
            parse_message(&bind),
            Some((
                1,
                Operation::Bind {
                    result: LdapResult {
                        code: 14,
                        message: String::new()
                    },
                    server_credentials: Some(b"token".to_vec()),
                }
            ))
        );
        let refused = message(
            2,
            &result(0x65, 8, "00002028: integrity checking required", &[]),
        );
        assert_eq!(
            parse_message(&refused),
            Some((
                2,
                Operation::Done(LdapResult {
                    code: 8,
                    message: "00002028: integrity checking required".to_string()
                })
            ))
        );
        let attribute = |name: &str, values: &[&str]| {
            let mut content = ber::tlv(0x04, name.as_bytes());
            let values: Vec<u8> = values
                .iter()
                .flat_map(|v| ber::tlv(0x04, v.as_bytes()))
                .collect();
            content.extend(ber::tlv(0x31, &values));
            ber::tlv(0x30, &content)
        };
        let mut attributes = attribute("sAMAccountName", &["SQL$"]);
        attributes.extend(attribute("objectClass", &["top", "user", "computer"]));
        attributes.extend(attribute("msDS-SupportedEncryptionTypes", &["28"]));
        let mut entry = ber::tlv(0x04, b"CN=SQL,CN=Computers,DC=example,DC=local");
        entry.extend(ber::tlv(0x30, &attributes));
        let Some((3, Operation::Entry(read))) = parse_message(&message(3, &ber::tlv(0x64, &entry)))
        else {
            panic!("an entry");
        };
        assert_eq!(
            account(&read),
            Account {
                name: "SQL$".to_string(),
                kind: AccountKind::Computer,
                encryption_types: Some(28),
            }
        );
        assert_eq!(parse_message(&bind[..bind.len() - 1]), None);
    }

    #[test]
    fn requests_are_encoded_as_rfc_4511_gives_them() {
        let bind = bind_request(b"tok");
        assert_eq!(
            bind,
            [
                0x60, 0x14, 0x02, 0x01, 0x03, 0x04, 0x00, 0xA3, 0x0D, 0x04, 0x06, b'G', b'S', b'S',
                b'A', b'P', b'I', 0x04, 0x03, b't', b'o', b'k'
            ]
        );
        let search = search_request("", "servicePrincipalName", "MSSQLSvc/sql:1433", 8);
        let (content, rest) = ber::element(&search, 0x63).unwrap();
        assert!(rest.is_empty());
        assert_eq!(ber::element(content, 0x04).unwrap().0, b"");
        let filter = ber::field(content, 0xA3).unwrap();
        let (attribute, value) = ber::element(filter, 0x04).unwrap();
        assert_eq!(attribute, b"servicePrincipalName");
        assert_eq!(ber::element(value, 0x04).unwrap().0, b"MSSQLSvc/sql:1433");
    }

    #[test]
    fn the_strongest_layer_offered_is_chosen_and_none_is_refused() {
        assert_eq!(
            choose_layer(&[0x07, 0x00, 0x10, 0x00]),
            Some((true, [0x04, 0x01, 0x00, 0x00]))
        );
        assert_eq!(
            choose_layer(&[0x03, 0x00, 0x10, 0x00]),
            Some((false, [0x02, 0x01, 0x00, 0x00]))
        );
        // Neither signed nor sealed: refused.
        assert_eq!(choose_layer(&[0x01, 0, 0, 0]), None);
        assert_eq!(choose_layer(&[0x00, 0, 0, 0]), None);
        assert_eq!(choose_layer(&[0x07, 0, 0]), None);
    }

    /// A server on this computer that reads one message and answers it with
    /// `answer`, then holds the connection until the client is done.
    fn answering_server(answer: Vec<u8>) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while !matches!(
                ber::framed_length(&request),
                Ok(Some(length)) if request.len() >= length
            ) {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let _ = stream.write_all(&answer);
            let _ = stream.read(&mut buffer);
        });
        Server {
            host: address.ip().to_string(),
            port: address.port(),
            scope: Scope::Domain,
            base: String::new(),
        }
    }

    /// More than one account is all a search needs: a server that sends
    /// more, ignoring the size limit, is not read any further.
    #[test]
    fn a_search_reads_two_accounts_at_most() {
        let entry = |name: &str| {
            let mut attribute = ber::tlv(0x04, b"sAMAccountName");
            attribute.extend(ber::tlv(0x31, &ber::tlv(0x04, name.as_bytes())));
            let mut entry = ber::tlv(0x04, b"");
            entry.extend(ber::tlv(0x30, &ber::tlv(0x30, &attribute)));
            message(1, &ber::tlv(0x64, &entry))
        };
        let mut answer: Vec<u8> = ["a", "b", "c"].into_iter().flat_map(entry).collect();
        answer.extend(message(1, &result(0x65, 0, "", &[])));
        let server = answering_server(answer);
        let mut connection =
            Connection::open(&server, Instant::now() + Duration::from_secs(10)).unwrap();
        match connection.search(&server, "servicePrincipalName", "MSSQLSvc/sql:1433") {
            Ok(Lookup::Searched { scope, accounts }) => {
                assert_eq!(scope, Scope::Domain);
                let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                assert_eq!(names, ["a", "b"]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A server that ends the bind with success before it proved itself and
    /// agreed a signed or sealed layer is not trusted: its answer could be
    /// anyone's. Live, for the ticket the bind presents to the test
    /// environment's domain controller, which another server answers.
    #[test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    fn kerberos_spn_lookup_trusts_no_bind_the_server_did_not_complete() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let server = answering_server(message(1, &result(0x61, 0, "", &[])));
        let mut connection =
            Connection::open(&server, Instant::now() + Duration::from_secs(10)).unwrap();
        match connection.bind("dc.example.local") {
            Err(Lookup::Failed(Failure::Ldap { code: 2, .. })) => {}
            other => panic!("{other:?}"),
        }
        assert!(connection.layer.is_none());
    }

    /// The SPN looked up live in the Kerberos test environment's directory:
    /// its global catalog has it on SQL Server's computer account, and has
    /// no account with an SPN no one registered.
    #[test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    fn kerberos_spn_lookup_searches_the_realms_global_catalog() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let query = |spn: &str| Query {
            spn: spn.to_string(),
            realm: Some("EXAMPLE.LOCAL".to_string()),
            deadline: Instant::now() + Duration::from_secs(10),
        };
        match lookup(query("MSSQLSvc/sql.example.local:1433")) {
            Lookup::Searched { scope, accounts } => {
                assert_eq!(scope, Scope::Forest);
                assert_eq!(accounts.len(), 1, "{accounts:?}");
                assert_eq!(accounts[0].name, "SQL$");
                assert_eq!(accounts[0].kind, AccountKind::Computer);
            }
            other => panic!("{other:?}"),
        }
        match lookup(query("MSSQLSvc/nobody.example.local:1433")) {
            Lookup::Searched { accounts, .. } => assert!(accounts.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    /// A domain controller of the realm searched live: its search covers its
    /// domain and returns an account's supported encryption types when they
    /// are set (`KERBEROS_TEST_SQL_ETYPES`, SQL Server's account's, unset
    /// for none); and more than one account ends the search at its size
    /// limit with the two read.
    #[test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    fn kerberos_spn_lookup_searches_a_domain_controller() {
        if std::env::var("KERBEROS_TEST").is_err() {
            return;
        }
        let deadline = || Instant::now() + Duration::from_secs(10);
        let servers = directory_servers("EXAMPLE.LOCAL");
        let dc = Server {
            host: "dc.example.local".to_string(),
            port: 389,
            scope: Scope::Domain,
            base: "DC=example,DC=local".to_string(),
        };
        assert_eq!(
            servers,
            [
                Server {
                    port: 3268,
                    scope: Scope::Forest,
                    base: String::new(),
                    ..dc.clone()
                },
                dc.clone(),
            ]
        );
        let expected = std::env::var("KERBEROS_TEST_SQL_ETYPES")
            .ok()
            .map(|types| types.parse::<u32>().unwrap());
        let mut connection = Connection::open(&dc, deadline()).unwrap();
        match connection.lookup(&dc, "MSSQLSvc/sql.example.local:1433") {
            Lookup::Searched { scope, accounts } => {
                assert_eq!(scope, Scope::Domain);
                assert_eq!(accounts.len(), 1, "{accounts:?}");
                assert_eq!(accounts[0].name, "SQL$");
                assert_eq!(accounts[0].encryption_types, expected);
            }
            other => panic!("{other:?}"),
        }
        let mut connection = Connection::open(&dc, deadline()).unwrap();
        connection.bind(&dc.host).unwrap();
        match connection.search(&dc, "objectClass", "computer") {
            Ok(Lookup::Searched { accounts, .. }) => {
                assert_eq!(accounts.len(), 2, "{accounts:?}");
                assert!(
                    accounts.iter().all(|a| a.kind == AccountKind::Computer),
                    "{accounts:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}

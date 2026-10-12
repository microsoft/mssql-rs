// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection input: the server given to `-S`, parsed locally.
//!
//! sqlcmd names its server as `[protocol:]server[\instance][,port]`, where the
//! protocol is `tcp`, `np` (named pipes; the server is then a pipe path such as
//! `\\host\pipe\sql\query`), `lpc` (shared memory) or `admin` (dedicated
//! administrator connection). `(localdb)\instance` names a LocalDB instance.
//! Parsing makes no external call and does not decide whether a connection can
//! succeed.

use std::borrow::Cow;
use std::fmt;

use crate::i18n;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// No prefix and no port: the client's protocol order. On Windows it
    /// tries shared memory first for this computer, then TCP, then named
    /// pipes; the diagnosis follows the transports the client tried. With a
    /// port it is `Tcp`, as for the client.
    Default,
    Tcp,
    NamedPipe,
    SharedMemory,
    Admin,
    LocalDb,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Default | Protocol::Tcp => "tcp",
            Protocol::NamedPipe => "namedPipe",
            Protocol::SharedMemory => "sharedMemory",
            Protocol::Admin => "admin",
            Protocol::LocalDb => "localDb",
        }
    }

    /// Whether the connection goes over TCP, so name resolution and a TCP
    /// connect apply.
    pub fn uses_tcp(self) -> bool {
        matches!(self, Protocol::Default | Protocol::Tcp | Protocol::Admin)
    }
}

/// The parsed server: what later checks need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub protocol: Protocol,
    /// The host as written; `.` and `(local)` mean this machine.
    pub host: String,
    pub instance: Option<String>,
    pub port: Option<u16>,
}

impl Target {
    /// The protocol as Connection input reports it, before any transport is
    /// tried: `auto` for an unprefixed server without a port on Windows,
    /// where the client tries shared memory (for this computer), TCP and
    /// named pipes in turn; elsewhere the client has only TCP.
    pub fn protocol_name(&self, windows: bool) -> &'static str {
        match self.protocol {
            Protocol::Default if windows => "auto",
            protocol => protocol.name(),
        }
    }

    /// The name the client resolves, as it decides when it parses the server:
    /// `.`, `(local)`, `localhost` and this computer's name are this machine,
    /// which over TCP, to the dedicated administrator port or with an
    /// instance is `localhost`, and otherwise is the computer's own name.
    /// Explicit IP addresses are left as given.
    pub fn resolvable_host(&self) -> Cow<'_, str> {
        if !self.is_local_name() {
            return Cow::Borrowed(&self.host);
        }
        if matches!(self.protocol, Protocol::Tcp | Protocol::Admin) || self.instance.is_some() {
            return Cow::Borrowed("localhost");
        }
        match mssql_tds::connection::this_computer_name() {
            Some(name) => Cow::Owned(name),
            None => Cow::Borrowed(&self.host),
        }
    }

    /// Whether the host is one of the names for this machine, but not an
    /// address.
    fn is_local_name(&self) -> bool {
        let host = self.host.as_str();
        host == "."
            || host.eq_ignore_ascii_case("(local)")
            || host.eq_ignore_ascii_case("localhost")
            || mssql_tds::connection::this_computer_name()
                .is_some_and(|name| host.eq_ignore_ascii_case(&name))
    }

    /// Whether the port must come from SQL Server Browser: a named instance
    /// with no port given.
    pub fn needs_instance_lookup(&self) -> bool {
        self.protocol.uses_tcp() && self.port.is_none() && self.instance.is_some()
    }

    /// Whether the server is this computer, as the client decides when it
    /// tries shared memory first: `.`, `(local)`, `localhost`, a loopback
    /// address, or this computer's name.
    pub fn is_local(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "::1") || self.is_local_name()
    }
}

/// Why the server string could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No server was given.
    Empty,
    /// The host part is empty, e.g. `tcp:,1433` or `\inst`.
    MissingHost,
    /// The LocalDB instance after `(localdb)\` is empty. Elsewhere an empty
    /// instance (`host\`) is the default instance, as for the client.
    MissingInstance,
    /// The port is not a number from 0 to 65535.
    InvalidPort(String),
    /// A port given with a protocol that takes none (`np:`, `lpc:`, `admin:`).
    PortNotAllowed(&'static str),
    /// A pipe path that is not `\\host\pipe\name`.
    InvalidPipe(String),
    /// `MSSQLSERVER`, the default instance's reserved name, given without a
    /// port: the client refuses it, as it would a connection.
    ReservedInstance,
    /// Shared memory (`lpc:`) to a host that is not this computer: the client
    /// opens shared memory only to a local server.
    SharedMemoryNotLocal(String),
    /// Shared memory (`lpc:`) off Windows, where the client has none.
    SharedMemoryNotSupported,
    /// LocalDB (`(localdb)\instance`) off Windows, where the client has none.
    LocalDbNotSupported,
    /// Named pipes (`np:`, or a `\\host\pipe\name` path) off Windows, where
    /// the client has none.
    NamedPipesNotSupported,
}

impl ParseError {
    /// A stable identifier for the error.
    pub fn code(&self) -> &'static str {
        match self {
            ParseError::Empty => "emptyServer",
            ParseError::MissingHost => "missingHost",
            ParseError::MissingInstance => "missingInstance",
            ParseError::InvalidPort(_) => "invalidPort",
            ParseError::PortNotAllowed(_) => "portNotAllowed",
            ParseError::InvalidPipe(_) => "invalidPipePath",
            ParseError::ReservedInstance => "reservedInstance",
            ParseError::SharedMemoryNotLocal(_) => "sharedMemoryNotLocal",
            ParseError::SharedMemoryNotSupported => "sharedMemoryNotSupported",
            ParseError::LocalDbNotSupported => "localDbNotSupported",
            ParseError::NamedPipesNotSupported => "namedPipesNotSupported",
        }
    }

    pub fn message_id(&self) -> &'static str {
        match self {
            ParseError::Empty => "diagnose.target.empty",
            ParseError::MissingHost => "diagnose.target.missing_host",
            ParseError::MissingInstance => "diagnose.target.missing_instance",
            ParseError::InvalidPort(_) => "diagnose.target.invalid_port",
            ParseError::PortNotAllowed(_) => "diagnose.target.port_not_allowed",
            ParseError::InvalidPipe(_) => "diagnose.target.invalid_pipe",
            ParseError::ReservedInstance => "diagnose.target.reserved_instance",
            ParseError::SharedMemoryNotLocal(_) => "diagnose.target.shared_memory_not_local",
            ParseError::SharedMemoryNotSupported => "diagnose.target.shared_memory_not_supported",
            // The client's own messages.
            ParseError::LocalDbNotSupported => "diagnose.target.localdb_not_supported",
            ParseError::NamedPipesNotSupported => "diagnose.target.named_pipes_not_supported",
        }
    }

    pub fn message_args(&self) -> Vec<(&'static str, String)> {
        match self {
            ParseError::InvalidPort(port) => vec![("port", port.clone())],
            ParseError::PortNotAllowed(protocol) => vec![("protocol", (*protocol).to_string())],
            ParseError::InvalidPipe(path) => vec![("path", path.clone())],
            ParseError::SharedMemoryNotLocal(host) => vec![("host", host.clone())],
            _ => Vec::new(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let owned_args = self.message_args();
        let args: Vec<(&str, &dyn fmt::Display)> = owned_args
            .iter()
            .map(|(name, value)| (*name, value as &dyn fmt::Display))
            .collect();
        write!(f, "{}", i18n::tr(self.message_id(), &args))
    }
}

/// The port the client connects to over TCP when the port is empty.
const DEFAULT_PORT: u16 = 1433;

/// Parses the server as given to `-S`.
pub fn parse(server: &str) -> Result<Target, ParseError> {
    parse_for(server, cfg!(windows))
}

/// [`parse`] as the client parses on Windows (`windows`) or elsewhere, where
/// it refuses LocalDB, named pipes and shared memory.
pub(crate) fn parse_for(server: &str, windows: bool) -> Result<Target, ParseError> {
    let server = server.trim();
    if server.is_empty() {
        return Err(ParseError::Empty);
    }

    // LocalDB first, as the client does: `(localdb)\` or `(localdb)/` and the
    // rest is the instance, whatever it holds. A bare `(localdb)` is a host.
    if server.get(..10).is_some_and(|p| {
        p.eq_ignore_ascii_case("(localdb)\\") || p.eq_ignore_ascii_case("(localdb)/")
    }) {
        // Off Windows the client refuses LocalDB before anything else.
        if !windows {
            return Err(ParseError::LocalDbNotSupported);
        }
        let instance = server[10..].trim();
        if instance.is_empty() {
            return Err(ParseError::MissingInstance);
        }
        return Ok(Target {
            protocol: Protocol::LocalDb,
            host: server[..9].to_string(),
            instance: Some(instance.to_string()),
            port: None,
        });
    }
    // As the client: only `tcp`, `np`, `lpc` or `admin` before the first
    // colon, trimmed and in any case (`tcp :db01` is TCP), is a prefix.
    // Anything else there, an IPv6 address's first group (`dead:beef::1`) or
    // an unknown prefix (`udp:db01`), is part of the server name.
    let (protocol, rest) = match server
        .split_once(':')
        .map(|(p, r)| (p.trim().to_lowercase(), r))
    {
        Some((prefix, rest)) => match prefix.as_str() {
            "tcp" => (Protocol::Tcp, rest.trim()),
            "np" => (Protocol::NamedPipe, rest.trim()),
            "lpc" => (Protocol::SharedMemory, rest.trim()),
            "admin" => (Protocol::Admin, rest.trim()),
            _ => (Protocol::Default, server),
        },
        None => (Protocol::Default, server),
    };
    // As the client: a pipe path (`\\host\pipe\...`), bare or after any
    // prefix (`tcp:`, `admin:` and `lpc:` included), is a named pipe, before
    // a comma is taken for a port.
    if rest.starts_with("\\\\") {
        return parse_pipe(rest, windows);
    }

    // The client takes a port only over TCP.
    let takes_no_port = match protocol {
        Protocol::NamedPipe => Some("np"),
        Protocol::SharedMemory => Some("lpc"),
        Protocol::Admin => Some("admin"),
        _ => None,
    };
    if let Some(prefix) = takes_no_port
        && rest.contains(',')
    {
        return Err(ParseError::PortNotAllowed(prefix));
    }
    if protocol == Protocol::NamedPipe {
        return parse_pipe(rest, windows);
    }

    // As the client: the first comma starts the port, and an instance name
    // after the port (`db01,1433\SQL2022`) is dropped, as the port decides.
    // The client trims the whole text after the comma before it cuts the
    // port at the `\`, so a space before that `\` stays in the port, which it
    // refuses. Any number it parses is a port, 0 included (the connect then
    // fails); an empty one (`db01,`) is no port (`None` inside).
    let (rest, port) = match rest.split_once(',') {
        Some((rest, port)) => {
            let port = port.trim();
            let port = port.split('\\').next().unwrap_or(port);
            if port.is_empty() {
                (rest, Some(None))
            } else {
                match port.parse::<u16>() {
                    Ok(p) => (rest, Some(Some(p))),
                    Err(_) => return Err(ParseError::InvalidPort(port.to_string())),
                }
            }
        }
        None => (rest, None),
    };

    let (host, instance) = match rest.split_once('\\') {
        Some((host, instance)) => {
            let instance = instance.trim();
            // As the client: an empty instance is the default instance,
            // with or without a port (`db01\`, `tcp:db01\,1433`).
            let instance = (!instance.is_empty()).then(|| instance.to_string());
            (host.trim(), instance)
        }
        None => (rest.trim(), None),
    };
    if host.is_empty() {
        return Err(ParseError::MissingHost);
    }
    // A comma makes it TCP, as for the client: an unprefixed server with a
    // port, even an empty one, is not tried over shared memory or named
    // pipes. With an empty port the client asks SQL Server Browser for a
    // named instance's port (`db01\SQL2022,`), over TCP only, and otherwise
    // connects to 1433 (`db01,`).
    let protocol = match (protocol, port) {
        (Protocol::Default, Some(_)) => Protocol::Tcp,
        (protocol, _) => protocol,
    };
    let (protocol, port) = match port {
        Some(None) if instance.is_some() => (Protocol::Tcp, None),
        Some(None) => (protocol, Some(DEFAULT_PORT)),
        Some(Some(port)) => (protocol, Some(port)),
        None => (protocol, None),
    };
    // The client refuses the default instance's reserved name unless a port
    // is given, which takes priority over the instance.
    if port.is_none()
        && instance
            .as_deref()
            .is_some_and(|i| i.eq_ignore_ascii_case("MSSQLSERVER"))
    {
        return Err(ParseError::ReservedInstance);
    }
    if protocol == Protocol::SharedMemory {
        shared_memory_host(host, windows)?;
    }

    Ok(Target {
        protocol,
        host: host.to_string(),
        instance,
        port,
    })
}

/// As the client: shared memory only on Windows, and only to this computer:
/// `.`, or `(local)`, `localhost` or this computer's name in any case.
fn shared_memory_host(host: &str, windows: bool) -> Result<(), ParseError> {
    if !windows {
        return Err(ParseError::SharedMemoryNotSupported);
    }
    let local = host == "."
        || host.eq_ignore_ascii_case("(local)")
        || host.eq_ignore_ascii_case("localhost")
        || mssql_tds::connection::this_computer_name()
            .is_some_and(|name| host.eq_ignore_ascii_case(&name));
    if local {
        Ok(())
    } else {
        Err(ParseError::SharedMemoryNotLocal(host.to_string()))
    }
}

/// A named pipe, `\\host\pipe\name` (the host is the first path segment), or
/// after `np:` a server name the client builds the pipe path from. Off
/// Windows the client refuses a pipe once its form is checked.
fn parse_pipe(path: &str, windows: bool) -> Result<Target, ParseError> {
    let (host, pipe, instance) = match path.strip_prefix("\\\\") {
        Some(unc) => {
            let (host, pipe) = unc.split_once('\\').unwrap_or((unc, ""));
            (host, Some(pipe), None)
        }
        // `np:host\instance`: the client asks SQL Server Browser for the
        // instance's pipe.
        None => match path.split_once('\\') {
            Some((host, instance)) => {
                let instance = instance.trim();
                // A pipe takes no port, so the reserved name is refused here.
                if instance.eq_ignore_ascii_case("MSSQLSERVER") {
                    return Err(ParseError::ReservedInstance);
                }
                // An empty instance is the default instance, as for the
                // client.
                let instance = (!instance.is_empty()).then(|| instance.to_string());
                (host.trim(), None, instance)
            }
            None => (path.trim(), None, None),
        },
    };
    if host.is_empty() {
        return Err(ParseError::MissingHost);
    }
    // As the client requires: `pipe\` and a name after the host.
    if let Some(pipe) = pipe {
        let named = pipe
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("pipe\\"))
            && pipe.len() > 5;
        if !named {
            return Err(ParseError::InvalidPipe(path.to_string()));
        }
    }
    if !windows {
        return Err(ParseError::NamedPipesNotSupported);
    }
    Ok(Target {
        protocol: Protocol::NamedPipe,
        host: host.to_string(),
        instance,
        port: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_computer_is_local() {
        for host in [".", "(local)", "localhost", "LOCALHOST", "127.0.0.1", "::1"] {
            let target = parse(host).unwrap();
            assert!(target.is_local(), "{host}");
        }
        let remote = parse("db01").unwrap();
        assert!(!remote.is_local());
    }

    fn target(protocol: Protocol, host: &str, instance: Option<&str>, port: Option<u16>) -> Target {
        Target {
            protocol,
            host: host.to_string(),
            instance: instance.map(str::to_string),
            port,
        }
    }

    #[test]
    fn server_forms_are_parsed() {
        let cases = [
            ("db01", target(Protocol::Default, "db01", None, None)),
            (
                "tcp:db01,1433",
                target(Protocol::Tcp, "db01", None, Some(1433)),
            ),
            (
                "db01\\SQL2022",
                target(Protocol::Default, "db01", Some("SQL2022"), None),
            ),
            (
                "db01\\SQL2022,50001",
                target(Protocol::Tcp, "db01", Some("SQL2022"), Some(50001)),
            ),
            (
                "TCP: 10.0.0.5 , 1500",
                target(Protocol::Tcp, "10.0.0.5", None, Some(1500)),
            ),
            ("tcp :db01", target(Protocol::Tcp, "db01", None, None)),
            ("admin:db01", target(Protocol::Admin, "db01", None, None)),
            (
                "(localdb)\\MSSQLLocalDB",
                target(Protocol::LocalDb, "(localdb)", Some("MSSQLLocalDB"), None),
            ),
            (
                "np:\\\\db01\\pipe\\sql\\query",
                target(Protocol::NamedPipe, "db01", None, None),
            ),
            // Bare IPv6 addresses, an alphabetic first group included.
            (
                "dead:beef::1",
                target(Protocol::Default, "dead:beef::1", None, None),
            ),
            (
                "dead:beef::1,1433",
                target(Protocol::Tcp, "dead:beef::1", None, Some(1433)),
            ),
            (
                "tcp:fe80::1,1433",
                target(Protocol::Tcp, "fe80::1", None, Some(1433)),
            ),
            ("::1", target(Protocol::Default, "::1", None, None)),
            // A pipe path without the np: prefix.
            (
                "\\\\db01\\pipe\\sql\\query",
                target(Protocol::NamedPipe, "db01", None, None),
            ),
            // Other pipe paths the client takes, and np: with a server name.
            (
                "\\\\.\\PIPE\\MSSQL$SQL2022\\sql\\query",
                target(Protocol::NamedPipe, ".", None, None),
            ),
            (
                "\\\\db01\\pipe\\custom,1",
                target(Protocol::NamedPipe, "db01", None, None),
            ),
            ("np:db01", target(Protocol::NamedPipe, "db01", None, None)),
            (
                "np:db01\\SQL2022",
                target(Protocol::NamedPipe, "db01", Some("SQL2022"), None),
            ),
            (
                "(localdb)/MSSQLLocalDB",
                target(Protocol::LocalDb, "(localdb)", Some("MSSQLLocalDB"), None),
            ),
            // The rest after (localdb)\ is the instance, as the client takes it.
            (
                "(LocalDB)\\MSSQLSERVER",
                target(Protocol::LocalDb, "(LocalDB)", Some("MSSQLSERVER"), None),
            ),
            (
                "(localdb)\\foo,1433",
                target(Protocol::LocalDb, "(localdb)", Some("foo,1433"), None),
            ),
            // A bare (localdb) is only a host name.
            (
                "(localdb)",
                target(Protocol::Default, "(localdb)", None, None),
            ),
            // With a prefix, (localdb) is only a host name, as the client takes it.
            (
                "tcp:(localdb)\\MSSQLLocalDB",
                target(Protocol::Tcp, "(localdb)", Some("MSSQLLocalDB"), None),
            ),
            // MSSQLSERVER with a port: the port takes priority.
            (
                "db01\\MSSQLSERVER,1433",
                target(Protocol::Tcp, "db01", Some("MSSQLSERVER"), Some(1433)),
            ),
            // Port 0 is a port, as for the client; the connect then fails.
            ("db01,0", target(Protocol::Tcp, "db01", None, Some(0))),
            // An empty port is 1433, as for the client, an instance after it
            // dropped; with a named instance before it, the instance's port
            // comes from SQL Server Browser, over TCP.
            ("db01,", target(Protocol::Tcp, "db01", None, Some(1433))),
            (
                "tcp:db01, ",
                target(Protocol::Tcp, "db01", None, Some(1433)),
            ),
            (
                "db01,\\SQL2022",
                target(Protocol::Tcp, "db01", None, Some(1433)),
            ),
            ("db01\\,", target(Protocol::Tcp, "db01", None, Some(1433))),
            (
                "db01\\SQL2022,",
                target(Protocol::Tcp, "db01", Some("SQL2022"), None),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_for(input, true), Ok(expected), "{input}");
        }
    }

    /// As the client: only tcp, np, lpc and admin are a prefix; anything else
    /// before the first colon is part of the server name, which name
    /// resolution then looks up.
    #[test]
    fn an_unknown_prefix_is_part_of_the_server_name() {
        for windows in [true, false] {
            let cases = [
                (
                    "udp:db01",
                    target(Protocol::Default, "udp:db01", None, None),
                ),
                (
                    " UDP:db01 ",
                    target(Protocol::Default, "UDP:db01", None, None),
                ),
                (
                    "udp:fe80::1",
                    target(Protocol::Default, "udp:fe80::1", None, None),
                ),
                (
                    "http:db01\\SQL2022,1500",
                    target(Protocol::Tcp, "http:db01", Some("SQL2022"), Some(1500)),
                ),
                (
                    "tcp1:db01",
                    target(Protocol::Default, "tcp1:db01", None, None),
                ),
            ];
            for (input, expected) in cases {
                assert_eq!(parse_for(input, windows), Ok(expected), "{input}");
            }
        }
    }

    /// Off Windows the client refuses LocalDB, named pipes and shared memory,
    /// with its own messages; a malformed one is still reported as such.
    #[test]
    fn windows_only_forms_are_refused_off_windows() {
        let cases = [
            ("(localdb)\\MSSQLLocalDB", ParseError::LocalDbNotSupported),
            ("(LocalDB)/MSSQLLocalDB", ParseError::LocalDbNotSupported),
            ("(localdb)\\ ", ParseError::LocalDbNotSupported),
            ("np:db01", ParseError::NamedPipesNotSupported),
            ("np:db01\\SQL2022", ParseError::NamedPipesNotSupported),
            (
                "np:\\\\db01\\pipe\\sql\\query",
                ParseError::NamedPipesNotSupported,
            ),
            (
                "\\\\.\\pipe\\sql\\query",
                ParseError::NamedPipesNotSupported,
            ),
            ("lpc:.", ParseError::SharedMemoryNotSupported),
            ("lpc:localhost", ParseError::SharedMemoryNotSupported),
            ("lpc:remoteserver", ParseError::SharedMemoryNotSupported),
            ("np:db01,1433", ParseError::PortNotAllowed("np")),
            ("np:", ParseError::MissingHost),
            (
                "\\\\db01\\garbage",
                ParseError::InvalidPipe("\\\\db01\\garbage".to_string()),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_for(input, false), Err(expected), "{input:?}");
        }
        // The client takes these off Windows: a bare (localdb) is a host, and
        // with a prefix (localdb) is only a host name.
        assert_eq!(
            parse_for("(localdb)", false),
            Ok(target(Protocol::Default, "(localdb)", None, None))
        );
        assert_eq!(
            parse_for("tcp:(localdb)\\MSSQLLocalDB", false),
            Ok(target(
                Protocol::Tcp,
                "(localdb)",
                Some("MSSQLLocalDB"),
                None
            ))
        );
        assert_eq!(
            ParseError::LocalDbNotSupported.to_string(),
            "LocalDB is not supported on this platform. LocalDB is a Windows-only feature. \
             Use a TCP connection string instead (e.g., 'tcp:server,port')."
        );
        assert_eq!(
            ParseError::NamedPipesNotSupported.to_string(),
            "Named Pipes (np:) protocol is not supported on this platform. Named Pipes are a \
             Windows-only feature. Use TCP instead (e.g., 'tcp:server,port')."
        );
        // `parse` parses as the platform it runs on does.
        for input in ["(localdb)\\MSSQLLocalDB", "np:db01", "lpc:.", "db01"] {
            assert_eq!(parse(input), parse_for(input, cfg!(windows)), "{input}");
        }
    }

    #[test]
    fn unusable_servers_are_reported_specifically() {
        let cases = [
            ("", ParseError::Empty),
            ("  ", ParseError::Empty),
            ("tcp:,1433", ParseError::MissingHost),
            ("\\inst", ParseError::MissingHost),
            ("db01,abc", ParseError::InvalidPort("abc".to_string())),
            ("db01,65536", ParseError::InvalidPort("65536".to_string())),
            ("db01,70000", ParseError::InvalidPort("70000".to_string())),
            ("np:", ParseError::MissingHost),
            ("(localdb)\\ ", ParseError::MissingInstance),
            ("db01\\MSSQLSERVER", ParseError::ReservedInstance),
            ("tcp:db01\\mssqlserver", ParseError::ReservedInstance),
            // A pipe takes no port, so the reserved name is refused there too.
            ("np:db01\\MSSQLSERVER", ParseError::ReservedInstance),
            // An empty port is no port: the reserved name is refused.
            ("db01\\MSSQLSERVER,", ParseError::ReservedInstance),
            // The first comma starts the port, as for the client.
            ("db01,1433,1", ParseError::InvalidPort("1433,1".to_string())),
            ("\\\\", ParseError::MissingHost),
            // A port only over TCP, as the client requires.
            ("np:db01,1433", ParseError::PortNotAllowed("np")),
            ("lpc:.,1433", ParseError::PortNotAllowed("lpc")),
            ("admin:db01,1434", ParseError::PortNotAllowed("admin")),
            // A pipe path needs pipe\ and a name after the host.
            ("\\\\db01", ParseError::InvalidPipe("\\\\db01".to_string())),
            (
                "\\\\db01\\garbage",
                ParseError::InvalidPipe("\\\\db01\\garbage".to_string()),
            ),
            (
                "np:\\\\db01\\pipe\\",
                ParseError::InvalidPipe("\\\\db01\\pipe\\".to_string()),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_for(input, true), Err(expected), "{input:?}");
        }
    }

    /// Shared memory reaches only this computer, under the names the client
    /// takes for it on Windows; any other host is refused, as the client
    /// refuses it.
    #[test]
    fn shared_memory_is_accepted_only_for_this_computer() {
        let mut local: Vec<String> = [
            ".",
            "(local)",
            "(LOCAL)",
            "(Local)",
            "localhost",
            "LocalHost",
        ]
        .map(String::from)
        .into();
        if let Some(computer) = mssql_tds::connection::this_computer_name() {
            local.push(computer.to_lowercase());
            local.push(computer.to_uppercase());
        }
        for host in local {
            assert_eq!(
                parse_for(&format!("lpc:{host}"), true),
                Ok(target(Protocol::SharedMemory, &host, None, None)),
                "{host}"
            );
        }
        assert_eq!(
            parse_for("lpc:.\\SQLEXPRESS", true),
            Ok(target(
                Protocol::SharedMemory,
                ".",
                Some("SQLEXPRESS"),
                None
            ))
        );
        // An address is not one of the names.
        for host in ["remoteserver", "db01.contoso.com", "127.0.0.1"] {
            assert_eq!(
                parse_for(&format!("lpc:{host}"), true),
                Err(ParseError::SharedMemoryNotLocal(host.to_string())),
                "{host}"
            );
        }
        assert_eq!(
            parse_for("lpc:remoteserver\\SQL2022", true),
            Err(ParseError::SharedMemoryNotLocal("remoteserver".to_string()))
        );
    }

    /// The client drops an instance name after the port; the port decides.
    /// It trims the whole text after the comma before it cuts the port at
    /// the `\`, so a space before the `\` is in the port, which it refuses.
    #[test]
    fn an_instance_after_the_port_is_dropped_as_the_client_drops_it() {
        assert_eq!(
            parse("tcp:db01,1433\\SQL2022"),
            Ok(target(Protocol::Tcp, "db01", None, Some(1433)))
        );
        assert_eq!(
            parse("db01, 50001\\SQL2022"),
            Ok(target(Protocol::Tcp, "db01", None, Some(50001)))
        );
        assert_eq!(
            parse("db01, 50001 \\SQL2022"),
            Err(ParseError::InvalidPort("50001 ".to_string()))
        );
    }

    /// Spacing around the port and an instance, an empty port and port 0, as
    /// the client's own parser takes them: Connection input accepts and
    /// refuses the same servers, with the port the client connects to, or a
    /// SQL Server Browser lookup where the client makes one.
    #[test]
    fn port_and_instance_spacing_matches_the_client() {
        use mssql_tds::connection::client_context::ClientContext;
        for server in [
            // Ordinary ports.
            "tcp:127.0.0.1,50001",
            "127.0.0.1,1433",
            "tcp:127.0.0.1, 50001",
            "tcp:127.0.0.1 , 50001",
            "tcp:127.0.0.1,50001 ",
            "TCP: 127.0.0.1 , 1500 ",
            // An instance before the port.
            "127.0.0.1\\SQL2022,50001",
            "127.0.0.1\\SQL2022 , 50001",
            "127.0.0.1\\ SQL2022 ,50001",
            // An instance after the port, which the port overrides.
            "tcp:127.0.0.1,50001\\SQL2022",
            "tcp:127.0.0.1, 50001\\SQL2022",
            "tcp:127.0.0.1,50001\\ SQL2022",
            // A space before that instance is in the port.
            "tcp:127.0.0.1, 50001 \\SQL2022",
            "tcp:127.0.0.1,50001 \\SQL2022",
            "127.0.0.1, 50001 \\SQL2022",
            "tcp:127.0.0.1,50001\t\\SQL2022",
            // Not a port at all.
            "tcp:127.0.0.1,5 0001",
            "tcp:127.0.0.1,70000",
            // Port 0, which the client takes as a port.
            "tcp:127.0.0.1,0",
            "127.0.0.1,0\\SQL2022",
            // An empty port: 1433, or a named instance's port from SQL
            // Server Browser.
            "tcp:127.0.0.1,",
            "127.0.0.1,",
            "tcp:127.0.0.1, ",
            "127.0.0.1,\\SQL2022",
            "127.0.0.1\\,",
            "127.0.0.1\\SQL2022,",
            "127.0.0.1\\SQL2022 , ",
            "127.0.0.1\\MSSQLSERVER,",
        ] {
            let client = ClientContext::default().parse_datasource(server);
            let ours = parse(server);
            assert_eq!(
                ours.is_ok(),
                client.is_ok(),
                "{server:?}: ours {ours:?}, client {client:?}"
            );
            if let (Ok(ours), Ok(client)) = (ours, client) {
                assert!(ours.protocol.uses_tcp(), "{server:?}");
                if client.needs_ssrp() {
                    // A named instance with no port: SQL Server Browser.
                    assert!(ours.needs_instance_lookup(), "{server:?}: {ours:?}");
                    assert_eq!(
                        ours.instance.as_deref(),
                        Some(client.instance_name.as_str())
                    );
                } else {
                    // The port the client connects to: the one given, or
                    // 1433 for an empty one. It decides: no Browser lookup.
                    let port = client.protocol_parameter.parse::<u16>().unwrap_or(1433);
                    assert_eq!(ours.port, Some(port), "{server:?}");
                    assert!(!ours.needs_instance_lookup(), "{server:?}");
                }
            }
        }
    }

    #[test]
    fn a_named_instance_without_a_port_needs_sql_browser() {
        assert!(parse("db01\\SQL2022").unwrap().needs_instance_lookup());
        assert!(
            !parse("db01\\SQL2022,50001")
                .unwrap()
                .needs_instance_lookup()
        );
        // The default instance's reserved name needs a port, which then decides.
        assert!(
            !parse("db01\\MSSQLSERVER,1433")
                .unwrap()
                .needs_instance_lookup()
        );
        assert!(!parse("db01").unwrap().needs_instance_lookup());
        assert!(
            !parse_for("np:\\\\db01\\pipe\\sql\\query", true)
                .unwrap()
                .needs_instance_lookup()
        );
    }

    #[test]
    fn this_machine_resolves_as_the_client_resolves_it() {
        let computer = mssql_tds::connection::this_computer_name();
        let over_tcp = |server: &str| parse(server).unwrap().resolvable_host().into_owned();
        assert_eq!(over_tcp("tcp:."), "localhost");
        assert_eq!(over_tcp("tcp:(local)"), "localhost");
        assert_eq!(over_tcp("admin:localhost"), "localhost");
        assert_eq!(over_tcp("db01"), "db01");
        // An address is left as given, never read as a name for this machine.
        assert_eq!(over_tcp("tcp:127.0.0.1,1433"), "127.0.0.1");
        assert_eq!(over_tcp("::1"), "::1");
        if let Some(computer) = computer {
            // Over TCP, or with an instance, this computer's name is localhost;
            // otherwise the client keeps the name.
            assert_eq!(over_tcp(&format!("tcp:{computer},1433")), "localhost");
            assert_eq!(over_tcp(&format!("{computer}\\SQL2022")), "localhost");
            assert_eq!(over_tcp(&computer), computer);
            assert_eq!(over_tcp("."), computer);
        }
    }

    /// The client reads this computer's name from the operating system, not
    /// from an environment variable that may be unset.
    #[test]
    fn this_computers_name_is_local_without_an_environment_variable() {
        let Some(computer) = mssql_tds::connection::this_computer_name() else {
            return;
        };
        for host in [computer.clone(), computer.to_uppercase()] {
            assert!(parse(&host).unwrap().is_local(), "{host}");
        }
    }

    /// As the client: an empty instance is the default instance, with or
    /// without a port; only LocalDB needs a name after `\`.
    #[test]
    fn an_empty_instance_is_the_default_instance() {
        assert_eq!(
            parse("tcp:db01\\,1433"),
            Ok(target(Protocol::Tcp, "db01", None, Some(1433)))
        );
        assert_eq!(
            parse("db01\\,1433"),
            Ok(target(Protocol::Tcp, "db01", None, Some(1433)))
        );
        for windows in [true, false] {
            assert_eq!(
                parse_for("tcp:db01\\", windows),
                Ok(target(Protocol::Tcp, "db01", None, None))
            );
            assert_eq!(
                parse_for(" db01 \\ ", windows),
                Ok(target(Protocol::Default, "db01", None, None))
            );
        }
        assert_eq!(
            parse_for("np:db01\\", true),
            Ok(target(Protocol::NamedPipe, "db01", None, None))
        );
        assert_eq!(
            parse_for("lpc:.\\", true),
            Ok(target(Protocol::SharedMemory, ".", None, None))
        );
        assert_eq!(
            parse_for("(localdb)\\", true),
            Err(ParseError::MissingInstance)
        );
        assert!(!parse("db01\\").unwrap().needs_instance_lookup());
    }

    /// The client's own parser takes these spellings as the default
    /// instance; so does Connection input, on this platform.
    #[test]
    fn an_empty_instance_matches_the_client() {
        use mssql_tds::connection::client_context::ClientContext;
        for server in [
            "tcp:127.0.0.1\\",
            "127.0.0.1\\",
            "np:127.0.0.1\\",
            "lpc:.\\",
            "tcp:127.0.0.1\\,1433",
            "127.0.0.1\\SQL2022",
        ] {
            let client = ClientContext::default().parse_datasource(server);
            let ours = parse(server);
            assert_eq!(ours.is_ok(), client.is_ok(), "{server}: {ours:?}");
            if let (Ok(ours), Ok(client)) = (ours, client) {
                assert_eq!(
                    ours.instance.unwrap_or_default(),
                    client.instance_name,
                    "{server}"
                );
            }
        }
    }

    /// The client takes a pipe path after any prefix it recognizes, `tcp:`,
    /// `admin:` and `lpc:` included, as a named pipe, before a comma could be
    /// a port; Connection input takes the same servers as the same pipe.
    #[test]
    fn a_pipe_path_after_any_prefix_is_a_named_pipe_as_for_the_client() {
        use mssql_tds::connection::client_context::ClientContext;
        let pipe = "\\\\127.0.0.1\\pipe\\sql\\query";
        let mut servers = vec![pipe.to_string()];
        for prefix in ["np:", "tcp:", "admin:", "lpc:", "TCP :", "Admin: "] {
            servers.push(format!("{prefix}{pipe}"));
        }
        servers.push("tcp:\\\\127.0.0.1\\pipe\\custom,1".to_string());
        servers.push("admin:\\\\.\\PIPE\\MSSQL$SQL2022\\sql\\query".to_string());
        for server in &servers {
            let ours = parse_for(server, true).unwrap_or_else(|e| panic!("{server}: {e:?}"));
            assert_eq!(ours.protocol, Protocol::NamedPipe, "{server}");
            assert_eq!(ours.port, None, "{server}");
            assert!(!ours.needs_instance_lookup(), "{server}");
            // Off Windows each is refused as a bare pipe path is.
            assert_eq!(
                parse_for(server, false),
                Err(ParseError::NamedPipesNotSupported),
                "{server}"
            );
            // The client's own parser, on this platform, agrees.
            let client = ClientContext::default().parse_datasource(server);
            if cfg!(windows) {
                let client = client.unwrap_or_else(|e| panic!("{server}: {e:?}"));
                assert_eq!(client.protocol_name, "np", "{server}");
                assert_eq!(ours.host, client.original_server_name, "{server}");
                assert_eq!(parse(server), Ok(ours), "{server}");
            } else {
                let message = client.expect_err(server).to_string();
                assert!(
                    message.contains(&ParseError::NamedPipesNotSupported.to_string()),
                    "{server}: {message}"
                );
                assert_eq!(
                    parse(server),
                    Err(ParseError::NamedPipesNotSupported),
                    "{server}"
                );
            }
        }
        // A malformed pipe path is reported as such after any prefix too, on
        // either platform, and the client refuses it.
        for prefix in ["", "np:", "tcp:", "admin:", "lpc:"] {
            let server = format!("{prefix}\\\\db01\\garbage");
            for windows in [true, false] {
                assert_eq!(
                    parse_for(&server, windows),
                    Err(ParseError::InvalidPipe("\\\\db01\\garbage".to_string())),
                    "{server}"
                );
            }
            assert!(
                ClientContext::default().parse_datasource(&server).is_err(),
                "{server}"
            );
        }
    }
}

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

use std::fmt;

/// The transport the server string selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// No prefix: diagnosed over TCP, the connection attempt included. On
    /// Windows sqlcmd can also use shared memory or named pipes; those are
    /// diagnosed only when asked for with `lpc:` or `np:`.
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
    /// The name to resolve: `.` and `(local)` are this machine.
    pub fn resolvable_host(&self) -> &str {
        if self.host == "." || self.host.eq_ignore_ascii_case("(local)") {
            "localhost"
        } else {
            &self.host
        }
    }

    /// Whether the port must come from SQL Server Browser: a named instance
    /// with no port given.
    pub fn needs_instance_lookup(&self) -> bool {
        self.protocol.uses_tcp() && self.port.is_none() && self.instance.is_some()
    }
}

/// Why the server string could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No server was given.
    Empty,
    /// A protocol prefix sqlcmd does not know.
    UnknownProtocol(String),
    /// The host part is empty, e.g. `tcp:,1433` or `\inst`.
    MissingHost,
    /// The instance part is empty, e.g. `host\`.
    MissingInstance,
    /// The port is not a number from 1 to 65535.
    InvalidPort(String),
    /// A port given with a protocol that takes none (`np:`, `lpc:`, `admin:`).
    PortNotAllowed(&'static str),
    /// A pipe path that is not `\\host\pipe\name`.
    InvalidPipe(String),
    /// `MSSQLSERVER`, the default instance's reserved name, given without a
    /// port: the client refuses it, as it would a connection.
    ReservedInstance,
}

impl ParseError {
    /// A stable identifier for the error.
    pub fn code(&self) -> &'static str {
        match self {
            ParseError::Empty => "emptyServer",
            ParseError::UnknownProtocol(_) => "unknownProtocol",
            ParseError::MissingHost => "missingHost",
            ParseError::MissingInstance => "missingInstance",
            ParseError::InvalidPort(_) => "invalidPort",
            ParseError::PortNotAllowed(_) => "portNotAllowed",
            ParseError::InvalidPipe(_) => "invalidPipePath",
            ParseError::ReservedInstance => "reservedInstance",
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "no server was given"),
            ParseError::UnknownProtocol(p) => write!(
                f,
                "unknown protocol prefix `{p}:` (expected tcp, np, lpc or admin)"
            ),
            ParseError::MissingHost => write!(f, "the server name is empty"),
            ParseError::MissingInstance => write!(f, "the instance name after `\\` is empty"),
            ParseError::InvalidPort(p) => {
                write!(f, "`{p}` is not a port number from 1 to 65535")
            }
            ParseError::PortNotAllowed(p) => {
                write!(f, "a port is given only with TCP; `{p}:` takes none")
            }
            ParseError::InvalidPipe(p) => {
                write!(
                    f,
                    "`{p}` is not a pipe path of the form \\\\host\\pipe\\name"
                )
            }
            ParseError::ReservedInstance => write!(
                f,
                "`MSSQLSERVER` is the default instance's reserved name; give the server without it"
            ),
        }
    }
}

/// Parses the server as given to `-S`.
pub fn parse(server: &str) -> Result<Target, ParseError> {
    let server = server.trim();
    if server.is_empty() {
        return Err(ParseError::Empty);
    }

    // A bare IPv6 address (`dead:beef::1`) has several colons before any port
    // or instance; its first group is not a protocol prefix.
    // A pipe path (`\\host\pipe\...`) is a named-pipe server without the prefix.
    if server.starts_with("\\\\") {
        return parse_pipe(server);
    }
    // LocalDB first, as the client does: `(localdb)\` or `(localdb)/` and the
    // rest is the instance, whatever it holds. A bare `(localdb)` is a host.
    if server.get(..10).is_some_and(|p| {
        p.eq_ignore_ascii_case("(localdb)\\") || p.eq_ignore_ascii_case("(localdb)/")
    }) {
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
    let address_part = server.split([',', '\\']).next().unwrap_or(server);
    let bare_ipv6 = address_part.matches(':').count() > 1;
    let (protocol, rest) = match server.split_once(':').map(|(p, r)| (p.trim(), r)) {
        // The client trims the prefix too: `tcp :db01` is TCP.
        Some((prefix, rest))
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            match prefix.to_ascii_lowercase().as_str() {
                "tcp" => (Protocol::Tcp, rest.trim()),
                "np" => (Protocol::NamedPipe, rest.trim()),
                "lpc" => (Protocol::SharedMemory, rest.trim()),
                "admin" => (Protocol::Admin, rest.trim()),
                // Only a hexadecimal first group can be part of an IPv6 address.
                _ if bare_ipv6 && prefix.chars().all(|c| c.is_ascii_hexdigit()) => {
                    (Protocol::Default, server)
                }
                _ => return Err(ParseError::UnknownProtocol(prefix.to_string())),
            }
        }
        _ => (Protocol::Default, server),
    };

    // The client takes a port only over TCP.
    let takes_no_port = match protocol {
        Protocol::NamedPipe if !rest.starts_with("\\\\") => Some("np"),
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
        return parse_pipe(rest);
    }

    let (rest, port) = match rest.rsplit_once(',') {
        Some((rest, port)) => {
            let port = port.trim();
            match port.parse::<u16>() {
                Ok(p) if p > 0 => (rest, Some(p)),
                _ => return Err(ParseError::InvalidPort(port.to_string())),
            }
        }
        None => (rest, None),
    };

    let (host, instance) = match rest.split_once('\\') {
        Some((host, instance)) => {
            let instance = instance.trim();
            if instance.is_empty() {
                return Err(ParseError::MissingInstance);
            }
            (host.trim(), Some(instance.to_string()))
        }
        None => (rest.trim(), None),
    };
    if host.is_empty() {
        return Err(ParseError::MissingHost);
    }
    // The client refuses the default instance's reserved name unless a port
    // is given, which takes priority over the instance.
    if port.is_none()
        && instance
            .as_deref()
            .is_some_and(|i| i.eq_ignore_ascii_case("MSSQLSERVER"))
    {
        return Err(ParseError::ReservedInstance);
    }

    Ok(Target {
        protocol,
        host: host.to_string(),
        instance,
        port,
    })
}

/// A named pipe, `\\host\pipe\name` (the host is the first path segment), or
/// after `np:` a server name the client builds the pipe path from.
fn parse_pipe(path: &str) -> Result<Target, ParseError> {
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
                if instance.is_empty() {
                    return Err(ParseError::MissingInstance);
                }
                (host.trim(), None, Some(instance.to_string()))
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
                target(Protocol::Default, "db01", Some("SQL2022"), Some(50001)),
            ),
            (
                "TCP: 10.0.0.5 , 1500",
                target(Protocol::Tcp, "10.0.0.5", None, Some(1500)),
            ),
            ("lpc:.", target(Protocol::SharedMemory, ".", None, None)),
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
                target(Protocol::Default, "dead:beef::1", None, Some(1433)),
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
                target(Protocol::Default, "db01", Some("MSSQLSERVER"), Some(1433)),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse(input), Ok(expected), "{input}");
        }
    }

    #[test]
    fn unusable_servers_are_reported_specifically() {
        let cases = [
            ("", ParseError::Empty),
            ("  ", ParseError::Empty),
            ("udp:db01", ParseError::UnknownProtocol("udp".to_string())),
            // Not hexadecimal, so not an IPv6 group: an unknown prefix.
            (
                "udp:fe80::1",
                ParseError::UnknownProtocol("udp".to_string()),
            ),
            ("tcp:,1433", ParseError::MissingHost),
            ("\\inst", ParseError::MissingHost),
            ("db01\\", ParseError::MissingInstance),
            ("db01,abc", ParseError::InvalidPort("abc".to_string())),
            ("db01,0", ParseError::InvalidPort("0".to_string())),
            ("db01,70000", ParseError::InvalidPort("70000".to_string())),
            ("np:", ParseError::MissingHost),
            ("np:db01\\", ParseError::MissingInstance),
            ("(localdb)\\ ", ParseError::MissingInstance),
            ("db01\\MSSQLSERVER", ParseError::ReservedInstance),
            ("tcp:db01\\mssqlserver", ParseError::ReservedInstance),
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
            assert_eq!(parse(input), Err(expected), "{input:?}");
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
            !parse("np:\\\\db01\\pipe\\sql\\query")
                .unwrap()
                .needs_instance_lookup()
        );
    }

    #[test]
    fn this_machine_resolves_as_localhost() {
        assert_eq!(parse(".").unwrap().resolvable_host(), "localhost");
        assert_eq!(parse("(local)").unwrap().resolvable_host(), "localhost");
        assert_eq!(parse("db01").unwrap().resolvable_host(), "db01");
    }
}

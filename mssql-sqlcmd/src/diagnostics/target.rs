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
    /// No prefix: the client's protocol order. On Windows it tries shared
    /// memory first for this computer, then TCP, then named pipes; the
    /// diagnosis follows the transports the client tried.
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
    /// Shared memory (`lpc:`) to a host that is not this computer: the client
    /// opens shared memory only to a local server.
    SharedMemoryNotLocal(String),
    /// Shared memory (`lpc:`) off Windows, where the client has none.
    SharedMemoryNotSupported,
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
            ParseError::SharedMemoryNotLocal(_) => "sharedMemoryNotLocal",
            ParseError::SharedMemoryNotSupported => "sharedMemoryNotSupported",
        }
    }

    pub fn message_id(&self) -> &'static str {
        match self {
            ParseError::Empty => "diagnose.target.empty",
            ParseError::UnknownProtocol(_) => "diagnose.target.unknown_protocol",
            ParseError::MissingHost => "diagnose.target.missing_host",
            ParseError::MissingInstance => "diagnose.target.missing_instance",
            ParseError::InvalidPort(_) => "diagnose.target.invalid_port",
            ParseError::PortNotAllowed(_) => "diagnose.target.port_not_allowed",
            ParseError::InvalidPipe(_) => "diagnose.target.invalid_pipe",
            ParseError::ReservedInstance => "diagnose.target.reserved_instance",
            ParseError::SharedMemoryNotLocal(_) => "diagnose.target.shared_memory_not_local",
            ParseError::SharedMemoryNotSupported => "diagnose.target.shared_memory_not_supported",
        }
    }

    pub fn message_args(&self) -> Vec<(&'static str, String)> {
        match self {
            ParseError::UnknownProtocol(protocol) => vec![("protocol", protocol.clone())],
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

    // As the client: the first comma starts the port, and an instance name
    // after the port (`db01,1433\SQL2022`) is dropped, as the port decides.
    let (rest, port) = match rest.split_once(',') {
        Some((rest, port)) => {
            let port = port.split('\\').next().unwrap_or(port).trim();
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
                // The port decides over the instance, so an empty one is not
                // an error then (`tcp:db01\,1433`), as for the client.
                if port.is_none() {
                    return Err(ParseError::MissingInstance);
                }
                (host.trim(), None)
            } else {
                (host.trim(), Some(instance.to_string()))
            }
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
    if protocol == Protocol::SharedMemory {
        shared_memory_host(host)?;
    }

    Ok(Target {
        protocol,
        host: host.to_string(),
        instance,
        port,
    })
}

/// As the client: shared memory only on Windows, and only to this computer:
/// `.`, `(local)` as written, or `localhost` or this computer's name in any
/// case.
fn shared_memory_host(host: &str) -> Result<(), ParseError> {
    if !cfg!(windows) {
        return Err(ParseError::SharedMemoryNotSupported);
    }
    let local = host == "."
        || host == "(local)"
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
                // A pipe takes no port, so the reserved name is refused here.
                if instance.eq_ignore_ascii_case("MSSQLSERVER") {
                    return Err(ParseError::ReservedInstance);
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
                target(Protocol::Default, "db01", Some("SQL2022"), Some(50001)),
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
            // A pipe takes no port, so the reserved name is refused there too.
            ("np:db01\\MSSQLSERVER", ParseError::ReservedInstance),
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
            assert_eq!(parse(input), Err(expected), "{input:?}");
        }
    }

    /// Shared memory reaches only this computer, under the names the client
    /// takes for it; any other host is refused, as the client refuses it.
    #[cfg(windows)]
    #[test]
    fn shared_memory_is_accepted_only_for_this_computer() {
        let mut local: Vec<String> = [".", "(local)", "localhost", "LocalHost"]
            .map(String::from)
            .into();
        if let Some(computer) = mssql_tds::connection::this_computer_name() {
            local.push(computer.to_lowercase());
            local.push(computer.to_uppercase());
        }
        for host in local {
            assert_eq!(
                parse(&format!("lpc:{host}")),
                Ok(target(Protocol::SharedMemory, &host, None, None)),
                "{host}"
            );
        }
        assert_eq!(
            parse("lpc:.\\SQLEXPRESS"),
            Ok(target(
                Protocol::SharedMemory,
                ".",
                Some("SQLEXPRESS"),
                None
            ))
        );
        // An address is not one of the names; the client takes `(local)`
        // only as written.
        for host in ["remoteserver", "db01.contoso.com", "127.0.0.1", "(LOCAL)"] {
            assert_eq!(
                parse(&format!("lpc:{host}")),
                Err(ParseError::SharedMemoryNotLocal(host.to_string())),
                "{host}"
            );
        }
        assert_eq!(
            parse("lpc:remoteserver\\SQL2022"),
            Err(ParseError::SharedMemoryNotLocal("remoteserver".to_string()))
        );
    }

    /// Off Windows the client has no shared memory, for any host.
    #[cfg(not(windows))]
    #[test]
    fn shared_memory_is_refused_off_windows() {
        for server in ["lpc:.", "lpc:localhost", "lpc:remoteserver"] {
            assert_eq!(
                parse(server),
                Err(ParseError::SharedMemoryNotSupported),
                "{server}"
            );
        }
    }

    /// The client drops an instance name after the port; the port decides.
    #[test]
    fn an_instance_after_the_port_is_dropped_as_the_client_drops_it() {
        assert_eq!(
            parse("tcp:db01,1433\\SQL2022"),
            Ok(target(Protocol::Tcp, "db01", None, Some(1433)))
        );
        assert_eq!(
            parse("db01, 50001 \\SQL2022"),
            Ok(target(Protocol::Default, "db01", None, Some(50001)))
        );
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

    #[test]
    fn an_empty_instance_is_accepted_when_a_port_decides() {
        assert_eq!(
            parse("tcp:db01\\,1433"),
            Ok(target(Protocol::Tcp, "db01", None, Some(1433)))
        );
        assert_eq!(
            parse("db01\\,1433"),
            Ok(target(Protocol::Default, "db01", None, Some(1433)))
        );
        assert_eq!(parse("tcp:db01\\"), Err(ParseError::MissingInstance));
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The error text a normal sqlcmd run shows for the same event, so the errors
//! of a diagnosis read like the ones a query prints.
//!
//! sqlcmd's queries connect through the ODBC driver, which words a failure as
//! its network layer (SNI) does: a provider prefix (`TCP Provider: `, ...) and
//! the operating system's message on Windows, or `Error code 0x...` with the
//! Windows code elsewhere. After a failed connection it adds summaries
//! (`Login timeout expired`, `A network-related or instance-specific error
//! ...`). Each record keeps the event diagnose observed: where the driver
//! reports another one after its own retries and fallbacks (a timeout for a
//! refused port, a named-pipe error for a name that does not resolve), the
//! record says what happened, in the driver's wording for it.
//!
//! That wording is the driver's own, from its resources, not sqlcmd's: these
//! lines are the en-US driver's, verbatim, in every locale (literals, not
//! catalog messages), so they read exactly as the en-US driver prints them.
//! Matching a localized driver's resources is out of scope. Only sqlcmd's
//! own prefix around them (`Sqlcmd: Error: ...`) follows the report's locale.

use super::target::Protocol;
use super::{Check, CheckId, Encrypt, ErrorRecord, LocalizedText};

/// The client that made the connection attempt, as sqlcmd's error lines name it.
pub const CLIENT: &str = "mssql-tds";

/// SNI error 26, with the status SNI shows when SQL Server Browser gives no port.
const ERROR_LOCATING_SERVER: &str =
    "SQL Server Network Interfaces: Error Locating Server/Instance Specified [xFFFFFFFF]. ";
/// The driver's HYT00 and 08001 summaries.
const LOGIN_TIMEOUT_EXPIRED: &str = "Login timeout expired";
const CLIENT_UNABLE: &str = "Client unable to establish connection";
const ENCRYPTION_ERRORS_LINK: &str = "https://go.microsoft.com/fwlink/?linkid=2226722";

/// Windows error codes, which the driver reports on every platform.
const WAIT_TIMEOUT: i32 = 258;
/// What SNI reports for a pipe that did not open in time: `WaitNamedPipe`'s
/// error when its time-out elapses.
const ERROR_SEM_TIMEOUT: i32 = 121;
const WSAENETUNREACH: i32 = 10051;
const WSAECONNRESET: i32 = 10054;
const WSAETIMEDOUT: i32 = 10060;
const WSAECONNREFUSED: i32 = 10061;
const WSAEHOSTUNREACH: i32 = 10065;
const WSAHOST_NOT_FOUND: i32 = 11001;
const WSATRY_AGAIN: i32 = 11002;
const WSANO_DATA: i32 = 11004;

/// The driver's text, the same in every locale.
fn driver(text: impl Into<String>) -> LocalizedText {
    LocalizedText::literal(text)
}

fn network_related(server: &str) -> LocalizedText {
    driver(format!(
        "A network-related or instance-specific error has occurred while establishing a \
         connection to {server}. Server is not found or not accessible. Check if instance \
         name is correct and if SQL Server is configured to allow remote connections. For \
         more information see SQL Server Books Online."
    ))
}

fn client_unable_encryption() -> LocalizedText {
    driver(format!(
        "Client unable to establish connection. For solutions related to encryption errors, \
         see {ENCRYPTION_ERRORS_LINK}"
    ))
}

/// The connection's encryption settings (`-N`, `-C`), which decide the
/// summary the driver adds after a failed TLS handshake.
#[derive(Clone, Copy, Debug)]
pub(super) struct Encryption {
    pub encrypt: Encrypt,
    pub trust_server_certificate: bool,
}

impl Encryption {
    /// sqlcmd's default: `-N` mandatory, without `-C`.
    #[cfg(test)]
    const DEFAULT: Encryption = Encryption {
        encrypt: Encrypt::Mandatory,
        trust_server_certificate: false,
    };

    /// Whether the driver's summary links the encryption guide: as
    /// msodbcsql's `CTdsParser::SetUpConnection` (`netconn.cpp`) posts it when
    /// the TLS handshake fails other than by timing out, only for
    /// `Encrypt=Mandatory` (`fEncrypt == ENCRYPT_ON`) without
    /// `TrustServerCertificate`; optional and strict encryption, and `-C`,
    /// get the plain summary, whatever the TLS error was.
    fn links_the_encryption_guide(self) -> bool {
        self.encrypt == Encrypt::Mandatory && !self.trust_server_certificate
    }
}

/// Gives each record the driver's text for its event, orders SQL Server's
/// errors as the driver lists them, and adds the summaries the driver gives
/// after the connection fails. `server` is the server as the user gave it.
pub(super) fn apply(
    errors: &mut Vec<ErrorRecord>,
    checks: &[Check],
    protocol: Option<Protocol>,
    server: &str,
    encryption: Encryption,
) {
    apply_with(errors, checks, protocol, server, encryption, cfg!(windows));
}

/// [`apply_with`] for sqlcmd's default encryption.
#[cfg(test)]
fn apply_for(
    errors: &mut Vec<ErrorRecord>,
    checks: &[Check],
    protocol: Option<Protocol>,
    server: &str,
    windows: bool,
) {
    apply_with(
        errors,
        checks,
        protocol,
        server,
        Encryption::DEFAULT,
        windows,
    );
}

fn apply_with(
    errors: &mut Vec<ErrorRecord>,
    checks: &[Check],
    protocol: Option<Protocol>,
    server: &str,
    encryption: Encryption,
    windows: bool,
) {
    order_server_errors(errors);
    for record in errors.iter_mut() {
        // A transport the client moved past is not in a normal run's errors.
        record.text = if record.superseded {
            None
        } else {
            text(record, protocol, windows)
        };
    }
    add_summaries(errors, checks, server, encryption, windows);
}

/// SQL Server error numbers the driver maps to a SQLSTATE of their own
/// (msodbcsql's `g_rgSqlStateMap`, `sqlcstr.cpp`), such as 18456 to 28000.
const MAPPED_SERVER_ERRORS: [u32; 118] = [
    109, 110, 120, 121, 168, 206, 207, 208, 210, 211, 213, 220, 229, 230, 232, 233, 234, 235, 236,
    237, 238, 241, 242, 244, 245, 246, 248, 256, 266, 267, 272, 273, 295, 296, 305, 310, 409, 512,
    515, 517, 518, 529, 535, 544, 547, 550, 628, 911, 916, 919, 1007, 1010, 1205, 1211, 1505, 1508,
    1906, 1911, 1913, 2501, 2601, 2615, 2627, 2705, 2706, 2714, 2727, 2740, 3605, 3606, 3607, 3622,
    3701, 3718, 3902, 3903, 3906, 3908, 4002, 4017, 4019, 4401, 4409, 4501, 4502, 4506, 4701, 4902,
    4924, 5701, 5703, 6401, 7112, 8101, 8115, 8134, 8152, 8153, 16902, 16916, 16930, 16931, 16934,
    16947, 17809, 18450, 18452, 18456, 18458, 18459, 18463, 18464, 18465, 18466, 18467, 18468,
    18487, 18488,
];

/// The resource ids of the SQLSTATEs the driver gives a SQL Server error with
/// no mapping of its own, by class (`sqlcerr.cpp`): 42000 above class 10,
/// HY000 above class 18, 01000 (a warning) up to class 10.
const IDS_37_000: u32 = 40136;
const IDS_S1_000_01: u32 = 40146;
const IDS_01_000_00: u32 = 40401;

/// The driver's precedence for a SQL Server error, its `std_error`: 0 for a
/// number mapped to its own SQLSTATE, else the resource id of its class's
/// default SQLSTATE. Lower comes first.
fn native_precedence(record: &ErrorRecord) -> u32 {
    if record
        .number
        .is_some_and(|n| n != 0 && MAPPED_SERVER_ERRORS.binary_search(&n).is_ok())
    {
        return 0;
    }
    match record.class.unwrap_or(0) {
        class if class > 18 => IDS_S1_000_01,
        class if class > 10 => IDS_37_000,
        _ => IDS_01_000_00,
    }
}

/// The driver lists a login's errors as ODBC 3 sorts them (`SortErrors`,
/// `sqlcerr.cpp`): by row, then by precedence, `std_error`, then in the
/// order received; not by class. An error with its own SQLSTATE (18456 is
/// 28000) comes before one given its class's (50000, class 16, is 42000,
/// as 4060 is), whatever the classes. A login's errors share a row.
fn order_server_errors(errors: &mut [ErrorRecord]) {
    let at: Vec<usize> = (0..errors.len())
        .filter(|&i| errors[i].source == "sqlServer")
        .collect();
    let mut server: Vec<ErrorRecord> = at.iter().map(|&i| errors[i].clone()).collect();
    // Stable: the order received within a precedence.
    server.sort_by_key(native_precedence);
    for (i, record) in at.into_iter().zip(server) {
        errors[i] = record;
    }
}

/// The driver's text for one record, when the driver has an equivalent.
fn text(record: &ErrorRecord, protocol: Option<Protocol>, windows: bool) -> Option<LocalizedText> {
    let tcp = |code: i32| driver(format!("TCP Provider: {}", system_text(code, windows)));
    match (record.source, record.check) {
        ("sqlServer", _) => Some(record.message.clone()),
        ("sqlBrowser", _) => Some(driver(ERROR_LOCATING_SERVER)),
        ("os", CheckId::NameResolution) => dns_code(record, windows).map(tcp),
        ("os", CheckId::TcpConnect) if protocol.is_none_or(Protocol::uses_tcp) => {
            tcp_code(record, windows).map(tcp)
        }
        // SNI opens a LocalDB instance as a plain named pipe, reported as
        // the named-pipe provider's.
        ("os", _) if matches!(protocol, Some(Protocol::NamedPipe | Protocol::LocalDb)) => {
            pipe_text("Named Pipes Provider: ", record)
        }
        ("os", _) if protocol == Some(Protocol::SharedMemory) => {
            pipe_text("Shared Memory Provider: ", record)
        }
        ("os", _) if protocol.is_none_or(Protocol::uses_tcp) => tcp_code(record, windows).map(tcp),
        ("tls", _) => tls_text(&record.message.render("en-US"), windows)
            .map(|t| driver(format!("SSL Provider: {t}"))),
        _ => None,
    }
}

/// SNI's text for a pipe that did not open (error 40), named pipes' or shared
/// memory's (`prefix`): the operating system's code, or for one the time
/// limit abandoned the code `WaitNamedPipe` gives when its time-out elapses.
fn pipe_text(prefix: &str, record: &ErrorRecord) -> Option<LocalizedText> {
    let code = record
        .os_error
        .or_else(|| (record.code.as_deref() == Some("timedOut")).then_some(ERROR_SEM_TIMEOUT))?;
    Some(driver(format!(
        "{prefix}Could not open a connection to SQL Server [{code}]. "
    )))
}

/// The Windows code for a failed name lookup.
fn dns_code(record: &ErrorRecord, windows: bool) -> Option<i32> {
    if windows && record.os_error.is_some() {
        return record.os_error;
    }
    match record.code.as_deref()? {
        "notFound" => Some(WSAHOST_NOT_FOUND),
        "temporaryFailure" => Some(WSATRY_AGAIN),
        "noAddresses" => Some(WSANO_DATA),
        "timedOut" => Some(WSAETIMEDOUT),
        _ => None,
    }
}

/// The Windows code for a failed TCP connect. The driver reports a connect
/// that ran out of time as a wait that timed out.
fn tcp_code(record: &ErrorRecord, windows: bool) -> Option<i32> {
    match record.code.as_deref() {
        Some("refused") => Some(WSAECONNREFUSED),
        Some("timedOut") => Some(WAIT_TIMEOUT),
        Some("hostUnreachable") => Some(WSAEHOSTUNREACH),
        Some("networkUnreachable") => Some(WSAENETUNREACH),
        Some("reset") => Some(WSAECONNRESET),
        // Elsewhere the number is the platform's own, not a Windows code.
        _ if windows => record.os_error,
        _ => None,
    }
}

/// The text the driver shows for a Windows error code after a provider's
/// prefix: the operating system's message (in the system's language) on
/// Windows, `Error code 0x...` elsewhere.
fn system_text(code: i32, windows: bool) -> String {
    if windows {
        os_message(code)
    } else {
        // The driver writes the code's bits in hex, as Windows does.
        format!("Error code 0x{:X}", code.cast_unsigned())
    }
}

/// The operating system's message for an error code, without Rust's
/// ` (os error N)`. The message ends its line in the driver's output, so
/// sqlcmd's closing period follows it there; without its own period it
/// reads the same on one line.
fn os_message(code: i32) -> String {
    let text = std::io::Error::from_raw_os_error(code).to_string();
    let suffix = format!(" (os error {code})");
    let text = text.strip_suffix(&suffix).unwrap_or(&text).trim();
    text.strip_suffix('.').unwrap_or(text).to_string()
}

/// The driver's text for a failed TLS handshake. On Windows: the system's
/// message for the Schannel status (or, when the server closed the connection
/// during the handshake, for a reset). Elsewhere: the OpenSSL error, as
/// `[error:<code>:<library>::<reason>:<verify result>]`.
fn tls_text(message: &str, windows: bool) -> Option<String> {
    if windows {
        if let Some(at) = message.find("status=0x") {
            let hex = message.get(at + 9..at + 17)?;
            let status = u32::from_str_radix(hex, 16).ok()?;
            return Some(os_message(status.cast_signed()));
        }
        return message
            .contains("peer closed connection during TLS handshake")
            .then(|| os_message(WSAECONNRESET));
    }
    let rest = &message[message.find("error:")? + "error:".len()..];
    let mut parts = rest.splitn(5, ':');
    let code = parts.next()?;
    if code.len() != 8 || !code.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let library = parts.next()?;
    let _function = parts.next()?;
    let reason = parts.next()?;
    let verify = parts
        .next()
        .and_then(|tail| tail.split_once(" (")?.1.split_once(')'))
        .map(|(verify, _)| verify);
    Some(match verify {
        Some(verify) => format!("[error:{code}:{library}::{reason}:{verify}]"),
        None => format!("[error:{code}:{library}::{reason}]"),
    })
}

/// The summaries the driver adds after a failed connection. Windows lists
/// the network error before `Login timeout expired` (added only when a time
/// limit ran out); Linux and macOS after.
fn add_summaries(
    errors: &mut Vec<ErrorRecord>,
    checks: &[Check],
    server: &str,
    encryption: Encryption,
    windows: bool,
) {
    // The failure a normal run reports: the first failed check with errors
    // still current (after the protocol order moved past TCP, the pipe's).
    let failed_checks = || checks.iter().filter(|c| c.coverage.failed());
    let Some(failed) = failed_checks()
        .find(|c| errors.iter().any(|e| e.check == c.id && !e.superseded))
        .or_else(|| failed_checks().next())
    else {
        return;
    };
    let check = failed.id;
    let worded = |source: &str| {
        errors
            .iter()
            .any(|e| e.check == check && e.source == source && e.text.is_some())
    };
    let summary = |code: &str, text: LocalizedText| {
        let mut record = ErrorRecord::new("client", check, String::new());
        record.code = Some(code.to_string());
        record.message = text.clone();
        record.text = Some(text);
        record
    };
    let network = match check {
        CheckId::NameResolution | CheckId::TcpConnect => worded("os"),
        CheckId::InstanceResolution => worded("sqlBrowser"),
        CheckId::ConnectionAttempt => worded("os"),
        _ => false,
    };
    let tls = check == CheckId::ConnectionAttempt && worded("tls");
    // Only when a time limit ran out: the driver prints it after its own
    // retries do, but diagnose reports the event it observed, and a refused
    // port or an unknown name is not a timeout.
    let timed_out = errors.iter().any(|e| {
        e.check == check
            && !e.superseded
            && matches!(
                e.code.as_deref(),
                Some("timedOut" | "notAttempted" | "noAnswer")
            )
    });
    // A connection attempt the time limit ended before the driver had an
    // operating-system or provider message to word still gets the line, as
    // the driver prints it for any login that runs out of time.
    if timed_out && (network || check == CheckId::ConnectionAttempt) {
        let expired = summary("loginTimeoutExpired", driver(LOGIN_TIMEOUT_EXPIRED));
        if windows {
            errors.push(expired);
        } else {
            let first = errors
                .iter()
                .position(|e| e.check == check && e.text.is_some())
                .unwrap_or(errors.len());
            errors.insert(first, expired);
        }
    }
    if network {
        errors.push(summary("networkRelatedError", network_related(server)));
    } else if tls && !timed_out {
        // As the driver: after a handshake that failed (one that timed out
        // gets the timeout instead), the summary follows the connection's
        // settings, not which TLS error it was.
        let text = if encryption.links_the_encryption_guide() {
            client_unable_encryption()
        } else {
            driver(CLIENT_UNABLE)
        };
        errors.push(summary("clientUnableToEstablishConnection", text));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Coverage, Field};
    use super::*;

    fn record(source: &'static str, check: CheckId, message: &str) -> ErrorRecord {
        ErrorRecord::new(source, check, message)
    }

    fn os(check: CheckId, code: &str, os_error: Option<i32>, message: &str) -> ErrorRecord {
        let mut r = record("os", check, message);
        r.code = Some(code.to_string());
        r.os_error = os_error;
        r
    }

    fn server(number: u32, class: i32, message: &str) -> ErrorRecord {
        let mut r = record("sqlServer", CheckId::ConnectionAttempt, message);
        r.number = Some(number);
        r.class = Some(class);
        r
    }

    fn failed(id: CheckId) -> Check {
        Check::new(id, Coverage::Diagnosed)
    }

    fn texts(errors: &[ErrorRecord]) -> Vec<String> {
        errors
            .iter()
            .map(|e| {
                e.text
                    .as_ref()
                    .map_or_else(|| format!("<{}>", e.message), |t| t.render("en-US"))
            })
            .collect()
    }

    const NETWORK: &str = "A network-related or instance-specific error has occurred while \
         establishing a connection to tcp:db01,1. Server is not found or not accessible. Check if \
         instance name is correct and if SQL Server is configured to allow remote connections. \
         For more information see SQL Server Books Online.";

    #[test]
    fn a_server_error_keeps_its_text_and_needs_no_summary() {
        let mut errors = vec![server(18456, 14, "Login failed for user 'sa'.")];
        let checks = [failed(CheckId::ConnectionAttempt)];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::Tcp),
            "tcp:db01,1",
            true,
        );
        assert_eq!(texts(&errors), ["Login failed for user 'sa'."]);
    }

    /// The order the driver lists SQL Server's errors in after `SortErrors`.
    fn ordered(chain: &[(u32, i32)]) -> Vec<u32> {
        let mut errors: Vec<ErrorRecord> = chain
            .iter()
            .map(|&(number, class)| server(number, class, "x"))
            .collect();
        let checks = [failed(CheckId::ConnectionAttempt)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", true);
        errors.iter().filter_map(|e| e.number).collect()
    }

    /// A failed login (18456, its own SQLSTATE 28000) comes before the
    /// database it could not open (4060, class 11, 42000), as the driver
    /// sorts them, whichever the server sent first.
    #[test]
    fn a_failed_login_comes_before_the_database_it_could_not_open() {
        assert_eq!(ordered(&[(4060, 11), (18456, 14)]), [18456, 4060]);
        assert_eq!(ordered(&[(18456, 14), (4060, 11)]), [18456, 4060]);
    }

    /// Measured with ODBC Driver 18: 18456 (class 14, 28000) then 50000
    /// (class 16, 42000) stays in that order; the driver sorts by SQLSTATE
    /// precedence, not by class.
    #[test]
    fn server_errors_are_ordered_by_sqlstate_precedence_not_by_class() {
        assert_eq!(ordered(&[(18456, 14), (50000, 16)]), [18456, 50000]);
        assert_eq!(ordered(&[(50000, 16), (18456, 14)]), [18456, 50000]);
        // Errors with their own SQLSTATE, in the order received; then 42000
        // (classes 11 to 18), HY000 (above 18) and 01000 (10 or less), each
        // in the order received whatever its class.
        assert_eq!(
            ordered(&[
                (50001, 10),
                (50002, 20),
                (50003, 16),
                (4060, 11),
                (18456, 14),
                (4002, 14),
                (50004, 18),
                (50005, 19),
                (50006, 0),
            ]),
            [18456, 4002, 50003, 4060, 50004, 50002, 50005, 50001, 50006]
        );
        // Records that are not SQL Server's keep their places.
        let mut errors = vec![
            server(50000, 16, "x"),
            os(CheckId::ConnectionAttempt, "failed", Some(10054), "reset"),
            server(18456, 14, "y"),
        ];
        apply_for(
            &mut errors,
            &[failed(CheckId::ConnectionAttempt)],
            Some(Protocol::Tcp),
            "db01",
            true,
        );
        assert_eq!(errors[0].number, Some(18456));
        assert_eq!(errors[1].source, "os");
        assert_eq!(errors[2].number, Some(50000));
    }

    /// The driver's table, as `GetSQLState` searches it: in order, so a
    /// binary search finds every number in it.
    #[test]
    fn the_mapped_server_errors_are_sorted() {
        assert!(MAPPED_SERVER_ERRORS.windows(2).all(|w| w[0] < w[1]));
        for n in [18456, 18452, 4002, 911, 1205] {
            assert!(MAPPED_SERVER_ERRORS.binary_search(&n).is_ok(), "{n}");
        }
        for n in [4060, 4064, 50000, 0] {
            assert!(MAPPED_SERVER_ERRORS.binary_search(&n).is_err(), "{n}");
        }
    }

    #[test]
    fn a_refused_port_on_windows_is_worded_then_summarized() {
        let mut errors = vec![os(
            CheckId::TcpConnect,
            "refused",
            Some(10061),
            "10.0.0.1:1: refused",
        )];
        let checks = [failed(CheckId::TcpConnect)];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::Tcp),
            "tcp:db01,1",
            true,
        );
        let expected_first = if cfg!(windows) {
            "TCP Provider: No connection could be made because the target machine actively \
             refused it"
                .to_string()
        } else {
            texts(&errors)[0].clone()
        };
        // Refused at once: no time limit ran out, so no timeout line.
        assert_eq!(texts(&errors), [expected_first.as_str(), NETWORK]);
        assert_eq!(errors[1].source, "client");
        assert_eq!(errors[1].code.as_deref(), Some("networkRelatedError"));
    }

    #[test]
    fn elsewhere_the_code_is_in_hex_and_the_timeout_comes_first() {
        let mut errors = vec![
            os(CheckId::TcpConnect, "refused", Some(111), "a: refused"),
            os(CheckId::TcpConnect, "timedOut", None, "b: no answer"),
        ];
        let checks = [failed(CheckId::TcpConnect)];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::Tcp),
            "tcp:db01,1",
            false,
        );
        assert_eq!(
            texts(&errors),
            [
                "Login timeout expired",
                "TCP Provider: Error code 0x274D",
                "TCP Provider: Error code 0x102",
                NETWORK,
            ]
        );
    }

    #[test]
    fn a_name_not_found_uses_the_windows_code_on_every_platform() {
        let mut errors = vec![os(CheckId::NameResolution, "notFound", None, "not known")];
        let checks = [failed(CheckId::NameResolution)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", false);
        assert_eq!(texts(&errors)[0], "TCP Provider: Error code 0x2AF9");
        // An unknown name is not a timeout: only the network summary.
        assert_eq!(errors.len(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_system_message_is_used() {
        assert_eq!(os_message(WAIT_TIMEOUT), "The wait operation timed out");
        assert_eq!(os_message(WSAHOST_NOT_FOUND), "No such host is known");
        assert_eq!(
            tls_text(
                "Schannel TLS handshake failed: InitializeSecurityContextW failed: \
                 SEC_E_UNTRUSTED_ROOT (status=0x80090325)",
                true
            )
            .as_deref(),
            Some("The certificate chain was issued by an authority that is not trusted")
        );
        assert_eq!(
            tls_text(
                "Schannel TLS handshake failed: peer closed connection during TLS handshake",
                true
            )
            .as_deref(),
            Some("An existing connection was forcibly closed by the remote host")
        );
    }

    #[test]
    fn an_openssl_error_is_written_as_the_driver_writes_it() {
        let message = "TLS handshake failed while connecting to 'db01': error:0A000086:SSL \
             routines:tls_post_process_server_certificate:certificate verify failed:\
             ../ssl/statem/statem_clnt.c:1883: (self-signed certificate). Certificate SANs: x";
        assert_eq!(
            tls_text(message, false).as_deref(),
            Some(
                "[error:0A000086:SSL routines::certificate verify failed:self-signed certificate]"
            )
        );
        assert_eq!(tls_text("no OpenSSL code here", false), None);
        assert_eq!(tls_text("error:short:x:y:z", false), None);
    }

    /// With sqlcmd's defaults (`-N` mandatory, no `-C`) a failed handshake
    /// gets the summary that links the encryption guide.
    #[test]
    fn a_certificate_failure_gets_the_encryption_summary() {
        let mut errors = vec![record(
            "tls",
            CheckId::ConnectionAttempt,
            "TLS handshake failed: error:0A000086:SSL routines:f:certificate verify failed:x:1: \
             (self-signed certificate).",
        )];
        let mut check = failed(CheckId::ConnectionAttempt);
        check.fields.push(Field::plain("tlsFailure", "trustChain"));
        apply_for(&mut errors, &[check], Some(Protocol::Tcp), "db01", false);
        assert_eq!(
            texts(&errors),
            [
                "SSL Provider: [error:0A000086:SSL routines::certificate verify failed:self-signed \
                 certificate]",
                client_unable_encryption().render("en-US").as_str(),
            ]
        );
    }

    /// As the driver (`CTdsParser::SetUpConnection`, `netconn.cpp`): after a
    /// failed TLS handshake the summary links the encryption guide only for
    /// `-N` mandatory without `-C`, whichever TLS error it was; optional and
    /// strict encryption, or `-C`, get the plain one. A handshake that timed
    /// out gets the timeout line instead.
    #[test]
    fn the_encryption_summary_follows_the_connection_settings() {
        let trust_chain = || {
            let mut check = failed(CheckId::ConnectionAttempt);
            check.fields.push(Field::plain("tlsFailure", "trustChain"));
            (
                record(
                    "tls",
                    CheckId::ConnectionAttempt,
                    "TLS handshake failed: error:0A000086:SSL routines:f:certificate verify \
                     failed:x:1: (self-signed certificate).",
                ),
                check,
            )
        };
        let reset = || {
            let mut check = failed(CheckId::ConnectionAttempt);
            check.fields.push(Field::plain("tlsFailure", "reset"));
            (
                record(
                    "tls",
                    CheckId::ConnectionAttempt,
                    "Schannel TLS handshake failed: peer closed connection during TLS handshake",
                ),
                check,
            )
        };
        for (encrypt, trust, linked) in [
            (Encrypt::Mandatory, false, true),
            (Encrypt::Mandatory, true, false),
            (Encrypt::Optional, false, false),
            (Encrypt::Optional, true, false),
            (Encrypt::Strict, false, false),
            (Encrypt::Strict, true, false),
        ] {
            let encryption = Encryption {
                encrypt,
                trust_server_certificate: trust,
            };
            // OpenSSL's trust-chain failure (as off Windows) and Schannel's
            // reset (as on Windows), each worded as the driver words it.
            for (failure, windows) in [(trust_chain(), false), (reset(), true)] {
                let (tls, check) = failure;
                let mut errors = vec![tls];
                apply_with(
                    &mut errors,
                    &[check],
                    Some(Protocol::Tcp),
                    "db01",
                    encryption,
                    windows,
                );
                let expected = if linked {
                    client_unable_encryption().render("en-US")
                } else {
                    CLIENT_UNABLE.to_string()
                };
                assert_eq!(texts(&errors).last(), Some(&expected), "{encryption:?}");
                assert_eq!(errors.len(), 2, "{encryption:?}");
            }
        }

        // A handshake that timed out: the timeout line, no 08001 summary.
        let (tls, check) = reset();
        let mut timeout = record("client", CheckId::ConnectionAttempt, "timed out");
        timeout.code = Some("timedOut".to_string());
        let mut errors = vec![tls, timeout];
        apply_with(
            &mut errors,
            &[check],
            Some(Protocol::Tcp),
            "db01",
            Encryption::DEFAULT,
            true,
        );
        let codes: Vec<Option<&str>> = errors.iter().map(|e| e.code.as_deref()).collect();
        assert!(codes.contains(&Some("loginTimeoutExpired")), "{codes:?}");
        assert!(
            !codes.contains(&Some("clientUnableToEstablishConnection")),
            "{codes:?}"
        );
    }

    /// A reset handshake with `-C`: the plain summary (with sqlcmd's
    /// defaults it links the encryption guide, as any failed handshake does).
    #[test]
    fn a_reset_handshake_gets_the_plain_summary() {
        let mut errors = vec![record(
            "tls",
            CheckId::ConnectionAttempt,
            "Schannel TLS handshake failed: peer closed connection during TLS handshake",
        )];
        let mut check = failed(CheckId::ConnectionAttempt);
        check.fields.push(Field::plain("tlsFailure", "reset"));
        let trusted = Encryption {
            encrypt: Encrypt::Mandatory,
            trust_server_certificate: true,
        };
        apply_with(
            &mut errors,
            &[check],
            Some(Protocol::Tcp),
            "db01",
            trusted,
            true,
        );
        assert_eq!(
            texts(&errors).last().map(String::as_str),
            Some(CLIENT_UNABLE)
        );
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn sql_server_browser_and_named_pipes_use_the_sni_messages() {
        let mut no_answer = record("sqlBrowser", CheckId::InstanceResolution, "no answer");
        no_answer.code = Some("noAnswer".to_string());
        let mut errors = vec![no_answer];
        let checks = [failed(CheckId::InstanceResolution)];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::Tcp),
            r"db01\SQL1",
            true,
        );
        assert_eq!(
            texts(&errors)[0],
            "SQL Server Network Interfaces: Error Locating Server/Instance Specified [xFFFFFFFF]. "
        );
        // The Browser did not answer in time: the timeout line, then the
        // network summary.
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[1].code.as_deref(), Some("loginTimeoutExpired"));

        let mut pipe = vec![os(
            CheckId::ConnectionAttempt,
            "failed",
            Some(2),
            "not found",
        )];
        let checks = [failed(CheckId::ConnectionAttempt)];
        apply_for(
            &mut pipe,
            &checks,
            Some(Protocol::NamedPipe),
            "np:db01",
            true,
        );
        assert_eq!(
            texts(&pipe)[0],
            "Named Pipes Provider: Could not open a connection to SQL Server [2]. "
        );
        assert_eq!(pipe.len(), 2);
    }

    /// Shared memory opens a pipe, and SNI words its failure as the shared
    /// memory provider's, then gives the network summary, as for TCP.
    #[test]
    fn a_failed_shared_memory_open_uses_the_shared_memory_provider() {
        let mut errors = vec![os(CheckId::TcpConnect, "failed", Some(2), "not found")];
        let checks = [failed(CheckId::TcpConnect)];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::SharedMemory),
            "lpc:localhost",
            true,
        );
        assert_eq!(
            texts(&errors),
            [
                "Shared Memory Provider: Could not open a connection to SQL Server [2]. ",
                "A network-related or instance-specific error has occurred while establishing a \
                 connection to lpc:localhost. Server is not found or not accessible. Check if instance \
                 name is correct and if SQL Server is configured to allow remote connections. \
                 For more information see SQL Server Books Online."
            ]
        );
    }

    /// SNI opens a LocalDB instance as a plain named pipe: a pipe that does
    /// not open is worded as the named-pipe provider's, then summarized.
    #[test]
    fn a_failed_localdb_pipe_uses_the_named_pipes_provider() {
        let mut errors = vec![os(CheckId::TcpConnect, "failed", Some(2), "not found")];
        let checks = [failed(CheckId::TcpConnect)];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::LocalDb),
            "(localdb)\\MSSQLLocalDB",
            true,
        );
        assert_eq!(
            texts(&errors),
            [
                "Named Pipes Provider: Could not open a connection to SQL Server [2]. ",
                "A network-related or instance-specific error has occurred while establishing a \
                 connection to (localdb)\\MSSQLLocalDB. Server is not found or not accessible. \
                 Check if instance name is correct and if SQL Server is configured to allow \
                 remote connections. For more information see SQL Server Books Online."
            ]
        );
    }

    /// A pipe open the time limit abandoned (no operating-system error) gets
    /// the lines a TCP timeout gets: SNI's error for a pipe that did not open
    /// in time (`WaitNamedPipe`'s ERROR_SEM_TIMEOUT, 121), `Login timeout
    /// expired` and the network summary; for named pipes, LocalDB's pipe and
    /// shared memory.
    #[test]
    fn an_abandoned_pipe_gets_the_timeout_lines() {
        for (protocol, provider, server) in [
            (Protocol::NamedPipe, "Named Pipes Provider: ", "np:db01"),
            (
                Protocol::LocalDb,
                "Named Pipes Provider: ",
                "(localdb)\\MSSQLLocalDB",
            ),
            (
                Protocol::SharedMemory,
                "Shared Memory Provider: ",
                "lpc:localhost",
            ),
        ] {
            let mut errors = vec![os(
                CheckId::TcpConnect,
                "timedOut",
                None,
                "no answer within the 8 s time limit",
            )];
            let checks = [Check::new(CheckId::TcpConnect, Coverage::Inconclusive)];
            apply_for(&mut errors, &checks, Some(protocol), server, true);
            let codes: Vec<Option<&str>> = errors.iter().map(|e| e.code.as_deref()).collect();
            assert_eq!(
                codes,
                [
                    Some("timedOut"),
                    Some("loginTimeoutExpired"),
                    Some("networkRelatedError")
                ],
                "{protocol:?}"
            );
            assert_eq!(
                texts(&errors)[0],
                format!("{provider}Could not open a connection to SQL Server [121]. ")
            );
        }
    }

    /// A pipe open that ended on the pipe's own time limit (all instances
    /// busy, the client's `TimedOut`, with or without Windows' semaphore
    /// time-out 121) gets the same lines as one the time limit abandoned.
    #[test]
    fn a_pipe_that_timed_out_on_its_own_gets_the_timeout_lines() {
        for (protocol, provider, server) in [
            (Protocol::NamedPipe, "Named Pipes Provider: ", "np:db01"),
            (
                Protocol::LocalDb,
                "Named Pipes Provider: ",
                "(localdb)\\MSSQLLocalDB",
            ),
            (
                Protocol::SharedMemory,
                "Shared Memory Provider: ",
                "lpc:localhost",
            ),
        ] {
            for os_error in [Some(121), None] {
                let mut errors = vec![os(
                    CheckId::TcpConnect,
                    "timedOut",
                    os_error,
                    "Named pipe wait failed after 5001ms: The semaphore timeout period has \
                     expired. (os error 121)",
                )];
                let checks = [Check::new(CheckId::TcpConnect, Coverage::Inconclusive)];
                apply_for(&mut errors, &checks, Some(protocol), server, true);
                let codes: Vec<Option<&str>> = errors.iter().map(|e| e.code.as_deref()).collect();
                assert_eq!(
                    codes,
                    [
                        Some("timedOut"),
                        Some("loginTimeoutExpired"),
                        Some("networkRelatedError")
                    ],
                    "{protocol:?} {os_error:?}"
                );
                assert_eq!(
                    texts(&errors)[0],
                    format!("{provider}Could not open a connection to SQL Server [121]. "),
                    "{protocol:?} {os_error:?}"
                );
            }
        }
    }

    /// A connection attempt the time limit ended before the driver had an
    /// operating-system or provider message still reads as a login timeout,
    /// on every platform; a failure that is not a timeout does not.
    #[test]
    fn a_timeout_without_an_os_message_still_gets_the_timeout_line() {
        for windows in [true, false] {
            let mut timeout = record(
                "client",
                CheckId::ConnectionAttempt,
                "Timeout while connecting",
            );
            timeout.code = Some("timedOut".to_string());
            let mut errors = vec![timeout];
            let checks = [failed(CheckId::ConnectionAttempt)];
            apply_for(
                &mut errors,
                &checks,
                Some(Protocol::Tcp),
                "tcp:db01,1433",
                windows,
            );
            let codes: Vec<Option<&str>> = errors.iter().map(|e| e.code.as_deref()).collect();
            assert_eq!(
                codes,
                [Some("timedOut"), Some("loginTimeoutExpired")],
                "windows: {windows}"
            );
            assert_eq!(texts(&errors)[1], LOGIN_TIMEOUT_EXPIRED);
        }

        let mut errors = vec![record(
            "client",
            CheckId::ConnectionAttempt,
            "unexpected token",
        )];
        let checks = [failed(CheckId::ConnectionAttempt)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", true);
        assert_eq!(errors.len(), 1);

        // A query that ran out of time after the login is not a login timeout.
        let mut query = record("client", CheckId::SessionValidation, "no answer");
        query.code = Some("timedOut".to_string());
        let mut errors = vec![query];
        let checks = [failed(CheckId::SessionValidation)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", true);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn a_failure_the_connection_got_past_adds_no_summary() {
        // One address refused, another connected: the connection went on.
        let mut errors = vec![os(
            CheckId::TcpConnect,
            "refused",
            Some(10061),
            "a: refused",
        )];
        let checks = [Check::new(CheckId::TcpConnect, Coverage::Passed)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", true);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].text.is_some());
    }

    #[test]
    fn a_superseded_record_gets_no_text_and_the_summaries_follow_the_last_transport() {
        let mut dns = os(
            CheckId::NameResolution,
            "notFound",
            Some(11001),
            "not known",
        );
        dns.superseded = true;
        let pipe = os(CheckId::ConnectionAttempt, "failed", Some(53), "not found");
        let mut errors = vec![dns, pipe];
        let checks = [
            failed(CheckId::NameResolution),
            failed(CheckId::ConnectionAttempt),
        ];
        apply_for(
            &mut errors,
            &checks,
            Some(Protocol::NamedPipe),
            "db01",
            true,
        );
        assert!(errors[0].text.is_none());
        assert_eq!(
            texts(&errors)[1..],
            [
                "Named Pipes Provider: Could not open a connection to SQL Server [53]. ",
                "A network-related or instance-specific error has occurred while establishing a \
                 connection to db01. Server is not found or not accessible. Check if instance \
                 name is correct and if SQL Server is configured to allow remote connections. \
                 For more information see SQL Server Books Online."
            ]
        );
        assert!(
            errors
                .iter()
                .skip(2)
                .all(|e| e.check == CheckId::ConnectionAttempt)
        );
    }

    /// The driver's lines are the en-US driver's in every locale: a
    /// translated report does not paraphrase them, and SQL Server's own text
    /// is passed through as it is.
    #[test]
    fn the_driver_wording_stays_the_en_us_drivers_in_every_locale() {
        let mut errors = vec![
            os(CheckId::TcpConnect, "timedOut", None, "a: no answer"),
            record(
                "tls",
                CheckId::TcpConnect,
                "error:0A000086:SSL routines:x:certificate verify failed",
            ),
        ];
        let checks = [failed(CheckId::TcpConnect)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", false);
        let mut pipe = vec![os(CheckId::TcpConnect, "failed", Some(2), "not found")];
        apply_for(
            &mut pipe,
            &checks,
            Some(Protocol::SharedMemory),
            "lpc:localhost",
            true,
        );
        let mut browser = vec![record("sqlBrowser", CheckId::InstanceResolution, "x")];
        browser[0].code = Some("noAnswer".to_string());
        apply_for(
            &mut browser,
            &[failed(CheckId::InstanceResolution)],
            Some(Protocol::Tcp),
            "db01\\i",
            true,
        );
        let all: Vec<&ErrorRecord> = errors.iter().chain(&pipe).chain(&browser).collect();
        assert!(
            all.iter().filter(|e| e.text.is_some()).count() >= 6,
            "{all:#?}"
        );
        for error in all {
            let Some(text) = &error.text else { continue };
            let english = text.render("en-US");
            for locale in ["qps-ploc", "de-DE", "ja-JP"] {
                assert_eq!(text.render(locale), english, "{locale}");
            }
            assert!(!english.contains("[!!!"), "{english}");
        }

        let login = server(18456, 14, "Login failed for user 'sa'.");
        let login = text(&login, Some(Protocol::Tcp), true).unwrap();
        assert_eq!(login.render("qps-ploc"), "Login failed for user 'sa'.");
    }

    #[test]
    fn errors_with_no_driver_equivalent_keep_no_text() {
        let mut errors = vec![
            record("sqlcmd", CheckId::ConnectionInput, "-Q cannot be used"),
            record("client", CheckId::ConnectionAttempt, "something else"),
        ];
        apply_for(&mut errors, &[], Some(Protocol::Tcp), "db01", true);
        assert!(errors.iter().all(|e| e.text.is_none()));
    }
}

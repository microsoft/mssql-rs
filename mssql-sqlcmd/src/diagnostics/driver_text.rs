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

use super::target::Protocol;
use super::{Check, CheckId, ErrorRecord, LocalizedArg, LocalizedText};

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
const WSAENETUNREACH: i32 = 10051;
const WSAECONNRESET: i32 = 10054;
const WSAETIMEDOUT: i32 = 10060;
const WSAECONNREFUSED: i32 = 10061;
const WSAEHOSTUNREACH: i32 = 10065;
const WSAHOST_NOT_FOUND: i32 = 11001;
const WSATRY_AGAIN: i32 = 11002;
const WSANO_DATA: i32 = 11004;

fn network_related(server: &str) -> LocalizedText {
    LocalizedText::new(
        "diagnose.driver.network_related",
        format!(
            "A network-related or instance-specific error has occurred while establishing a \
             connection to {server}. Server is not found or not accessible. Check if instance \
             name is correct and if SQL Server is configured to allow remote connections. For \
             more information see SQL Server Books Online."
        ),
        vec![("server", server.to_string())],
    )
}

fn client_unable_encryption() -> LocalizedText {
    LocalizedText::new(
        "diagnose.driver.client_unable_encryption",
        format!(
            "Client unable to establish connection. For solutions related to encryption errors, \
             see {ENCRYPTION_ERRORS_LINK}"
        ),
        vec![("url", ENCRYPTION_ERRORS_LINK.to_string())],
    )
}

/// `<provider>{message}`, the provider's catalog message around `message`.
fn provider(id: &'static str, prefix: &str, message: LocalizedArg) -> LocalizedText {
    let english = format!("{prefix}{}", message.render("en-US"));
    LocalizedText::new_with_args(id, english, vec![message])
}

fn tcp_provider(message: LocalizedArg) -> LocalizedText {
    provider("diagnose.driver.tcp_provider", "TCP Provider: ", message)
}

/// Gives each record the driver's text for its event, orders SQL Server's
/// errors as the driver lists them, and adds the summaries the driver gives
/// after the connection fails. `server` is the server as the user gave it.
pub(super) fn apply(
    errors: &mut Vec<ErrorRecord>,
    checks: &[Check],
    protocol: Option<Protocol>,
    server: &str,
) {
    apply_for(errors, checks, protocol, server, cfg!(windows));
}

fn apply_for(
    errors: &mut Vec<ErrorRecord>,
    checks: &[Check],
    protocol: Option<Protocol>,
    server: &str,
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
    add_summaries(errors, checks, server, windows);
}

/// The driver lists a login's errors with the most severe first (a failed
/// login, class 14, before the database it could not open, class 11).
fn order_server_errors(errors: &mut [ErrorRecord]) {
    let at: Vec<usize> = (0..errors.len())
        .filter(|&i| errors[i].source == "sqlServer")
        .collect();
    let mut server: Vec<ErrorRecord> = at.iter().map(|&i| errors[i].clone()).collect();
    server.sort_by_key(|e| std::cmp::Reverse(e.class.unwrap_or(0)));
    for (i, record) in at.into_iter().zip(server) {
        errors[i] = record;
    }
}

/// The driver's text for one record, when the driver has an equivalent.
fn text(record: &ErrorRecord, protocol: Option<Protocol>, windows: bool) -> Option<LocalizedText> {
    let tcp = |code: i32| tcp_provider(system_text(code, windows));
    match (record.source, record.check) {
        ("sqlServer", _) => Some(record.message.clone()),
        ("sqlBrowser", _) => Some(LocalizedText::new(
            "diagnose.driver.error_locating_server",
            ERROR_LOCATING_SERVER,
            Vec::new(),
        )),
        ("os", CheckId::NameResolution) => dns_code(record, windows).map(tcp),
        ("os", CheckId::TcpConnect) => tcp_code(record, windows).map(tcp),
        ("os", _) if protocol == Some(Protocol::NamedPipe) => record.os_error.map(|code| {
            LocalizedText::new(
                "diagnose.driver.named_pipes_could_not_open",
                format!(
                    "Named Pipes Provider: Could not open a connection to SQL Server [{code}]. "
                ),
                vec![("code", code.to_string())],
            )
        }),
        ("os", _) if protocol.is_none_or(Protocol::uses_tcp) => tcp_code(record, windows).map(tcp),
        ("tls", _) => tls_text(&record.message.render("en-US"), windows).map(|t| {
            provider(
                "diagnose.driver.ssl_provider",
                "SSL Provider: ",
                LocalizedArg::text("message", t),
            )
        }),
        _ => None,
    }
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

/// The text the driver shows for a Windows error code, as the `message`
/// argument of a provider's message: the operating system's message (in the
/// system's language) on Windows, `Error code 0x...` elsewhere.
fn system_text(code: i32, windows: bool) -> LocalizedArg {
    if windows {
        LocalizedArg::text("message", os_message(code))
    } else {
        // The driver writes the code's bits in hex, as Windows does.
        let hex = format!("{:X}", code.cast_unsigned());
        LocalizedArg::message_with_args(
            "message",
            "diagnose.driver.error_code",
            format!("Error code 0x{hex}"),
            vec![LocalizedArg::text("code", hex)],
        )
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
fn add_summaries(errors: &mut Vec<ErrorRecord>, checks: &[Check], server: &str, windows: bool) {
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
        let expired = summary(
            "loginTimeoutExpired",
            LocalizedText::new(
                "diagnose.driver.login_timeout_expired",
                LOGIN_TIMEOUT_EXPIRED,
                Vec::new(),
            ),
        );
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
    } else if tls {
        let certificate = matches!(
            failed.field("tlsFailure"),
            Some("trustChain" | "expired" | "hostnameMismatch")
        );
        let text = if certificate {
            client_unable_encryption()
        } else {
            LocalizedText::new("diagnose.driver.client_unable", CLIENT_UNABLE, Vec::new())
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

    #[test]
    fn server_errors_are_listed_most_severe_first() {
        let mut errors = vec![
            server(
                4060,
                11,
                "Cannot open database \"x\" requested by the login.",
            ),
            server(18456, 14, "Login failed for user 'sa'."),
        ];
        let checks = [failed(CheckId::ConnectionAttempt)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", true);
        let numbers: Vec<u32> = errors.iter().filter_map(|e| e.number).collect();
        assert_eq!(numbers, [18456, 4060]);
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

    #[test]
    fn a_reset_handshake_gets_the_plain_summary() {
        let mut errors = vec![record(
            "tls",
            CheckId::ConnectionAttempt,
            "Schannel TLS handshake failed: peer closed connection during TLS handshake",
        )];
        let mut check = failed(CheckId::ConnectionAttempt);
        check.fields.push(Field::plain("tlsFailure", "reset"));
        apply_for(&mut errors, &[check], Some(Protocol::Tcp), "db01", true);
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

    #[test]
    fn the_driver_wording_is_translated_and_server_text_is_not() {
        let mut errors = vec![os(CheckId::TcpConnect, "refused", None, "a: refused")];
        let checks = [failed(CheckId::TcpConnect)];
        apply_for(&mut errors, &checks, Some(Protocol::Tcp), "db01", false);
        for error in &errors {
            let pseudo = error.text.as_ref().unwrap().render("qps-ploc");
            assert!(pseudo.starts_with("[!!!"), "{pseudo}");
        }
        // The provider's message and the `Error code` inside it, each translated.
        // A refused port is not a timeout: the provider's line comes first.
        let provider = errors[0].text.as_ref().unwrap().render("qps-ploc");
        assert_eq!(provider.matches("[!!!").count(), 2, "{provider}");

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

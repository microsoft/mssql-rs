// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQL Server Resolution Protocol (SSRP) implementation.
//!
//! Queries the SQL Server Browser service (UDP port 1434) to resolve
//! named instances to their actual connection endpoints (TCP port,
//! Named Pipe path, etc.).
//!
//! ## Protocol
//! - Send [`CLNT_UCAST_INST`] (0x04) + null-terminated ASCII instance name
//! - Receive [`SVR_RESP`] (0x05) + 2-byte LE payload size + semicolon-delimited metadata/protocols
//!
//! Reference: `SSRP::SsrpGetInfo()` in msodbcsql `/Sql/Common/DK/sni/src/ssrp.cpp`

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::select_all;
use tokio::net::UdpSocket;
use tracing::{debug, trace};

use crate::connection::client_context::TransportContext;
use crate::core::TdsResult;
use crate::error::Error;

// ---------------------------------------------------------------------------
// Protocol constants (from msodbcsql ssrp.cpp)
// ---------------------------------------------------------------------------

/// SQL Server Browser listens on UDP port 1434.
pub(crate) const SSRP_PORT: u16 = 1434;

/// Request type: unicast query for a specific named instance.
const CLNT_UCAST_INST: u8 = 0x04;

/// Response marker byte from SQL Browser.
const SVR_RESP: u8 = 0x05;

/// Default timeout for SSRP queries (matches msodbcsql DEFAULT_SSRPGETINFO_TIMEOUT).
pub(crate) const DEFAULT_SSRP_TIMEOUT_MS: u64 = 1000;

/// Maximum number of resolved IP addresses to query (matches msodbcsql MAX_SOCKET_NUM).
const MAX_SSRP_ADDRESSES: usize = 64;

/// Minimum valid SSRP response size (matches msodbcsql SPT:351275 check).
const MIN_RESPONSE_SIZE: usize = 15;

/// Maximum UDP receive buffer.
const RECV_BUF_SIZE: usize = 1024;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// SSRP query response containing instance information.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SsrpInstanceInfo {
    /// Instance name.
    pub instance_name: String,
    /// Protocol identifier (`"tcp"`, `"np"`, etc.).
    pub protocol: String,
    /// TCP port (if protocol is `"tcp"`).
    pub tcp_port: Option<u16>,
    /// Named pipe path (if protocol is `"np"`).
    pub pipe_path: Option<String>,
}

/// Full parsed response from SQL Server Browser.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct SsrpResponse {
    pub server_name: String,
    pub instance_name: String,
    pub is_clustered: bool,
    pub version: String,
    pub protocols: Vec<SsrpInstanceInfo>,
}

/// Query SQL Server Browser with explicit port and timeout.
///
/// Exposed as `pub(crate)` so tests can point at a mock browser on a non-standard port.
pub(crate) async fn get_instance_info_ext(
    server: &str,
    instance: &str,
    ssrp_port: u16,
    timeout_ms: u64,
) -> TdsResult<Vec<SsrpInstanceInfo>> {
    let resolve = tokio::net::lookup_host((server, ssrp_port));
    let response = query_browser(server, instance, resolve, timeout_ms, None)
        .await
        .map_err(Error::from)?;
    Ok(response.protocols)
}

/// Why a SQL Server Browser lookup failed, as [`lookup_instance`] reports it,
/// with the operating system's error where there is one.
#[derive(Debug)]
pub enum SsrpLookupError {
    /// The server name did not resolve.
    Resolve {
        /// The server looked up.
        server: String,
        /// The resolver's error.
        error: std::io::Error,
    },
    /// The server name resolved to no address.
    NoAddresses {
        /// The server looked up.
        server: String,
    },
    /// No UDP socket could be opened; the last bind error.
    Socket(Option<std::io::Error>),
    /// Every UDP send failed; the last send error.
    Send(Option<std::io::Error>),
    /// Receiving the answer failed.
    Receive(std::io::Error),
    /// SQL Server Browser did not answer in time.
    NoAnswer {
        /// How long it was given.
        timeout_ms: u64,
        /// The first address asked.
        address: String,
    },
    /// Resolving the server used up the whole time limit, so SQL Server
    /// Browser was never asked.
    TimedOut {
        /// The server looked up.
        server: String,
        /// How long it was given.
        timeout_ms: u64,
    },
    /// The answer could not be parsed.
    InvalidResponse(Error),
}

impl SsrpLookupError {
    /// A stable identifier for the failure.
    pub fn code(&self) -> &'static str {
        match self {
            SsrpLookupError::Resolve { .. } => "nameResolutionFailed",
            SsrpLookupError::NoAddresses { .. } => "noAddresses",
            SsrpLookupError::Socket(_) => "socketFailed",
            SsrpLookupError::Send(_) => "sendFailed",
            SsrpLookupError::Receive(_) => "receiveFailed",
            SsrpLookupError::NoAnswer { .. } => "noAnswer",
            SsrpLookupError::TimedOut { .. } => "timedOut",
            SsrpLookupError::InvalidResponse(_) => "invalidResponse",
        }
    }

    /// The operating system's error code, when the failure has one.
    pub fn os_error(&self) -> Option<i32> {
        match self {
            SsrpLookupError::Resolve { error, .. } | SsrpLookupError::Receive(error) => {
                error.raw_os_error()
            }
            SsrpLookupError::Socket(error) | SsrpLookupError::Send(error) => {
                error.as_ref().and_then(std::io::Error::raw_os_error)
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for SsrpLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsrpLookupError::Resolve { server, error } => write!(
                f,
                "Failed to resolve server '{server}' for SQL Browser query: {error}"
            ),
            SsrpLookupError::NoAddresses { server } => {
                write!(f, "No addresses resolved for server '{server}'")
            }
            SsrpLookupError::Socket(_) => {
                write!(f, "Failed to create any UDP sockets for SQL Browser query")
            }
            SsrpLookupError::Send(_) => write!(
                f,
                "All UDP sends to SQL Server Browser failed. \
                 Verify network connectivity to the server."
            ),
            SsrpLookupError::Receive(error) => {
                write!(f, "UDP receive error from SQL Server Browser: {error}")
            }
            SsrpLookupError::NoAnswer {
                timeout_ms,
                address,
            } => write!(
                f,
                "SQL Server Browser did not respond within {timeout_ms}ms. \
                 Verify that the SQL Server Browser service is running on '{address}'."
            ),
            SsrpLookupError::TimedOut { server, timeout_ms } => write!(
                f,
                "Resolving '{server}' for the SQL Server Browser lookup did not complete within {timeout_ms}ms."
            ),
            SsrpLookupError::InvalidResponse(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SsrpLookupError {}

impl From<SsrpLookupError> for Error {
    fn from(error: SsrpLookupError) -> Self {
        match error {
            SsrpLookupError::InvalidResponse(error) => error,
            other => Error::ConnectionError(other.to_string()),
        }
    }
}

/// Asks SQL Server Browser on `server` for the endpoints of the named
/// `instance`, as a client resolving a named instance does. `timeout_ms`
/// bounds the whole lookup, resolving `server` included: resolving that
/// runs out of time is [`SsrpLookupError::TimedOut`], and a Browser that
/// does not answer in the time left is [`SsrpLookupError::NoAnswer`]. Used by
/// connection diagnostics, which report this step on its own.
pub async fn lookup_instance(
    server: &str,
    instance: &str,
    timeout_ms: u64,
) -> Result<Vec<SsrpInstanceInfo>, SsrpLookupError> {
    let resolve = tokio::net::lookup_host((server, SSRP_PORT));
    lookup_instance_with(server, instance, resolve, timeout_ms).await
}

async fn lookup_instance_with<A>(
    server: &str,
    instance: &str,
    resolve: impl std::future::Future<Output = std::io::Result<A>>,
    timeout_ms: u64,
) -> Result<Vec<SsrpInstanceInfo>, SsrpLookupError>
where
    A: Iterator<Item = SocketAddr>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    query_browser(server, instance, resolve, timeout_ms, Some(deadline))
        .await
        .map(|response| response.protocols)
}

/// Convert SSRP instance info into an ordered list of [`TransportContext`] variants.
pub(crate) fn build_transport_list(
    instance_info: Vec<SsrpInstanceInfo>,
    server: &str,
    _instance: &str,
) -> Vec<TransportContext> {
    let mut transports = Vec::new();
    for info in instance_info {
        match info.protocol.as_str() {
            "tcp" => {
                if let Some(port) = info.tcp_port {
                    transports.push(TransportContext::Tcp {
                        host: server.to_string(),
                        port,
                        instance_name: None,
                    });
                }
            }
            "np" => {
                if let Some(pipe) = info.pipe_path {
                    transports.push(TransportContext::NamedPipe { pipe_name: pipe });
                }
            }
            _ => {} // Unknown protocol — skip
        }
    }
    transports
}

/// Build a CLNT_UCAST_INST request packet.
///
/// Format: `[0x04][ASCII instance name][0x00]`
pub(crate) fn build_instance_request(instance: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(instance.len() + 2);
    buf.push(CLNT_UCAST_INST);
    buf.extend_from_slice(instance.as_bytes());
    buf.push(0x00); // null terminator
    buf
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse a raw SVR_RESP datagram into an [`SsrpResponse`].
///
/// Validates the 3-byte header (marker + LE u16 payload size) then parses
/// the semicolon-delimited key-value payload.
pub(crate) fn parse_ssrp_response(buf: &[u8]) -> TdsResult<SsrpResponse> {
    if buf.len() < MIN_RESPONSE_SIZE {
        return Err(Error::ProtocolError(format!(
            "SSRP response too short: {} bytes (minimum {})",
            buf.len(),
            MIN_RESPONSE_SIZE
        )));
    }

    if buf[0] != SVR_RESP {
        return Err(Error::ProtocolError(format!(
            "Invalid SSRP response marker: 0x{:02X} (expected 0x{:02X})",
            buf[0], SVR_RESP
        )));
    }

    let size = u16::from_le_bytes([buf[1], buf[2]]) as usize;
    if size != buf.len() - 3 {
        return Err(Error::ProtocolError(format!(
            "SSRP response size mismatch: header says {} but payload is {} bytes",
            size,
            buf.len() - 3
        )));
    }

    let payload = &buf[3..];
    let payload_str = std::str::from_utf8(payload)
        .map_err(|_| Error::ProtocolError("SSRP response contains invalid UTF-8".to_string()))?;
    let payload_str = payload_str.trim_end_matches('\0');

    parse_ssrp_payload(payload_str)
}

/// Parse the semicolon-delimited payload string.
///
/// Expected format (keys are case-sensitive per SQL Browser):
/// ```text
/// ServerName;VALUE;InstanceName;VALUE;IsClustered;VALUE;Version;VALUE;tcp;PORT;np;PIPE;;
/// ```
fn parse_ssrp_payload(payload: &str) -> TdsResult<SsrpResponse> {
    let tokens: Vec<&str> = payload.split(';').collect();

    let mut server_name = String::new();
    let mut instance_name = String::new();
    let mut is_clustered = false;
    let mut version = String::new();
    let mut protocols = Vec::new();
    let mut past_version = false;

    let mut i = 0;
    while i + 1 < tokens.len() {
        let key = tokens[i];
        let value = tokens[i + 1];
        i += 2;

        if key.is_empty() {
            break; // ;; terminator
        }

        if !past_version {
            match key {
                "ServerName" => server_name = value.to_string(),
                "InstanceName" => instance_name = value.to_string(),
                "IsClustered" => is_clustered = value.eq_ignore_ascii_case("Yes"),
                "Version" => {
                    version = value.to_string();
                    past_version = true;
                }
                _ => {
                    // Unexpected key before Version — treat as protocol start
                    past_version = true;
                    push_protocol(key, value, &instance_name, &mut protocols);
                }
            }
        } else {
            push_protocol(key, value, &instance_name, &mut protocols);
        }
    }

    if instance_name.is_empty() && server_name.is_empty() {
        return Err(Error::ProtocolError(
            "SSRP response payload did not contain expected metadata fields".to_string(),
        ));
    }

    Ok(SsrpResponse {
        server_name,
        instance_name,
        is_clustered,
        version,
        protocols,
    })
}

/// Push a protocol entry if it's a recognised type with a valid parameter.
fn push_protocol(
    key: &str,
    value: &str,
    instance_name: &str,
    protocols: &mut Vec<SsrpInstanceInfo>,
) {
    match key {
        "tcp" => {
            if let Ok(port) = value.parse::<u16>()
                && port > 0
            {
                protocols.push(SsrpInstanceInfo {
                    instance_name: instance_name.to_string(),
                    protocol: "tcp".to_string(),
                    tcp_port: Some(port),
                    pipe_path: None,
                });
            }
        }
        "np" if !value.is_empty() => {
            protocols.push(SsrpInstanceInfo {
                instance_name: instance_name.to_string(),
                protocol: "np".to_string(),
                tcp_port: None,
                pipe_path: Some(value.to_string()),
            });
        }
        _ => {} // via, rpc, spx, adsp, sm — skip
    }
}

/// Full SSRP query: resolve hostname (`resolve`, the Browser port included),
/// send CLNT_UCAST_INST to all addresses, return the first valid SVR_RESP.
/// Without a `deadline`, resolving is not bounded and the answer is waited for
/// `timeout_ms` after it; with one, both share it.
async fn query_browser<A>(
    server: &str,
    instance: &str,
    resolve: impl std::future::Future<Output = std::io::Result<A>>,
    timeout_ms: u64,
    deadline: Option<tokio::time::Instant>,
) -> Result<SsrpResponse, SsrpLookupError>
where
    A: Iterator<Item = SocketAddr>,
{
    debug!(server, instance, timeout_ms, "Querying SQL Server Browser");

    let request = build_instance_request(instance);

    // Resolve server to all IP addresses
    let resolved = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, resolve)
            .await
            .map_err(|_| SsrpLookupError::TimedOut {
                server: server.to_string(),
                timeout_ms,
            })?,
        None => resolve.await,
    };
    let addrs: Vec<SocketAddr> = resolved
        .map_err(|error| SsrpLookupError::Resolve {
            server: server.to_string(),
            error,
        })?
        .take(MAX_SSRP_ADDRESSES)
        .collect();

    if addrs.is_empty() {
        return Err(SsrpLookupError::NoAddresses {
            server: server.to_string(),
        });
    }

    debug!(address_count = addrs.len(), "Resolved server addresses");

    let deadline =
        deadline.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_millis(timeout_ms));
    // The deadline covers opening the sockets and sending too, not only
    // waiting for the answer: a send that stalls is no answer in time.
    let asked = tokio::time::Instant::now();
    let raw =
        match tokio::time::timeout_at(deadline, send_and_receive_first(&request, &addrs, deadline))
            .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(SsrpLookupError::NoAnswer {
                    timeout_ms: u64::try_from(
                        deadline.saturating_duration_since(asked).as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                    address: addrs
                        .first()
                        .map(|a| a.ip().to_string())
                        .unwrap_or_default(),
                });
            }
        };

    trace!(len = raw.len(), "Received SSRP response");

    parse_ssrp_response(&raw).map_err(SsrpLookupError::InvalidResponse)
}

/// Send `request` to every address in `addrs` (one UDP socket per address family)
/// and return the first valid response by `deadline`.
///
/// Uses `recv_from` and validates the sender address against `addrs` to prevent
/// spoofed UDP packets from redirecting instance resolution.
async fn send_and_receive_first(
    request: &[u8],
    addrs: &[SocketAddr],
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, SsrpLookupError> {
    let has_v4 = addrs.iter().any(|a| a.is_ipv4());
    let has_v6 = addrs.iter().any(|a| a.is_ipv6());

    let mut sockets: Vec<Arc<UdpSocket>> = Vec::new();
    let expected_ips: std::collections::HashSet<std::net::IpAddr> =
        addrs.iter().map(|a| a.ip()).collect();

    // IPv4
    let mut any_send_succeeded = false;
    let mut bind_error = None;
    let mut send_error = None;
    if has_v4 {
        match UdpSocket::bind("0.0.0.0:0").await {
            Ok(sock) => {
                let sock = Arc::new(sock);
                for addr in addrs.iter().filter(|a| a.is_ipv4()) {
                    match sock.send_to(request, addr).await {
                        Ok(_) => any_send_succeeded = true,
                        Err(e) => send_error = Some(e),
                    }
                }
                sockets.push(sock);
            }
            Err(e) => {
                debug!("Failed to bind IPv4 UDP socket: {}", e);
                bind_error = Some(e);
            }
        }
    }

    // IPv6
    if has_v6 {
        match UdpSocket::bind("[::]:0").await {
            Ok(sock) => {
                let sock = Arc::new(sock);
                for addr in addrs.iter().filter(|a| a.is_ipv6()) {
                    match sock.send_to(request, addr).await {
                        Ok(_) => any_send_succeeded = true,
                        Err(e) => send_error = Some(e),
                    }
                }
                sockets.push(sock);
            }
            Err(e) => {
                debug!("Failed to bind IPv6 UDP socket: {}", e);
                bind_error = Some(e);
            }
        }
    }

    if sockets.is_empty() {
        return Err(SsrpLookupError::Socket(bind_error));
    }

    if !any_send_succeeded {
        return Err(SsrpLookupError::Send(send_error));
    }

    // Race all socket recv futures against the timeout.
    // Use recv_from and validate the source IP against resolved addresses
    // to reject spoofed datagrams.
    let expected_ips = Arc::new(expected_ips);
    let recv_futures: Vec<_> = sockets
        .iter()
        .map(|sock| {
            let sock = Arc::clone(sock);
            let expected = Arc::clone(&expected_ips);
            Box::pin(async move {
                loop {
                    let mut buf = vec![0u8; RECV_BUF_SIZE];
                    let (n, sender) = sock.recv_from(&mut buf).await?;
                    if expected.contains(&sender.ip()) {
                        buf.truncate(n);
                        return Ok::<Vec<u8>, std::io::Error>(buf);
                    }
                    // Discard datagram from unexpected sender and keep waiting
                }
            })
        })
        .collect();

    let waited = deadline.saturating_duration_since(tokio::time::Instant::now());
    match tokio::time::timeout_at(deadline, select_all(recv_futures)).await {
        Ok((Ok(buf), _, _)) => Ok(buf),
        Ok((Err(e), _, _)) => Err(SsrpLookupError::Receive(e)),
        Err(_) => Err(SsrpLookupError::NoAnswer {
            timeout_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
            address: addrs
                .first()
                .map(|a| a.ip().to_string())
                .unwrap_or_default(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Packet encoding ---------------------------------------------------

    #[test]
    fn test_build_instance_request() {
        let pkt = build_instance_request("SQLEXPRESS");
        assert_eq!(pkt[0], CLNT_UCAST_INST);
        assert_eq!(&pkt[1..pkt.len() - 1], b"SQLEXPRESS");
        assert_eq!(*pkt.last().unwrap(), 0x00);
    }

    // -- Response parsing --------------------------------------------------

    fn make_response(payload: &str) -> Vec<u8> {
        let payload_bytes = payload.as_bytes();
        let size = payload_bytes.len() as u16;
        let mut buf = Vec::with_capacity(3 + payload_bytes.len());
        buf.push(SVR_RESP);
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(payload_bytes);
        buf
    }

    #[test]
    fn test_parse_valid_response() {
        let payload = "ServerName;MYSERVER;InstanceName;SQLEXPRESS;IsClustered;No;Version;16.0.1000.6;tcp;54321;np;\\\\MYSERVER\\pipe\\MSSQL$SQLEXPRESS\\sql\\query;;";
        let buf = make_response(payload);

        let resp = parse_ssrp_response(&buf).unwrap();
        assert_eq!(resp.server_name, "MYSERVER");
        assert_eq!(resp.instance_name, "SQLEXPRESS");
        assert!(!resp.is_clustered);
        assert_eq!(resp.version, "16.0.1000.6");
        assert_eq!(resp.protocols.len(), 2);
        assert_eq!(resp.protocols[0].protocol, "tcp");
        assert_eq!(resp.protocols[0].tcp_port, Some(54321));
        assert_eq!(resp.protocols[1].protocol, "np");
        assert!(resp.protocols[1].pipe_path.is_some());
    }

    #[test]
    fn test_parse_tcp_only_response() {
        let payload =
            "ServerName;SRV;InstanceName;INST;IsClustered;Yes;Version;15.0.2000.5;tcp;1433;;";
        let buf = make_response(payload);

        let resp = parse_ssrp_response(&buf).unwrap();
        assert_eq!(resp.protocols.len(), 1);
        assert_eq!(resp.protocols[0].tcp_port, Some(1433));
        assert!(resp.is_clustered);
    }

    #[test]
    fn test_parse_response_too_short() {
        let buf = vec![SVR_RESP, 0x01, 0x00, b'x'];
        assert!(parse_ssrp_response(&buf).is_err());
    }

    #[test]
    fn test_parse_response_bad_marker() {
        let mut buf =
            make_response("ServerName;S;InstanceName;I;IsClustered;No;Version;1;tcp;99;;");
        buf[0] = 0xFF;
        assert!(parse_ssrp_response(&buf).is_err());
    }

    #[test]
    fn test_parse_response_size_mismatch() {
        let mut buf =
            make_response("ServerName;S;InstanceName;I;IsClustered;No;Version;1;tcp;99;;");
        buf[1] = 0xFF; // corrupt size field
        assert!(parse_ssrp_response(&buf).is_err());
    }

    // -- build_transport_list ----------------------------------------------

    #[test]
    fn test_build_transport_list() {
        let info = vec![
            SsrpInstanceInfo {
                instance_name: "INST1".to_string(),
                protocol: "tcp".to_string(),
                tcp_port: Some(1433),
                pipe_path: None,
            },
            SsrpInstanceInfo {
                instance_name: "INST1".to_string(),
                protocol: "np".to_string(),
                tcp_port: None,
                pipe_path: Some(r"\\.\pipe\MSSQL$INST1\sql\query".to_string()),
            },
        ];

        let transports = build_transport_list(info, "localhost", "INST1");
        assert_eq!(transports.len(), 2);
    }

    // -- Live UDP round-trip against a localhost mock -----------------------

    #[tokio::test]
    async fn test_query_browser_localhost() {
        // Start a tiny mock SQL Browser on a random port
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();

        let payload =
            "ServerName;MOCK;InstanceName;TESTINST;IsClustered;No;Version;16.0.1000.6;tcp;55555;;";
        let payload_bytes = payload.as_bytes();
        let size = payload_bytes.len() as u16;

        // Respond to the first datagram we receive
        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            let (n, addr) = socket.recv_from(&mut buf).await.unwrap();
            assert!(n > 1);
            assert_eq!(buf[0], CLNT_UCAST_INST);

            let mut resp = Vec::with_capacity(3 + payload_bytes.len());
            resp.push(SVR_RESP);
            resp.extend_from_slice(&size.to_le_bytes());
            resp.extend_from_slice(payload_bytes);
            socket.send_to(&resp, addr).await.unwrap();
        });

        let info = get_instance_info_ext("127.0.0.1", "TESTINST", port, 2000)
            .await
            .unwrap();

        assert_eq!(info.len(), 1);
        assert_eq!(info[0].protocol, "tcp");
        assert_eq!(info[0].tcp_port, Some(55555));
    }

    #[tokio::test]
    async fn test_query_browser_timeout() {
        // Bind a socket but never respond — tests the timeout path.
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();

        // Keep socket alive but never send a response
        let _hold = socket;

        let result = get_instance_info_ext("127.0.0.1", "NOPE", port, 200).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string().to_lowercase();
        assert!(
            msg.contains("did not respond") || msg.contains("timeout"),
            "unexpected error: {}",
            msg
        );
    }

    /// Resolves to `address` after `delay`, as a slow DNS server would.
    async fn resolve_after(
        delay: u64,
        address: SocketAddr,
    ) -> std::io::Result<std::vec::IntoIter<SocketAddr>> {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Ok(vec![address].into_iter())
    }

    /// Resolving takes part of the time limit; a Browser that stays silent
    /// for the rest is no answer, not a lookup that ran out of time.
    #[tokio::test]
    async fn a_silent_browser_after_slow_resolving_is_no_answer() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let _hold = socket;

        let started = std::time::Instant::now();
        let error = lookup_instance_with("db01", "NOPE", resolve_after(150, address), 400)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "noAnswer", "{error}");
        let waited = match error {
            SsrpLookupError::NoAnswer { timeout_ms, .. } => timeout_ms,
            _ => unreachable!(),
        };
        assert!(waited < 400, "the Browser had only the time left: {waited}");
        assert!(started.elapsed() < Duration::from_millis(1500));
    }

    /// Resolving that uses up the whole time limit never asks the Browser.
    #[tokio::test]
    async fn resolving_that_uses_up_the_time_limit_is_timed_out() {
        let address = SocketAddr::from(([127, 0, 0, 1], 9));
        let started = std::time::Instant::now();
        let error = lookup_instance_with("db01", "NOPE", resolve_after(5000, address), 200)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "timedOut", "{error}");
        assert!(started.elapsed() < Duration::from_millis(1500));
    }
}

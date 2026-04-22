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
pub const SSRP_PORT: u16 = 1434;

/// Request type: unicast query for a specific named instance.
const CLNT_UCAST_INST: u8 = 0x04;

/// Response marker byte from SQL Browser.
const SVR_RESP: u8 = 0x05;

/// Request type: query for Dedicated Admin Connection port.
const CLNT_UCAST_DAC: u8 = 0x0F;

/// Default timeout for SSRP queries (matches msodbcsql DEFAULT_SSRPGETINFO_TIMEOUT).
pub const DEFAULT_SSRP_TIMEOUT_MS: u64 = 1000;

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

/// Query SQL Server Browser for instance information using the default port and timeout.
pub async fn get_instance_info(server: &str, instance: &str) -> TdsResult<Vec<SsrpInstanceInfo>> {
    get_instance_info_ext(server, instance, SSRP_PORT, DEFAULT_SSRP_TIMEOUT_MS).await
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
    let response = query_browser(server, instance, ssrp_port, timeout_ms).await?;
    Ok(response.protocols)
}

/// Query SQL Server Browser for Dedicated Admin Connection (DAC) port.
///
/// Sends CLNT_UCAST_DAC (0x0F) and parses the 6-byte response.
#[allow(dead_code)]
pub async fn get_admin_port(_server: &str, _instance: &str) -> TdsResult<u16> {
    // DAC port resolution will be implemented in a future PR.
    Err(Error::ProtocolError(
        "SSRP DAC port resolution is not yet implemented. \
         Please specify an explicit port for admin connections."
            .to_string(),
    ))
}

/// Convert SSRP instance info into an ordered list of [`TransportContext`] variants.
pub fn build_transport_list(
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

/// Build a CLNT_UCAST_DAC request packet.
///
/// Format: `[0x0F][0x01][ASCII instance name][0x00]`
#[allow(dead_code)]
pub(crate) fn build_dac_request(instance: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(instance.len() + 3);
    buf.push(CLNT_UCAST_DAC);
    buf.push(0x01); // protocol version
    buf.extend_from_slice(instance.as_bytes());
    buf.push(0x00);
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
        "np" => {
            if !value.is_empty() {
                protocols.push(SsrpInstanceInfo {
                    instance_name: instance_name.to_string(),
                    protocol: "np".to_string(),
                    tcp_port: None,
                    pipe_path: Some(value.to_string()),
                });
            }
        }
        _ => {} // via, rpc, spx, adsp, sm — skip
    }
}

/// Full SSRP query: resolve hostname, send CLNT_UCAST_INST to all addresses,
/// return the first valid SVR_RESP.
async fn query_browser(
    server: &str,
    instance: &str,
    ssrp_port: u16,
    timeout_ms: u64,
) -> TdsResult<SsrpResponse> {
    debug!(
        server,
        instance, ssrp_port, timeout_ms, "Querying SQL Server Browser"
    );

    let request = build_instance_request(instance);

    // Resolve server to all IP addresses
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((server, ssrp_port))
        .await
        .map_err(|e| {
            Error::ConnectionError(format!(
                "Failed to resolve server '{}' for SQL Browser query: {}",
                server, e
            ))
        })?
        .take(MAX_SSRP_ADDRESSES)
        .collect();

    if addrs.is_empty() {
        return Err(Error::ConnectionError(format!(
            "No addresses resolved for server '{}'",
            server
        )));
    }

    debug!(address_count = addrs.len(), "Resolved server addresses");

    let raw = send_and_receive_first(&request, &addrs, timeout_ms).await?;

    trace!(len = raw.len(), "Received SSRP response");

    parse_ssrp_response(&raw)
}

/// Send `request` to every address in `addrs` (one UDP socket per address family)
/// and return the first valid response within `timeout_ms`.
///
/// Uses `recv_from` and validates the sender address against `addrs` to prevent
/// spoofed UDP packets from redirecting instance resolution.
async fn send_and_receive_first(
    request: &[u8],
    addrs: &[SocketAddr],
    timeout_ms: u64,
) -> TdsResult<Vec<u8>> {
    let has_v4 = addrs.iter().any(|a| a.is_ipv4());
    let has_v6 = addrs.iter().any(|a| a.is_ipv6());

    let mut sockets: Vec<Arc<UdpSocket>> = Vec::new();
    let expected_ips: std::collections::HashSet<std::net::IpAddr> =
        addrs.iter().map(|a| a.ip()).collect();

    // IPv4
    let mut any_send_succeeded = false;
    if has_v4 {
        match UdpSocket::bind("0.0.0.0:0").await {
            Ok(sock) => {
                let sock = Arc::new(sock);
                for addr in addrs.iter().filter(|a| a.is_ipv4()) {
                    if sock.send_to(request, addr).await.is_ok() {
                        any_send_succeeded = true;
                    }
                }
                sockets.push(sock);
            }
            Err(e) => debug!("Failed to bind IPv4 UDP socket: {}", e),
        }
    }

    // IPv6
    if has_v6 {
        match UdpSocket::bind("[::]:0").await {
            Ok(sock) => {
                let sock = Arc::new(sock);
                for addr in addrs.iter().filter(|a| a.is_ipv6()) {
                    if sock.send_to(request, addr).await.is_ok() {
                        any_send_succeeded = true;
                    }
                }
                sockets.push(sock);
            }
            Err(e) => debug!("Failed to bind IPv6 UDP socket: {}", e),
        }
    }

    if sockets.is_empty() {
        return Err(Error::ConnectionError(
            "Failed to create any UDP sockets for SQL Browser query".to_string(),
        ));
    }

    if !any_send_succeeded {
        return Err(Error::ConnectionError(
            "All UDP sends to SQL Server Browser failed. \
             Verify network connectivity to the server."
                .to_string(),
        ));
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

    match tokio::time::timeout(Duration::from_millis(timeout_ms), select_all(recv_futures)).await {
        Ok((Ok(buf), _, _)) => Ok(buf),
        Ok((Err(e), _, _)) => Err(Error::ConnectionError(format!(
            "UDP receive error from SQL Server Browser: {}",
            e
        ))),
        Err(_) => Err(Error::ConnectionError(format!(
            "SQL Server Browser did not respond within {}ms. \
             Verify that the SQL Server Browser service is running on '{}'.",
            timeout_ms,
            addrs
                .first()
                .map(|a| a.ip().to_string())
                .unwrap_or_default()
        ))),
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

    #[test]
    fn test_build_dac_request() {
        let pkt = build_dac_request("MYINST");
        assert_eq!(pkt[0], CLNT_UCAST_DAC);
        assert_eq!(pkt[1], 0x01);
        assert_eq!(&pkt[2..pkt.len() - 1], b"MYINST");
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

    // -- DAC stub ----------------------------------------------------------

    #[tokio::test]
    async fn test_get_admin_port_not_implemented() {
        let result = get_admin_port("localhost", "MSSQLSERVER").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not yet implemented")
        );
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
}

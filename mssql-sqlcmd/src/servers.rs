// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `-L` server enumeration.
//!
//! SQL Server Browser answers a broadcast on UDP 1434 with a description of
//! every instance a host is running. The driver's own SSRP code resolves a
//! *named* instance and is crate-private, so the broadcast is done here.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::messages::EOL;

use tokio::net::UdpSocket;

/// SQL Server Resolution Protocol listens here.
const SSRP_PORT: u16 = 1434;

/// `CLNT_BCAST_EX` — "describe every instance you have".
const CLNT_BCAST_EX: u8 = 0x02;

/// `SVR_RESP` — the first byte of a well-formed reply.
const SVR_RESP: u8 = 0x05;

/// How long to keep listening after the broadcast goes out. The reference is
/// similarly patient; instances answer within a few hundred milliseconds on a
/// quiet network.
const LISTEN_FOR: Duration = Duration::from_secs(2);

/// A browser reply can describe several instances and is capped well below this.
const MAX_REPLY: usize = 4096;

/// Renders the `-L` listing. `clean` is `-Lc`, which drops the header.
pub async fn list(clean: bool) -> String {
    list_from(discover().await.into_iter().collect(), clean)
}

/// Rendering, split from discovery so it can be tested without a network.
fn list_from(mut names: Vec<String>, clean: bool) -> String {
    names.sort();

    let mut out = String::new();
    if !clean {
        out.push_str(&format!("{EOL}Servers:{EOL}"));
    }
    for name in names {
        if clean {
            out.push_str(&format!("{name}{EOL}"));
        } else {
            // The reference indents each entry under the header.
            out.push_str(&format!("    {name}{EOL}"));
        }
    }
    out
}

/// Broadcasts and collects whatever answers before the deadline.
///
/// Enumeration is best-effort by nature: a firewall, a network that drops
/// broadcasts, or a host with the Browser service stopped all produce silence
/// rather than an error, and the reference prints an empty list in each case.
async fn discover() -> BTreeSet<String> {
    let mut found = BTreeSet::new();

    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        return found;
    };
    if socket.set_broadcast(true).is_err() {
        return found;
    }
    if socket
        .send_to(&[CLNT_BCAST_EX], ("255.255.255.255", SSRP_PORT))
        .await
        .is_err()
    {
        return found;
    }

    let deadline = tokio::time::Instant::now() + LISTEN_FOR;
    let mut buffer = vec![0u8; MAX_REPLY];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await {
            Ok(Ok((len, _))) => {
                for name in parse_reply(&buffer[..len]) {
                    found.insert(name);
                }
            }
            // A malformed or truncated datagram is not worth abandoning the
            // whole enumeration over; keep listening until the deadline.
            Ok(Err(_)) => continue,
            Err(_) => break,
        }
    }

    found
}

/// Pulls `SERVER\INSTANCE` names out of one browser reply.
///
/// The payload is `0x05`, a little-endian length, then `key;value;` pairs with
/// `;;` separating one instance from the next.
fn parse_reply(datagram: &[u8]) -> Vec<String> {
    if datagram.len() < 3 || datagram[0] != SVR_RESP {
        return Vec::new();
    }
    let declared = u16::from_le_bytes([datagram[1], datagram[2]]) as usize;
    let body = &datagram[3..];
    let body = &body[..declared.min(body.len())];
    let text = String::from_utf8_lossy(body);

    let mut names = Vec::new();
    for record in text.split(";;") {
        let fields: Vec<&str> = record.split(';').collect();
        let server = field(&fields, "ServerName");
        let instance = field(&fields, "InstanceName");
        match (server, instance) {
            (Some(server), Some(instance)) if !server.is_empty() => {
                // A default instance is named `MSSQLSERVER` and is listed by
                // host name alone, the way it would be typed into `-S`.
                if instance.eq_ignore_ascii_case("MSSQLSERVER") {
                    names.push(server.to_string());
                } else {
                    names.push(format!("{server}\\{instance}"));
                }
            }
            _ => continue,
        }
    }
    names
}

/// Reads the value following `key` in a flat `key;value;key;value` list.
fn field<'a>(fields: &[&'a str], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .position(|f| f.eq_ignore_ascii_case(key))
        .and_then(|i| fields.get(i + 1))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(body: &str) -> Vec<u8> {
        let bytes = body.as_bytes();
        let mut out = vec![SVR_RESP];
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(bytes);
        out
    }

    #[test]
    fn a_named_instance_is_listed_with_its_host() {
        let datagram = reply(
            "ServerName;HOST;InstanceName;SQLEXPRESS;IsClustered;No;Version;15.0.2000.5;tcp;1433;",
        );
        assert_eq!(parse_reply(&datagram), vec!["HOST\\SQLEXPRESS"]);
    }

    #[test]
    fn a_default_instance_is_listed_by_host_alone() {
        let datagram = reply("ServerName;HOST;InstanceName;MSSQLSERVER;IsClustered;No;");
        assert_eq!(parse_reply(&datagram), vec!["HOST"]);
    }

    #[test]
    fn one_reply_may_describe_several_instances() {
        let datagram = reply(
            "ServerName;HOST;InstanceName;ONE;IsClustered;No;;ServerName;HOST;InstanceName;TWO;IsClustered;No;",
        );
        assert_eq!(parse_reply(&datagram), vec!["HOST\\ONE", "HOST\\TWO"]);
    }

    #[test]
    fn junk_is_ignored_rather_than_guessed_at() {
        assert!(parse_reply(&[]).is_empty());
        assert!(parse_reply(&[0x01, 0, 0]).is_empty());
        assert!(parse_reply(&reply("nothing useful here")).is_empty());
    }

    #[test]
    fn a_length_longer_than_the_datagram_does_not_panic() {
        let mut datagram = reply("ServerName;HOST;InstanceName;ONE;");
        datagram[1] = 0xFF;
        datagram[2] = 0xFF;
        assert_eq!(parse_reply(&datagram), vec!["HOST\\ONE"]);
    }

    #[test]
    fn the_header_is_present_unless_clean_was_asked_for() {
        assert!(list_from(Vec::new(), false).starts_with(&format!("{EOL}Servers:{EOL}")));
        assert_eq!(list_from(Vec::new(), true), "");
    }

    #[test]
    fn entries_are_indented_under_the_header_but_not_when_clean() {
        let names = vec!["HOST".to_string()];
        assert_eq!(
            list_from(names.clone(), false),
            format!("{EOL}Servers:{EOL}    HOST{EOL}")
        );
        assert_eq!(list_from(names, true), format!("HOST{EOL}"));
    }

    #[test]
    fn entries_are_sorted() {
        let names = vec!["B".to_string(), "A".to_string()];
        assert_eq!(list_from(names, true), format!("A{EOL}B{EOL}"));
    }
}

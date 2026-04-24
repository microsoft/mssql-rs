// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Service Principal Name (SPN) utilities for SQL Server authentication.
//!
//! SPNs are used by Kerberos to identify the SQL Server service.
//! The format for SQL Server is: `MSSQLSvc/<hostname>:<port_or_instance>`

/// SQL Server service class for SPN.
const SQL_SERVICE_CLASS: &str = "MSSQLSvc";

// Well-known loopback identifiers used by `is_loopback_address`.
const LOOPBACK_LOCALHOST: &str = "localhost";
const LOOPBACK_IPV4: &str = "127.0.0.1";
const LOOPBACK_IPV6: &str = "::1";
const LOOPBACK_IPV6_BRACKETED: &str = "[::1]";
const LOOPBACK_DOT: &str = ".";

/// Generates a Service Principal Name (SPN) for SQL Server.
///
/// The SPN format is: `MSSQLSvc/<hostname>:<port_or_instance>`
///
/// # Arguments
///
/// * `server` - The server hostname. Should be a fully qualified domain name (FQDN)
///   for Kerberos to work correctly (e.g., `server.contoso.com`).
/// * `instance` - Optional named instance. If provided, the instance name is used
///   instead of the port number.
/// * `port` - The TCP port number. Used when `instance` is `None` or empty.
///
/// # Returns
///
/// The constructed SPN string.
///
/// # Examples
///
/// ```
/// use mssql_tds::security::make_spn;
///
/// // Default instance with port
/// let spn = make_spn("server.contoso.com", None, 1433);
/// assert_eq!(spn, "MSSQLSvc/server.contoso.com:1433");
///
/// // Named instance
/// let spn = make_spn("server.contoso.com", Some("INSTANCE1"), 1433);
/// assert_eq!(spn, "MSSQLSvc/server.contoso.com:INSTANCE1");
/// ```
pub fn make_spn(server: &str, instance: Option<&str>, port: u16) -> String {
    match instance {
        Some(inst) if !inst.is_empty() => {
            format!("{}/{}:{}", SQL_SERVICE_CLASS, server, inst)
        }
        _ => {
            format!("{}/{}:{}", SQL_SERVICE_CLASS, server, port)
        }
    }
}

/// Determines if a hostname is likely a loopback address.
///
/// This is used on Windows to determine if an empty SPN retry
/// should be attempted for NTLM fallback. Also matches the local
/// computer name, since connections to the machine's own hostname
/// are effectively loopback (e.g. SSRP-resolved named pipes return
/// the machine name even when the user specified "localhost").
pub fn is_loopback_address(hostname: &str) -> bool {
    let hostname_lower = hostname.to_lowercase();
    matches!(
        hostname_lower.as_str(),
        LOOPBACK_LOCALHOST | LOOPBACK_IPV4 | LOOPBACK_IPV6 | LOOPBACK_IPV6_BRACKETED | LOOPBACK_DOT
    ) || hostname_lower.starts_with("127.")
        || is_local_computer_name(hostname)
}

/// Checks if the given hostname matches the local computer name.
fn is_local_computer_name(hostname: &str) -> bool {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .is_some_and(|name| name.eq_ignore_ascii_case(hostname))
}

/// Checks if a string looks like an IP address (v4 or v6).
fn is_ip_address(host: &str) -> bool {
    // Simple check: if it parses as an IP address, it's an IP
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Checks if a hostname appears to be a fully qualified domain name (FQDN).
///
/// A hostname is considered an FQDN if it contains at least one dot and
/// is not an IP address.
fn is_fqdn(hostname: &str) -> bool {
    hostname.contains('.') && !is_ip_address(hostname)
}

/// Canonicalizes a hostname for SPN construction.
///
/// This function ensures the hostname is in the correct format for Kerberos
/// SPN matching by performing DNS resolution when necessary:
///
/// - **IP addresses**: Performs reverse DNS lookup to get the FQDN
/// - **Short names** (no dots): Performs forward DNS lookup to get the FQDN
/// - **FQDNs**: Returned as-is (already in correct format)
/// - **Loopback addresses**: Returned as-is (Kerberos typically not used)
///
/// # Arguments
///
/// * `hostname` - The hostname or IP address to canonicalize
///
/// # Returns
///
/// The canonicalized hostname (FQDN if resolution succeeded), or the original
/// hostname if resolution fails or is not needed.
///
/// # Examples
///
/// ```ignore
/// // IP address -> reverse DNS
/// let fqdn = canonicalize_hostname("192.168.1.100");
/// // Returns "sqlserver.contoso.com" if reverse DNS succeeds
///
/// // Short name -> forward DNS
/// let fqdn = canonicalize_hostname("sqlserver");
/// // Returns "sqlserver.contoso.com" if forward DNS succeeds
///
/// // Already FQDN -> unchanged
/// let fqdn = canonicalize_hostname("sqlserver.contoso.com");
/// // Returns "sqlserver.contoso.com"
/// ```
///
/// # Note
///
/// DNS resolution is a blocking operation. In async contexts, consider
/// calling this from a blocking task pool.
pub fn canonicalize_hostname(hostname: &str) -> String {
    // Loopback addresses don't need canonicalization (Kerberos not typically used)
    if is_loopback_address(hostname) {
        tracing::debug!(
            "Skipping hostname canonicalization for loopback address: {}",
            hostname
        );
        return hostname.to_string();
    }

    // If it's an IP address, do reverse DNS lookup
    if let Ok(ip) = hostname.parse::<std::net::IpAddr>() {
        tracing::debug!("Performing reverse DNS lookup for IP address: {}", hostname);
        match dns_lookup::lookup_addr(&ip) {
            Ok(fqdn) => {
                tracing::debug!("Reverse DNS resolved {} -> {}", hostname, fqdn);
                return fqdn;
            }
            Err(e) => {
                tracing::warn!(
                    "Reverse DNS lookup failed for {}: {}. Using IP address in SPN (may fail with Kerberos)",
                    hostname,
                    e
                );
                return hostname.to_string();
            }
        }
    }

    // If it's already an FQDN, return as-is
    if is_fqdn(hostname) {
        tracing::debug!("Hostname {} appears to be FQDN, using as-is", hostname);
        return hostname.to_string();
    }

    // Short name: do forward DNS lookup to get FQDN
    tracing::debug!(
        "Performing forward DNS lookup to canonicalize short name: {}",
        hostname
    );
    match dns_lookup::getaddrinfo(Some(hostname), None, None) {
        Ok(addrs) => {
            // Iterate through results to find a valid address
            for addr_info in addrs.flatten() {
                // Got an IP, now reverse lookup to get the canonical name
                let ip = addr_info.sockaddr.ip();
                match dns_lookup::lookup_addr(&ip) {
                    Ok(fqdn) => {
                        tracing::debug!("Forward+reverse DNS resolved {} -> {}", hostname, fqdn);
                        return fqdn;
                    }
                    Err(_) => {
                        // Forward lookup worked but reverse failed, try next address
                        continue;
                    }
                }
            }
            // All addresses tried, none had reverse DNS
            tracing::debug!(
                "Reverse lookup failed for all addresses of {}, using original",
                hostname
            );
        }
        Err(e) => {
            tracing::debug!(
                "Forward DNS lookup failed for {}: {:?}. Using hostname as-is",
                hostname,
                e
            );
        }
    }

    // Fallback: return original hostname
    hostname.to_string()
}

/// Generates a Service Principal Name (SPN) for SQL Server with hostname canonicalization.
///
/// This is the preferred function for SPN generation as it performs DNS resolution
/// to ensure the hostname matches what's registered in Active Directory.
///
/// # Arguments
///
/// * `server` - The server hostname or IP address
/// * `instance` - Optional named instance name
/// * `port` - The TCP port number (used when instance is None)
///
/// # Returns
///
/// The constructed SPN string with a canonicalized hostname.
///
/// # Example
///
/// ```ignore
/// // Connecting by IP - will resolve to FQDN for SPN
/// let spn = make_spn_canonicalized("192.168.1.100", None, 1433);
/// // Returns "MSSQLSvc/sqlserver.contoso.com:1433" if reverse DNS works
/// ```
pub fn make_spn_canonicalized(server: &str, instance: Option<&str>, port: u16) -> String {
    let canonical_server = canonicalize_hostname(server);
    make_spn(&canonical_server, instance, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================
    // make_spn tests
    // ========================

    #[test]
    fn test_make_spn_default_instance() {
        let spn = make_spn("server.contoso.com", None, 1433);
        assert_eq!(spn, "MSSQLSvc/server.contoso.com:1433");
    }

    #[test]
    fn test_make_spn_custom_port() {
        let spn = make_spn("server.contoso.com", None, 1500);
        assert_eq!(spn, "MSSQLSvc/server.contoso.com:1500");
    }

    #[test]
    fn test_make_spn_named_instance() {
        let spn = make_spn("server.contoso.com", Some("INSTANCE1"), 1433);
        assert_eq!(spn, "MSSQLSvc/server.contoso.com:INSTANCE1");
    }

    #[test]
    fn test_make_spn_empty_instance_uses_port() {
        let spn = make_spn("server.contoso.com", Some(""), 1433);
        assert_eq!(spn, "MSSQLSvc/server.contoso.com:1433");
    }

    #[test]
    fn test_make_spn_simple_hostname() {
        let spn = make_spn("myserver", None, 1433);
        assert_eq!(spn, "MSSQLSvc/myserver:1433");
    }

    #[test]
    fn test_make_spn_ip_address() {
        let spn = make_spn("192.168.1.100", None, 1433);
        assert_eq!(spn, "MSSQLSvc/192.168.1.100:1433");
    }

    // ========================
    // is_loopback_address tests
    // ========================

    #[test]
    fn test_is_loopback_localhost() {
        assert!(is_loopback_address("localhost"));
        assert!(is_loopback_address("LOCALHOST"));
        assert!(is_loopback_address("LocalHost"));
    }

    #[test]
    fn test_is_loopback_ipv4() {
        assert!(is_loopback_address("127.0.0.1"));
        assert!(is_loopback_address("127.0.0.2"));
        assert!(is_loopback_address("127.255.255.255"));
    }

    #[test]
    fn test_is_loopback_ipv6() {
        assert!(is_loopback_address("::1"));
        assert!(is_loopback_address("[::1]"));
    }

    #[test]
    fn test_is_loopback_dot() {
        assert!(is_loopback_address("."));
    }

    #[test]
    fn test_is_loopback_computer_name() {
        // The local computer name should be treated as loopback
        if let Ok(name) = hostname::get().and_then(|h| {
            h.into_string()
                .map_err(|_| std::io::Error::other("non-utf8"))
        }) {
            assert!(
                is_loopback_address(&name),
                "local computer name '{}' should be loopback",
                name
            );
            assert!(
                is_loopback_address(&name.to_uppercase()),
                "case-insensitive match"
            );
        }
    }

    #[test]
    fn test_is_not_loopback() {
        assert!(!is_loopback_address("server.contoso.com"));
        assert!(!is_loopback_address("192.168.1.1"));
        assert!(!is_loopback_address("definitely-not-this-machine-name-xyz"));
    }

    // ========================
    // is_ip_address tests
    // ========================

    #[test]
    fn test_is_ip_address_ipv4() {
        assert!(is_ip_address("192.168.1.100"));
        assert!(is_ip_address("127.0.0.1"));
        assert!(is_ip_address("10.0.0.1"));
    }

    #[test]
    fn test_is_ip_address_ipv6() {
        assert!(is_ip_address("::1"));
        assert!(is_ip_address("fe80::1"));
        assert!(is_ip_address("2001:db8::1"));
    }

    #[test]
    fn test_is_not_ip_address() {
        assert!(!is_ip_address("server.contoso.com"));
        assert!(!is_ip_address("myserver"));
        assert!(!is_ip_address("localhost"));
    }

    // ========================
    // is_fqdn tests
    // ========================

    #[test]
    fn test_is_fqdn() {
        assert!(is_fqdn("server.contoso.com"));
        assert!(is_fqdn("sql.example.local"));
        assert!(is_fqdn("a.b"));
    }

    #[test]
    fn test_is_not_fqdn() {
        // Short names without dots
        assert!(!is_fqdn("myserver"));
        assert!(!is_fqdn("localhost"));
        // IP addresses with dots are not FQDNs
        assert!(!is_fqdn("192.168.1.100"));
        assert!(!is_fqdn("10.0.0.1"));
    }

    // ========================
    // canonicalize_hostname tests
    // ========================

    #[test]
    fn test_canonicalize_hostname_loopback_unchanged() {
        // Loopback addresses should be returned as-is
        assert_eq!(canonicalize_hostname("localhost"), "localhost");
        assert_eq!(canonicalize_hostname("127.0.0.1"), "127.0.0.1");
        assert_eq!(canonicalize_hostname("::1"), "::1");
    }

    #[test]
    fn test_canonicalize_hostname_fqdn_unchanged() {
        // Already an FQDN - returned as-is
        assert_eq!(
            canonicalize_hostname("server.contoso.com"),
            "server.contoso.com"
        );
    }

    // Note: Tests for actual DNS resolution would require network access
    // and are better suited for integration tests

    // ========================
    // make_spn_canonicalized tests
    // ========================

    #[test]
    fn test_make_spn_canonicalized_fqdn() {
        // FQDN should be used as-is
        let spn = make_spn_canonicalized("server.contoso.com", None, 1433);
        assert_eq!(spn, "MSSQLSvc/server.contoso.com:1433");
    }

    #[test]
    fn test_make_spn_canonicalized_loopback() {
        // Loopback should not trigger DNS lookup
        let spn = make_spn_canonicalized("localhost", None, 1433);
        assert_eq!(spn, "MSSQLSvc/localhost:1433");
    }
}

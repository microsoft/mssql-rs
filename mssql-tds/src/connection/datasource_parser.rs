// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data source string parser for SQL Server connections
//!
//! This module implements the SQL Network Interface (SNI) layer's connection string
//! parsing logic, compatible with ODBC driver behavior.

use crate::connection::connection_actions::{
    ConnectionActionChain, ConnectionActionChainBuilder, ConnectionMetadata, ResultSlot,
};
use crate::core::TdsResult;
use crate::error::Error;

/// Named pipe path prefix (UNC path format)
const NAMED_PIPE_PREFIX: &str = "\\\\";

/// Represents a parsed data source string with all connection parameters
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDataSource {
    /// Protocol name (tcp, np, lpc, admin) or empty for auto-detection
    pub protocol_name: String,
    /// Server name (resolved, e.g., localhost or actual hostname)
    pub server_name: String,
    /// Original server name as provided by user
    pub original_server_name: String,
    /// Instance name (e.g., SQLEXPRESS) or empty for default instance
    pub instance_name: String,
    /// Protocol parameter (port number for TCP, pipe path for Named Pipes)
    pub protocol_parameter: String,
    /// Alias used for connection caching (server\instance format)
    pub alias: String,
    /// Whether this connection can use the connection cache (LastConnectCache in ODBC)
    ///
    /// Connection caching stores previously successful connection protocol details
    /// (typically from SQL Browser/SSRP resolutions) to speed up subsequent connections.
    /// Caching is disabled when sufficient connection information is already provided:
    ///
    /// - Port explicitly specified (e.g., `server,1433`)
    /// - Named pipe path provided (e.g., `\\server\pipe\sql\query`)
    /// - Parallel connect enabled (MultiSubnetFailover)
    /// - Non-standard instance names
    /// - LocalDB connections
    ///
    /// Caching is only useful for simple connection strings like `server\instance`
    /// that require SSRP resolution to discover the port.
    pub can_use_cache: bool,
    /// Whether the instance name follows standard naming convention
    pub standard_instance_name: bool,
    /// Whether parallel connection is requested (MultiSubnetFailover)
    pub parallel_connect: bool,
}

/// Protocol types for SQL Server connections
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    /// TCP/IP protocol
    Tcp,
    /// Named Pipes protocol
    NamedPipe,
    /// Shared Memory protocol (local only, Windows)
    SharedMemory,
    /// Dedicated Admin Connection
    Admin,
    /// Auto-detect (no protocol specified)
    Auto,
}

impl ParsedDataSource {
    /// Creates a new ParsedDataSource with default values
    pub fn new() -> Self {
        Self {
            protocol_name: String::new(),
            server_name: String::new(),
            original_server_name: String::new(),
            instance_name: String::new(),
            protocol_parameter: String::new(),
            alias: String::new(),
            can_use_cache: true,
            standard_instance_name: true,
            parallel_connect: false,
        }
    }

    /// Parse a data source string into structured connection parameters
    ///
    /// # Arguments
    /// * `datasource` - The data source string (e.g., "tcp:server,1433", "server\instance")
    /// * `parallel_connect` - Whether MultiSubnetFailover is enabled
    ///
    /// # Examples
    /// ```
    /// use mssql_tds::connection::datasource_parser::ParsedDataSource;
    ///
    /// let parsed = ParsedDataSource::parse("tcp:myserver,1433", false)?;
    /// assert_eq!(parsed.protocol_name, "tcp");
    /// assert_eq!(parsed.server_name, "myserver");
    /// assert_eq!(parsed.protocol_parameter, "1433");
    /// ```
    pub fn parse(datasource: &str, parallel_connect: bool) -> TdsResult<Self> {
        let mut result = Self::new();
        result.parallel_connect = parallel_connect;

        // Step 1: Normalize the string (lowercase, trim whitespace)
        let original = datasource.trim();
        let normalized = original.to_lowercase();
        if normalized.is_empty() {
            return Err(Error::ProtocolError(
                "Data source string cannot be empty".to_string(),
            ));
        }

        // Step 2: Check for LocalDB format
        // LocalDB is only supported on Windows
        #[cfg(windows)]
        if normalized.starts_with("(localdb)\\") || normalized.starts_with("(localdb)/") {
            return Self::parse_localdb(original, &normalized, parallel_connect);
        }

        // On non-Windows platforms, reject LocalDB with a clear error message
        #[cfg(not(windows))]
        if normalized.starts_with("(localdb)\\") || normalized.starts_with("(localdb)/") {
            return Err(Error::ProtocolError(
                "LocalDB is not supported on this platform. LocalDB is a Windows-only feature. \
                 Use a TCP connection string instead (e.g., 'tcp:server,port')."
                    .to_string(),
            ));
        }

        // Step 3: Parse protocol prefix (tcp:, np:, lpc:, admin:)
        let (after_protocol_norm, after_protocol_orig) =
            Self::parse_protocol(original, &normalized, &mut result)?;

        // Step 4: Check for named pipe (starts with \\)
        if after_protocol_norm.starts_with(NAMED_PIPE_PREFIX) {
            return Self::parse_named_pipe(after_protocol_orig, &mut result);
        }

        // Step 5: Parse parameter (port) - look for comma
        let (after_parameter_norm, after_parameter_orig) =
            Self::parse_parameter(after_protocol_orig, after_protocol_norm, &mut result)?;

        // Step 6: Parse instance name - look for backslash
        let (after_instance_norm, after_instance_orig) =
            Self::parse_instance(after_parameter_orig, after_parameter_norm, &mut result)?;

        // Step 7: Parse and resolve server name
        Self::parse_server(after_instance_orig, after_instance_norm, &mut result)?;

        // Step 8: Validate protocol constraints
        Self::validate_protocol(&mut result)?;

        // Step 9: Build alias and determine cache eligibility
        Self::build_alias(&mut result);

        Ok(result)
    }

    /// Parse protocol prefix from the data source string
    ///
    /// Extracts the optional protocol prefix (tcp:, np:, lpc:, admin:) from the beginning
    /// of the data source string. The function intelligently distinguishes between protocol
    /// prefixes and IPv6 addresses, which also contain colons.
    ///
    /// # ODBC Compatibility
    ///
    /// This implementation matches ODBC/SNI behavior for connection string parsing:
    /// - Protocol prefixes are extracted using colon (`:`) delimiter
    /// - IPv6 addresses are detected and not treated as protocol prefixes
    /// - When a comma (`,`) is found later (in parse_parameter), TCP is automatically
    ///   selected as the default protocol if no protocol prefix was specified
    ///
    /// # Protocol Detection Logic
    ///
    /// 1. Searches for the first colon (`:`) delimiter in the normalized string
    /// 2. Checks if the string is an IPv6 address by detecting:
    ///    - Double colon (`::`) patterns
    ///    - Multiple colons (more than one)
    /// 3. If not IPv6, validates the prefix against known protocols
    /// 4. Stores the protocol in `result.protocol_name` if valid
    ///
    /// # Supported Protocols
    ///
    /// - `tcp` - TCP/IP protocol
    /// - `np` - Named Pipes protocol
    /// - `lpc` - Local Procedure Call (Shared Memory)
    /// - `admin` - Dedicated Admin Connection (DAC)
    ///
    /// # Arguments
    ///
    /// * `original` - The original data source string (preserves case)
    /// * `normalized` - The lowercase version for case-insensitive matching
    /// * `result` - Mutable reference to store the extracted protocol name
    ///
    /// # Returns
    ///
    /// Returns a tuple of two string slices (normalized, original) representing
    /// the remaining portion of the string after the protocol prefix is removed.
    /// If no valid protocol is found, returns the input strings unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// // Valid protocol prefix
    /// // Input: "tcp:myserver,1433"
    /// // Extracts: protocol_name = "tcp"
    /// // Returns: ("myserver,1433", "myserver,1433")
    ///
    /// // IPv6 address (not a protocol)
    /// // Input: "::1"
    /// // Extracts: nothing (IPv6 detected)
    /// // Returns: ("::1", "::1")
    ///
    /// // IPv6 with port
    /// // Input: "2001:db8::1,1433"
    /// // Extracts: nothing (multiple colons = IPv6)
    /// // Returns: ("2001:db8::1,1433", "2001:db8::1,1433")
    ///
    /// // No protocol specified
    /// // Input: "myserver\instance"
    /// // Extracts: nothing
    /// // Returns: ("myserver\instance", "myserver\instance")
    ///
    /// // Invalid protocol ignored
    /// // Input: "http:myserver"
    /// // Extracts: nothing (http not in allowed list)
    /// // Returns: ("http:myserver", "http:myserver")
    /// ```
    fn parse_protocol<'a>(
        original: &'a str,
        normalized: &'a str,
        result: &mut ParsedDataSource,
    ) -> TdsResult<(&'a str, &'a str)> {
        // Look for colon delimiter in normalized string
        if let Some(colon_pos) = normalized.find(':') {
            // Check if this is IPv6 address (multiple colons)
            let before_colon = &normalized[..colon_pos];

            // IPv6 addresses contain :: or multiple colons
            let is_ipv6 = normalized.contains("::") || normalized.matches(':').count() > 1;

            if !is_ipv6 && !before_colon.is_empty() {
                // Valid protocol prefix
                let protocol = before_colon.trim();
                if matches!(protocol, "tcp" | "np" | "lpc" | "admin") {
                    result.protocol_name = protocol.to_string();
                    return Ok((
                        normalized[colon_pos + 1..].trim_start(),
                        original[colon_pos + 1..].trim_start(),
                    ));
                }
            }
        }

        // No protocol or IPv6 address - return as-is
        Ok((normalized, original))
    }

    /// Parse named pipe path (\\server\pipe\...)
    fn parse_named_pipe(input: &str, result: &mut ParsedDataSource) -> TdsResult<ParsedDataSource> {
        // Named pipe format: \\server\pipe\...
        if !input.starts_with(NAMED_PIPE_PREFIX) {
            return Err(Error::ProtocolError("Invalid named pipe path".to_string()));
        }

        // Extract server name (between \\ and next \)
        let after_slashes = &input[2..];
        let server_end = after_slashes.find('\\').ok_or_else(|| {
            Error::ProtocolError("Invalid named pipe path - no server specified".to_string())
        })?;

        let server = &after_slashes[..server_end];
        if server.is_empty() {
            return Err(Error::ProtocolError(
                "Invalid named pipe path - blank server".to_string(),
            ));
        }

        // Store the full pipe path
        result.protocol_parameter = input.to_string();
        result.protocol_name = "np".to_string();

        // Resolve server name (. becomes localhost)
        result.original_server_name = server.to_string();
        result.server_name = if server == "." {
            "localhost".to_string()
        } else {
            server.to_string()
        };

        // Parse instance from pipe path
        let pipe_path = &after_slashes[server_end..];

        // Standard default instance: \pipe\sql\query
        if pipe_path == "\\pipe\\sql\\query" {
            result.instance_name = String::new(); // default instance
            result.standard_instance_name = true;
        }
        // Standard named instance: \pipe\MSSQL$instance\sql\query
        else if pipe_path.starts_with("\\pipe\\mssql$") && pipe_path.ends_with("\\sql\\query") {
            let start = "\\pipe\\mssql$".len();
            let end = pipe_path.len() - "\\sql\\query".len();
            result.instance_name = pipe_path[start..end].to_string();
            result.standard_instance_name = true;
        }
        // Non-standard pipe
        else if let Some(custom_path) = pipe_path.strip_prefix("\\pipe\\") {
            // Use "pipe<rest_of_path>" format
            result.instance_name = format!("pipe{}", custom_path);
            result.standard_instance_name = false;
        } else {
            return Err(Error::ProtocolError(format!(
                "Invalid named pipe path: {}",
                input
            )));
        }

        result.can_use_cache = false;

        // Validate protocol constraints (including parallel connect)
        Self::validate_protocol(result)?;

        Ok(result.clone())
    }

    /// Parse protocol parameter (port for TCP)
    ///
    /// # ODBC Compatibility - Comma Detection
    ///
    /// This function implements ODBC/SNI behavior for comma (`,`) handling:
    /// - When a comma is found, it indicates port separation: `<servername>,<port>`
    /// - **Automatically defaults to TCP protocol** if no protocol prefix was specified
    /// - Port takes priority over instance name (instance name is stripped if both present)
    /// - Returns an error if a non-TCP protocol was explicitly specified with a comma
    ///
    /// # Examples
    ///
    /// ```
    /// // Comma defaults to TCP (ODBC behavior)
    /// // Input: "myserver,1433"
    /// // Result: protocol_name = "tcp", protocol_parameter = "1433", server = "myserver"
    ///
    /// // Explicit protocol with comma
    /// // Input: "tcp:myserver,1433"
    /// // Result: protocol_name = "tcp", protocol_parameter = "1433", server = "myserver"
    ///
    /// // Port takes priority over instance
    /// // Input: "myserver\instance,1433"
    /// // Result: protocol_parameter = "1433", instance_name = "" (ignored)
    ///
    /// // Error: Non-TCP protocol with comma
    /// // Input: "np:myserver,1433"
    /// // Result: Error (port only valid for TCP)
    /// ```
    fn parse_parameter<'a>(
        original: &'a str,
        normalized: &'a str,
        result: &mut ParsedDataSource,
    ) -> TdsResult<(&'a str, &'a str)> {
        // Look for comma separator in normalized string
        if let Some(comma_pos) = normalized.find(',') {
            let parameter = original[comma_pos + 1..].trim();

            // ODBC Compatibility: If no protocol specified, default to TCP
            // This matches ODBC/SNI behavior where comma implies TCP connection
            if result.protocol_name.is_empty() {
                result.protocol_name = "tcp".to_string();
            }

            // Port is only valid for TCP protocol
            if result.protocol_name != "tcp" {
                return Err(Error::ProtocolError(format!(
                    "Port parameter only valid for TCP protocol, got: {}",
                    result.protocol_name
                )));
            }

            // Store port number, strip any instance name after it
            let port = if let Some(backslash_pos) = parameter.find('\\') {
                &parameter[..backslash_pos]
            } else {
                parameter
            };

            result.protocol_parameter = port.to_string();
            result.can_use_cache = false;

            // Return the part before comma (server and possibly instance)
            return Ok((
                normalized[..comma_pos].trim_end(),
                original[..comma_pos].trim_end(),
            ));
        }

        Ok((normalized, original))
    }

    /// Parse instance name (after backslash)
    fn parse_instance<'a>(
        original: &'a str,
        normalized: &'a str,
        result: &mut ParsedDataSource,
    ) -> TdsResult<(&'a str, &'a str)> {
        // Look for backslash separator in normalized string
        if let Some(backslash_pos) = normalized.find('\\') {
            let instance = original[backslash_pos + 1..].trim();

            // Port takes priority - if port is already set, ignore instance
            if result.protocol_parameter.is_empty() {
                // Validate: "mssqlserver" is reserved and invalid (case-insensitive check)
                if instance.eq_ignore_ascii_case("mssqlserver") {
                    return Err(Error::ProtocolError(
                        "Instance name 'MSSQLSERVER' is reserved".to_string(),
                    ));
                }

                result.instance_name = instance.to_string();
            }

            // Return server part (before backslash)
            return Ok((
                normalized[..backslash_pos].trim_end(),
                original[..backslash_pos].trim_end(),
            ));
        }

        Ok((normalized, original))
    }

    /// Parse and resolve server name
    fn parse_server(
        original: &str,
        normalized: &str,
        result: &mut ParsedDataSource,
    ) -> TdsResult<()> {
        let server = original.trim();
        result.original_server_name = server.to_string();

        // Check if this is a local host alias (use normalized for comparison)
        let normalized_server = normalized.trim();
        let is_local = normalized_server == "."
            || normalized_server == "(local)"
            || normalized_server == "localhost"
            || Self::is_computer_name(normalized_server);

        if is_local {
            // For TCP and admin protocols, keep "localhost" as-is
            // This preserves TLS certificate hostname validation behavior
            // Named Pipes and Shared Memory may need the actual computer name
            if result.protocol_name == "admin" || result.protocol_name == "tcp" {
                result.server_name = "localhost".to_string();
            } else {
                // For np, lpc, or unspecified protocols, resolve to actual computer name
                result.server_name =
                    Self::get_computer_name().unwrap_or_else(|| server.to_string());
            }
        } else {
            result.server_name = server.to_string();
        }

        Ok(())
    }

    /// Validate protocol-specific constraints
    fn validate_protocol(result: &mut ParsedDataSource) -> TdsResult<()> {
        // Platform-specific protocol validation
        #[cfg(not(windows))]
        {
            // Named Pipes are only supported on Windows
            if result.protocol_name == "np" {
                return Err(Error::ProtocolError(
                    "Named Pipes (np:) protocol is not supported on this platform. \
                     Named Pipes are a Windows-only feature. Use TCP instead (e.g., 'tcp:server,port')."
                        .to_string(),
                ));
            }

            // Shared Memory (LPC) is only supported on Windows
            if result.protocol_name == "lpc" {
                return Err(Error::ProtocolError(
                    "Shared Memory (lpc:) protocol is not supported on this platform. \
                     Shared Memory is a Windows-only feature. Use TCP instead (e.g., 'tcp:server,port')."
                        .to_string(),
                ));
            }
        }

        // LPC (Shared Memory) requires local server
        #[cfg(windows)]
        if result.protocol_name == "lpc" {
            let is_local = result.original_server_name == "."
                || result.original_server_name == "(local)"
                || result
                    .original_server_name
                    .eq_ignore_ascii_case("localhost")
                || Self::is_computer_name(&result.original_server_name);

            if !is_local {
                return Err(Error::ProtocolError(
                    "Shared Memory (lpc) protocol requires local server".to_string(),
                ));
            }
        }

        // Parallel connect (MultiSubnetFailover) has restrictions
        if result.parallel_connect {
            // Cannot use np, lpc, or admin with parallel connect
            if matches!(result.protocol_name.as_str(), "np" | "lpc" | "admin") {
                return Err(Error::ProtocolError(format!(
                    "Protocol '{}' is incompatible with parallel connect (MultiSubnetFailover)",
                    result.protocol_name
                )));
            }

            // If no protocol specified, default to TCP
            if result.protocol_name.is_empty() {
                result.protocol_name = "tcp".to_string();
            }
        }

        Ok(())
    }

    /// Build alias and determine cache eligibility
    fn build_alias(result: &mut ParsedDataSource) {
        // Build alias as server\instance or just server
        if result.instance_name.is_empty() {
            result.alias = result.server_name.clone();
        } else {
            result.alias = format!("{}\\{}", result.server_name, result.instance_name);
        }

        // Cache is disabled if any of these conditions are true:
        // - Named pipe path specified
        // - Explicit port specified
        // - LPC/admin protocol with instance
        // - Parallel connect enabled
        // - Non-standard instance name

        if !result.protocol_parameter.is_empty() {
            result.can_use_cache = false;
        }

        // DAC (admin:) never uses cache - SQL Browser doesn't support DAC resolution
        if result.protocol_name == "admin" {
            result.can_use_cache = false;
        }

        if result.protocol_name == "lpc" && !result.instance_name.is_empty() {
            result.can_use_cache = false;
        }

        if result.parallel_connect {
            result.can_use_cache = false;
        }

        if !result.standard_instance_name {
            result.can_use_cache = false;
        }
    }

    /// Parse LocalDB connection string (Windows only)
    #[cfg(windows)]
    fn parse_localdb(
        original: &str,
        normalized: &str,
        parallel_connect: bool,
    ) -> TdsResult<ParsedDataSource> {
        // Format: (localdb)\instancename or (localdb)/instancename
        let instance_start = if normalized.starts_with("(localdb)\\") {
            "(localdb)\\".len()
        } else if normalized.starts_with("(localdb)/") {
            "(localdb)/".len()
        } else {
            return Err(Error::ProtocolError("Invalid LocalDB format".to_string()));
        };

        let instance_name = original[instance_start..].trim();
        if instance_name.is_empty() {
            return Err(Error::ProtocolError(
                "LocalDB instance name cannot be empty".to_string(),
            ));
        }

        // LocalDB connections will be resolved to named pipes later
        // For now, return a placeholder that indicates LocalDB
        let mut result = ParsedDataSource::new();
        result.protocol_name = "localdb".to_string();
        result.server_name = original.to_string(); // Store original for later resolution
        result.original_server_name = original.to_string();
        result.instance_name = instance_name.to_string();
        result.can_use_cache = false; // LocalDB connections are never cached
        result.alias = original.to_string();
        result.parallel_connect = parallel_connect;

        Ok(result)
    }

    /// Check if the given name matches the computer name
    fn is_computer_name(name: &str) -> bool {
        // Get computer name and compare
        if let Some(computer_name) = Self::get_computer_name() {
            name.eq_ignore_ascii_case(&computer_name)
        } else {
            false
        }
    }

    /// Get the local computer name
    fn get_computer_name() -> Option<String> {
        hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok())
    }

    /// Get the protocol type from the parsed data source
    pub fn get_protocol_type(&self) -> ProtocolType {
        match self.protocol_name.as_str() {
            "tcp" => ProtocolType::Tcp,
            "np" => ProtocolType::NamedPipe,
            "lpc" => ProtocolType::SharedMemory,
            "admin" => ProtocolType::Admin,
            "" => ProtocolType::Auto,
            #[cfg(windows)]
            "localdb" => ProtocolType::NamedPipe, // LocalDB uses named pipes
            _ => ProtocolType::Auto,
        }
    }

    /// Check if this represents a local connection
    pub fn is_local(&self) -> bool {
        self.original_server_name == "."
            || self.original_server_name == "(local)"
            || self.original_server_name.eq_ignore_ascii_case("localhost")
            || self.original_server_name == "127.0.0.1"
            || self.original_server_name == "::1"
            || Self::is_computer_name(&self.original_server_name)
    }

    /// Check if SSRP (SQL Server Resolution Protocol) is needed
    ///
    /// SSRP is used when:
    /// - Named instance is specified
    /// - No explicit port is provided
    ///
    /// Per ODBC/SNI behavior:
    /// - `tcp:server\instance` (no port) → SSRP is required
    /// - `tcp:server,port\instance` → SSRP is NOT required (instance is ignored per MDAC behavior)
    /// - `server\instance` (no protocol) → SSRP is required
    pub fn needs_ssrp(&self) -> bool {
        // SSRP is needed if:
        // 1. Instance name is specified, AND
        // 2. No explicit port is provided (protocol_parameter is empty or not a valid port)
        //
        // Note: Even with tcp: prefix, if no port is given, SSRP is required.
        // This matches ODBC/SNI behavior where instance name is only ignored
        // when a port (comma) is explicitly specified.
        !self.instance_name.is_empty() && self.protocol_parameter.is_empty()
    }

    /// Generate the connection action chain for this data source
    ///
    /// This method converts the parsed data source into an ordered sequence of
    /// actions to establish a connection. The action chain represents the connection
    /// strategy, including any necessary pre-connection steps like cache checks,
    /// SSRP queries, or protocol waterfall attempts.
    ///
    /// # Arguments
    /// * `timeout_ms` - Connection timeout in milliseconds (default: 15000)
    ///
    /// # Returns
    /// A `ConnectionActionChain` containing the ordered sequence of actions
    ///
    /// # Examples
    /// ```
    /// use mssql_tds::connection::datasource_parser::ParsedDataSource;
    ///
    /// // Simple TCP connection
    /// let parsed = ParsedDataSource::parse("tcp:myserver,1433", false)?;
    /// let chain = parsed.to_connection_actions(15000);
    /// assert_eq!(chain.len(), 1); // Single ConnectTcp action
    ///
    /// // Named instance requiring SSRP
    /// let parsed = ParsedDataSource::parse("myserver\\SQLEXPRESS", false)?;
    /// let chain = parsed.to_connection_actions(15000);
    /// // Chain: CheckCache -> QuerySsrp -> UpdateCache -> ConnectTcp
    /// assert_eq!(chain.len(), 4);
    /// ```
    pub fn to_connection_actions(&self, timeout_ms: u64) -> ConnectionActionChain {
        let metadata = ConnectionMetadata {
            source_string: format!(
                "{}\\{}",
                self.original_server_name,
                if self.instance_name.is_empty() {
                    String::new()
                } else {
                    self.instance_name.clone()
                }
            )
            .trim_end_matches('\\')
            .to_string(),
            server_name: self.server_name.clone(),
            instance_name: self.instance_name.clone(),
            explicit_protocol: !self.protocol_name.is_empty(),
            timeout_ms,
        };

        let mut builder = ConnectionActionChainBuilder::new(metadata);

        // Step 1: Check cache if applicable
        if self.can_use_cache {
            builder.add_check_cache(&self.alias);
        }

        // Step 2: Handle LocalDB resolution (Windows only)
        #[cfg(windows)]
        if self.protocol_name == "localdb" {
            builder.add_resolve_localdb(&self.instance_name);
            builder.add_connect_named_pipe_from_slot(ResultSlot::ResolvedPipePath);
            return builder.build();
        }

        // Step 3: Handle SSRP query if needed
        if self.needs_ssrp() {
            builder.add_ssrp_query(&self.server_name, &self.instance_name);

            // After SSRP, update cache if allowed
            if self.can_use_cache {
                // Note: port will be determined at execution time from SSRP result
                builder.add_update_cache(&self.alias, 0); // 0 is placeholder
            }

            // Connect using resolved port
            builder.add_connect_tcp_from_slot(&self.server_name, ResultSlot::ResolvedPort);
            return builder.build();
        }

        // Step 4: Explicit protocol - single connection attempt
        if !self.protocol_name.is_empty() {
            match self.get_protocol_type() {
                ProtocolType::Tcp => {
                    let port = self.protocol_parameter.parse::<u16>().unwrap_or(1433);
                    builder.add_connect_tcp(&self.server_name, port);
                }
                ProtocolType::NamedPipe => {
                    let pipe = if !self.protocol_parameter.is_empty() {
                        self.protocol_parameter.clone()
                    } else if !self.instance_name.is_empty() {
                        if self.instance_name.to_lowercase() == "default" {
                            format!("\\\\{}\\pipe\\sql\\query", self.server_name)
                        } else {
                            format!(
                                "\\\\{}\\pipe\\MSSQL${}\\sql\\query",
                                self.server_name, self.instance_name
                            )
                        }
                    } else {
                        format!("\\\\{}\\pipe\\sql\\query", self.server_name)
                    };
                    builder.add_connect_named_pipe(&pipe);
                }
                ProtocolType::SharedMemory => {
                    #[cfg(windows)]
                    {
                        let instance = if self.instance_name.is_empty() {
                            "MSSQLSERVER"
                        } else {
                            &self.instance_name
                        };
                        builder.add_connect_shared_memory(instance);
                    }
                }
                ProtocolType::Admin => {
                    builder.add_connect_dac(&self.server_name);
                }
                _ => {}
            }
            return builder.build();
        }

        // Step 5: Parallel connect (MultiSubnetFailover)
        if self.parallel_connect {
            // Try TCP to all resolved addresses in parallel
            let port = self.protocol_parameter.parse::<u16>().unwrap_or(1433);
            builder.add_parallel_tcp_connect(&self.server_name, port);
            return builder.build();
        }

        // Step 6: Auto-detect (protocol waterfall)
        builder.add_protocol_waterfall(&self.server_name, self.is_local());
        builder.build()
    }
}

impl Default for ParsedDataSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tcp_with_port() {
        let parsed = ParsedDataSource::parse("tcp:myserver,1433", false).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.protocol_parameter, "1433");
        assert_eq!(parsed.instance_name, "");
        assert_eq!(parsed.alias, "myserver");
        assert!(!parsed.can_use_cache);
    }

    #[test]
    fn test_port_without_protocol_prefix() {
        // When comma with port is specified, should default to TCP
        let parsed = ParsedDataSource::parse("myserver,1433", false).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.protocol_parameter, "1433");
        assert_eq!(parsed.instance_name, "");
        assert!(!parsed.can_use_cache);
    }

    #[test]
    fn test_named_instance() {
        let parsed = ParsedDataSource::parse("myserver\\SQLEXPRESS", false).unwrap();
        assert_eq!(parsed.protocol_name, "");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.instance_name, "SQLEXPRESS");
        assert_eq!(parsed.alias, "myserver\\SQLEXPRESS");
        assert!(parsed.can_use_cache);
    }

    #[test]
    #[cfg(windows)]
    fn test_named_pipe_default_instance() {
        let parsed = ParsedDataSource::parse("np:\\\\myserver\\pipe\\sql\\query", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.instance_name, "");
        assert_eq!(parsed.protocol_parameter, "\\\\myserver\\pipe\\sql\\query");
        assert!(!parsed.can_use_cache);
        assert!(parsed.standard_instance_name);
    }

    #[test]
    #[cfg(windows)]
    fn test_named_pipe_named_instance() {
        let parsed =
            ParsedDataSource::parse("\\\\myserver\\pipe\\mssql$inst1\\sql\\query", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.instance_name, "inst1");
        assert!(parsed.standard_instance_name);
    }

    #[test]
    #[cfg(windows)]
    fn test_named_pipe_non_standard() {
        let parsed =
            ParsedDataSource::parse("\\\\myserver\\pipe\\custom\\myapp\\sql", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.instance_name, "pipecustom\\myapp\\sql");
        assert!(!parsed.standard_instance_name);
    }

    #[test]
    fn test_local_server_resolution() {
        let parsed = ParsedDataSource::parse("(local)\\SQLEXPRESS", false).unwrap();
        assert_eq!(parsed.original_server_name, "(local)");
        assert_eq!(parsed.instance_name, "SQLEXPRESS");
        assert!(parsed.is_local());
    }

    #[test]
    fn test_port_with_instance_port_wins() {
        let parsed = ParsedDataSource::parse("myserver\\INST1,1433", false).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
        assert_eq!(parsed.protocol_parameter, "1433");
        assert_eq!(parsed.instance_name, ""); // Instance ignored when port specified
    }

    #[test]
    fn test_admin_protocol() {
        let parsed = ParsedDataSource::parse("admin:localhost", false).unwrap();
        assert_eq!(parsed.protocol_name, "admin");
        assert_eq!(parsed.server_name, "localhost");
    }

    #[test]
    #[cfg(windows)]
    fn test_lpc_local_only() {
        // LPC with local server should succeed
        let parsed = ParsedDataSource::parse("lpc:.", false).unwrap();
        assert_eq!(parsed.protocol_name, "lpc");

        // LPC with remote server should fail
        let result = ParsedDataSource::parse("lpc:remoteserver", false);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(windows)]
    fn test_np_with_localhost() {
        // Named Pipes protocol with localhost explicitly specified
        // Note: server_name is resolved to actual computer name for np protocol
        let parsed = ParsedDataSource::parse("np:localhost", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.original_server_name, "localhost");
        assert!(parsed.is_local());
        // server_name will be the actual computer name, not "localhost"

        // Named Pipes with dot (local) notation
        let parsed = ParsedDataSource::parse("np:.", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.original_server_name, ".");
        assert!(parsed.is_local());
        // server_name resolved to computer name

        // Named Pipes with (local) notation
        let parsed = ParsedDataSource::parse("np:(local)", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.original_server_name, "(local)");
        assert!(parsed.is_local());
        // server_name resolved to computer name
    }

    #[test]
    #[cfg(windows)]
    fn test_lpc_shared_memory_local_variants() {
        // Shared Memory (LPC) with localhost
        // Note: server_name is resolved to actual computer name for lpc protocol
        let parsed = ParsedDataSource::parse("lpc:localhost", false).unwrap();
        assert_eq!(parsed.protocol_name, "lpc");
        assert_eq!(parsed.original_server_name, "localhost");
        assert!(parsed.is_local());
        // server_name will be the actual computer name, not "localhost"

        // Shared Memory with dot notation
        let parsed = ParsedDataSource::parse("lpc:.", false).unwrap();
        assert_eq!(parsed.protocol_name, "lpc");
        assert_eq!(parsed.original_server_name, ".");
        assert!(parsed.is_local());
        // server_name resolved to computer name

        // Shared Memory with (local) notation
        let parsed = ParsedDataSource::parse("lpc:(local)", false).unwrap();
        assert_eq!(parsed.protocol_name, "lpc");
        assert_eq!(parsed.original_server_name, "(local)");
        assert!(parsed.is_local());
        // server_name resolved to computer name

        // Shared Memory with instance name
        let parsed = ParsedDataSource::parse("lpc:.\\SQLEXPRESS", false).unwrap();
        assert_eq!(parsed.protocol_name, "lpc");
        assert_eq!(parsed.original_server_name, ".");
        assert_eq!(parsed.instance_name, "SQLEXPRESS");
        assert!(parsed.is_local());
        // server_name resolved to computer name
    }

    #[test]
    fn test_parallel_connect_restrictions() {
        // Parallel with TCP should work
        let parsed = ParsedDataSource::parse("tcp:myserver,1433", true).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
        assert!(parsed.parallel_connect);

        // Parallel with NP should fail
        let result = ParsedDataSource::parse("np:\\\\server\\pipe\\sql\\query", true);
        assert!(result.is_err());

        // Parallel with no protocol should default to TCP
        let parsed = ParsedDataSource::parse("myserver", true).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
    }

    #[test]
    fn test_ipv6_address() {
        // IPv6 address should not be confused with protocol prefix
        let parsed = ParsedDataSource::parse("::1", false).unwrap();
        assert_eq!(parsed.protocol_name, "");
        assert_eq!(parsed.server_name, "::1");
    }

    #[test]
    fn test_reserved_instance_name() {
        // "MSSQLSERVER" is reserved
        let result = ParsedDataSource::parse("myserver\\MSSQLSERVER", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_needs_ssrp() {
        // Named instance without port needs SSRP
        let parsed = ParsedDataSource::parse("myserver\\SQLEXPRESS", false).unwrap();
        assert!(parsed.needs_ssrp());

        // With port, no SSRP needed
        let parsed = ParsedDataSource::parse("myserver,1433", false).unwrap();
        assert!(!parsed.needs_ssrp());

        // No instance, no SSRP needed
        let parsed = ParsedDataSource::parse("myserver", false).unwrap();
        assert!(!parsed.needs_ssrp());
    }

    #[test]
    #[cfg(windows)]
    fn test_localdb_parsing() {
        let parsed = ParsedDataSource::parse("(localdb)\\MSSQLLocalDB", false).unwrap();
        assert_eq!(parsed.protocol_name, "localdb");
        assert_eq!(parsed.instance_name, "MSSQLLocalDB");
        assert!(!parsed.can_use_cache);

        // Forward slash also supported
        let parsed = ParsedDataSource::parse("(localdb)/v11.0", false).unwrap();
        assert_eq!(parsed.instance_name, "v11.0");

        // Empty instance name should fail
        let result = ParsedDataSource::parse("(localdb)\\", false);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(not(windows))]
    fn test_localdb_not_supported_on_non_windows() {
        // LocalDB should return a clear error on non-Windows platforms
        let result = ParsedDataSource::parse("(localdb)\\MSSQLLocalDB", false);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = err.to_string().to_lowercase();
        assert!(
            err_msg.contains("localdb") && err_msg.contains("not supported"),
            "Error should mention LocalDB is not supported: {}",
            err
        );

        // Forward slash variant should also fail
        let result = ParsedDataSource::parse("(localdb)/v11.0", false);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(not(windows))]
    fn test_named_pipe_not_supported_on_non_windows() {
        // Named Pipes should return a clear error on non-Windows platforms
        let result = ParsedDataSource::parse("np:myserver", false);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = err.to_string().to_lowercase();
        assert!(
            err_msg.contains("named pipe") && err_msg.contains("not supported"),
            "Error should mention Named Pipes is not supported: {}",
            err
        );

        // Full pipe path should also fail
        let result = ParsedDataSource::parse("np:\\\\myserver\\pipe\\sql\\query", false);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(not(windows))]
    fn test_shared_memory_not_supported_on_non_windows() {
        // Shared Memory (LPC) should return a clear error on non-Windows platforms
        let result = ParsedDataSource::parse("lpc:.", false);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = err.to_string().to_lowercase();
        assert!(
            err_msg.contains("shared memory") && err_msg.contains("not supported"),
            "Error should mention Shared Memory is not supported: {}",
            err
        );

        // localhost variant should also fail
        let result = ParsedDataSource::parse("lpc:localhost", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_case_insensitivity() {
        let parsed = ParsedDataSource::parse("TCP:MyServer,1433", false).unwrap();
        assert_eq!(parsed.protocol_name, "tcp"); // Normalized to lowercase

        let parsed = ParsedDataSource::parse("MyServer\\SqlExpress", false).unwrap();
        assert_eq!(parsed.instance_name, "SqlExpress"); // Case preserved
    }

    #[test]
    fn test_whitespace_trimming() {
        let parsed = ParsedDataSource::parse("  tcp:myserver , 1433  ", false).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.protocol_parameter, "1433");
    }

    // ========== Action Chain Generation Tests ==========

    #[test]
    fn test_action_chain_simple_tcp() {
        // Simple TCP with explicit port
        let parsed = ParsedDataSource::parse("tcp:myserver,1433", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have single ConnectTcp action
        assert_eq!(chain.len(), 1);
        let actions = chain.actions();
        assert!(matches!(
            actions[0],
            crate::connection::connection_actions::ConnectionAction::ConnectTcp { .. }
        ));

        // Verify description is reasonable
        let desc = chain.describe();
        assert!(desc.contains("myserver"));
        assert!(desc.contains("TCP"));
    }

    #[test]
    fn test_action_chain_named_instance_with_ssrp() {
        // Named instance without port requires SSRP
        let parsed = ParsedDataSource::parse("myserver\\SQLEXPRESS", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have: CheckCache -> QuerySsrp -> UpdateCache -> ConnectTcpFromSlot
        assert_eq!(chain.len(), 4);
        let actions = chain.actions();

        use crate::connection::connection_actions::ConnectionAction;
        assert!(matches!(actions[0], ConnectionAction::CheckCache { .. }));
        assert!(matches!(actions[1], ConnectionAction::QuerySsrp { .. }));
        assert!(matches!(actions[2], ConnectionAction::UpdateCache { .. }));
        assert!(matches!(
            actions[3],
            ConnectionAction::ConnectTcpFromSlot { .. }
        ));

        // Verify metadata
        let metadata = chain.metadata();
        assert_eq!(metadata.server_name, "myserver");
        assert_eq!(metadata.instance_name, "SQLEXPRESS");
        assert!(!metadata.explicit_protocol);
    }

    #[test]
    fn test_action_chain_explicit_protocol_no_cache() {
        // Explicit port disables caching
        let parsed = ParsedDataSource::parse("myserver,1433", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have single ConnectTcp action (no cache check)
        assert_eq!(chain.len(), 1);
        let actions = chain.actions();
        assert!(matches!(
            actions[0],
            crate::connection::connection_actions::ConnectionAction::ConnectTcp { .. }
        ));
    }

    #[test]
    #[cfg(windows)]
    fn test_action_chain_named_pipe() {
        // Named pipe with explicit path
        let parsed = ParsedDataSource::parse("np:\\\\myserver\\pipe\\sql\\query", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have single ConnectNamedPipe action
        assert_eq!(chain.len(), 1);
        let actions = chain.actions();
        assert!(matches!(
            actions[0],
            crate::connection::connection_actions::ConnectionAction::ConnectNamedPipe { .. }
        ));

        let metadata = chain.metadata();
        assert!(metadata.explicit_protocol);
    }

    #[test]
    fn test_action_chain_protocol_waterfall() {
        // No explicit protocol - should use waterfall
        let parsed = ParsedDataSource::parse("myserver", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have: CheckCache -> TrySequence
        assert_eq!(chain.len(), 2);
        let actions = chain.actions();

        use crate::connection::connection_actions::ConnectionAction;
        assert!(matches!(actions[0], ConnectionAction::CheckCache { .. }));

        if let ConnectionAction::TrySequence {
            actions: waterfall, ..
        } = &actions[1]
        {
            // Should have TCP at minimum, possibly more on Windows
            assert!(!waterfall.is_empty());

            // At least one should be TCP
            let has_tcp = waterfall
                .iter()
                .any(|a| matches!(a, ConnectionAction::ConnectTcp { .. }));
            assert!(has_tcp, "Waterfall should include TCP");
        } else {
            panic!("Expected TrySequence action for protocol waterfall");
        }
    }

    #[test]
    fn test_action_chain_parallel_connect() {
        // MultiSubnetFailover should use parallel connect
        let parsed = ParsedDataSource::parse("myserver,1433", true).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have TCP connection (parallel logic is in the action)
        assert_eq!(chain.len(), 1);
        assert!(chain.metadata().explicit_protocol);

        let desc = chain.describe();
        assert!(desc.contains("Explicit protocol: true"));
    }

    #[test]
    #[cfg(windows)]
    fn test_action_chain_localdb() {
        // LocalDB should resolve then connect via named pipe
        let parsed = ParsedDataSource::parse("(localdb)\\MSSQLLocalDB", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // Should have: ResolveLocalDb -> ConnectNamedPipeFromSlot
        assert_eq!(chain.len(), 2);
        let actions = chain.actions();

        use crate::connection::connection_actions::ConnectionAction;
        assert!(matches!(
            actions[0],
            ConnectionAction::ResolveLocalDb { .. }
        ));
        assert!(matches!(
            actions[1],
            ConnectionAction::ConnectNamedPipeFromSlot { .. }
        ));

        let metadata = chain.metadata();
        assert_eq!(metadata.instance_name, "MSSQLLocalDB");
    }

    #[test]
    fn test_action_chain_admin_dac() {
        // Admin protocol should use DAC connection
        let parsed = ParsedDataSource::parse("admin:localhost", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // DAC doesn't use cache (SQL Browser doesn't support DAC resolution)
        // Should have single ConnectDac action
        assert_eq!(chain.len(), 1);
        let actions = chain.actions();

        use crate::connection::connection_actions::ConnectionAction;
        assert!(matches!(actions[0], ConnectionAction::ConnectDac { .. }));
    }

    #[test]
    #[cfg(windows)]
    fn test_action_chain_shared_memory() {
        // LPC protocol should use shared memory
        let parsed = ParsedDataSource::parse("lpc:.", false).unwrap();
        let chain = parsed.to_connection_actions(15000);

        // LPC with no instance enables caching, so: CheckCache -> ConnectSharedMemory
        assert_eq!(chain.len(), 2);
        let actions = chain.actions();

        use crate::connection::connection_actions::ConnectionAction;
        assert!(matches!(actions[0], ConnectionAction::CheckCache { .. }));
        assert!(matches!(
            actions[1],
            ConnectionAction::ConnectSharedMemory { .. }
        ));
    }

    #[test]
    fn test_action_chain_timeout_propagation() {
        // Verify timeout is propagated to actions
        let parsed = ParsedDataSource::parse("tcp:myserver,1433", false).unwrap();
        let chain = parsed.to_connection_actions(30000); // 30 second timeout

        assert_eq!(chain.metadata().timeout_ms, 30000);

        // Verify action contains the timeout
        let actions = chain.actions();
        if let crate::connection::connection_actions::ConnectionAction::ConnectTcp {
            timeout_ms,
            ..
        } = &actions[0]
        {
            assert_eq!(*timeout_ms, 30000);
        } else {
            panic!("Expected ConnectTcp action");
        }
    }

    #[test]
    fn empty_datasource_returns_error() {
        assert!(ParsedDataSource::parse("", false).is_err());
        assert!(ParsedDataSource::parse("   ", false).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_default_instance() {
        let parsed = ParsedDataSource::parse("np:\\\\myserver\\pipe\\sql\\query", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.server_name, "myserver");
        assert!(parsed.instance_name.is_empty());
        assert!(parsed.standard_instance_name);
        assert!(!parsed.can_use_cache);
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_named_instance() {
        let parsed =
            ParsedDataSource::parse("np:\\\\myserver\\pipe\\mssql$inst1\\sql\\query", false)
                .unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.server_name, "myserver");
        assert_eq!(parsed.instance_name, "inst1");
        assert!(parsed.standard_instance_name);
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_custom_path() {
        let parsed = ParsedDataSource::parse("np:\\\\myserver\\pipe\\custom\\path", false).unwrap();
        assert_eq!(parsed.protocol_name, "np");
        assert_eq!(parsed.instance_name, "pipecustom\\path");
        assert!(!parsed.standard_instance_name);
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_dot_server_becomes_localhost() {
        let parsed = ParsedDataSource::parse("np:\\\\.\\pipe\\sql\\query", false).unwrap();
        assert_eq!(parsed.server_name, "localhost");
        assert_eq!(parsed.original_server_name, ".");
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_invalid_path() {
        assert!(ParsedDataSource::parse("np:\\\\server", false).is_err());
    }

    #[test]
    fn port_on_non_tcp_protocol_is_error() {
        // On non-Windows, np: itself is unsupported. Use admin: instead.
        assert!(ParsedDataSource::parse("admin:myserver,1433", false).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn parallel_connect_incompatible_protocols() {
        assert!(ParsedDataSource::parse("np:\\\\srv\\pipe\\sql\\query", true).is_err());
        assert!(ParsedDataSource::parse("admin:myserver", true).is_err());
    }

    #[test]
    fn parallel_connect_defaults_to_tcp() {
        let parsed = ParsedDataSource::parse("myserver", true).unwrap();
        assert_eq!(parsed.protocol_name, "tcp");
        assert!(!parsed.can_use_cache);
    }

    #[test]
    fn admin_protocol_disables_cache() {
        let parsed = ParsedDataSource::parse("admin:myserver", false).unwrap();
        assert_eq!(parsed.protocol_name, "admin");
        assert!(!parsed.can_use_cache);
    }

    #[cfg(windows)]
    #[test]
    fn lpc_with_instance_disables_cache() {
        let parsed = ParsedDataSource::parse("lpc:localhost\\myinst", false).unwrap();
        assert_eq!(parsed.protocol_name, "lpc");
        assert!(!parsed.can_use_cache);
    }

    #[test]
    fn ipv6_address_not_treated_as_protocol() {
        let parsed = ParsedDataSource::parse("::1", false).unwrap();
        assert!(parsed.protocol_name.is_empty() || parsed.protocol_name == "tcp");
        assert!(parsed.server_name.contains("::1"));
    }

    #[test]
    fn default_parsed_datasource() {
        let ds = ParsedDataSource::default();
        assert!(ds.server_name.is_empty());
        assert!(ds.protocol_name.is_empty());
        assert!(ds.can_use_cache);
    }
}

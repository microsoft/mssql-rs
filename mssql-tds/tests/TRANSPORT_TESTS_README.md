# Transport Protocol Tests

This directory contains integration tests for different SQL Server transport protocols.

## Test Files

- **test_transport_protocols.rs**: Comprehensive tests for TCP, Named Pipe, and Shared Memory protocols

## Transport Protocols

### TCP (Cross-platform)
- Standard network protocol
- Works on all platforms (Windows, Linux, macOS)
- Default protocol for remote connections
- Format: `host:port` (e.g., `localhost:1433`)

### Named Pipes (Windows only)
- Windows inter-process communication mechanism
- Can be used for both local and remote connections
- Formats:
  - Default instance: `\\server\pipe\sql\query` or `\\.\pipe\sql\query` (local)
  - Named instance: `\\server\pipe\MSSQL$INSTANCE\sql\query` or `\\.\pipe\MSSQL$INSTANCE\sql\query` (local)

### Shared Memory (Windows only, local only)
- Fastest protocol for local connections
- Uses Named Pipes internally with special path
- Formats:
  - Default instance: Empty instance name or "MSSQLSERVER"
  - Named instance: Instance name (e.g., "SQLEXPRESS")
- Internally uses pipe format: `\\.\pipe\sql\query` or `\\.\pipe\MSSQL$INSTANCE\sql\query`

## Environment Variables

The tests require the following environment variables (typically set in `.env` file):

> Note: nextest runs each test binary with its crate directory as the working directory, so
> `mssql-tds/.env` takes precedence over a workspace-root `.env`. Real environment variables
> always win over `dotenv`, so exporting them in the shell is the most reliable way to point a
> run at a specific endpoint.

```bash
# Required for all tests
DB_USERNAME=your_username
SQL_PASSWORD=your_password

# TCP tests
DB_HOST=localhost           # Server hostname or IP
DB_PORT=1433               # Server port (optional, defaults to 1433)

# Named Pipe and Shared Memory tests (Windows only)
DB_INSTANCE=SQLEXPRESS     # Instance name (optional, defaults to default instance)

# Encryption settings
TRUST_SERVER_CERTIFICATE=true
CERT_HOST_NAME=localhost   # Optional: hostname in certificate

# Debugging
ENABLE_TEST_TRACE=false         # Set to true for detailed logging
```

## Running Tests

### All Tests (Linux/macOS - TCP only)
```bash
cargo test --package mssql-tds --test test_transport_protocols
```

### All Tests (Windows - TCP, Named Pipe, Shared Memory)
```bash
cargo test --package mssql-tds --test test_transport_protocols
```

### Specific Protocol Tests

#### TCP Tests (all platforms)
```bash
cargo test --package mssql-tds --test test_transport_protocols test_tcp_connection
```

#### Named Pipe Tests (Windows only)
```bash
# All Named Pipe tests
cargo test --package mssql-tds --test test_transport_protocols test_named_pipe

# Specific tests
cargo test --package mssql-tds --test test_transport_protocols test_named_pipe_default_instance
cargo test --package mssql-tds --test test_transport_protocols test_named_pipe_named_instance
cargo test --package mssql-tds --test test_transport_protocols test_named_pipe_local_default_instance
cargo test --package mssql-tds --test test_transport_protocols test_named_pipe_local_named_instance
```

#### Shared Memory Tests (Windows only)
```bash
# All Shared Memory tests
cargo test --package mssql-tds --test test_transport_protocols test_shared_memory

# Specific tests
cargo test --package mssql-tds --test test_transport_protocols test_shared_memory_default_instance
cargo test --package mssql-tds --test test_transport_protocols test_shared_memory_named_instance
cargo test --package mssql-tds --test test_transport_protocols test_shared_memory_mssqlserver_instance
```

#### Unit Tests
```bash
# Test TransportContext helper methods
cargo test --package mssql-tds --test test_transport_protocols test_transport_context
```

## Setting Up SQL Server for Testing

### Windows Local Testing

1. **Install SQL Server** with Named Pipes and Shared Memory enabled
   - Download from: https://www.microsoft.com/en-us/sql-server/sql-server-downloads
   - During installation or via SQL Server Configuration Manager, ensure:
     - Named Pipes protocol is enabled
     - Shared Memory protocol is enabled
     - TCP/IP protocol is enabled

2. **Enable protocols** via SQL Server Configuration Manager:
   ```
   SQL Server Configuration Manager
   → SQL Server Network Configuration
   → Protocols for [INSTANCE]
   → Right-click each protocol → Enable
   ```

3. **Restart SQL Server** service after enabling protocols

4. **Create test user** (if needed):
   ```sql
   CREATE LOGIN testuser WITH PASSWORD = 'YourPassword123!';
   GO
   GRANT VIEW SERVER STATE TO testuser;
   GO
   ```

5. **Configure environment variables**:
   ```bash
   DB_HOST=localhost
   DB_USERNAME=testuser
   SQL_PASSWORD=YourPassword123!
   DB_INSTANCE=SQLEXPRESS           # Or empty for default instance
   TRUST_SERVER_CERTIFICATE=true
   ```

### Verifying Protocol Availability

You can check which protocols are enabled by running:

```sql
-- Check network protocols
SELECT * FROM sys.dm_exec_connections WHERE session_id = @@SPID;

-- Check server properties
SELECT * FROM sys.configurations WHERE name LIKE '%protocol%';
```

## Troubleshooting

### Named Pipes

**Error: "Could not connect to Named Pipe"**
- Ensure Named Pipes protocol is enabled in SQL Server Configuration Manager
- Restart SQL Server service after enabling
- Check Windows Firewall settings
- Verify pipe path format is correct

**Error: "Access Denied"**
- Ensure user has appropriate permissions
- Check if SQL Server is running under correct service account

### Shared Memory

**Error: "Could not connect via Shared Memory"**
- Ensure Shared Memory protocol is enabled
- Shared Memory only works for local connections
- Verify instance name is correct
- For default instance, use empty string or "MSSQLSERVER"

### General

**Error: "Environment variable not set"**
- Create a `.env` file in the project root with required variables
- Alternatively, export environment variables before running tests

**Connection timeout**
- Increase timeout in connection string (not yet implemented in these tests)
- Check if SQL Server is running: `services.msc` → SQL Server (INSTANCE)

**TLS/Certificate errors**
- Set `TRUST_SERVER_CERTIFICATE=true` for local testing
- For production, configure proper certificates

## Test Coverage

The test suite covers:

- ✅ TCP connections (default protocol)
- ✅ Named Pipe connections (remote server)
- ✅ Named Pipe connections (named instance)
- ✅ Named Pipe connections (local default instance)
- ✅ Named Pipe connections (local named instance)
- ✅ Shared Memory connections (default instance)
- ✅ Shared Memory connections (named instance)
- ✅ Shared Memory connections (explicit MSSQLSERVER)
- ✅ TransportContext helper methods (get_server_name, is_local, get_protocol)

## Future Enhancements

- [ ] Add connection string parsing tests
- [ ] Add protocol fallback tests (try protocols in order)
- [ ] Add SQL Browser (SSRP) integration tests
- [ ] Add performance comparison between protocols
- [ ] Add error handling tests for invalid pipe paths
- [ ] Add timeout tests
- [ ] Add concurrent connection tests

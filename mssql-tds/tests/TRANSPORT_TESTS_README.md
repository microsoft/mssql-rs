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
DB_DATABASE=UserDb         # Optional generally; common helpers default to master

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

### Float16 vector integration tests

Float32 vector tests run normally. Every float16 live test (including mixed float32/float16
cases and explicit V1 rejection tests) is ignored by default. Explicit opt-in requires both
nextest `--run-ignored ignored-only` with the `test(vector16)` filter and `VECTOR16_TEST=1`.
If selected without that exact environment value, the test returns early with a diagnostic;
it does not attempt a connection or probe server support. With opt-in enabled, missing
credentials, incorrect targets, unsupported features, and SQL errors fail the test rather
than being treated as skips.

Set an explicit `DB_DATABASE` and point `DB_HOST`/`DB_PORT` at a server with float16
vector support. The helper verifies the current database matches that configuration.
It does not require a particular developer instance or database name.

For local PR365 validation, use only instance `SQLSERVERVECTOR` and database `UserDb`:

- Set `DB_DATABASE=UserDb` explicitly. The general helper defaults to `master`; the
   float16 helper rejects an omitted or empty database before connecting.
- Point `DB_HOST` and `DB_PORT` at the TCP endpoint for `SQLSERVERVECTOR`. TCP uses the
   explicit port; setting `DB_INSTANCE` alone does not select a named instance for TCP.
- Set `VECTOR16_EXPECTED_INSTANCE=SQLSERVERVECTOR` to verify the instance on each
   float16 connection. This optional safeguard accepts any configured instance name.
- Set `DB_INSTANCE=SQLSERVERVECTOR` for any Named Pipe/Shared Memory tests in the same run.
- Have an administrator preprovision preview support in `UserDb` on a server build that
   supports float16 vectors and Vector feature V2. Tests never enable preview features,
   change database configuration, set global flags, or issue `USE` statements.
- Verify the actual instance name and current database with a read-only query before the
   local run. Temporary test tables/procedures are session-local; no persistent schema is created.

All float16 live integration test names contain `vector16`, including the mixed-type cases.
After setting the environment above, run:

```bash
cargo nextest run -p mssql-tds --test test_vector_type --test test_bulk_copy_vector --run-ignored ignored-only -E 'test(vector16)' --success-output immediate
```

Run the same two binaries without `--run-ignored ignored-only` and the filter to run float32
regression tests while leaving float16 tests ignored. V1
float16 RPC rejection tests cover both values and typed NULLs; they still require opt-in
and the configured endpoint. Negotiation/wire unit tests do not need a live server.

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

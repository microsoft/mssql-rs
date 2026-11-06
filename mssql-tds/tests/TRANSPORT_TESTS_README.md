# Transport Protocol Tests

This directory contains integration tests for different SQL Server transport protocols.

## Test Files

- **test_transport_protocols.rs**: Comprehensive tests for TCP, Named Pipe, and Shared Memory protocols

## Transport Protocols

### TCP (Cross-platform)
- Standard network protocol
- Works on all platforms (Windows, Linux, macOS)
- Default protocol for remote connections
- Datasource format: `tcp:host,port` (e.g., `tcp:localhost,1433`)

### Named Pipes (Windows only)
- Windows inter-process communication mechanism
- Can be used for both local and remote connections
- Formats:
  - Default instance: `\\server\pipe\sql\query` or `\\.\pipe\sql\query` (local)
  - Named instance: `\\server\pipe\MSSQL$INSTANCE\sql\query` or `\\.\pipe\MSSQL$INSTANCE\sql\query` (local)

### Shared Memory (Windows only, local only)
- Datasource format: `lpc:.` for the default local instance
- The current implementation uses local Named Pipes internally

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
DB_DATABASE=UserDb         # Only read by the float16 helper; other tests always use master

# TCP tests
DB_HOST=localhost           # Server hostname or IP
DB_PORT=1433               # Server port (optional, defaults to 1433)

# Encryption settings
TRUST_SERVER_CERTIFICATE=false
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

Requirements:

- Set `DB_DATABASE` explicitly. The general helper defaults to `master`; the float16
   helper rejects an omitted or empty database before connecting.
- Point `DB_HOST` and `DB_PORT` at the TCP endpoint of a float16-capable server. TCP uses
   the explicit port; setting `DB_INSTANCE` alone does not select a named instance for TCP.
- Optionally set `VECTOR16_EXPECTED_INSTANCE` to verify the instance name on each float16
   connection. This safeguard accepts any configured instance name.
- Have an administrator preprovision preview support in the target database on a server
   build that supports float16 vectors and Vector feature V2. Tests never enable preview
   features, change database configuration, set global flags, or issue `USE` statements.
- Verify the actual instance name and current database with a read-only query before the
   run. Temporary test tables/procedures are session-local; no persistent schema is created.

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

From the repository root, after configuring the environment:

```bash
cargo nextest run -p mssql-tds --test test_transport_protocols
```

Named Pipe and Shared Memory cases are Windows-only. Filter the same test binary
by protocol or test name:

```bash
cargo nextest run -p mssql-tds --test test_transport_protocols -E 'test(test_tcp)'
cargo nextest run -p mssql-tds --test test_transport_protocols -E 'test(named_pipe)'
cargo nextest run -p mssql-tds --test test_transport_protocols -E 'test(shared_memory)'
```

Tests using Strict encryption return early when
`TRUST_SERVER_CERTIFICATE=true`; that flag cannot bypass Strict validation.
To exercise those cases, trust the server certificate and leave the flag false.
Named Pipe and Shared Memory cases in this binary target the default instance;
`DB_INSTANCE` does not redirect them. LocalDB cases target `MSSQLLocalDB` and
use Windows integrated authentication.

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

4. **Create a test login** as an administrator (if needed):
   ```sql
   CREATE LOGIN testuser WITH PASSWORD = 'YourPassword123!';
   GO
   ```

5. **Configure environment variables**:
   ```bash
   DB_HOST=localhost
   DB_USERNAME=testuser
   SQL_PASSWORD=YourPassword123!
   TRUST_SERVER_CERTIFICATE=false
   ```

### Verifying Protocol Availability

Check the transport used by the current connection:

```sql
SELECT CONNECTIONPROPERTY('net_transport') AS net_transport;
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
- For the default instance, use `lpc:.`

### General

**Error: "Environment variable not set"**
- Create a `.env` file in the project root with required variables
- Alternatively, export environment variables before running tests

**Connection timeout**
- These tests construct `ClientContext` directly; connection-string timeout
  settings do not configure them.
- Check if SQL Server is running: `services.msc` → SQL Server (INSTANCE)

**TLS/Certificate errors**
- Trust the server certificate and check `CERT_HOST_NAME`.
- `TRUST_SERVER_CERTIFICATE=true` permits local TDS 7.4 `Encryption=On` tests
  with an untrusted certificate, but skips the Strict cases.

## Test Coverage

The binary covers TCP, remote/local default-instance Named Pipes, the local
Shared Memory path, and LocalDB. It includes Strict and TDS 7.4 encrypted
connections plus ten concurrent connections for each local Named Pipe and
Shared Memory path. It does not test named-instance selection, protocol fallback,
or `TransportContext` helper methods.

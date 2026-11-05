# Tracing in mssql-js

This document describes how to configure and use tracing in the `mssql-js` library, including all supported environment variables and their behaviors.

## Environment Variables

### `MSSQLJS_TRACE`

- **Type:** `bool` (`true` or `false`)
- **Default:** `false`
- **Description:**
  - Enables or disables tracing.
  - If not set or set to `false`, tracing is disabled.
  - If set to `true`, tracing is enabled and the other tracing environment variables are considered.

### `MSSQLJS_TRACE_OUTPUTS`

- **Type:** `string` (comma-separated list: `file`, `console`)
- **Default:** `console`
- **Description:**
  - Specifies where trace output is sent.
  - Supported values: `file`, `console`, or both (e.g. `file,console`).
  - Unknown values are ignored with a warning.
  - If neither `file` nor `console` is specified, tracing is not enabled and a warning is printed.
  - **Security Note:** File logging is disabled by default. Console output (stdout) is the recommended and secure default.

### `MSSQLJS_TRACE_DIR`

- **Type:** `string` (directory path)
- **Default:** None (must be explicitly set for file logging)
- **Description:**
  - **REQUIRED** if `file` output is enabled in `MSSQLJS_TRACE_OUTPUTS`.
  - Specifies the directory where the trace log file (`mssqljs_trace.log`) will be written.
  - If the directory does not exist, the library will attempt to create it.
  - **Security Requirements:**
    - You **MUST** explicitly set this variable to enable file logging.
    - It is **strongly recommended** to avoid temporary directories (`/tmp`, `/var/tmp`, system temp).
    - Use a secure, application-controlled directory with proper permissions.
    - Examples: `/var/log/myapp`, `/app/logs`, `C:\ProgramData\MyApp\logs`
  - **Security Rationale:**
    - Trace logs may contain sensitive information (connection strings, passwords, query parameters, authentication tokens).
    - Temporary directories are often world-readable and inappropriate for sensitive data.
    - Explicit directory specification ensures conscious security decisions.
  - If an insecure path is detected, a warning will be printed but logging will proceed.

### `MSSQLJS_TRACE_LEVEL`

- **Type:** `string` (log level or filter expression)
- **Default:** `info`
- **Description:**
  - Sets the minimum log level for tracing output. Accepts standard levels: `error`, `warn`, `info`, `debug`, `trace`.
  - You can also use advanced filter expressions supported by [`tracing_subscriber::EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html).
  - If set to an invalid value, a warning is printed to the console and the log level falls back to `info`.

## Behaviors

- **Trace Outputs:**
  - Controlled by `MSSQLJS_TRACE_OUTPUTS`.
  - `file`: Write logs to a file (requires `MSSQLJS_TRACE_DIR` to be set).
  - `console`: Write logs to the console (stdout). **This is the default and recommended approach.**
  - Both can be enabled simultaneously.
  - If neither is enabled, tracing is not set up and a warning is printed.

- **Trace File Location:**
  - If `MSSQLJS_TRACE_DIR` is set, the log file is written to that directory as `mssqljs_trace.log`.
  - If `MSSQLJS_TRACE_DIR` is not set, file logging is **disabled** with an error message.
  - If `MSSQLJS_TRACE_DIR` points to an insecure location (temp directories), a **warning** is printed but logging proceeds.

- **Directory Creation:**
  - If the target directory does not exist, the library attempts to create it.
  - If directory creation fails, an error is printed and file logging is disabled.

- **File Creation Failure:**
  - If the log file cannot be created (e.g., due to permissions), an error is printed to stderr and file logging is disabled.
  - If `console` was requested, logs continue on stdout. Otherwise, a failed file-only configuration falls back to stderr, including when `MSSQLJS_TRACE_DIR` is missing or its directory cannot be created.
  - Protect stderr captures as well as file logs: a failed file destination does not disable tracing.

- **Security Warnings:**
  - File logging to temporary directories (`/tmp`, `/var/tmp`, Windows temp) generates a **warning**.
  - File logging without explicit `MSSQLJS_TRACE_DIR` is **disabled**.
  - Warnings alert users to potential security risks while allowing intentional use of temporary directories when needed.

- **Invalid Trace Level:**
  - If `MSSQLJS_TRACE_LEVEL` is set to an invalid value, a warning is printed to the console and the log level falls back to `info`.

- **Unknown Trace Output:**
  - If `MSSQLJS_TRACE_OUTPUTS` contains an unknown value, a warning is printed and that value is ignored.

- **Thread Safety:**
  - Tracing is initialized only once, even if `connect` is called multiple times.

## Security Considerations

**⚠️ IMPORTANT: Trace logs may contain sensitive information!**

Trace logs can include:
- Database connection strings (potentially with embedded credentials)
- SQL query text (which may contain sensitive data)
- Authentication tokens and passwords
- Application data passed as query parameters
- Internal network topology and configuration

### Best Practices

1. **Use Console Output (Default):**
   - The default `console` output is recommended for most deployments.
   - Let your container orchestration (Docker, Kubernetes) or log management system handle log collection.
   - This avoids file permission issues and insecure storage.

2. **Secure File Logging:**
   - Only enable file logging when necessary.
   - **Always** use a dedicated, application-controlled directory with restricted permissions.
   - **Never** use temporary directories (`/tmp`, `/var/tmp`, system temp).
   - Set appropriate file permissions (e.g., `chmod 600` or `700` on Unix).

3. **Production Environments:**
   - Disable tracing in production unless actively debugging.
   - If enabled, ensure logs are protected by access controls.
   - Consider log rotation and secure deletion policies.
   - Implement log scrubbing/redaction if needed.

4. **Compliance:**
   - Be aware of data protection regulations (GDPR, HIPAA, etc.).
   - Trace logs containing personal data may require specific handling.
   - Document your logging practices in security policies.

### Paths with Security Warnings

File logging to these locations will generate security warnings:
- `/tmp/*` (Unix/Linux)
- `/var/tmp/*` (Unix/Linux)
- System temp directory (any platform)
- `C:\Temp\*` or `C:\Windows\Temp\*` (Windows)

Logging will still proceed, but warnings remind you of the security implications.

## Example Usage

```sh
# Enable tracing with console output only (default, recommended for most use cases)
export MSSQLJS_TRACE=true

# Console only (explicit) - Recommended for Docker/Kubernetes
export MSSQLJS_TRACE=true
export MSSQLJS_TRACE_OUTPUTS=console

# File logging requires explicit secure directory
export MSSQLJS_TRACE=true
export MSSQLJS_TRACE_OUTPUTS=file
export MSSQLJS_TRACE_DIR=/var/log/myapp  # Must not be /tmp or system temp

# Both file and console
export MSSQLJS_TRACE=true
export MSSQLJS_TRACE_OUTPUTS=file,console
export MSSQLJS_TRACE_DIR=/var/log/myapp

# Set log level to debug
export MSSQLJS_TRACE=true
export MSSQLJS_TRACE_LEVEL=debug

# On Windows:
set MSSQLJS_TRACE=true
set MSSQLJS_TRACE_OUTPUTS=console,file
set MSSQLJS_TRACE_DIR=C:\ProgramData\MyApp\logs
set MSSQLJS_TRACE_LEVEL=trace

# Docker/Kubernetes example (console only, platform handles log collection)
docker run -e MSSQLJS_TRACE=true -e MSSQLJS_TRACE_OUTPUTS=console myapp

# This will work but generate security warnings (not recommended):
export MSSQLJS_TRACE=true
export MSSQLJS_TRACE_OUTPUTS=file
export MSSQLJS_TRACE_DIR=/tmp  # WARNING: Insecure path, but will proceed
```

## Troubleshooting

- **No log file is created:**
  - Check that `MSSQLJS_TRACE` is set to `true`.
  - Ensure `MSSQLJS_TRACE_OUTPUTS` includes `file`.
  - Ensure the directory specified by `MSSQLJS_TRACE_DIR` exists and is writable, or that its parent allows directory creation.
  - Check for warnings in the console about file creation failures.

- **No logs at expected level:**
  - Check the value of `MSSQLJS_TRACE_LEVEL`.
  - If an invalid value is set, a warning will be printed and the log level will default to `info`.

- **No logs at all:**
  - Ensure at least one valid output (`file` or `console`) is specified in `MSSQLJS_TRACE_OUTPUTS`.
  - Check for warnings about unknown outputs.

## References

- [tracing crate documentation](https://docs.rs/tracing)
- [tracing-subscriber EnvFilter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)

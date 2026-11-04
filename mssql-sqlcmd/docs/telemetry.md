# Telemetry

`mssql-sqlcmd` defines a small, exporter-independent OpenTelemetry metrics
surface for native sqlcmd. The global meter is a no-op unless the host installs
an approved meter provider, so this library does not send telemetry by itself.

## Schema

The instrumentation scope is `mssql-sqlcmd`. Metric and property names use the
`sqlcmdcli_` prefix expected by the downstream product telemetry schema.

| Metric | Meaning |
|---|---|
| `sqlcmdcli_invocation` | A sqlcmd process invocation |
| `sqlcmdcli_completion` | A completed invocation |

Both metrics include:

- `sqlcmdcli_schema_version`: `1`
- `sqlcmdcli_version`: the native sqlcmd version, when it is a bounded
  version-like value
- `sqlcmdcli_os`: Rust's target operating-system identifier
- `sqlcmdcli_arch`: Rust's target architecture identifier
- `sqlcmdcli_implementation`: `mssql-sqlcmd-rust`
- `sqlcmdcli_runtime`: `native`

Completion adds `sqlcmdcli_outcome`, one of `succeeded`, `failed`, `canceled`,
`invalid_invocation`, or `unknown`. The raw exit code is not recorded.

The API does not accept arbitrary properties. It never records query text or
results, command-line arguments, connection strings, credentials, server,
host, IP, database or user names, filesystem paths, error messages, or other
free-form values. A version containing whitespace, path separators, or other
unexpected characters is omitted.

## Integration boundary

Native sqlcmd can call
`mssql_sqlcmd_telemetry_record_invocation` once after it has resolved its
version and `mssql_sqlcmd_telemetry_record_completion` once when the process
outcome is known. Both calls catch Rust panics and return no status, so
telemetry cannot change command output, behavior, or exit status.

The approved Application Insights exporter and the mapping from these
OpenTelemetry metrics to DataX `AppEvents` are not yet defined. This crate does
not add a network exporter or claim that an OTLP route reaches DataX. A device
identifier is also intentionally absent because no approved algorithm or
existing repository implementation is available.

# sqlcmd Application Insights Telemetry

## Status

`mssql-sqlcmd` exports one privacy-safe `sqlcmdcli_usage` event per native
sqlcmd run when `APPLICATIONINSIGHTS_CONNECTION_STRING` contains a valid
Application Insights connection string. Missing or invalid configuration is a
no-op and never changes sqlcmd output or exit behavior.

The connection string is supplied only at runtime. It must not be hardcoded,
logged, committed, or copied into diagnostics.

## Data flow

```mermaid
flowchart LR
    Native[Native sqlcmd]
    ABI[mssql-sqlcmd C ABI]
    Event[OpenTelemetry ai.custom event]
    Exporter[Application Insights exporter]
    Filter[AppEvents-only HTTP filter]
    AI[Application Insights AppEvents]

    Native --> ABI --> Event --> Exporter --> Filter --> AI
```

The exporter maps an OpenTelemetry span event named `ai.custom` to an
Application Insights custom event when the event contains
`ai.customEvent.name`. The implementation sets that attribute to
`sqlcmdcli_usage`.

The exporter also always creates an Application Insights
`RemoteDependency` envelope for the enclosing OpenTelemetry span. The HTTP
client intentionally removes that envelope from the compressed exporter
payload before transmission. Tests inspect the final request body and require
exactly one `Microsoft.ApplicationInsights.Event` envelope and no
`Microsoft.ApplicationInsights.RemoteDependency` envelope.

The exporter has no documented option to retain a custom event while
suppressing its enclosing span envelope. Filtering the exporter's serialized
HTTP request is the least invasive supported integration: it keeps
`Exporter::new_from_connection_string` and the exporter's event mapping and
serialization, without forking the crate or duplicating its private envelope
model.

## Event schema

The event name is `sqlcmdcli_usage`. It contains only these bounded properties:

| AppEvents property | Value |
|---|---|
| `sqlcmdcli_schema_version` | `1` |
| `sqlcmdcli_version` | Native sqlcmd version, when it passes validation |
| `sqlcmdcli_os` | Rust target operating-system identifier |
| `sqlcmdcli_arch` | Rust target architecture identifier |
| `sqlcmdcli_implementation` | `mssql-sqlcmd-rust` |
| `sqlcmdcli_runtime` | `native` |

The version is omitted unless it is 1-64 ASCII bytes containing only letters,
digits, `.`, `-`, `+`, or `_`.

The transport rejects an event with any other custom property, requires the
exact event envelope and name, and removes all exporter-generated envelope
tags, including automatic SDK and operation attributes, before transmission.

There is no completion event and no successful-connection event. The model
does not include diagnose, JSON, `devdeviceid`, or human/agent classification.

## Privacy contract

The API is typed and does not accept an arbitrary property map. It never
collects or exports:

- query text or results
- command-line arguments
- connection strings
- credentials or tokens
- server, host, IP, database, or user names
- filesystem paths
- raw error messages
- arbitrary free-form strings
- device identifiers

## Configuration and failure behavior

Configuration is optional and explicit through
`APPLICATIONINSIGHTS_CONNECTION_STRING`. The Rust exporter is constructed with
`Exporter::new_from_connection_string`. The HTTP client uses two-second connect
and request timeouts.

Initialization, queueing, export, filtering, flush, and shutdown are best
effort. Errors are available only as debug tracing and do not write to sqlcmd's
normal output. Rust panics are caught at the C boundary.

The queue and export batch are bounded at two items and normal export is
deferred until shutdown, so filtering, compression, HTTP, and response errors
can be reflected in the shutdown result. An all-filtered or malformed request
is an error rather than a synthetic success. For partial-ingestion responses,
filtered response indices are translated to the original exporter batch and
the original received/accepted counts are preserved for correct retry
semantics.

Native sqlcmd must call shutdown after recording the event. Shutdown uses the
caller's timeout capped at one second; after the bound expires, sqlcmd continues
exiting. The HTTP request timeout is two seconds so the one-second process-exit
bound remains authoritative.

## C ABI

```c
void mssql_sqlcmd_telemetry_record_usage(
    MssqlSqlcmdText sqlcmd_version);

int32_t mssql_sqlcmd_telemetry_shutdown(uint32_t timeout_ms);
```

`sqlcmd_version.data` may be null. A null, empty, oversized, or invalid value is
omitted while the usage event remains valid.

Native sqlcmd should call `mssql_sqlcmd_telemetry_record_usage` exactly once per
process run, then call `mssql_sqlcmd_telemetry_shutdown` during final process
cleanup. Both functions are fail-open no-ops when telemetry is not configured.
Shutdown returns `0` when completed, unconfigured, or already started; `6` when
the bounded wait expires; and `3` for a provider failure or caught panic.

## Dependency review

The transport uses `opentelemetry`, `opentelemetry_sdk`,
`opentelemetry-application-insights`, `opentelemetry-http`, and reqwest with the
repository's native-TLS convention. The Application Insights exporter is a
community crate and explicitly states that it is not an official Microsoft
product. Shipping approval for that dependency remains an owning-team decision.
The implementation has been validated only with in-memory HTTP clients and
synthetic connection strings; it does not claim live DataX delivery.

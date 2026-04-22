# Plan: SQL Browser Support in mssql-rs

## TL;DR

Implement SQL Server Browser (SSRP) support in `mssql-rs` so that connections using `server\instance` format resolve the TCP port via the SQL Browser service on UDP port 1434. The architecture in mssql-rs already has action chain plumbing (`QuerySsrp`, `CheckCache`, `UpdateCache`, `ConnectTcpFromSlot`) and stub functions in `ssrp.rs` — the work is to implement the UDP protocol with sender IP validation, wire it into the connection provider, add a process-lifetime instance cache, support configurable timeouts, and implement DAC port resolution.

---

## Design: How msodbcsql Resolves Instance Names (Reverse Engineered)

### SSRP Protocol (SQL Server Resolution Protocol)

**Overview**: When a connection string specifies `server\instance` without an explicit port, the driver must query the SQL Server Browser service (listening on UDP port 1434) to discover which TCP port the named instance is listening on.

### Request Format — CLNT_UCAST_INST (0x04)

| Offset | Size | Value |
|--------|------|-------|
| 0 | 1 byte | `0x04` (CLNT_UCAST_INST) |
| 1 | N+1 bytes | ASCII instance name, null-terminated |

Example for instance "SQLEXPRESS": `[0x04, 'S', 'Q', 'L', 'E', 'X', 'P', 'R', 'E', 'S', 'S', 0x00]`

### Response Format — SVR_RESP (0x05)

| Offset | Size | Value |
|--------|------|-------|
| 0 | 1 byte | `0x05` (SVR_RESP) |
| 1 | 2 bytes | Response size (little-endian u16) |
| 3 | 1 byte | Version |
| 4-13 | 10 bytes | Reserved |
| 14+ | N bytes | Semicolon-delimited ASCII protocol string |

Response string format: `ServerName;InstanceName;IsClustered;Version;tcp;PORT;np;\\server\pipe\path;...`

### DAC Request — CLNT_UCAST_DAC (0x0F)

| Offset | Size | Value |
|--------|------|-------|
| 0 | 1 byte | `0x0F` (CLNT_UCAST_DAC) |
| 1 | 1 byte | Protocol version = 1 |
| 2 | N+1 | ASCII instance name, null-terminated |

DAC Response: 6 bytes — `[0x05, 0x06, 0x00, 0x01, port_lo, port_hi]`

### Connection Flow

```
"server\instance" in datasource
    → ParsedDataSource::parse() extracts server + instance
    → needs_ssrp() returns true (no explicit port)
    → to_connection_actions() builds: [CheckCache, QuerySsrp, UpdateCache, ConnectTcpFromSlot]
    → execute_action_chain() processes actions:
        1. CheckCache → look up "server\instance" in process-lifetime cache
        2. If miss → QuerySsrp → UDP to server:1434, parse SVR_RESP, extract TCP port
        3. UpdateCache → store server\instance → port
        4. ConnectTcpFromSlot → TCP connect to server:resolved_port
        5. Normal TDS login7 handshake
```

### Key Behaviors from msodbcsql

1. **DNS multi-address**: Resolve server hostname → up to 64 IP addresses; send SSRP request to each
2. **Timeout**: 1000ms default for SSRP, hard cap 5000ms for all addresses to respond
3. **Response validation**: Minimum 15 bytes, first byte must be 0x05, size field must match actual size
4. **Protocol filtering**: Response may contain tcp/np/sm — filter to supported protocols
5. **Port is "untrusted"**: Mark SSRP-resolved ports as untrusted (from network, not user-specified)
6. **MultiSubnetFailover + instance = error**: MSF is not supported with named instances in msodbcsql
7. **Error SNIE_26**: "Error Locating Server/Instance Specified" when SSRP fails

---

## Design: Architecture for mssql-rs Implementation

### Module Structure

```
mssql-tds/src/
  ssrp.rs                          # SSRP protocol: UDP send/recv, parse, constants, inline #[cfg(test)] mock helpers
  connection/
    connection_actions.rs           # QuerySsrp, CheckCache, UpdateCache actions and ExecutionContext
    client_context.rs               # TransportContext enum, ssrp_timeout_ms option
    datasource_parser.rs            # needs_ssrp(), to_connection_actions(), MSF+instance validation
    instance_cache.rs               # NEW: Process-lifetime cache for instance→port
  connection_provider/
    tds_connection_provider.rs      # Action chain execution with resolve_ssrp() method
```

### Key Design Decisions

1. **UDP with Tokio**: Use `tokio::net::UdpSocket` for async UDP — consistent with the rest of the async codebase
2. **Multi-address resolution**: Resolve hostname to all IPs (max 64), send SSRP to each via separate IPv4/IPv6 sockets, take first valid response (mirrors msodbcsql behavior)
3. **Cache scope**: Process-lifetime `std::sync::RwLock<HashMap>` with `OnceLock` singleton — no extra dependencies. Cache key is `"server\instance"` lowercase
4. **No broadcast/multicast**: Unlike msodbcsql, skip CLNT_BCAST_EX and IPv6 multicast for now — only unicast CLNT_UCAST_INST (covers 99% of use cases)
5. **Inline test mocks**: SSRP tests will use lightweight UDP mock helpers in `ssrp.rs` `#[cfg(test)]` modules rather than a separate mock crate module — simpler, no cross-crate test dependency
6. **Sender IP validation**: `recv_from` will validate that UDP responses come from expected server IPs to mitigate spoofing
7. **No panics in library code**: All fallible operations will use `ok_or_else()` / `?` propagation instead of `.expect()` or `.unwrap()`

---

## User Stories & PRs

### US1: SSRP Protocol Core — UDP Request/Response

**Goal**: Implement the SSRP wire protocol in `ssrp.rs` — sending CLNT_UCAST_INST and parsing SVR_RESP responses.

**Scope**:
- Define protocol constants (`CLNT_UCAST_INST`=0x04, `SVR_RESP`=0x05, `CLNT_UCAST_DAC`=0x0F, `SSRP_PORT`=1434, `DEFAULT_SSRP_TIMEOUT_MS`=1000, `MAX_SSRP_ADDRESSES`=64)
- Implement `build_instance_request(instance: &str) -> Vec<u8>` — encode CLNT_UCAST_INST packet
- Implement `parse_ssrp_response(buf: &[u8]) -> TdsResult<SsrpResponse>` — validate 3-byte header (marker + LE u16 size, min 15 bytes), parse semicolon-delimited protocol string into `SsrpInstanceInfo` structs
- Implement `query_browser()` — async UDP query that resolves DNS to all addresses, creates separate IPv4/IPv6 sockets, sends to each, races responses via `futures::select_all` with timeout
- Public API: `get_instance_info(server, instance)` (default timeout) and `get_instance_info_ext(server, instance, timeout_ms)` (configurable timeout)
- Sender IP validation: `recv_from` will check that responses originate from expected server IPs, discarding spoofed datagrams in a loop
- Send failure tracking: if all UDP sends fail, return `ConnectionError` immediately rather than waiting for timeout
- Unit tests for packet encoding/decoding (no network needed) + localhost UDP round-trip test using inline mock helpers in `#[cfg(test)]`

**Files to modify**:
- `mssql-tds/src/ssrp.rs` — replace stubs with real implementation

> **Note**: US1, US2, and US3 will be combined into a single PR since they form the minimum viable SSRP feature.

---

### US2: SSRP Test Infrastructure

**Goal**: Build test infrastructure for SSRP without requiring a live SQL Server — using inline mock UDP helpers within `ssrp.rs`.

**Scope**:
- Inline `#[cfg(test)]` module in `mssql-tds/src/ssrp.rs` with mock UDP response helpers
- Mock helper will bind a localhost UDP socket (port 0), respond to CLNT_UCAST_INST with a configurable SVR_RESP payload
- Test coverage: successful resolution, unknown instance (timeout), malformed response rejection, request packet encoding, response parsing edge cases
- Tests will use `get_instance_info_ext()` pointed at `127.0.0.1:<mock_port>` to exercise the full async UDP flow

**Files to modify**:
- `mssql-tds/src/ssrp.rs` — add `#[cfg(test)]` module with mock helpers and tests

> **Note**: A `MockSqlBrowser` struct in `mssql-mock-tds` was considered but will not be used — inline test helpers are simpler and avoid cross-crate test dependencies. All SSRP protocol tests will live inline in `ssrp.rs`.

---

### US3: Wire SSRP into Connection Provider

**Goal**: Remove the "not implemented" error and execute QuerySsrp action in `execute_action_chain()`, enabling `server\instance` connections.

**Scope**:
- In `tds_connection_provider.rs`: remove the `requires_ssrp()` → error block
- Extract `resolve_ssrp()` async method: takes `&ConnectionActionChain` and `&mut ExecutionContext`, calls `ssrp::get_instance_info()`, finds TCP port from results, stores via `store_outcome(ActionOutcome::SsrpResolved { port })`
- `ConnectTcpFromSlot` will read the resolved port from `ExecutionContext` and create `TransportContext::Tcp`
- Error handling: SSRP failure → `ConnectionError("Error Locating Server/Instance Specified")`; no TCP protocol in response → `ProtocolError`; use `.ok_or_else()` instead of `.expect()` to avoid panics in library code
- Update test `test_instance_name_without_port_returns_error` to verify error message contains "browser"/"instance"/"locating"

**Files to modify**:
- `mssql-tds/src/connection_provider/tds_connection_provider.rs` — action chain execution, `resolve_ssrp()` method
- `mssql-tds/src/connection/connection_actions.rs` — `store_outcome(ActionOutcome::SsrpResolved)` handling
- `mssql-tds/tests/test_no_protocol_resolution.rs` — update test expectations

---

### US4: Connection Cache for Instance Resolution

**Goal**: Implement the process-lifetime cache so repeated connections to the same instance skip SSRP queries.

**Scope**:
- New module `mssql-tds/src/connection/instance_cache.rs`
- `InstanceCache` struct: `RwLock<HashMap<String, CachedInstance>>` with `CachedInstance { port: u16, created_at: Instant }`
- Global singleton via `std::sync::OnceLock` — no extra dependencies required
- TTL: default 5 minutes (prevent stale ports after instance restart)
- In `execute_action_chain()`: check cache before SSRP query; on hit, store result via `store_outcome(ActionOutcome::CacheHit { port })` which will populate both `CachedConnectionInfo` and `ResolvedPort` slots
- After successful SSRP resolution, update cache with resolved port
- Cache invalidation: `invalidate()` method (gated behind `#[cfg(test)]` initially)
- Cache key: `cache_key()` will lowercase `"server\instance"` for case-insensitive matching
- Unit tests: insert/get, miss, invalidate, TTL expiry, overwrite refresh

**Files to modify**:
- `mssql-tds/src/connection/instance_cache.rs` — new file
- `mssql-tds/src/connection_provider/tds_connection_provider.rs` — cache check/update in `execute_action_chain()`
- `mssql-tds/src/connection/connection_actions.rs` — `store_outcome(ActionOutcome::CacheHit)` handling

---

### US5: Configurable SSRP Timeout and MSF Validation

**Goal**: Add configurable SSRP timeout and validate MultiSubnetFailover + instance name constraint.

**Scope**:
- Add `ssrp_timeout_ms: Option<u64>` to `ClientContext` (connection option), defaults to `None` (falls back to `DEFAULT_SSRP_TIMEOUT_MS` = 1000ms)
- Pass timeout through `resolve_ssrp()` → `get_instance_info_ext()` for per-connection control
- Validate: `MultiSubnetFailover=true` + named instance → `ConnectionError` matching msodbcsql SNIE_48 ("Connecting to a SQL Server instance ... is not supported when using MultiSubnetFailover")
- MSF + instance + explicit port will be allowed (only reject when SSRP resolution would be needed)
- Tests: `test_msf_with_named_instance_rejected`, `test_msf_with_instance_and_explicit_port_allowed`

**Files to modify**:
- `mssql-tds/src/connection/client_context.rs` — add `ssrp_timeout_ms` field
- `mssql-tds/src/connection_provider/tds_connection_provider.rs` — pass timeout to SSRP, add MSF+instance validation
- `mssql-tds/tests/test_no_protocol_resolution.rs` — new test cases

> **Note**: Multi-address DNS resolution and the 64-address cap are already implemented in US1's `query_browser()`.

---

### US6: DAC (Dedicated Admin Connection) Port Resolution

**Goal**: Replace the `get_admin_port()` stub with a working implementation for admin connections.

**Scope**:
- Implement `get_admin_port(server, instance)` and `get_admin_port_ext(server, instance, timeout_ms)` — send CLNT_UCAST_DAC (0x0F), parse 6-byte response
- `parse_dac_response(buf)` will validate: exactly 6 bytes, SVR_RESP marker (0x05), expected size field (0x0006), protocol version 1, non-zero port
- Will reuse the same `send_and_receive_first()` UDP infrastructure as instance resolution (sender IP validation, multi-address, timeout)
- Tests: valid DAC parse, wrong length, bad marker byte, zero port rejected, live mock UDP round-trip

**Files to modify**:
- `mssql-tds/src/ssrp.rs` — implement `get_admin_port()`, `get_admin_port_ext()`, `parse_dac_response()`

---

## Implementation Milestones & PR Structure

### PR 1: Core SSRP + Connection Wiring (US1 + US2 + US3)
- Branch: `dev/saurabh/ssrp-browser-support` → `development`
- Combines protocol implementation, test infrastructure, and connection provider wiring into one PR since they form the minimum viable feature
- **Gate**: All SSRP packet tests pass; `server\instance` connections will resolve via UDP; `cargo btest` clean

### PR 2: Instance Cache (US4)
- Branch: `dev/saurabh/ssrp-instance-cache` → stacked on PR 1
- **Gate**: Cache hit/miss/TTL/invalidation tests pass; `cargo btest` clean

### PR 3: Timeout Configuration + MSF Validation (US5)
- Branch: `dev/saurabh/ssrp-timeout-multiaddr` → stacked on PR 2
- **Gate**: Timeout passthrough works; MSF+instance rejected; `cargo btest` clean

### PR 4: DAC Port Resolution (US6)
- Branch: `dev/saurabh/ssrp-dac` → stacked on PR 3
- **Gate**: DAC port resolution tests pass; `cargo btest` clean

---

## Relevant Files

### mssql-tds (modify)
- `mssql-tds/src/ssrp.rs` — Protocol constants, packet encode/decode (`build_instance_request`, `build_dac_request`), response parsing (`parse_ssrp_response`, `parse_dac_response`), async UDP queries (`get_instance_info`, `get_instance_info_ext`, `get_admin_port`, `get_admin_port_ext`, `query_browser`), sender IP validation, inline `#[cfg(test)]` mock helpers
- `mssql-tds/src/connection_provider/tds_connection_provider.rs` — `execute_action_chain()` with `resolve_ssrp()` extracted method, cache check/update, MSF+instance validation, SSRP timeout passthrough
- `mssql-tds/src/connection/connection_actions.rs` — `ActionOutcome::SsrpResolved`, `ActionOutcome::CacheHit`, `ExecutionContext` slot management
- `mssql-tds/src/connection/client_context.rs` — `ssrp_timeout_ms: Option<u64>` field
- `mssql-tds/src/connection/datasource_parser.rs` — `needs_ssrp()`, `to_connection_actions()`

### mssql-tds (new)
- `mssql-tds/src/connection/instance_cache.rs` — `InstanceCache` with `RwLock<HashMap>` + `OnceLock` singleton, TTL, `cache_key()` lowercasing

### mssql-tds (test files)
- `mssql-tds/src/ssrp.rs` (`#[cfg(test)]` module) — All SSRP protocol unit tests and mock UDP round-trip tests
- `mssql-tds/tests/test_no_protocol_resolution.rs` — Instance name connection tests, MSF+instance validation tests

### msodbcsql (reference only — do not modify)
- `Sql/Common/DK/sni/src/ssrp.cpp` — SSRP protocol reference: `SsrpGetInfo()`, `GetAdminPort()`, `ParseSsrpString()`, `SsrpSocket::Open()`
- `Sql/Common/DK/sni/src/open.cpp` — Connection flow reference: `MakeProtocolList()`, error codes SNIE_26/28/42/43

## Verification

1. `cargo bfmt` — formatting check passes
2. `cargo bclippy` — no warnings (clippy -D warnings)
3. `cargo btest` — all unit + integration tests pass via nextest
4. Protocol tests: packet encoding/decoding, response parsing (valid, malformed, edge cases), DAC response validation
5. UDP round-trip tests: inline mock helpers will respond to SSRP queries on localhost; verify `get_instance_info_ext()` and `get_admin_port_ext()` end-to-end
6. Connection flow test: `server\instance` without port will trigger SSRP → connection attempt with resolved port
7. Negative tests: unknown instance → `ConnectionError`; timeout → `ConnectionError`; malformed response → rejected; MSF+instance → `ConnectionError` (SNIE_48)
8. Cache tests: insert/get, miss, invalidate, TTL expiry, overwrite refresh
9. Sender validation: UDP responses from unexpected IPs will be discarded

## Decisions

- **Unicast only (no broadcast/multicast)**: Skip CLNT_BCAST_EX and IPv6 multicast — these are for SQL Server enumeration, not instance resolution. Can be added later if needed.
- **No Named Pipes resolution**: SSRP responses include NP paths, but mssql-rs currently only supports TCP on non-Windows. Parse and store NP info in `SsrpInstanceInfo` but only use TCP for connection.
- **Cache is process-lifetime**: Not persisted to disk. Simpler and avoids stale state across process restarts.
- **Instance names are case-insensitive**: Cache keys will be lowercased before lookup/store.
- **`RwLock<HashMap>` + `OnceLock` over DashMap**: Avoids adding a dependency for a simple use case. `RwLock` is sufficient since contention is low (cache is checked once per connection, not in a hot loop).
- **Inline test mocks over `mssql-mock-tds` module**: SSRP tests will bind a localhost UDP socket and respond with crafted packets. This is simpler than maintaining a `MockSqlBrowser` struct in the mock crate and avoids cross-crate test coupling.
- **Sender IP validation**: `recv_from` + `HashSet<IpAddr>` validation will mitigate UDP response spoofing — datagrams from unexpected source IPs are discarded in a loop.
- **No panics in library code**: All fallible operations will use `ok_or_else()` / `?` propagation instead of `.expect()` or `.unwrap()`.

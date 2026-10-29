# Connection and decoding behavior

Detailed behavior for SDK and driver authors using
[`mssql-tds`](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/README.md).

## Connection policy and migration

Eligible TCP connections default to MultiSubnetFailover (MSF): attempt all
resolved addresses immediately and use the first successful connection. There
is no Transparent Network IP Resolution (TNIR) first-address phase. One address
is attempted once; zero addresses or more than 64 is an error with MSF on.
MSF off attempts the complete address list sequentially, respecting
`IPAddressPreference`. Parallel mode does not prioritize an address family.
Parallel attempts share the TCP-connect budget, including DNS; sequential
attempts retain their per-address budgets. Both remain inside the existing
login deadline. Cancelling a parallel race aborts its remaining socket tasks.

Precedence is **explicit connection > application startup default > deployment
environment > built-in true**. Set `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT` to
case-insensitive `true` or `false` before starting the application. Other values
are errors unless a higher-priority setting overrides them; the variable is
never reread after the first connection attempt or default lookup. Rust
applications can instead call
`connection::multi_subnet_failover::set_default_multi_subnet_failover(false)`
before first use. Late calls fail. Set this before opening pools; separately
loaded native modules have separate startup state, so use the environment to
configure multiple bindings together. Changing the environment or a default
does not change an established connection, a recovery, or a secondary login.

The Rust `ClientContext::multi_subnet_failover` field changed from `bool` to
`Option<bool>`:

| Value | Meaning |
|---|---|
| `None` | Inherit startup defaults |
| `Some(false)` | Explicit opt-out; sequential TCP |
| `Some(true)` | Explicit MSF; incompatible options are errors |

An inherited on default is suppressed for named pipes, shared memory, LocalDB,
DAC (`admin:`), a nonempty failover partner, or a named instance requiring SQL
Browser (SSRP). Those retain their existing transport/discovery paths.
`tcp:host\instance,port` is eligible because discovery is unnecessary. On Windows,
a local target without a protocol or port (`localhost`, `.`, `(local)`, loopback
IP, or the machine name) also suppresses inherited MSF. Local aliases are
case-insensitive, including `(Local)` and `(LOCAL)`. This retains the
shared-memory-first waterfall, so a local instance with TCP/IP disabled remains
reachable. An explicit per-connection MSF-on value, `tcp:`, or a port still selects
TCP; application and environment defaults do not force local connections onto it.
On other platforms, or for non-local targets, bare names without discovery select
TCP when MSF is on. When TCP is selected, local aliases (`localhost`, `.`, `(local)`)
remain `localhost` for resolution and certificate hostname validation rather than
becoming the machine hostname.
With certificate hostname validation enabled, connecting by IP requires a
matching IP SAN or an explicit expected hostname (`host_name_in_cert` /
ODBC `HostnameInCertificate`). MSF does not relax TLS validation.
This does not implement database-mirroring failover.

`TdsClient::multi_subnet_failover_policy()` reports the captured value, source,
and any applicability suppression; the same fields appear in connection
tracing. Cloned contexts preserve this policy. A caller can deliberately change
the explicit override on a secondary context before opening a new connection;
it cannot bypass the new endpoint's applicability checks.

| Surface | Explicit opt-out | Application startup default |
|---|---|---|
| Rust | `context.multi_subnet_failover = Some(false)` | `set_default_multi_subnet_failover(false)?` |
| ODBC | `MultiSubnetFailover=No` or `SQL_COPT_SS_MULTISUBNET_FAILOVER=OFF` | Set the process environment before driver use |
| Native Python, sync/async | `{"multi_subnet_failover": False}` (also `"No"`) | `mssql_py_core.set_default_multi_subnet_failover(False)` |
| Node.js | `multiSubnetFailover: false` | `setDefaultMultiSubnetFailover(false)` |
| CLI | `--multi-subnet-failover false` | Environment |

Native Python reads `multi_subnet_failover` and
`transparent_network_ip_resolution` from the context dictionary passed to
`PyCoreConnection` or `PyAsyncConnection.connect`. These are dictionary options,
not an ODBC connection-string parser; higher-level clients must forward them.
Check the consuming SDK's mapper before relying on either setting.

ODBC attributes outrank keywords. Omitted native options remain inherited;
native Python also treats `multi_subnet_failover=None` as omission, not false.
Native Python accepts case-insensitive `"Yes"`/`"No"` strings as well as booleans;
other MSF values are errors.
Legacy TNIR off/`Disabled` inputs in ODBC and native Python are accepted no-ops;
on/`Enabled` is unsupported, and invalid values fail validation. **Disabling
TNIR does not disable MSF.** See the
[parity decisions](https://github.com/microsoft/mssql-rs/blob/main/mssql-odbc/docs/parity-deviations.md) for the intentional
differences from msodbcsql and outstanding approval/evidence.

## DNS fallback caching

LOGIN7 advertises DNS caching (`0x0B`). Each TCP connection keeps its resolved
addresses until login completes with LOGINACK and a successful DONE. The last
FEATUREEXTACK then decides whether to publish those addresses in the process-wide
fallback cache: DNS bit 0 allows them; a missing or negative DNS acknowledgement
removes the host. A malformed FEATUREEXTACK revokes caching after that accepted
response is drained, then returns its parsing error.

Responses without FEATUREEXTACK leave an earlier allowance unchanged, including
gateway redirects. Rejected, truncated, and authentication-continuation responses
also leave it unchanged, so a failed retry cannot discard its own fallback.
Pending resolutions belong to their connection, not a shared host-limited map.
Concurrent accepted verdicts take effect in completion order and publish each
connection's own addresses. Cached IPv6 addresses retain their scope IDs.

Every connect still attempts DNS first. Fallback is used only if the resolver
returns an error before the connection deadline, including a resolver-reported
timeout. A resolver that remains pending until the caller's deadline expires
does **not** get extra time for a cached TCP attempt. Reserving an earlier DNS
sub-deadline for fallback is not implemented; caching does not guarantee that
every form of DNS outage can be recovered within the login timeout.

The deliberate differences from msodbcsql's eager cache are recorded in the
[ODBC parity registry](https://github.com/microsoft/mssql-rs/blob/main/mssql-odbc/docs/parity-deviations.md).

## Execution stack usage

Command execution keeps session-recovery state in a separately boxed future,
allocated only when a dead connection needs recovery. Healthy commands do not
pay for that allocation or carry the login state inline. This reduces caller
stack use for synchronous adapters such as ODBC on musl's small worker stacks,
without moving execution to another thread or changing cancellation and timeout
handling.

Connection establishment boxes the action-chain future once per connection.
This keeps transport attempts, TLS/login, and client construction out of the
caller's future, leaving room for the Driver Manager and native TLS on small
application stacks. The connection is still polled on the calling thread with
the same cancellation and login deadline; no worker-thread handoff is introduced.

## SQL string decoding

`SqlString::decode` and `to_utf8_string` use the encoding supplied by TDS metadata,
not a byte-order mark (BOM) inferred from the value. Leading BOM-shaped bytes are
data: UTF-16LE `FF FE` remains U+FEFF, `FE FF` remains U+FFFE without switching to
big-endian decoding, and Windows-1252 `EF BB BF` remains U+00EF U+00BB U+00BF.

This behavior also applies to decoded column strings in the `mssql-js` row writer
and the `mssql-py-core` cursor, not just ODBC. Applications using those bindings
can therefore receive leading characters that were previously stripped or decoded
under the wrong encoding. Malformed-sequence handling is unchanged; preserving
raw unpaired UTF-16 units in ODBC `SQL_C_WCHAR` buffers is a separate behavior.

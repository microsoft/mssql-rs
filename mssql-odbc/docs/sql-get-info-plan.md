# SQLGetInfoW support

## Scope and work items

This document records the complete `SQLGetInfoW` surface in mssql-odbc: what is
implemented, what intentionally differs from msodbcsql, what remains, and how
the behavior is tested. The owning user story is
[AB#46381](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46381),
**mssql-odbc | Driver info**.

| Work item | State | SQLGetInfo scope |
|---|---|---|
| [AB#47086](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47086) | Closed | First-release support for the 21 information types then blocking mssql-python. |
| [AB#48149](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/48149) | Active | 61 mssql-python payload names representing 59 distinct information IDs. `SQL_OWNER_USAGE` aliases `SQL_SCHEMA_USAGE`; `SQL_QUALIFIER_USAGE` aliases `SQL_CATALOG_USAGE`. |
| [AB#47996](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47996) | New | Complete the remaining 68 ODBC 3.x public information types and residual parity work. |

[AB#46406](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/46406),
`SQLGetTypeInfoW`, is another closed child of AB#46381. It is deliberately
outside this document because it returns a result set rather than an
`SQLGetInfoW` scalar value.

The current implementation therefore covers the original loader and
transaction information, AB#47086, and AB#48149. It is not yet the complete
ODBC 3.x public surface; unsupported residual identifiers remain owned by
AB#47996.

## Compatibility evidence

Parity evidence has two sources:

- Classic source: `Sql/Ntdbms/sqlncli/odbc/sqlcinfo.cpp` in the msodbcsql repo.
- Runtime measurement: retail msodbcsql18 18.6.2.1,
  `SQL_DRIVER_VER=18.06.0002`, through the comparison leg of
  `tests/e2e/tests/get_info_test.cpp`.

The source and retail build disagree on two values. Runtime behavior is the
compatibility contract: `SQL_OUTER_JOINS="F"`,
and `SQL_CONCAT_NULL_BEHAVIOR=SQL_CB_NULL`. The statement, character-literal,
and binary-literal limits all track the packet size as `128 * packet_size`,
using the requested/configured packet size (from `SQLSetConnectAttr`,
`PacketSize=`, or `DEFAULT_PACKET_SIZE`) both before and after connecting —
never the TDS-negotiated value. Source reading supports this for msodbcsql:
`SQLGetConnectAttr` and `SQLGetInfo` both read the same `dwOptions` slot the
LOGIN7 request was built from (`sqlcconn.cpp:3326`, `sqlcmisc.cpp:3465`,
`sqlcinfo.cpp:1186`), and nothing in msodbcsql writes the ENVCHANGE-negotiated
size back into that slot — the negotiated value only resizes msodbcsql's
internal TDS buffer (`TdsHlp.cpp: BATCHCTX::NewPacketSize`), a detail
invisible to the ODBC API in the unencrypted case. Runtime measurement over
an *encrypted* connection contradicts that for retail msodbcsql18 18.6.2.1,
though: requesting `PacketSize=16384` reports back 16192, a smaller,
TLS-driven reduction the static source reading above didn't surface. This
driver has no such reduction (proven independent of negotiation by the
`Encrypt=no` mock-server unit test in `driver_connect.rs`), so the two
diverge whenever `PacketSize=` asks for more than an encrypted session
allows — see the divergence table below.
Retail's 524288 example is for its default 4096-byte packet; this driver's
`DEFAULT_PACKET_SIZE` is 8000, so an unconnected or default-configured handle
reports 1024000 instead — a real, unavoidable divergence whenever the
configured size differs from retail's default, tracked here rather than
silently matching retail's number.

A `0` (msodbcsql's "let the connection pick its own default" sentinel, from
either `SQLSetConnectAttr(SQL_ATTR_PACKET_SIZE, 0)` or `PacketSize=0`) is
exempt from the clamp on both the source and this driver, on both the
attribute and connection-string-keyword paths — `sqlcconn.cpp:1639-1642`
stores the keyword's value into the same `dwOptions` slot the attribute
writes, verbatim, with no clamp of its own — and both then report the same
`128 * 0 = 0` for the three limits above rather than a resolved default.
This driver matches that: `state.packet_size`/`ConnectionParams::packet_size`
stay `0` through `SQLGetConnectAttr`/`SQLGetInfo` while disconnected, and only
`ClientContext` (which cannot hold a literal `0`) resolves to its own default
at connect time.

## Capability ledger

Capability masks describe this driver rather than copying features that only
msodbcsql implements. These are observable implementation gaps, not permanent
policy decisions. The global deliberate-deviation list therefore does not
claim them as such.

| Information type | mssql-odbc answer | Difference and owner |
|---|---:|---|
| `SQL_ASYNC_MODE` | `SQL_AM_NONE` | msodbcsql advertises statement async. Planned Phase 15 in `plan.md`. |
| `SQL_DYNAMIC_CURSOR_ATTRIBUTES1/2` | `0` | Dynamic cursors are not implemented. Planned Phase 10 in `plan.md`. |
| `SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1` | `SQL_CA1_NEXT` | Only next-oriented forward fetch is implemented. Additional cursor operations belong to Phase 10. |
| `SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2` | `SQL_CA2_READ_ONLY_CONCURRENCY \| SQL_CA2_MAX_ROWS_SELECT` | Reports only implemented concurrency and `SQL_ATTR_MAX_ROWS`; the broader msodbcsql mask belongs to Phase 10. |
| `SQL_KEYSET_CURSOR_ATTRIBUTES1/2` | `0` | Keyset cursors are not implemented. Planned Phase 10. |
| `SQL_STATIC_CURSOR_ATTRIBUTES1/2` | `0` | Static cursors are not implemented. Planned Phase 10. |
| `SQL_BOOKMARK_PERSISTENCE` | `0` | Bookmark fetch and persistence are not implemented. Planned Phase 10. |
| `SQL_CURSOR_SENSITIVITY` | `SQL_UNSPECIFIED` | No sensitive cursor implementation. Planned Phase 10. |
| `SQL_SCROLL_OPTIONS` | `SQL_SO_FORWARD_ONLY` | Only forward-only cursors are implemented. Planned Phase 10. |
| `SQL_FETCH_DIRECTION` | `SQL_FD_FETCH_NEXT` | Deprecated identifier truthfully mirrors forward-only fetch support. Phase 10 owns additional directions. |
| `SQL_POSITIONED_STATEMENTS` | `0` | Positioned update/delete is not implemented. Planned Phase 10. |
| `SQL_SCROLL_CONCURRENCY` | `SQL_SCCO_READ_ONLY` | Deprecated identifier truthfully mirrors read-only cursor support. Phase 10 owns additional modes. |
| `SQL_STATIC_SENSITIVITY` | `0` | Deprecated identifier; static cursor changes are not implemented. Planned Phase 10. |

The remaining implemented values in the AB#48149 slice match retail
msodbcsql, including both legacy aliases. `SQL_DATABASE_NAME` is dynamic and
follows the connection's current catalog after login,
`SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)`, and server `ENVCHANGE` tokens.

## Other SQLGetInfo differences

The E2E suite also exercises differences outside the capability table. They
remain visible here so every comparison skip or weakened parity assertion has
a durable owner:

| Information type | Difference | Owner |
|---|---|---|
| `SQL_USER_NAME` | msodbcsql lazily queries `USER_NAME()` and reports the database user. This driver reports the authenticated login captured at connection time because it has no internal-query facility. | AB#47996 owns residual SQLGetInfo parity; the E2E test keeps the distinction observable. |
| `SQL_PARAM_ARRAY_ROW_COUNTS` | This driver reports `SQL_PARC_NO_BATCH`; retail msodbcsql reports `SQL_PARC_BATCH`. | Execution semantics and measurement are recorded in `docs/parameters_plan.md`. |
| Invalid/reserved ID `65000` | Both driver cores return `HY096`. The Windows Driver Manager instead answers `SQL_SUCCESS` before exposing either driver's result; Unix forwards `HY096`. | Residual validation belongs to AB#47996; the Rust unit test pins the driver core and `ReservedInfoTypeFollowsDriverManagerContract` pins the public surface. |
| `SQL_MAX_STATEMENT_LEN`, `SQL_MAX_CHAR_LITERAL_LEN`, `SQL_MAX_BINARY_LITERAL_LEN` with an explicit `PacketSize=` over an encrypted connection | Measured against retail msodbcsql18 18.6.2.1: requesting `PacketSize=16384` reports back 16192 (`SQLGetConnectAttr(SQL_ATTR_PACKET_SIZE)` and the derived `128*x` limits both reflect the reduced number), a TLS-driven reduction not visible in the static source reading above. mssql-odbc always reports the requested/configured size, never a negotiated one. | AB#47996 owns residual SQLGetInfo parity; `MaxLengthsUseTheConnectionStringPacketSize` is skipped on the msodbcsql comparison leg (`SKIP_IF_COMPARING_MSODBCSQL`) rather than asserting a value retail doesn't guarantee. |

`SQL_CURSOR_COMMIT_BEHAVIOR` predates the pipeline slices. Its deliberate
Driver Manager interaction difference is recorded in
`docs/transactions_plan.md`.

## Test inventory

Rust unit tests validate all 50 `STATIC_INFO` table entries' uniqueness and
dispatch (each is fetched through the real `SQLGetInfoW` path and compared
against its table value), an independent ODBC-spec width list that catches an
entry using the wrong `InfoValue` variant, the two aliases, dynamic
connection state, probes, truncation, diagnostic clearing, and `HY096`. The
E2E suite is what
pins the *actual* correct value and width against msodbcsql for entries the
comparison leg covers; the Rust suite alone cannot prove a value is right,
only that the code returns whatever the table says.

The public E2E suite validates every implemented name in the two mssql-python
slices, retail parity, the truthful capability ledger, identity and catalog
changes, diagnostic clearing, and string/numeric buffer contracts. The classic
regression suite at
`testsrc/ntdbms/sqlncli/ODBC/ODBCGEN/Tests/Raidpp/Raidpp.CPP` adds four
`TCSQLGetInfo` cases:

- Variation 1: negative string `BufferLength` is `HY090` (covered by unit and E2E).
- Variation 2: bookmark fetch direction agrees with bookmark persistence
  (covered by the capability table: neither is advertised).
- Variation 3: numeric information ignores `BufferLength` (covered by E2E).
- Variation 4: short strings return `SQL_SUCCESS_WITH_INFO` and `01004`
  (covered by E2E).

Variation 3 also checks `SQL_DRIVER_HDESC`, `SQL_DRIVER_HLIB`,
`SQL_DRIVER_HENV`, `SQL_DRIVER_HDBC`, and `SQL_DRIVER_HSTMT`. Those handle
information types remain in AB#47996 rather than being silently claimed here.
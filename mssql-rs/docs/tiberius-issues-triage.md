# Tiberius issue triage — historical upstream record

> This historical triage was imported from [saurabh500/mssql-tiberius-bridge](https://github.com/saurabh500/mssql-tiberius-bridge). It describes that project's status at the time, not necessarily the current driver.

Source: https://github.com/prisma/tiberius/issues (101 open issues, fetched 2026-05-09)

This document categorizes the then-open issues in `prisma/tiberius` to determine which ones might also have affected the upstream driver, which uses Microsoft's `mssql-tds` rather than tiberius's protocol implementation. For each category it notes:
- **Likely status in the upstream driver**: how it probably behaves (based on its architecture: it wraps `mssql-tds` and exposes a tiberius-compatible facade).
- **Action**: whether to file an upstream issue and attempt a fix.

> **Architecture reminder**: The upstream driver does **not** inherit tiberius's protocol bugs. Bugs in `src/tds/codec/...` of tiberius are most likely *not* present in the upstream driver because it uses a different wire-protocol implementation. However, **API-surface gaps** (missing methods, missing trait impls, ergonomics) and **feature gaps** (transactions, prepared statements, AAD variants, etc.) are very likely shared.

## Triage Legend

| Symbol | Meaning |
|--------|---------|
| 🟢 | Upstream driver unaffected — tiberius-internal bug or already handled by `mssql-tds`. No action. |
| 🟡 | Possibly affected — needs hands-on repro in the upstream driver. |
| 🔴 | Almost certainly affected — file duplicate + fix. |
| ⚪ | Not actionable — usage question, unrelated, or invalid. No action. |
| 🔵 | Feature request — file in the upstream project if aligned with its roadmap. |

---

## A. Wire-Protocol / Decoder Bugs (in tiberius's `src/tds/codec/...`)

These are bugs in tiberius's TDS implementation. The upstream driver uses `mssql-tds`, an entirely separate parser, so most are **not** present.

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 418 | Display impl for PacketSize/Database EnvChange shows old/new swapped | 🟢 | — | tiberius-internal log formatting bug. mssql-tds has its own EnvChange impl. |
| 410 | BCP failure when DATE column precedes TIME column | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | tiberius-only BCP encoder bug. The upstream driver has no `bulk_insert` yet anyway. |
| 368 | Negative Numeric Floats sign issue (`-17.-80`) | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: did we already fix this? Numeric Display in `DecimalParts`. Worth a regression test. |
| 316 | Panic reading datetime field with date < 1900 (overflow in `time` feature) | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: try with `chrono`. mssql-tds may have same overflow. |
| 325 | UTF-16 error on broken characters | ✅ | regression test for [#90](https://github.com/saurabh500/mssql-tiberius-bridge/issues/90) | `Row::get` returns U+FFFD for lone surrogates and odd-length UTF-16LE input, for both owned and borrowed strings; valid surrogate pairs are preserved. |
| 322 | Bulk insert large varchar/nvarchar fails (BCP colid 8) | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | tiberius BCP-only; the upstream driver has no bulk_insert. |
| 358 | `bulk_insert` does not support Money MS SQL data type | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | tiberius-only. Money/SmallMoney *are* readable in the upstream driver. |
| 370 | (n/a placeholder) | — | — | — |
| 211 | Panic from `try_get` (unwrap path) | ✅ | [#42](https://github.com/saurabh500/mssql-tiberius-bridge/issues/42) (PR [#43](https://github.com/saurabh500/mssql-tiberius-bridge/pull/43)) | **Repro**: upstream driver `Row::try_get` shouldn't panic on type mismatch. |
| 263 | Cannot interpret `I16(None)` as `i32` | ⚪ | upstream driver `FromSql for i32` already widens from `SmallInt`/`TinyInt`; null via `Option<i32>` | **Repro**: upstream driver `FromSql for i32` — does it accept SmallInt(None)? Likely improvement. |
| 226 | Performance: handling buffers in `plp.rs` | 🟢 | — | tiberius-only PLP decoder; mssql-tds is separate. |

## B. TLS / Encryption Issues

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 412 | TDS 8.0 and "Strict" encryption option | ✅ | [#62](https://github.com/saurabh500/mssql-tiberius-bridge/issues/62) — Strict encryption wired | Feature gap. mssql-tds may already do TDS 7.4 encryption; check if it speaks 8.0/Strict. File as feature. |
| 412 | (same) | ✅ | [#62](https://github.com/saurabh500/mssql-tiberius-bridge/issues/62) — Strict encryption wired | — |
| 327 | `UnsupportedCertVersion` with rustls | 🟢 | — | tiberius-rustls bug. The upstream driver uses native-tls. |
| 320 | TLS handshake stalls when `EncryptionLevel::Off` | 🟡 | tracked in [#89](https://github.com/saurabh500/mssql-tiberius-bridge/issues/89) | **Repro**: connect with `EncryptionLevel::Off` in the upstream driver — does it skip handshake? |
| 305 | Unencrypted traffic despite `encrypt=true` due to TLS feature flags disablement | 🟢 | — | tiberius feature-flag config bug. The upstream driver's encryption is unconditional. |
| 364 | macOS 15 + SQL Server 2014 doesn't work (rustls/vendored-openssl) | ⚪ | out-of-scope (SQL Server 2014 — unsupported) | **Repro**: try the upstream driver against SQL Server 2014. native-tls may behave differently. |
| 274 | TLS handshake EOF | 🟡 | tracked in [#89](https://github.com/saurabh500/mssql-tiberius-bridge/issues/89) | **Repro**: similar — try basic connect with the upstream driver against the user's reported config. |
| 323 | Failing to connect with TLS (rustls compile errors) | 🟢 | — | tiberius rustls feature-set issue; N/A. |
| 218 | tiberius 0.9 keep crashing on macOS | ⚪ | out-of-scope (old tiberius version) | Old version; tiberius-internal. |
| 340 | Setting `HostNameInCertificate` property | ✅ | [#47](https://github.com/saurabh500/mssql-tiberius-bridge/issues/47) (PR [#50](https://github.com/saurabh500/mssql-tiberius-bridge/pull/50)) — `Config::host_name_in_certificate` | Feature: the upstream driver could expose `Config::host_name_in_certificate`. |
| 224 | `danger_accept_invalid_hostnames` + `danger_accept_invalid_certs` | 🟡 | tracked in [#48](https://github.com/saurabh500/mssql-tiberius-bridge/issues/48) — blocked on mssql-tds | The upstream driver already has `trust_cert` (skip verify). Hostname-only skip is a separate variant. |
| 381 | openssl support (TLS 1.0 for old SQL Server) | ⚪ | [#70](https://github.com/saurabh500/mssql-tiberius-bridge/issues/70) closed wontfix — only legacy SQL Server (≤2012) needs TLS<1.2 | mssql-tds uses native-tls (which on macOS uses SecureTransport, on Linux uses openssl). May already support TLS 1.0. |

## C. Authentication

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 407 | sspi-rs for SSPI NTLM on Linux/macOS without Kerberos | 🟡 | tracked in [#66](https://github.com/saurabh500/mssql-tiberius-bridge/issues/66) | The upstream driver currently does Kerberos via libgssapi dlopen on Linux. NTLM-on-Linux is a real gap. |
| 343 | Panic in libgssapi when using Integrated Security (Rust 1.78) | ⚪ | upstream driver libgssapi already includes upstream fix | **Repro**: upstream driver's libgssapi version — verify it includes the fix referenced (estokes/libgssapi#23). |
| 283 | Build problems with `Auth::Integrated` on CI | 🟢 | — | tiberius-feature build issue. The upstream driver gates differently. |
| 276 | GSS-NTLMSSP instead of Kerberos on Linux | 🟡 | tracked in [#66](https://github.com/saurabh500/mssql-tiberius-bridge/issues/66) | Same as 407 — NTLM-via-GSSAPI. |
| 175 | Is Azure SQL/AAD supported? | ✅ | `AuthMethod::aad_token` (PR [#26](https://github.com/saurabh500/mssql-tiberius-bridge/pull/26)) | Already done — the upstream driver has `AuthMethod::aad_token` (PR #26). |
| 97 | WindowsAuth should be available in Linux | 🟡 | tracked in [#66](https://github.com/saurabh500/mssql-tiberius-bridge/issues/66) | Same theme as #407/#276. The upstream driver uses libgssapi for Kerberos. NTLM gap. |
| 333 | Connection failure with special-character password | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: try special chars in `AuthMethod::sql_server`. Likely fine since we don't ADO-parse. |

## D. Bulk Insert / BCP

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 410 | BCP date+time column-order failure | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | (covered above) |
| 358 | Money in bulk_insert | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | (covered above) |
| 352 | NTEXT bulk insert example | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | how-to; the upstream driver has no bulk_insert. |
| 322 | Large varchar/nvarchar bulk insert | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | (covered above) |
| 319 | Insert into TEXT column | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | how-to; the upstream driver can `INSERT` via `query`/`execute`. |
| 311 | Bulk Insert: Allow column names | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | Feature for when the upstream driver implements bulk_insert. |
| 307 | Bulk insert with datetime field | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | how-to / tiberius-specific. |
| 302 | Bulk Copy Options | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | Feature for future bulk_insert. |
| 373 | Date issue in bulk insert (target schema) | ✅ | via [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) (PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84)) | tiberius-only. |

> **✅ Upstream driver update**: `Client::bulk_insert` (BCP) shipped in [#53](https://github.com/saurabh500/mssql-tiberius-bridge/issues/53) via PR [#84](https://github.com/saurabh500/mssql-tiberius-bridge/pull/84) and [PR #86](https://github.com/saurabh500/mssql-tiberius-bridge/pull/86) (Apache Arrow `RecordBatch` input under the `arrow` feature). All BCP-related rows above are now covered.

## E. Type Conversion / `FromSql` / `ToSql` / `IntoSql`

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 401 | Implement `IntoSql` for `rust_decimal` | 🟡 | tracked in [#87](https://github.com/saurabh500/mssql-tiberius-bridge/issues/87) — `ToSql for rust_decimal::Decimal` missing | **Check**: upstream driver's `ToSql for Decimal`. Probably already has. |
| 277 | `IntoSql` impl missing for `time` crate | ✅ | [#67](https://github.com/saurabh500/mssql-tiberius-bridge/issues/67) — `time` crate `ToSql` | The upstream driver added chrono ToSql in PR #23; `time` crate is a separate gap. |
| 244 | `impl IntoSql<'a>` in `.bind()` method | ⚪ | — | The upstream driver has no `Query::bind` builder; N/A. |
| 257 | Can't get `geography` type | ✅ | [#69](https://github.com/saurabh500/mssql-tiberius-bridge/issues/69) | Raw native bytes via `Vec<u8>` / `&[u8]`, with `ColumnType::Geography` / `Geometry` metadata. `ColumnValues::Bytes` is preserved. |
| 354 | jiff crate support | ✅ | [#68](https://github.com/saurabh500/mssql-tiberius-bridge/issues/68) — `jiff` support | Feature. Mirror chrono/time impls. |
| 277 | (dup) | ✅ | [#67](https://github.com/saurabh500/mssql-tiberius-bridge/issues/67) — `time` crate `ToSql` | — |
| 401 | (dup) | 🟡 | tracked in [#87](https://github.com/saurabh500/mssql-tiberius-bridge/issues/87) — `ToSql for rust_decimal::Decimal` missing | — |
| 169 | Owned variants on `FromSql` for `String`/`Vec` | ✅ | [#40](https://github.com/saurabh500/mssql-tiberius-bridge/issues/40) (PR [#43](https://github.com/saurabh500/mssql-tiberius-bridge/pull/43)) | **Check**: upstream driver `FromSql<String>` / `FromSql<Vec<u8>>`. |
| 367 | `sendStringParametersAsUnicode` connection property | ✅ | [#49](https://github.com/saurabh500/mssql-tiberius-bridge/issues/49) (PR [#50](https://github.com/saurabh500/mssql-tiberius-bridge/pull/50)) | Feature: control NVARCHAR-vs-VARCHAR encoding for string params. |
| 334 | Why tinyint converted to u8? | ⚪ | — | Tiberius design — TDS TinyInt is unsigned 1-byte. Not a bug. |
| 216 | Distinguishing `Intn` for bigint vs int | ⚪ | — | how-to / FromSql usage. |
| 219 | Case insensitive `row.get()` | ✅ | [#41](https://github.com/saurabh500/mssql-tiberius-bridge/issues/41) (PR [#43](https://github.com/saurabh500/mssql-tiberius-bridge/pull/43)) — `get_ci`/`try_get_ci` | Feature gap. **Check** if the upstream driver `Row::get(name)` is case-sensitive. Probably yes. |
| 221 | f64::NAN insert into decimal column | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: send NaN as `f64` param to the upstream driver — should error gracefully, not corrupt protocol. |
| 282 | Binding string adds quotation marks (with proc) | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: call a SP via the upstream driver with a `&str` param. |
| 278 | Dynamic interaction with query results | ⚪ | — | how-to / design discussion. |

## F. Connectivity / Connection Setup

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 414 | Client hostname in `LoginMessage` | ✅ | [#46](https://github.com/saurabh500/mssql-tiberius-bridge/issues/46) (PR [#50](https://github.com/saurabh500/mssql-tiberius-bridge/pull/50)) — `Config::client_name` | **Check**: does the upstream driver send a real hostname in Login7? It likely uses `localhost`. |
| 386 | smol 2.0 incompat with `sql-browser-smol` | 🟢 | — | tiberius-runtime. The upstream driver is tokio-only. |
| 360 | Timeout connecting over VPN | ⚪ | — | Diagnostic / network. |
| 345 | Dynamic ports in Docker | ✅ | SSRP supported (PR [#27](https://github.com/saurabh500/mssql-tiberius-bridge/pull/27)) | Use SSRP / instance_name. The upstream driver already supports SSRP (PR #27). |
| 313 | "Key-value pairs must be separated by a `;`" parsing | 🟢 | — | tiberius ADO-string parser. The upstream driver has its own `Config` builder. |
| 329 | Update `tokio_rustls` 0.24 → 0.25 | 🟢 | — | tiberius dep. N/A. |
| 337 | MultiSubnetFailover support | ✅ | [#61](https://github.com/saurabh500/mssql-tiberius-bridge/issues/61) — MultiSubnetFailover | Feature. mssql-tds may have routing redirect; MSF is separate. |
| 348 | Send ReadOnlyIntent (already merged in tiberius PR #297) | ✅ | `Config::readonly` (PR [#24](https://github.com/saurabh500/mssql-tiberius-bridge/pull/24)) | The upstream driver already has `Config::readonly` (PR #24). |
| 335 | Read-only routing examples | ⚪ | — | how-to. |
| 375 | azure-sql-edge on macOS hangs | ⚪ | environment-specific (azure-sql-edge on macOS) | **Repro**: try connecting the upstream driver to an azure-sql-edge container. |
| 198 | Check if TCP connection is alive | ✅ | [#44](https://github.com/saurabh500/mssql-tiberius-bridge/issues/44) (PR [#45](https://github.com/saurabh500/mssql-tiberius-bridge/pull/45)) — `Client::ping()` | Uses cached `is_connection_dead()` status without I/O; success is not proof of server responsiveness. |
| 299 | Reset connection (`sp_reset_connection`) | ✅ | `reset_session()` implemented for [#52](https://github.com/saurabh500/mssql-tiberius-bridge/issues/52); native reset is the default pool recycling policy | Explicit `READ COMMITTED` baseline; ping-only compatibility mode remains available. |
| 301 | How do I call `ping`? | ✅ | [#44](https://github.com/saurabh500/mssql-tiberius-bridge/issues/44) — `Client::ping()` | how-to. |
| 131 | Named pipes support | ✅ | [#60](https://github.com/saurabh500/mssql-tiberius-bridge/issues/60) — Named pipe transport | Feature. mssql-tds may not support named pipes. |
| 53 | Other connection methods than TCP | ✅ | [#60](https://github.com/saurabh500/mssql-tiberius-bridge/issues/60) — Named pipe transport | Same theme. |
| 125 | COUNT() fails on GCP | ⚪ | — | Environment issue. |
| 70 | diesel + tiberius | ⚪ | — | Out of scope. |

## G. API Surface / Ergonomics

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 404 | Implement `Debug` for `ToSql` | ✅ | [#64](https://github.com/saurabh500/mssql-tiberius-bridge/issues/64) — `Debug` for `ToSql` | Trait surface improvement. |
| 402 | Implement `Eq` for `Row` and `TokenRow` | ✅ | [#65](https://github.com/saurabh500/mssql-tiberius-bridge/issues/65) — `PartialEq`/`Eq` for `Row` | For test asserts. |
| 397 | Expose `BaseMetaDataColumn` & `TypeInfo` | ✅ | [#63](https://github.com/saurabh500/mssql-tiberius-bridge/issues/63) — column metadata | The upstream driver's `Column` already exposes type/precision/scale to some extent — verify completeness. |
| 403 | `BaseMetaDataColumn` does not retrieve Identity flag | ✅ | [#63](https://github.com/saurabh500/mssql-tiberius-bridge/issues/63) — column metadata | **Check**: upstream driver's column metadata — does it carry IDENTITY/nullable/size? |
| 217 | Column nullable/size/scale enhancement | ✅ | [#63](https://github.com/saurabh500/mssql-tiberius-bridge/issues/63) — column metadata | Same theme. |
| 383 | Constructing a `Row` for tests | 🔵 | — | The upstream driver's `Row::from_schema` is `pub` already; verify usable for tests. |
| 262 | Make `QueryIdx` public | ✅ | [#39](https://github.com/saurabh500/mssql-tiberius-bridge/issues/39) (PR [#43](https://github.com/saurabh500/mssql-tiberius-bridge/pull/43)) — `ColumnIndex` public | **Check**: the upstream driver has `ColumnIndex` trait — public? |
| 258 | How to write wrapper for try_get? | ⚪ | — | how-to (related to #262). |
| 336 | `Config::trust_cert_ca` should take `Into<PathBuf>` | ✅ | `Config::trust_cert_ca` exists (PR [#22](https://github.com/saurabh500/mssql-tiberius-bridge/pull/22)) | The upstream driver already added `trust_cert_ca` (PR #22) — check signature. |
| 382 | Raw identifier prefixes (r#) should be ignored | ⚪ | N/A — the upstream driver has no `IntoRow` derive macro | `IntoRow`/derive macros — the upstream driver has none yet. |
| 30 | Statements (prepared) | 🟡 | tracked in [#56](https://github.com/saurabh500/mssql-tiberius-bridge/issues/56) — Prepared Statements | Feature: prepared statements. mssql-tds may support `sp_prepare`/`sp_execute`. |
| 28 | Transactions | 🟡 | tracked in [#55](https://github.com/saurabh500/mssql-tiberius-bridge/issues/55) — Transactions API | Feature: high-level Transaction wrapper. |
| 115 | Add Serde (de)serialization | ✅ | [#57](https://github.com/saurabh500/mssql-tiberius-bridge/issues/57) (PR [#82](https://github.com/saurabh500/mssql-tiberius-bridge/pull/82)) — Serde row deserialization | Feature: `Row` ⇒ struct via serde. |
| 54 | Column Encryption (CEK) | 🟡 | tracked in [#58](https://github.com/saurabh500/mssql-tiberius-bridge/issues/58) — Always Encrypted (CEK) | Large feature. |
| 289 | ServiceBroker / SqlDependency | 🟡 | tracked in [#59](https://github.com/saurabh500/mssql-tiberius-bridge/issues/59) — Service Broker | Large feature. |

## H. Streaming / Query Results

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 380 | `QueryStream::into_results` doesn't return correct number | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: upstream driver `QueryResult::into_results` — empty SELECTs preserved? |
| 371 | QueryStream returns 1 row, ends with more remaining | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: the upstream driver has streaming via `into_row_stream` (PR #29). |
| 365 | Cannot return `RowStream` from a function (lifetime tied to connection) | 🔵 | — | The upstream driver has the same architecture; design issue. Could be addressed via owned stream. |
| 79 | Cancel safety on futures | ✅ | [#88](https://github.com/saurabh500/mssql-tiberius-bridge/issues/88) — native dead-state guard | Dropped in-flight upstream driver I/O marks the client dead; reuse fails fast and the pool replaces it. Raw `inner_mut()` calls remain unguarded. |
| 300 | Cancel is not safe (tokio::time::timeout corrupts state) | ✅ | [#88](https://github.com/saurabh500/mssql-tiberius-bridge/issues/88) — bounded timeout regression | External future drops use fail-fast retirement, not same-connection recovery. Normal stream drops between yielded rows preserve reuse. |
| 160 | `rows_affected` length incorrect when table has trigger | ✅ | regression test in PR [#51](https://github.com/saurabh500/mssql-tiberius-bridge/pull/51) | **Repro**: upstream driver's `ExecuteResult::rows_affected`. |
| 157 | "IN" prepared statement | ⚪ | — | how-to (TDS doesn't support array params). |

## I. Logging / Diagnostics

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 281 | Opt out of INFO level logs | ⚪ | — | tracing-subscriber filter, user-side. Document. |
| 332 | How to close these logs | ⚪ | — | Same. |
| 321 | Repo status | ⚪ | — | meta. |

## J. Documentation / How-To

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 377 | Extract date using `get` | ⚪ | — | how-to. |
| 275 | How to use stored procedures | ⚪ | — | how-to. |
| 310 | CREATE SCHEMA only works in `simple_query` | ⚪ | — | TDS RPC vs SQLBatch difference. Document. |
| 236 | CREATE FUNCTION error | ⚪ | — | how-to / SQL syntax. |
| 399 | CREATE VIEW error | ⚪ | — | how-to. |
| 101 | More examples | ⚪ | — | docs. |
| 344 | SQL Server 2000 invalid token | ⚪ | out-of-scope (SQL Server 2000 — unsupported) | unsupported version. |

## K. Performance

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 294 | tiberius slower than odbc-api / C# | 🟡 | — | **Benchmark**: upstream driver vs tiberius vs ODBC. |
| 226 | plp.rs buffer handling perf | 🟢 | — | tiberius-internal. |

## L. Build / Dependency / Security Advisories

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 417 | `cargo audit` fails on rustls-webpki RUSTSEC-2026-0098/9/0104 | ✅ | [#37](https://github.com/saurabh500/mssql-tiberius-bridge/issues/37) (PR [#38](https://github.com/saurabh500/mssql-tiberius-bridge/pull/38)) — cargo-audit clean + weekly CI | **Check**: run `cargo audit` on the upstream driver. mssql-tds uses native-tls so likely clean. |
| 329 | Update tokio_rustls 0.24→0.25 | 🟢 | — | tiberius dep. |
| 323 | TLS compile errors | 🟢 | — | tiberius. |
| 317 | rustls duplicate-definition errors | 🟢 | — | tiberius. |
| 42 | Replace BytesMut/Bytes with Vec | 🟢 | — | tiberius-internal refactor. |

## M. Misc / Out of Scope

| # | Title | Status | Upstream driver | Notes |
|---|-------|--------|--------|-------|
| 199 | (closed/missing) | ⚪ | — | — |

---

## Action plan — prioritized for the upstream driver

### Tier 1 — High-confidence repros (file + likely fix)

| Tib# | Upstream driver action | Type |
|------|---------------|------|
| 414  | Send real hostname in `LoginMessage` | Feature |
| 219  | Case-insensitive `Row::get(name)` opt-in | Feature |
| 198/299/301 | `Client::ping()` / `is_connected()` | Feature |
| 262  | Ensure `ColumnIndex` is public + ergonomic for wrapping | Verify/expose |
| 401/277/354 | `IntoSql`/`ToSql` for `rust_decimal`, `time`, `jiff` | Verify + add gaps |
| 367  | `sendStringParametersAsUnicode` config knob | Feature |
| 224  | `Config::accept_invalid_hostnames` (separate from `trust_cert`) | Feature |
| 340  | `Config::host_name_in_certificate` | Feature |
| 169  | `FromSql for String` (owned) and `for Vec<u8>` (owned) | Verify |
| 417  | Run `cargo audit` on the upstream driver | Audit |

### Tier 2 — Hands-on repro needed (may or may not affect the upstream driver)

| Tib# | Repro target |
|------|--------------|
| 368  | Negative numeric Display |
| 316  | DateTime < 1900 panic |
| 325  | UTF-16 broken char |
| 211  | `try_get` panic path |
| 263  | I16(None) → i32 conversion |
| 320  | TLS handshake on `EncryptionLevel::Off` |
| 274  | TLS handshake EOF |
| 364  | macOS + SQL Server 2014 |
| 375  | azure-sql-edge on macOS |
| 380/371 | Streaming/result count |
| 300/79 | Cancel safety |
| 160  | rows_affected with triggers |
| 282  | String param to stored proc |
| 343  | libgssapi panic on Rust 1.78 |
| 333  | Special-char password |
| 221  | NaN → decimal |

### Tier 3 — Feature backlog

| Tib# | Feature |
|------|---------|
| 28   | Transactions |
| 30   | Prepared statements |
| 115  | Serde row deserialization |
| 54   | Column Encryption (CEK) |
| 289  | ServiceBroker |
| 131/53 | Named pipes / non-TCP transports |
| 337  | MultiSubnetFailover |
| 412  | TDS 8.0 / Strict encryption |
| 311/302 | bulk_insert column names + options (after bulk_insert ships) |
| 397/403/217 | Full column metadata exposure |
| 404/402 | Debug/Eq trait surface |
| 382  | r# raw-identifier handling in IntoRow derive |
| 407/276/97 | NTLM on Linux without Kerberos |

### Tier 4 — Already done in the upstream driver

- 175 / 348 — AAD auth & ReadOnly intent (PRs #26, #24).
- 336 — `trust_cert_ca` exists (PR #22) — but verify signature matches request.

### Tier 5 — Out of scope / docs / unsupported

- 281 / 332 / 321 / 70 / 125 / 70 / 344 / 218 / 313 / 386 / 417 (rustls only) / 329 / 323 / 317 / 42 / 226 / 305 / 327 / 358 / 410 / 322 / 319 / 311 / 307 / 373 / 352 / 302 (most BCP) / 244 / 334 / 216 / 278 / 377 / 275 / 310 / 236 / 399 / 101 / 157

---

## Historical execution strategy

The original upstream driver grouped these issues into Tier 1 and Tier 2 work items and
tracked reproductions and fixes in its own issue tracker.

---

_Last updated: 2026-05-10_

---

## Upstream driver issues filed (cross-reference)

The following upstream driver issues were filed from this triage to track the work.

### ✅ Shipped (closed)

| Upstream driver | PR | Tiberius | What |
|--------|----|----------|------|
| #37 | #38 | #417 | cargo-audit clean + weekly CI |
| #39 | #43 | #262 | `ColumnIndex` public |
| #40 | #43 | #169 | Owned `String`/`Vec<u8>` `FromSql` |
| #41 | #43 | #219 | Case-insensitive `Row::get_ci`/`try_get_ci` |
| #42 | #43 | #211 | `try_get` panic audit |
| #44 | #45 | #198, #299, #301 | `Client::ping()` |
| #46 | #50 | #414 | `Config::client_name` |
| #47 | #50 | #340 | `Config::host_name_in_certificate` |
| #49 | #50 | #367 | `Config::send_string_parameters_as_unicode` |
| —  | #51 | #368, #316, #160, #380, #371, #282, #221, #333 | Tier-2 repro tests |
| #53 | #84 | #311, #302, #410, #358, #322, #307, #319, #352, #373 | `Client::bulk_insert` (BCP) + options |
| #57 | #82 | #115 | Serde `Deserialize` for `Row` |
| #60 | —  | #131, #53 | Named pipe + shared-memory transport |
| #61 | —  | #337 | MultiSubnetFailover |
| #62 | —  | #412 | TDS 8.0 Strict encryption (verify wiring) |
| #63 | —  | #397, #403, #217 | Full column metadata (Identity, nullable, size, scale, collation) |
| #64 | —  | #404 | `Debug` for `ToSql` |
| #65 | —  | #402 | `PartialEq`/`Eq` for `Row` |
| #67 | —  | #277 | `time` crate `ToSql`/`IntoSql` |
| #68 | —  | #354 | `jiff` crate support |
| #74 | —  | (infra) | Strict-encryption CI test infra |
| #85 | #86 | (new) | Apache Arrow `RecordBatch` input to `bulk_insert` (`arrow` feature) |
| #90 | — | #325 | Malformed UTF-16 NVARCHAR replacement-character regression test |

### 🟡 Open tracking issues (work pending)

| Upstream driver | Tiberius | Topic |
|--------|----------|-------|
| #1  | —    | `execute()` returns 0 affected rows for DML (needs mssql-tds DONE token row count) |
| #48 | #224 | `Config::accept_invalid_hostnames` (blocked on mssql-tds) |
| #55 | #28  | Transactions API (`Client::transaction` / `Transaction` wrapper) |
| #56 | #30  | Prepared Statements (`sp_prepare` / `sp_execute`) |
| #58 | #54  | Always Encrypted (CEK) |
| #59 | #289 | Service Broker / `SqlDependency` |
| #66 | #407, #276, #97 | NTLM on Linux/macOS without Kerberos |
| #69 | #257 | `geography` / `geometry` spatial types |
| #87 | #401 | `ToSql for rust_decimal::Decimal` (confirmed missing impl) |
| #88 | #300, #79 | Cancel-safety audit under `tokio::time::timeout` |
| #89 | #320, #274 | Verify `EncryptionLevel::Off` connect doesn't stall on TLS-capable servers |

### ⚪ Not filed / closed as out-of-scope

The upstream driver supports **SQL Server 2016 and newer**. Issues that exist only to
support older releases (2014, 2012, 2008 R2, 2008, 2005, 2000) are out of
scope and not tracked.

| Tib# | Reason |
|------|--------|
| #364 | Environment-specific (macOS 15 + **SQL Server 2014**) — out of support; not tracked. |
| #344 | **SQL Server 2000** invalid token — out of support; not tracked. |
| #381 | openssl backend for TLS 1.0/1.1 — only needed for **SQL Server 2008 R2 / 2012** (out of support). Upstream driver issue #70 closed as wontfix. |
| #218 | tiberius 0.9 crashing on macOS — old tiberius version; N/A. |
| #375 | Environment-specific (azure-sql-edge on macOS) — needs that exact combo to repro. |
| #343 | The upstream driver's libgssapi dependency already includes the upstream fix. |
| #382 | N/A — the upstream driver has no `IntoRow` derive macro. |
| #263 | N/A — upstream driver's `FromSql for i32` already widens from `SmallInt`/`TinyInt`; null handled via `Option<i32>`. |

### Totals (as of 2026-05-10)

- **22 shipped** (PRs landed or issues closed) covering ~36 distinct tiberius issues.
- **13 open** tracking issues (4 just filed: #87, #88, #89, #90).
- **1 closed wontfix** for legacy SQL Server (#70).
- **8 explicitly skipped** with rationale.
- ⇒ **44 upstream driver work items** classified from this triage.

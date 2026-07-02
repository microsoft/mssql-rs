# TLS Channel Binding for Extended Protection — Implementation Overview

> Branch: `david/channel-binding` compared against `origin/main`
> Scope: **11 files, +531 / −2** — adds TLS **channel binding** (`tls-unique`,
> RFC 5929 §3) extraction and wiring so integrated authentication participates in
> SQL Server **Extended Protection for Authentication (EPA)**. Closes FeatureGaps #3.

This document is a reviewer-oriented walkthrough: what channel binding is, why it
matters for EPA, which platforms are in scope, and a guided tour of the code
changes.

---

## 1. What is channel binding?

**Channel binding** cryptographically ties an application-layer authentication
exchange (here, the Kerberos/NTLM handshake done via SSPI/GSSAPI) to the specific
TLS channel it runs over. The client and server each derive a **channel binding
token (CBT)** from unique properties of their TLS session and mix it into the
authentication exchange. If the auth traffic is replayed or relayed over a
*different* TLS channel, the tokens don't match and authentication fails.

This defeats **authentication relay / man-in-the-middle** attacks: an attacker who
terminates the client's TLS connection and forwards the inner auth tokens to the
real server can no longer succeed, because the tokens are bound to the attacker's
TLS channel, not the server's.

There are two channel-binding types defined by RFC 5929:

| Type | RFC | What it binds to |
|---|---|---|
| `tls-server-end-point` | RFC 5929 §4 | A hash of the server's TLS **certificate** |
| **`tls-unique`** | **RFC 5929 §3** | A value **unique to the TLS session** itself |

**SQL Server EPA uses `tls-unique`.** For TLS 1.2 and below, the `tls-unique`
value is the TLS **Finished** message bytes of the handshake. On Windows, Schannel
computes this for us and returns it directly — we never hand-construct it or hash a
certificate.

> **TLS 1.3 note.** RFC 5929 predates TLS 1.3, which removed the classic
> `tls-unique` Finished-message semantics; the correct long-term binding there is
> `tls-exporter` (RFC 9266). Schannel today reports an all-zero application-data
> region for 1.3, which matches what SQL Server currently expects, so we pass the
> Schannel-provided bytes through unchanged. A `FUTURE:` comment in
> [`bindings.rs`](../mssql-tds/src/connection/transport/win_tls/bindings.rs)
> marks where `tls-exporter` derivation would go once it is supported end to end.

---

## 2. What is EPA, and how does channel binding relate to it?

**Extended Protection for Authentication (EPA)** is a SQL Server security feature
that hardens integrated (Windows/Kerberos/NTLM) authentication against relay and
MITM attacks by requiring the client to present a valid **channel binding token**
(and, via Service Binding, a correct SPN) during login. It is configured
server-side and has three levels:

| EPA level | Server behavior |
|---|---|
| `Off` | Channel bindings ignored. |
| `Allowed` | Bindings validated **if present**; connections without them still succeed (unless a relay is detected). |
| `Required` | Login is **rejected** unless a valid channel binding token is presented. |

Channel binding is the mechanism; EPA is the server-side policy that consumes it.
Concretely:

1. The client completes the TLS handshake.
2. The client extracts the `tls-unique` CBT from the TLS session.
3. The client feeds the CBT into the SSPI/GSSAPI authentication context so it is
   incorporated into the Kerberos/NTLM tokens sent in `LOGIN7`.
4. A server with EPA `Required` verifies the CBT matches its own view of the TLS
   channel; a mismatch (or a missing token) fails the login.

Before this change, mssql-tds never populated the CBT, so it could not connect to
instances configured with EPA `Required`. This is the gap the PR closes.

---

## 3. Platform scope

| Concern | Scope |
|---|---|
| **CBT extraction (producer)** | **Windows only**, via the in-tree Schannel-direct TLS engine (`win_tls`, feature `tls-schannel-direct`). Extraction uses `QueryContextAttributesW(SECPKG_ATTR_UNIQUE_BINDINGS)`. |
| **CBT consumption (auth)** | Cross-platform: the `IntegratedAuthConfig.channel_bindings` field is already consumed by both the Windows SSPI context and the Unix GSSAPI context. This PR only newly *populates* it on Windows. |
| **Transport requirement** | Encrypted connections only. Plaintext connections and non-Schannel TLS engines return `None`, leaving channel bindings unset (correct — there is no TLS channel to bind to). |
| **Auth requirement** | Only applied for **integrated security** logins; SQL-auth / fed-auth paths are unaffected. |

This mirrors the native SNI (`ssl.cpp::SetChannelBindings`) and msodbcsql
(`SNI_SslProvider.cpp::SetChannelBindings`) drivers, which obtain the token the
same way on Windows.

---

## 4. Overview of the code changes

The flow is **extract → cache → plumb → inject → consume**:

```
TLS handshake completes (win_tls)
      │  QueryContextAttributesW(SECPKG_ATTR_UNIQUE_BINDINGS)
      ▼
SchannelTlsStream.channel_binding            (cached at handshake end)
      │  Stream::channel_binding_token()
      ▼
NetworkTransport (NetworkWriter::channel_binding_token)
      │  reader_writer.channel_binding_token()
      ▼
LoginHandler::send_login7_request
      │  config.with_channel_bindings(token)   (integrated auth only)
      ▼
IntegratedAuthConfig.channel_bindings
      │  SECBUFFER_CHANNEL_BINDINGS input buffer
      ▼
SSPI (Windows) / GSSAPI (Unix) auth context → LOGIN7
```

### Files changed

| File | Change |
|---|---|
| [`win_tls/bindings.rs`](../mssql-tds/src/connection/transport/win_tls/bindings.rs) *(new)* | `query_unique_bindings()` — the FFI extraction: zeroes a `SecPkgContext_Bindings`, calls `QueryContextAttributesW(SECPKG_ATTR_UNIQUE_BINDINGS)`, copies the `BindingsLength`-byte `SEC_CHANNEL_BINDINGS` blob verbatim, and releases it with `FreeContextBuffer`. Includes a unit test for the error path. |
| [`win_tls/mod.rs`](../mssql-tds/src/connection/transport/win_tls/mod.rs) | Registers `pub(crate) mod bindings;`. |
| [`win_tls/stream.rs`](../mssql-tds/src/connection/transport/win_tls/stream.rs) | Adds `channel_binding: Option<Vec<u8>>` to `SchannelTlsStream`; captures it via `extract_channel_binding()` at both handshake-completion branches (`Done` / `DoneWithFlush`) **before** the security context is moved into the record layer; exposes `channel_binding_token()`. Extraction is **non-fatal**: on error it logs and returns `None` (a missing binding only matters if the server enforces EPA, which then fails the login with a clear error). |
| [`win_tls/engine.rs`](../mssql-tds/src/connection/transport/win_tls/engine.rs) | Forwards `Stream::channel_binding_token()` to the stream impl for `SchannelTlsStream<Box<dyn Stream>>`. |
| [`network_transport.rs`](../mssql-tds/src/connection/transport/network_transport.rs) | Adds `Stream::channel_binding_token()` (default `None`); forwards through `Box<dyn Stream>`; implements `NetworkWriter::channel_binding_token()` on `NetworkTransport` (forwards to the active stream). |
| [`io/reader_writer.rs`](../mssql-tds/src/io/reader_writer.rs) | Adds `NetworkWriter::channel_binding_token()` with a default `None` so non-TLS transports (e.g. test mocks) need not implement it. |
| [`handler/handler_factory.rs`](../mssql-tds/src/handler/handler_factory.rs) | In `send_login7_request`, for integrated-security logins, pulls the token from `reader_writer.channel_binding_token()` and, when present, applies `config.with_channel_bindings(token)`. |
| [`connection/client_context.rs`](../mssql-tds/src/connection/client_context.rs) | Comment-only: documents that `IntegratedAuthConfig.channel_bindings` starts `None` and is populated post-handshake in `send_login7_request`. |
| [`tests/test_extended_protection.rs`](../mssql-tds/tests/test_extended_protection.rs) *(new)* | Windows-only (`#![cfg(windows)]`) integration test, gated behind `EPA_TEST=1`: an integrated-auth encrypted login that must **succeed**. The pipeline runs it with EPA = Off (baseline) and EPA = Required — a successful login under `Required` proves a valid CBT was sent, since the server rejects logins lacking one. |
| [`.pipeline/scripts/Configure-ExtendedProtection.ps1`](../.pipeline/scripts/Configure-ExtendedProtection.ps1) *(new)* | Configures the EPA level + Force Encryption on the CI SQL instance, with revert support. |
| [`.pipeline/templates/validation-stages.yml`](../.pipeline/templates/validation-stages.yml) | Adds the "Extended Protection" CI step: run the login test under EPA = Off then EPA = Required (both expecting success), reverting EPA to Off in a `finally` block so the instance is restored even on failure. |

### The consumer (already present, unchanged)

The receiving side was already in place before this PR and is only newly *fed*:

- [`security/security_context.rs`](../mssql-tds/src/security/security_context.rs) —
  `IntegratedAuthConfig.channel_bindings: Option<Vec<u8>>` and the
  `with_channel_bindings()` builder.
- [`security/windows/sspi_context.rs`](../mssql-tds/src/security/windows/sspi_context.rs) —
  passes the bytes as a `SECBUFFER_CHANNEL_BINDINGS` input buffer to
  `InitializeSecurityContextW`.
- [`security/unix/gssapi_context.rs`](../mssql-tds/src/security/unix/gssapi_context.rs) —
  carries the same field for the Kerberos path.

Because Schannel returns the token already laid out as a `SEC_CHANNEL_BINDINGS`
structure — exactly the shape SSPI expects for `SECBUFFER_CHANNEL_BINDINGS` — the
bytes are passed through **verbatim** end to end, with no re-encoding and no
TLS-version branching in our code.

---

## 5. Testing

- **Unit test** (`bindings.rs`): exercises the extraction error path (query on a
  dummy context). Runs on any platform.
- **Integration test** (`tests/test_extended_protection.rs`, Windows + `EPA_TEST=1`):
  an encrypted, integrated-auth login that must **succeed**. The CI EPA step runs
  it under two server configurations:
  - *EPA = Off* — baseline: the connection works when channel binding is not
    enforced.
  - *EPA = Required* — the server rejects any login lacking a valid channel
    binding, so a successful login here proves the `tls-unique` token we send is
    real and server-validated.
- **CI**: the Windows validation job configures each EPA level on the local SQL
  instance, runs the login test, and reverts EPA to Off afterward.

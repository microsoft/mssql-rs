// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tracing spans for the stages of opening a connection.
//!
//! Each stage of a connection attempt is a span under the [`TARGET`] target,
//! named after the stage. The span is created when the stage starts and closed
//! when it ends, and records `ok = true` just before closing if the stage
//! succeeded; a stage that failed closes without it. A stage can run more than
//! once per connection (a TCP connect per resolved address, a second attempt
//! after a redirect).
//!
//! Connection diagnostics time the stages with a `tracing` layer. Without a
//! subscriber that enables the target, the spans are disabled and cost nothing.
//! The spans are never entered, so events logged during a stage are not
//! attributed to it.

/// Target of every connect-stage span.
pub const TARGET: &str = "mssql_tds::connect";

/// Name resolution of the server host. Fields: `host`; `addresses`, the
/// addresses found, comma-separated.
pub const DNS: &str = "dns";

/// A TCP connect. Fields: `address`, the address tried. With MultiSubnetFailover,
/// one span covers resolving the host and racing every address (`address` is the
/// one that answered first), and no `dns` span is opened.
pub const TCP: &str = "tcp";

/// The SQL Server Browser lookup of a named instance. Fields: `server`, `instance`.
pub const INSTANCE_LOOKUP: &str = "instance_lookup";

/// The PRELOGIN exchange. Fields: `encryption`, what was negotiated.
pub const PRELOGIN: &str = "prelogin";

/// The TLS handshake.
pub const TLS: &str = "tls";

/// LOGIN7 and the server's response, up to a successful login.
pub const LOGIN: &str = "login";

/// The field a stage records, as `true`, when it succeeds.
pub const OK: &str = "ok";

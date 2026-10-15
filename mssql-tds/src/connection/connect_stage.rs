// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tracing spans for the stages of opening a connection.
//!
//! Each stage of a connection attempt is a span under the [`TARGET`] target,
//! named after the stage. The span is created when the stage starts and closed
//! when it ends. Just before closing, a stage that succeeded records
//! [`OK`]` = true` and one that failed records [`OK`]` = false`, with what the
//! failure was ([`OS_ERROR`], [`ERROR`], ...). A span that closes without
//! [`OK`] was abandoned: the attempt was canceled, or a time limit around the
//! stage dropped it; it neither succeeded nor failed. A stage can run more
//! than once per connection (a TCP connect per resolved address, a second
//! attempt after a redirect or on the next transport).
//!
//! The spans record facts, not interpretations; connection diagnostics read
//! them with a `tracing` layer. Without a subscriber that enables the target,
//! the spans are disabled and cost nothing. The spans are never entered, so
//! events logged during a stage are not attributed to it.

use std::net::SocketAddr;

/// Target of every connect-stage span.
pub const TARGET: &str = "mssql_tds::connect";

/// Name resolution of the server host. Fields: [`HOST`]; [`ADDRESSES`], the
/// addresses found, in the order they are tried, comma-separated (IPv6 ones
/// with their scope, `fe80::1%12`, when they have one); on failure
/// [`OS_ERROR`] and [`ERROR`]. Resolving to no address fails without either.
pub const DNS: &str = "dns";

/// A TCP connect. Fields: [`ADDRESS`], the address tried (IPv6 with its
/// scope when it has one), and [`PORT`]; on failure [`OS_ERROR`],
/// [`IO_KIND`], [`ERROR`], and [`TIMED_OUT`] when the attempt's own time
/// limit ran out. With MultiSubnetFailover, one span covers resolving the
/// host and racing every address ([`ADDRESS`] is the one that answered
/// first), and no `dns` span is opened. A diagnostic connection that asks for
/// every address also connects to the addresses after the one that answered,
/// each in a span of its own.
pub const TCP: &str = "tcp";

/// The SQL Server Browser lookup of a named instance. Fields: [`SERVER`],
/// [`INSTANCE`], [`ADDRESSES`] once the server is resolved; on success
/// [`PORT`] and/or [`PIPE`], what the Browser listed for the instance; on
/// failure [`RESULT`], a stable code: `nameResolutionFailed`, `noAddresses`,
/// `socketFailed`, `sendFailed`, `receiveFailed`, `noAnswer`, `timedOut`,
/// `invalidResponse`, or [`NO_ENDPOINT`]; and [`OS_ERROR`] and [`ERROR`]
/// when there are.
pub const INSTANCE_LOOKUP: &str = "instance_lookup";

/// Opening a named pipe, the pipe shared memory uses, or a LocalDB
/// instance's pipe. Fields: [`KIND`] ([`KIND_NAMED_PIPE`],
/// [`KIND_SHARED_MEMORY`] or [`KIND_LOCALDB`]), [`PIPE`], the pipe's path; on
/// failure [`OS_ERROR`] and [`ERROR`].
pub const PIPE: &str = "pipe";

/// The PRELOGIN exchange. Fields: [`ENCRYPTION`], what was negotiated; on
/// failure [`OS_ERROR`] and [`ERROR`].
pub const PRELOGIN: &str = "prelogin";

/// The TLS handshake. On failure: [`OS_ERROR`] and [`ERROR`].
pub const TLS: &str = "tls";

/// LOGIN7 and the server's response, up to a successful login. Fields:
/// [`REDIRECTED`] when the server routed the login elsewhere; [`SPN`] with
/// integrated authentication; on failure [`OS_ERROR`] and [`ERROR`].
pub const LOGIN: &str = "login";

/// The field the login stage records, as `true`, when the server ended the
/// login with a routing redirect: the client then connects again to the
/// server it names, so the stage ended without failing. It closes without
/// [`OK`].
pub const REDIRECTED: &str = "redirected";

/// The field a stage records when it ends: `true` when it succeeded, `false`
/// when it failed. A stage that closes without it was abandoned.
pub const OK: &str = "ok";

/// The host name resolved.
pub const HOST: &str = "host";
/// The addresses a name resolved to, comma-separated.
pub const ADDRESSES: &str = "addresses";
/// The address a TCP connect tried.
pub const ADDRESS: &str = "address";
/// A TCP port: the one connected to, or the one SQL Server Browser listed.
pub const PORT: &str = "port";
/// The server SQL Server Browser was asked on.
pub const SERVER: &str = "server";
/// The named instance looked up.
pub const INSTANCE: &str = "instance";
/// A pipe's path.
pub const PIPE_PATH: &str = "pipe";
/// What a pipe is for.
pub const KIND: &str = "kind";
/// The negotiated encryption.
pub const ENCRYPTION: &str = "encryption";
/// The SPN integrated authentication requested, in the form the platform
/// takes (SSPI's `service/host:port`, GSSAPI's `service@host`).
pub const SPN: &str = "spn";
/// The operating system's error code, as an integer.
pub const OS_ERROR: &str = "os_error";
/// The `std::io::ErrorKind` of a failed I/O operation, as `Debug` prints it.
pub const IO_KIND: &str = "io_kind";
/// `true` when a TCP connect ran out of its own time limit.
pub const TIMED_OUT: &str = "timed_out";
/// The text of the error a stage failed with.
pub const ERROR: &str = "error";
/// The stable code of a failed instance lookup.
pub const RESULT: &str = "result";

/// [`KIND`] of a named pipe given or found.
pub const KIND_NAMED_PIPE: &str = "namedPipe";
/// [`KIND`] of the pipe shared memory uses.
pub const KIND_SHARED_MEMORY: &str = "sharedMemory";
/// [`KIND`] of a LocalDB instance's pipe.
pub const KIND_LOCALDB: &str = "localDb";

/// [`RESULT`] of an instance lookup that SQL Server Browser answered without
/// an endpoint the client can use.
pub const NO_ENDPOINT: &str = "noEndpoint";

/// An address as the stages record it: the IP, with its IPv6 scope when it
/// has one (`fe80::1%12`), without the port.
pub(crate) struct ScopedIp<'a>(pub(crate) &'a SocketAddr);

impl std::fmt::Display for ScopedIp<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            SocketAddr::V6(v6) if v6.scope_id() != 0 => write!(f, "{}%{}", v6.ip(), v6.scope_id()),
            address => write!(f, "{}", address.ip()),
        }
    }
}

/// The addresses as [`ADDRESSES`] lists them.
pub(crate) fn address_list(addresses: &[SocketAddr]) -> String {
    addresses
        .iter()
        .map(|address| ScopedIp(address).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Records that `stage` failed with the I/O error `error`.
pub(crate) fn record_io_error(stage: &tracing::Span, error: &std::io::Error) {
    if stage.is_disabled() {
        return;
    }
    if let Some(code) = error.raw_os_error() {
        stage.record(OS_ERROR, code);
    }
    stage.record(IO_KIND, tracing::field::debug(error.kind()));
    stage.record(ERROR, tracing::field::display(error));
    stage.record(OK, false);
}

/// Records that `stage` failed with `error`.
pub(crate) fn record_error(stage: &tracing::Span, error: &crate::error::Error) {
    if stage.is_disabled() {
        return;
    }
    if let crate::error::Error::Io(io) = error {
        record_io_error(stage, io);
        return;
    }
    stage.record(ERROR, tracing::field::display(error));
    stage.record(OK, false);
}

#[cfg(test)]
pub(crate) mod tests {
    //! A subscriber that records the connect stages, for the tests that pin
    //! the stage names and fields.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Metadata, Subscriber};

    /// A stage as recorded: its name and every field it recorded, as text.
    #[derive(Clone, Debug)]
    pub(crate) struct Recorded {
        pub(crate) name: &'static str,
        pub(crate) fields: BTreeMap<&'static str, String>,
        pub(crate) closed: bool,
    }

    impl Recorded {
        pub(crate) fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }
    }

    struct Fields<'a>(&'a mut BTreeMap<&'static str, String>);

    impl Visit for Fields<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name(), value.to_string());
        }
    }

    /// Records every connect-stage span, in creation order.
    #[derive(Clone, Default)]
    pub(crate) struct StageLog {
        spans: Arc<Mutex<Vec<Recorded>>>,
    }

    impl StageLog {
        pub(crate) fn stages(&self) -> Vec<Recorded> {
            self.spans.lock().unwrap().clone()
        }

        /// The stages named `name`.
        pub(crate) fn named(&self, name: &str) -> Vec<Recorded> {
            self.stages()
                .into_iter()
                .filter(|s| s.name == name)
                .collect()
        }
    }

    impl Subscriber for StageLog {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.is_span() && metadata.target() == super::TARGET
        }

        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            let mut fields = BTreeMap::new();
            attrs.record(&mut Fields(&mut fields));
            let mut spans = self.spans.lock().unwrap();
            spans.push(Recorded {
                name: attrs.metadata().name(),
                fields,
                closed: false,
            });
            Id::from_u64(spans.len() as u64)
        }

        fn record(&self, span: &Id, values: &Record<'_>) {
            let mut spans = self.spans.lock().unwrap();
            let index = usize::try_from(span.into_u64()).unwrap() - 1;
            values.record(&mut Fields(&mut spans[index].fields));
        }

        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}

        fn try_close(&self, span: Id) -> bool {
            let mut spans = self.spans.lock().unwrap();
            let index = usize::try_from(span.into_u64()).unwrap() - 1;
            spans[index].closed = true;
            true
        }
    }

    #[test]
    fn an_io_failure_records_what_the_operating_system_said() {
        let log = StageLog::default();
        tracing::subscriber::with_default(log.clone(), || {
            let stage = tracing::info_span!(
                target: super::TARGET,
                super::TCP,
                address = "::1",
                port = 1433u16,
                ok = tracing::field::Empty,
                os_error = tracing::field::Empty,
                io_kind = tracing::field::Empty,
                timed_out = tracing::field::Empty,
                error = tracing::field::Empty
            );
            super::record_io_error(&stage, &std::io::Error::from_raw_os_error(10061));
        });
        let tcp = &log.named(super::TCP)[0];
        assert_eq!(tcp.field(super::OK), Some("false"));
        assert_eq!(tcp.field(super::OS_ERROR), Some("10061"));
        assert!(tcp.field(super::IO_KIND).is_some());
        assert!(tcp.field(super::ERROR).is_some());
        assert!(tcp.closed);
    }

    #[test]
    fn a_scoped_address_keeps_its_scope() {
        let scoped: std::net::SocketAddr = "[fe80::1%12]:1433".parse().unwrap();
        assert_eq!(super::ScopedIp(&scoped).to_string(), "fe80::1%12");
        let plain: std::net::SocketAddr = "10.0.0.1:1433".parse().unwrap();
        assert_eq!(
            super::address_list(&[plain, scoped]),
            "10.0.0.1, fe80::1%12"
        );
    }
}

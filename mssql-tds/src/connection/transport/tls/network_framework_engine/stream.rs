// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A byte stream over a Network.framework connection.
//!
//! Network.framework reports everything by calling blocks on a queue of its own.
//! [`Shared`] records what they report and the `poll_*` methods read it back,
//! leaving a waker for the block to fire. No block holds the connection while it
//! is open, so dropping [`NwStream`] closes the connection, within [`DRAIN_LIMIT`]
//! even if the server has stopped reading, and releases all of it.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::future::poll_fn;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use std::{fmt, io, mem, ptr};

use block2::RcBlock;
use core_foundation::base::TCFType;
use core_foundation::error::CFError;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::ffi::{
    self, BlockBool, Dispatch, ERROR_DOMAIN_POSIX, ERROR_DOMAIN_TLS, Handle, MappedData, Nw,
    Object, STATE_CANCELLED, STATE_FAILED, STATE_READY, STATE_WAITING, Sec, TLS_1_2, TLS_1_3,
    VerifyComplete,
};
use super::verify::Verifier;
use crate::connection::transport::network_transport::Stream;

/// The largest piece of data a single receive is asked for.
const RECEIVE_MAX: u32 = 64 * 1024;

/// Received data held back from the connection, so a reader that stops reading
/// does not let memory grow.
const READ_BUFFER_LIMIT: usize = 64 * 1024;

const WRITE_CHUNK: usize = 64 * 1024;

/// How much sent data may be waiting for the connection to accept it before
/// writers have to wait. Network.framework advises keeping few sends outstanding.
const WRITE_WINDOW: usize = 256 * 1024;

/// How long a dropped stream may go on delivering what was written to it before
/// its connection is cancelled regardless.
const DRAIN_LIMIT: Duration = Duration::from_secs(1);

// Secure Transport status codes, which Network.framework reports in its TLS domain.
const ERR_SSL_CLOSED_GRACEFUL: i32 = -9805;
const ERR_SSL_CLOSED_ABORT: i32 = -9806;
const ERR_SSL_CLOSED_NO_NOTIFY: i32 = -9816;

/// Why a connection or one of its operations failed.
#[derive(Clone, Debug)]
pub(super) struct Failure {
    kind: io::ErrorKind,
    message: String,
    os_error: Option<i32>,
}

impl Failure {
    fn new(kind: io::ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            os_error: None,
        }
    }

    /// A failure the operating system reported as `code`, described exactly as a
    /// socket reporting it would be.
    fn os(code: i32) -> Self {
        let error = io::Error::from_raw_os_error(code);
        Self {
            kind: error.kind(),
            message: error.to_string(),
            os_error: Some(code),
        }
    }

    fn unknown() -> Self {
        Self::new(io::ErrorKind::Other, "the TLS connection failed")
    }

    /// # Safety
    ///
    /// `error` is a live `nw_error_t`.
    unsafe fn from_nw(error: Object) -> Self {
        // SAFETY: per the contract.
        let (domain, code) = unsafe {
            (
                ffi::nw_error_get_error_domain(error),
                ffi::nw_error_get_error_code(error),
            )
        };
        if domain == ERROR_DOMAIN_POSIX {
            return Self::os(code);
        }
        // SAFETY: per the contract.
        let message = unsafe { describe(error) }
            .unwrap_or_else(|| format!("Network.framework error {code} in domain {domain}"));
        Self::new(error_kind(domain, code), message)
    }

    /// Whether the failure means the server could not be reached at all, as
    /// opposed to failing once it was.
    pub(super) fn is_unreachable(&self) -> bool {
        matches!(
            self.os_error,
            Some(
                libc::ECONNREFUSED
                    | libc::ETIMEDOUT
                    | libc::EHOSTUNREACH
                    | libc::EHOSTDOWN
                    | libc::ENETUNREACH
                    | libc::ENETDOWN
                    | libc::EADDRNOTAVAIL
                    | libc::EAFNOSUPPORT
            )
        )
    }

    pub(super) fn to_io_error(&self) -> io::Error {
        match self.os_error {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::new(self.kind, self.message.clone()),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

fn error_kind(domain: u32, code: i32) -> io::ErrorKind {
    match (domain, code) {
        (ERROR_DOMAIN_TLS, ERR_SSL_CLOSED_GRACEFUL | ERR_SSL_CLOSED_NO_NOTIFY) => {
            io::ErrorKind::UnexpectedEof
        }
        (ERROR_DOMAIN_TLS, ERR_SSL_CLOSED_ABORT) => io::ErrorKind::ConnectionAborted,
        (ERROR_DOMAIN_TLS, _) => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    }
}

/// # Safety
///
/// `error` is a live `nw_error_t`.
unsafe fn describe(error: Object) -> Option<String> {
    // SAFETY: per the contract. The copy is owned (+1), which the wrapper takes over.
    let error = unsafe {
        let error = ffi::nw_error_copy_cf_error(error);
        if error.is_null() {
            return None;
        }
        CFError::wrap_under_create_rule(error.cast())
    };
    Some(error.description().to_string())
}

#[derive(Default)]
enum Phase {
    #[default]
    Connecting,
    Ready,
    Failed(Failure),
    Cancelled,
}

impl Phase {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::Cancelled)
    }

    fn failure(&self) -> Option<Failure> {
        match self {
            Self::Failed(failure) => Some(failure.clone()),
            Self::Cancelled => Some(Failure::new(
                io::ErrorKind::ConnectionAborted,
                "the TLS connection was cancelled",
            )),
            Self::Connecting | Self::Ready => None,
        }
    }
}

struct Chunk {
    data: MappedData,
    read: usize,
}

#[derive(Default)]
struct Inner {
    phase: Phase,
    phase_waker: Option<Waker>,

    chunks: VecDeque<Chunk>,
    buffered: usize,
    receiving: bool,
    eof: bool,
    read_error: Option<Failure>,
    read_waker: Option<Waker>,

    in_flight: usize,
    write_error: Option<Failure>,
    closing: bool,
    write_waker: Option<Waker>,
}

impl Inner {
    /// Applies a connection state change, returning whether anything changed.
    ///
    /// Network.framework reports a failed handshake, or a certificate the verify
    /// block rejected, as `waiting` with an error rather than as `failed`.
    fn apply_state(&mut self, state: u32, failure: Option<Failure>) -> bool {
        let next = match (state, failure) {
            (STATE_READY, _) => Phase::Ready,
            (STATE_FAILED, failure) => Phase::Failed(failure.unwrap_or_else(Failure::unknown)),
            (STATE_WAITING, Some(failure)) => Phase::Failed(failure),
            (STATE_CANCELLED, _) => Phase::Cancelled,
            _ => return false,
        };
        if self.phase.is_terminal() || matches!((&self.phase, &next), (Phase::Ready, Phase::Ready))
        {
            return false;
        }
        self.phase = next;
        true
    }

    fn take_wakers(&mut self) -> [Option<Waker>; 3] {
        [
            self.phase_waker.take(),
            self.read_waker.take(),
            self.write_waker.take(),
        ]
    }

    fn push(&mut self, data: MappedData) {
        self.buffered += data.as_slice().len();
        self.chunks.push_back(Chunk { data, read: 0 });
    }

    fn drain_into(&mut self, buf: &mut ReadBuf<'_>) {
        while buf.remaining() > 0 {
            let Some(chunk) = self.chunks.front_mut() else {
                break;
            };
            let unread = &chunk.data.as_slice()[chunk.read..];
            let len = unread.len().min(buf.remaining());
            buf.put_slice(&unread[..len]);
            chunk.read += len;
            self.buffered -= len;
            if chunk.read == chunk.data.as_slice().len() {
                self.chunks.pop_front();
            }
        }
    }

    /// Decides whether the reader should ask the connection for more data, and
    /// records that it is about to.
    fn begin_receive(&mut self) -> bool {
        let wanted = !self.receiving
            && !self.eof
            && self.read_error.is_none()
            && self.buffered < READ_BUFFER_LIMIT
            && matches!(self.phase, Phase::Ready);
        self.receiving |= wanted;
        wanted
    }

    fn connection_failure(&self) -> Option<io::Error> {
        let failure = self.write_error.clone().or_else(|| self.phase.failure());
        failure.map(|failure| failure.to_io_error())
    }

    fn write_failure(&self) -> Option<io::Error> {
        self.connection_failure()
            .or_else(|| self.closing.then(|| io::ErrorKind::BrokenPipe.into()))
    }

    fn is_finished(&self) -> bool {
        self.eof || self.read_error.is_some() || self.phase.is_terminal()
    }
}

fn wake_all(wakers: [Option<Waker>; 3]) {
    for waker in wakers.into_iter().flatten() {
        waker.wake();
    }
}

/// State written by Network.framework's blocks and read by the stream.
#[derive(Default)]
struct Shared {
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn on_state(&self, state: u32, error: Object) {
        // SAFETY: `error` is null or live for the duration of the block.
        let failure = (!error.is_null()).then(|| unsafe { Failure::from_nw(error) });
        let wakers = {
            let mut inner = self.lock();
            inner
                .apply_state(state, failure)
                .then(|| inner.take_wakers())
        };
        if let Some(wakers) = wakers {
            wake_all(wakers);
        }
    }

    fn on_received(&self, content: Object, is_complete: bool, error: Object) {
        // SAFETY: `content` is null or live dispatch data for the duration of the block.
        let data = unsafe { MappedData::new(content) };
        // SAFETY: `error` is null or live for the duration of the block.
        let failure = (!error.is_null()).then(|| unsafe { Failure::from_nw(error) });
        let waker = {
            let mut inner = self.lock();
            inner.receiving = false;
            if let Some(data) = data {
                inner.push(data);
            }
            if let Some(failure) = failure {
                inner.read_error = Some(failure);
            } else if is_complete {
                inner.eof = true;
            }
            inner.read_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn on_sent(&self, len: usize, error: Object) {
        // SAFETY: `error` is null or live for the duration of the block.
        let failure = (!error.is_null()).then(|| unsafe { Failure::from_nw(error) });
        let waker = {
            let mut inner = self.lock();
            inner.in_flight = inner.in_flight.saturating_sub(len);
            if let Some(failure) = failure {
                inner.write_error.get_or_insert(failure);
            }
            inner.write_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// What the TLS handshake settled on.
pub(super) struct Negotiated {
    pub(super) version: u16,
    pub(super) cipher_suite: u16,
    pub(super) alpn: Option<String>,
}

impl fmt::Display for Negotiated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let version = match self.version {
            TLS_1_3 => "TLS 1.3",
            TLS_1_2 => "TLS 1.2",
            0x0302 => "TLS 1.1",
            0x0301 => "TLS 1.0",
            _ => "an unknown TLS version",
        };
        write!(
            f,
            "version={version}, cipher_suite={:#06x}, alpn={:?}",
            self.cipher_suite, self.alpn,
        )
    }
}

/// How to reach and configure the connection.
pub(super) struct Options<'a> {
    pub(super) address: SocketAddr,
    /// Sent to the server as SNI.
    pub(super) server_name: Option<&'a str>,
    pub(super) alpn: &'a str,
    pub(super) keep_alive: KeepAlive,
}

/// TCP keep-alive timing, to the second, as for sockets.
#[derive(Clone, Copy, Debug)]
pub(super) struct KeepAlive {
    pub(super) idle: Duration,
    pub(super) interval: Duration,
}

/// A TLS connection driven by Network.framework.
pub(super) struct NwStream {
    connection: Handle<Nw>,
    /// The serial queue the connection's blocks run on.
    queue: Handle<Dispatch>,
    shared: Arc<Shared>,
}

impl NwStream {
    /// Starts connecting to the server, returning before the connection, let alone
    /// the handshake, has completed.
    pub(super) fn start(options: &Options<'_>, verifier: Arc<Verifier>) -> io::Result<Self> {
        let server_name = options.server_name.map(c_string).transpose()?;
        let alpn = c_string(options.alpn)?;
        let endpoint = address_endpoint(options.address)?;
        let parameters = tls_parameters(server_name, alpn, verifier, options.keep_alive)?;
        let queue = ffi::new_serial_queue()
            .ok_or_else(|| io::Error::other("failed to create a dispatch queue"))?;

        // SAFETY: both arguments are live objects, which the connection retains.
        let connection = unsafe {
            Handle::<Nw>::from_owned(ffi::nw_connection_create(
                endpoint.as_ptr(),
                parameters.as_ptr(),
            ))
        }
        .ok_or_else(|| io::Error::other("failed to create a Network.framework connection"))?;

        let shared = Arc::new(Shared::default());
        let on_state = {
            let shared = Arc::clone(&shared);
            RcBlock::new(move |state: u32, error: Object| shared.on_state(state, error))
        };
        // SAFETY: the connection is live, and the call retains the queue and copies
        // the block. The connection starts only once it is fully configured.
        unsafe {
            ffi::nw_connection_set_queue(connection.as_ptr(), queue.as_ptr());
            ffi::nw_connection_set_state_changed_handler(connection.as_ptr(), &on_state);
            ffi::nw_connection_start(connection.as_ptr());
        }

        Ok(Self {
            connection,
            queue,
            shared,
        })
    }

    /// Resolves once the handshake has completed or failed.
    pub(super) async fn established(&self) -> Result<(), Failure> {
        poll_fn(|cx| {
            let mut inner = self.shared.lock();
            match &inner.phase {
                Phase::Ready => Poll::Ready(Ok(())),
                Phase::Connecting => {
                    inner.phase_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
                terminal => Poll::Ready(Err(terminal.failure().unwrap_or_else(Failure::unknown))),
            }
        })
        .await
    }

    pub(super) fn negotiated(&self) -> Option<Negotiated> {
        // SAFETY: the connection is live, and every `copy` returns an owned (+1)
        // reference that its handle releases.
        unsafe {
            let definition = Handle::<Nw>::from_owned(ffi::nw_protocol_copy_tls_definition())?;
            let metadata = Handle::<Nw>::from_owned(ffi::nw_connection_copy_protocol_metadata(
                self.connection.as_ptr(),
                definition.as_ptr(),
            ))?;
            let security = Handle::<Sec>::from_owned(ffi::nw_tls_copy_sec_protocol_metadata(
                metadata.as_ptr(),
            ))?;
            let alpn = ffi::sec_protocol_metadata_get_negotiated_protocol(security.as_ptr());
            Some(Negotiated {
                version: ffi::sec_protocol_metadata_get_negotiated_tls_protocol_version(
                    security.as_ptr(),
                ),
                cipher_suite: ffi::sec_protocol_metadata_get_negotiated_tls_ciphersuite(
                    security.as_ptr(),
                ),
                alpn: (!alpn.is_null())
                    .then(|| CStr::from_ptr(alpn).to_string_lossy().into_owned()),
            })
        }
    }

    /// Reports whether the connection has ended, even once the stream is gone.
    #[cfg(test)]
    pub(super) fn end_probe(&self) -> impl Fn() -> bool + Send + use<> {
        let shared = Arc::clone(&self.shared);
        move || shared.lock().phase.is_terminal()
    }

    fn receive(&self) {
        let shared = Arc::clone(&self.shared);
        let on_received = RcBlock::new(
            move |content: Object, _context: Object, is_complete: BlockBool, error: Object| {
                shared.on_received(content, is_complete != 0, error);
            },
        );
        // SAFETY: the connection is live, and the call copies the block.
        unsafe {
            ffi::nw_connection_receive(self.connection.as_ptr(), 1, RECEIVE_MAX, &on_received);
        }
    }

    /// Closes the connection, negotiating the close with the server. Idempotent.
    ///
    /// Only cancelling makes a TLS connection send `close_notify`, and a cancel
    /// can discard the tail of the data sent just before it, even once those sends
    /// have completed. Sends are handled in order, so cancelling from the
    /// completion of an empty one loses nothing.
    fn close(&self) {
        let ready = {
            let mut inner = self.shared.lock();
            if mem::replace(&mut inner.closing, true) {
                return;
            }
            matches!(inner.phase, Phase::Ready)
        };
        // A send on a connection that is not ready may never complete.
        let barrier = if ready {
            ffi::copy_to_dispatch_data(&[])
        } else {
            None
        };
        match barrier {
            Some(barrier) => {
                let connection = self.connection.clone();
                self.send(&barrier, move |_| cancel(&connection));
            }
            None => cancel(&self.connection),
        }
    }

    /// Sends `data` on the connection's one stream.
    fn send(&self, data: &Handle<Dispatch>, on_sent: impl Fn(Object) + 'static) {
        let on_sent = RcBlock::new(on_sent);
        // SAFETY: the connection and `data` are live, and the call retains the data
        // and copies the block.
        unsafe {
            ffi::nw_connection_send(
                self.connection.as_ptr(),
                data.as_ptr(),
                ffi::stream_context(),
                false,
                &on_sent,
            );
        }
    }
}

/// Cancels the connection. Cancelling twice is harmless.
fn cancel(connection: &Handle<Nw>) {
    // SAFETY: the connection is live.
    unsafe { ffi::nw_connection_cancel(connection.as_ptr()) };
}

impl Drop for NwStream {
    fn drop(&mut self) {
        self.close();
        // The close waits behind earlier sends, which never complete if the server
        // has stopped reading, and nothing else is left to cancel the connection.
        if matches!(self.shared.lock().phase, Phase::Ready) {
            let connection = self.connection.clone();
            let cancel_late = RcBlock::new(move || cancel(&connection));
            ffi::run_after(DRAIN_LIMIT, &self.queue, &cancel_late);
        }
    }
}

impl AsyncRead for NwStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut inner = this.shared.lock();
        if inner.buffered > 0 {
            inner.drain_into(buf);
            let more = inner.begin_receive();
            drop(inner);
            if more {
                this.receive();
            }
            return Poll::Ready(Ok(()));
        }
        if let Some(failure) = &inner.read_error {
            return Poll::Ready(Err(failure.to_io_error()));
        }
        if inner.eof {
            return Poll::Ready(Ok(()));
        }
        match &inner.phase {
            Phase::Failed(failure) => return Poll::Ready(Err(failure.to_io_error())),
            // Only this side cancels a connection, so its end is the stream's end.
            Phase::Cancelled => return Poll::Ready(Ok(())),
            Phase::Connecting | Phase::Ready => {}
        }
        inner.read_waker = Some(cx.waker().clone());
        let more = inner.begin_receive();
        drop(inner);
        if more {
            this.receive();
        }
        Poll::Pending
    }
}

impl AsyncWrite for NwStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let len = buf.len().min(WRITE_CHUNK);
        {
            let mut inner = this.shared.lock();
            if let Some(error) = inner.write_failure() {
                return Poll::Ready(Err(error));
            }
            if inner.in_flight >= WRITE_WINDOW {
                inner.write_waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            inner.in_flight += len;
        }
        // The framework keeps the data until it has been sent, which outlasts `buf`.
        let Some(data) = ffi::copy_to_dispatch_data(&buf[..len]) else {
            let mut inner = this.shared.lock();
            inner.in_flight -= len;
            return Poll::Ready(Err(io::Error::other("out of memory")));
        };
        let shared = Arc::clone(&this.shared);
        this.send(&data, move |error| shared.on_sent(len, error));
        Poll::Ready(Ok(len))
    }

    // Waits for the connection to take everything written so far, so a failed send
    // is reported by the flush that follows it rather than by some later call.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.shared.lock();
        if let Some(error) = inner.connection_failure() {
            return Poll::Ready(Err(error));
        }
        if inner.in_flight > 0 {
            inner.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        {
            let mut inner = this.shared.lock();
            if inner.phase.is_terminal() {
                return Poll::Ready(Ok(()));
            }
            // Woken when the cancel completes, or when an earlier send does.
            inner.write_waker = Some(cx.waker().clone());
        }
        this.close();
        Poll::Pending
    }
}

impl Stream for NwStream {
    fn tls_handshake_starting(&mut self) {}

    fn tls_handshake_completed(&mut self) {}

    // A read keeps a receive outstanding even while the stream is idle, so a
    // server hanging up shows up here without anyone reading.
    fn is_connection_dead(&self) -> bool {
        self.shared.lock().is_finished()
    }
}

fn c_string(value: &str) -> io::Result<CString> {
    CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "value contains a NUL byte"))
}

fn sockaddr_in(address: &SocketAddrV4) -> libc::sockaddr_in {
    // SAFETY: `sockaddr_in` is plain old data, for which all zeroes is valid.
    let mut raw: libc::sockaddr_in = unsafe { mem::zeroed() };
    raw.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
    raw.sin_family = libc::AF_INET as libc::sa_family_t;
    raw.sin_port = address.port().to_be();
    raw.sin_addr.s_addr = u32::from_ne_bytes(address.ip().octets());
    raw
}

fn sockaddr_in6(address: &SocketAddrV6) -> libc::sockaddr_in6 {
    // SAFETY: `sockaddr_in6` is plain old data, for which all zeroes is valid.
    let mut raw: libc::sockaddr_in6 = unsafe { mem::zeroed() };
    raw.sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
    raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    raw.sin6_port = address.port().to_be();
    raw.sin6_flowinfo = address.flowinfo();
    raw.sin6_addr.s6_addr = address.ip().octets();
    raw.sin6_scope_id = address.scope_id();
    raw
}

fn address_endpoint(address: SocketAddr) -> io::Result<Handle<Nw>> {
    // SAFETY: each address is complete, with its length set, and the call copies it.
    let endpoint = unsafe {
        match &address {
            SocketAddr::V4(v4) => {
                let raw = sockaddr_in(v4);
                ffi::nw_endpoint_create_address(ptr::from_ref(&raw).cast())
            }
            SocketAddr::V6(v6) => {
                let raw = sockaddr_in6(v6);
                ffi::nw_endpoint_create_address(ptr::from_ref(&raw).cast())
            }
        }
    };
    // SAFETY: the endpoint is null or owned (+1).
    unsafe { Handle::from_owned(endpoint) }
        .ok_or_else(|| io::Error::other("failed to create a Network.framework endpoint"))
}

fn whole_seconds(duration: Duration) -> u32 {
    u32::try_from(duration.as_secs()).unwrap_or(u32::MAX)
}

fn tls_parameters(
    server_name: Option<CString>,
    alpn: CString,
    verifier: Arc<Verifier>,
    keep_alive: KeepAlive,
) -> io::Result<Handle<Nw>> {
    let configured = Arc::new(AtomicBool::new(false));
    let configure = {
        let configured = Arc::clone(&configured);
        RcBlock::new(move |options: Object| {
            if configure_tls(options, server_name.as_deref(), &alpn, &verifier).is_some() {
                configured.store(true, Ordering::Release);
            }
        })
    };
    let configure_tcp = RcBlock::new(move |options: Object| configure_tcp(options, keep_alive));
    // SAFETY: both blocks run before the call returns.
    let parameters = unsafe {
        Handle::<Nw>::from_owned(ffi::nw_parameters_create_secure_tcp(
            &configure,
            &configure_tcp,
        ))
    };
    match parameters {
        Some(parameters) if configured.load(Ordering::Acquire) => {
            // A socket would not go through a proxy either.
            // SAFETY: `parameters` is live.
            unsafe { ffi::nw_parameters_set_prefer_no_proxy(parameters.as_ptr(), true) };
            Ok(parameters)
        }
        _ => Err(io::Error::other("failed to configure TLS")),
    }
}

/// Runs inside Network.framework's configure block, setting what the socket
/// transport sets on its sockets.
fn configure_tcp(options: Object, keep_alive: KeepAlive) {
    // SAFETY: `options` is live TCP options for the duration of the configure block.
    unsafe {
        ffi::nw_tcp_options_set_no_delay(options, true);
        ffi::nw_tcp_options_set_enable_keepalive(options, true);
        ffi::nw_tcp_options_set_keepalive_idle_time(options, whole_seconds(keep_alive.idle));
        ffi::nw_tcp_options_set_keepalive_interval(options, whole_seconds(keep_alive.interval));
    }
}

/// Runs inside Network.framework's configure block.
fn configure_tls(
    options: Object,
    server_name: Option<&CStr>,
    alpn: &CStr,
    verifier: &Arc<Verifier>,
) -> Option<()> {
    // SAFETY: `options` is live for the duration of the configure block. The copy
    // is owned (+1).
    let security =
        unsafe { Handle::<Sec>::from_owned(ffi::nw_tls_copy_sec_protocol_options(options)) }?;
    let queue = ffi::verification_queue()?;
    let verify = {
        let verifier = Arc::clone(verifier);
        RcBlock::new(
            move |_metadata: Object, trust: Object, complete: *mut VerifyComplete| {
                let trusted = verifier.verify(trust);
                // SAFETY: `complete` is a live block for the duration of this one.
                if let Some(complete) = unsafe { complete.as_ref() } {
                    complete.call((BlockBool::from(trusted),));
                }
            },
        )
    };
    let options = security.as_ptr();
    // SAFETY: `options` is live. The calls copy the strings, retain the queue and
    // copy the block.
    unsafe {
        if let Some(name) = server_name {
            ffi::sec_protocol_options_set_tls_server_name(options, name.as_ptr());
        }
        ffi::sec_protocol_options_add_tls_application_protocol(options, alpn.as_ptr());
        ffi::sec_protocol_options_set_min_tls_protocol_version(options, TLS_1_2);
        ffi::sec_protocol_options_set_max_tls_protocol_version(options, TLS_1_3);
        // A resumed session presents no certificate to verify or pin.
        ffi::sec_protocol_options_set_tls_resumption_enabled(options, false);
        ffi::sec_protocol_options_set_tls_tickets_enabled(options, false);
        ffi::sec_protocol_options_set_verify_block(options, &verify, queue.as_ptr());
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(bytes: &[u8]) -> MappedData {
        let data = ffi::copy_to_dispatch_data(bytes).unwrap();
        // SAFETY: `data` is a live dispatch data object, which the mapping retains.
        unsafe { MappedData::new(data.as_ptr()) }.unwrap()
    }

    fn drained(inner: &mut Inner, capacity: usize) -> Vec<u8> {
        let mut storage = vec![0; capacity];
        let mut buf = ReadBuf::new(&mut storage);
        inner.drain_into(&mut buf);
        buf.filled().to_vec()
    }

    #[test]
    fn posix_errors_read_as_a_socket_would_report_them() {
        let failure = Failure::os(libc::ECONNREFUSED);
        let socket = io::Error::from_raw_os_error(libc::ECONNREFUSED);

        assert_eq!(failure.kind, io::ErrorKind::ConnectionRefused);
        assert_eq!(failure.to_string(), socket.to_string());
        let error = failure.to_io_error();
        assert_eq!(error.raw_os_error(), Some(libc::ECONNREFUSED));
        assert_eq!(error.to_string(), socket.to_string());
    }

    #[test]
    fn only_failing_to_reach_the_server_is_unreachable() {
        for code in [
            libc::ECONNREFUSED,
            libc::ETIMEDOUT,
            libc::EHOSTUNREACH,
            libc::EHOSTDOWN,
            libc::ENETUNREACH,
            libc::ENETDOWN,
            libc::EADDRNOTAVAIL,
            libc::EAFNOSUPPORT,
        ] {
            assert!(Failure::os(code).is_unreachable(), "os error {code}");
        }
        for code in [libc::ECONNRESET, libc::EPIPE, libc::ENOTCONN] {
            assert!(!Failure::os(code).is_unreachable(), "os error {code}");
        }
        assert!(!Failure::new(io::ErrorKind::InvalidData, "bad certificate").is_unreachable());
        assert!(!Failure::unknown().is_unreachable());
    }

    #[test]
    fn tls_closures_are_told_apart_from_tls_failures() {
        let kind = |code| error_kind(ERROR_DOMAIN_TLS, code);

        assert_eq!(kind(ERR_SSL_CLOSED_GRACEFUL), io::ErrorKind::UnexpectedEof);
        assert_eq!(kind(ERR_SSL_CLOSED_NO_NOTIFY), io::ErrorKind::UnexpectedEof);
        assert_eq!(kind(ERR_SSL_CLOSED_ABORT), io::ErrorKind::ConnectionAborted);
        assert_eq!(kind(-9808), io::ErrorKind::InvalidData);
    }

    #[test]
    fn errors_from_other_domains_are_generic() {
        assert_eq!(error_kind(2, -65554), io::ErrorKind::Other);
        assert_eq!(error_kind(0, 0), io::ErrorKind::Other);
    }

    #[test]
    fn socket_addresses_are_laid_out_as_the_kernel_expects() {
        let v4 = sockaddr_in(&SocketAddrV4::new([192, 0, 2, 1].into(), 1433));
        assert_eq!(usize::from(v4.sin_len), mem::size_of::<libc::sockaddr_in>());
        assert_eq!(i32::from(v4.sin_family), libc::AF_INET);
        assert_eq!(u16::from_be(v4.sin_port), 1433);
        assert_eq!(v4.sin_addr.s_addr.to_ne_bytes(), [192, 0, 2, 1]);

        let ip = "2001:db8::1".parse().unwrap();
        let v6 = sockaddr_in6(&SocketAddrV6::new(ip, 1434, 7, 3));
        assert_eq!(
            usize::from(v6.sin6_len),
            mem::size_of::<libc::sockaddr_in6>()
        );
        assert_eq!(i32::from(v6.sin6_family), libc::AF_INET6);
        assert_eq!(u16::from_be(v6.sin6_port), 1434);
        assert_eq!(v6.sin6_addr.s6_addr, ip.octets());
        assert_eq!(v6.sin6_flowinfo, 7);
        assert_eq!(v6.sin6_scope_id, 3);
    }

    #[test]
    fn every_address_family_makes_an_endpoint() {
        assert!(address_endpoint(SocketAddr::from(([127, 0, 0, 1], 1433))).is_ok());
        assert!(address_endpoint("[::1]:1433".parse().unwrap()).is_ok());
    }

    #[test]
    fn keep_alive_timing_is_truncated_to_whole_seconds() {
        assert_eq!(whole_seconds(Duration::from_millis(30_999)), 30);
        assert_eq!(whole_seconds(Duration::from_millis(999)), 0);
        assert_eq!(whole_seconds(Duration::from_secs(u64::MAX)), u32::MAX);
    }

    #[test]
    fn failures_display_their_message() {
        let failure = Failure::new(io::ErrorKind::InvalidData, "bad certificate");

        assert_eq!(failure.to_string(), "bad certificate");
        let error = failure.to_io_error();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "bad certificate");
    }

    #[test]
    fn a_cancelled_connection_is_an_aborted_one() {
        let failure = Phase::Cancelled.failure().unwrap();

        assert_eq!(failure.kind, io::ErrorKind::ConnectionAborted);
        assert!(Phase::Connecting.failure().is_none());
        assert!(Phase::Ready.failure().is_none());
    }

    #[test]
    fn the_connection_advances_to_ready_and_then_to_its_end() {
        let mut inner = Inner::default();

        assert!(
            !inner.apply_state(STATE_WAITING, None),
            "waiting is not news"
        );
        assert!(inner.apply_state(STATE_READY, None));
        assert!(
            !inner.apply_state(STATE_READY, None),
            "repeats are not news"
        );
        assert!(inner.apply_state(STATE_CANCELLED, None));
        assert!(matches!(inner.phase, Phase::Cancelled));
    }

    #[test]
    fn the_end_of_a_connection_is_final() {
        let mut inner = Inner::default();
        inner.apply_state(STATE_CANCELLED, None);

        assert!(!inner.apply_state(STATE_READY, None));
        assert!(!inner.apply_state(STATE_FAILED, Some(Failure::unknown())));
        assert!(matches!(inner.phase, Phase::Cancelled));
    }

    #[test]
    fn waiting_with_an_error_is_a_failure() {
        let mut inner = Inner::default();
        let failure = Failure::new(io::ErrorKind::InvalidData, "certificate rejected");

        assert!(inner.apply_state(STATE_WAITING, Some(failure)));

        let failure = inner.phase.failure().unwrap();
        assert_eq!(failure.kind, io::ErrorKind::InvalidData);
        assert_eq!(failure.to_string(), "certificate rejected");
    }

    #[test]
    fn a_failed_connection_without_an_error_still_fails() {
        let mut inner = Inner::default();

        assert!(inner.apply_state(STATE_FAILED, None));

        assert!(matches!(inner.phase, Phase::Failed(_)));
        assert!(inner.is_finished());
    }

    #[test]
    fn unknown_states_change_nothing() {
        let mut inner = Inner::default();

        assert!(!inner.apply_state(0, None));
        assert!(!inner.apply_state(2, None));
        assert!(matches!(inner.phase, Phase::Connecting));
    }

    #[test]
    fn a_write_failure_beats_a_closed_connection_which_beats_nothing() {
        let mut inner = Inner::default();
        assert!(inner.write_failure().is_none());

        inner.closing = true;
        assert_eq!(
            inner.write_failure().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );

        inner.phase = Phase::Failed(Failure::new(io::ErrorKind::ConnectionReset, "reset"));
        assert_eq!(
            inner.write_failure().unwrap().kind(),
            io::ErrorKind::ConnectionReset
        );

        inner.write_error = Some(Failure::new(io::ErrorKind::TimedOut, "slow"));
        assert_eq!(
            inner.connection_failure().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn the_stream_is_finished_when_nothing_more_can_arrive() {
        let mut inner = Inner::default();
        assert!(!inner.is_finished());

        inner.eof = true;
        assert!(inner.is_finished());
        inner.eof = false;

        inner.read_error = Some(Failure::unknown());
        assert!(inner.is_finished());
    }

    #[test]
    fn data_is_asked_for_only_when_it_can_be_used() {
        let mut inner = Inner::default();
        assert!(!inner.begin_receive(), "not connected yet");

        inner.phase = Phase::Ready;
        assert!(inner.begin_receive());
        assert!(inner.receiving);
        assert!(!inner.begin_receive(), "one receive at a time");

        inner.receiving = false;
        inner.buffered = READ_BUFFER_LIMIT;
        assert!(!inner.begin_receive(), "the reader is behind");

        inner.buffered = 0;
        inner.eof = true;
        assert!(!inner.begin_receive(), "nothing left to receive");

        inner.eof = false;
        inner.read_error = Some(Failure::unknown());
        assert!(!inner.begin_receive(), "the receive failed");
    }

    #[test]
    fn reads_drain_across_chunks_and_keep_the_remainder() {
        let mut inner = Inner::default();
        inner.push(chunk(b"hello "));
        inner.push(chunk(b"world"));
        assert_eq!(inner.buffered, 11);

        assert_eq!(drained(&mut inner, 8), b"hello wo");
        assert_eq!(inner.buffered, 3);

        assert_eq!(drained(&mut inner, 8), b"rld");
        assert_eq!(inner.buffered, 0);
        assert!(inner.chunks.is_empty());
    }

    #[test]
    fn nothing_is_drained_from_an_empty_buffer() {
        let mut inner = Inner::default();

        assert!(drained(&mut inner, 8).is_empty());
    }

    #[test]
    fn negotiated_parameters_are_described() {
        let describe = |version| {
            Negotiated {
                version,
                cipher_suite: 0x1301,
                alpn: Some("tds/8.0".into()),
            }
            .to_string()
        };

        assert_eq!(
            describe(TLS_1_3),
            "version=TLS 1.3, cipher_suite=0x1301, alpn=Some(\"tds/8.0\")"
        );
        assert!(describe(TLS_1_2).starts_with("version=TLS 1.2,"));
        assert!(describe(0x0302).starts_with("version=TLS 1.1,"));
        assert!(describe(0x0301).starts_with("version=TLS 1.0,"));
        assert!(describe(0).starts_with("version=an unknown TLS version,"));
    }

    #[test]
    fn strings_for_the_framework_cannot_contain_nul() {
        assert_eq!(c_string("tds/8.0").unwrap().to_bytes(), b"tds/8.0");
        assert_eq!(
            c_string("tds\0").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

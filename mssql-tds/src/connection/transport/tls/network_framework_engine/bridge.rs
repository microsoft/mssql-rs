// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Carries TLS records between Network.framework and the real transport.
//!
//! Network.framework opens its own connections: it cannot be handed a socket, nor
//! any other byte stream. The one transport it will drive that this process also
//! controls is a Unix domain socket path. [`Bridge`] listens on a private one,
//! Network.framework connects to it as if it were the server, and [`spawn_pump`]
//! relays the bytes between that socket and the stream the connection layer
//! established. Those bytes are TLS records, so the application data never
//! leaves Network.framework unencrypted.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, copy_bidirectional_with_sizes};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::connection::transport::network_transport::Stream;

/// `sockaddr_un.sun_path` holds 104 bytes on macOS, terminator included.
const MAX_SOCKET_PATH: usize = 103;

const SOCKET_NAME: &str = "s";
const DIR_PREFIX: &str = "mssql-tds-";
const DIR_ID_LEN: usize = 12;

const PUMP_BUFFER: usize = 32 * 1024;

/// How long the pump keeps running after the connection is dropped, so bytes
/// already handed to the socket, such as a `close_notify`, reach the server.
const DRAIN: Duration = Duration::from_secs(1);

/// A listening socket in a directory only this user can enter.
///
/// Removing the directory is tied to the lifetime of this value, so an abandoned
/// handshake leaves nothing behind.
pub(super) struct Bridge {
    listener: UnixListener,
    dir: PathBuf,
}

impl Bridge {
    pub(super) fn bind() -> io::Result<Self> {
        Self::bind_among([std::env::temp_dir(), PathBuf::from("/tmp")])
    }

    /// Binds under the first of `parents` whose socket path fits `sun_path`.
    fn bind_among(parents: impl IntoIterator<Item = PathBuf>) -> io::Result<Self> {
        let mut last_error = None;
        for parent in parents {
            let path_len = parent.as_os_str().len()
                + 1
                + DIR_PREFIX.len()
                + DIR_ID_LEN
                + 1
                + SOCKET_NAME.len();
            if path_len > MAX_SOCKET_PATH {
                last_error = Some(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("socket path under {parent:?} exceeds {MAX_SOCKET_PATH} bytes"),
                ));
                continue;
            }
            match Self::bind_in(&parent) {
                Ok(bridge) => return Ok(bridge),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::other("no directory for the TLS socket")))
    }

    fn bind_in(parent: &Path) -> io::Result<Self> {
        let dir = create_private_dir(parent)?;
        match UnixListener::bind(dir.join(SOCKET_NAME)) {
            Ok(listener) => Ok(Self { listener, dir }),
            Err(e) => {
                let _ = fs::remove_dir(&dir);
                Err(e)
            }
        }
    }

    pub(super) fn socket_path(&self) -> PathBuf {
        self.dir.join(SOCKET_NAME)
    }

    /// Waits for Network.framework's connection and stops listening, which removes
    /// the socket from the file system.
    pub(super) async fn accept(self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        check_peer(&stream)?;
        Ok(stream)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(SOCKET_NAME));
        let _ = fs::remove_dir(&self.dir);
    }
}

fn create_private_dir(parent: &Path) -> io::Result<PathBuf> {
    let mut attempts = 0;
    loop {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = parent.join(format!("{DIR_PREFIX}{}", &id[..DIR_ID_LEN]));
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => attempts += 1,
            Err(e) => return Err(e),
        }
    }
}

/// The directory already keeps other users out. This rejects a connection from
/// anywhere but this process, such as a process of the same user that found the path.
fn check_peer(stream: &UnixStream) -> io::Result<()> {
    let peer = stream.peer_cred()?;
    // SAFETY: `geteuid` has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    let own_pid = i32::try_from(std::process::id()).ok();
    if peer.uid() != own_uid || (peer.pid().is_some() && peer.pid() != own_pid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the TLS socket was connected to by another process",
        ));
    }
    Ok(())
}

/// What the pump learned about the real transport, for the stream that cannot
/// look at it itself.
#[derive(Default)]
pub(super) struct Health {
    closed: AtomicBool,
    error: Mutex<Option<(io::ErrorKind, String)>>,
}

impl Health {
    fn close(&self, error: Option<&io::Error>) {
        if let Some(error) = error {
            self.error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(|| (error.kind(), error.to_string()));
        }
        self.closed.store(true, Ordering::Release);
    }

    /// The transport reached end of stream, failed, or is no longer being pumped.
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The error that ended the transport, if it did not end cleanly.
    pub(super) fn error(&self) -> Option<io::Error> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(kind, message)| io::Error::new(*kind, message.clone()))
    }
}

/// Relays `socket` to and from `transport` until both have finished.
///
/// The pump stops after the returned guard is dropped, once the bytes already in
/// flight have been delivered or [`DRAIN`] has passed.
pub(super) fn spawn_pump(
    socket: UnixStream,
    transport: Box<dyn Stream>,
    health: Arc<Health>,
) -> DropGuard {
    let stop = CancellationToken::new();
    let guard = stop.clone().drop_guard();
    tokio::spawn(async move {
        let mut transport = Watched {
            inner: transport,
            health: Arc::clone(&health),
        };
        let mut socket = Socket(socket);
        let copy =
            copy_bidirectional_with_sizes(&mut socket, &mut transport, PUMP_BUFFER, PUMP_BUFFER);
        tokio::pin!(copy);
        tokio::select! {
            _ = &mut copy => {}
            () = stop.cancelled() => {
                let _ = timeout(DRAIN, &mut copy).await;
            }
        }
        // Marked before `socket` drops, so the connection never sees the end of the
        // stream while the transport still looks alive.
        health.close(None);
    });
    guard
}

/// The socket Network.framework connected to. It is gone once Network.framework
/// has closed the connection, which is usually before the server has finished
/// saying goodbye. What is left to relay then has nowhere to go, and says nothing
/// about the transport, so it must not cut the relay short.
struct Socket(UnixStream);

fn is_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
    )
}

impl AsyncRead for Socket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Socket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.0).poll_write(cx, buf) {
            Poll::Ready(Err(e)) if is_gone(&e) => Poll::Ready(Ok(buf.len())),
            poll => poll,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.0).poll_flush(cx) {
            Poll::Ready(Err(e)) if is_gone(&e) => Poll::Ready(Ok(())),
            poll => poll,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.0).poll_shutdown(cx) {
            Poll::Ready(Err(e)) if is_gone(&e) => Poll::Ready(Ok(())),
            poll => poll,
        }
    }
}

/// The transport, reporting to [`Health`] what happens to it.
struct Watched {
    inner: Box<dyn Stream>,
    health: Arc<Health>,
}

impl AsyncRead for Watched {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled = buf.filled().len();
        let has_room = buf.remaining() > 0;
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        match &poll {
            Poll::Ready(Ok(())) if has_room && buf.filled().len() == filled => {
                self.health.close(None);
            }
            Poll::Ready(Err(e)) => self.health.close(Some(e)),
            _ => {}
        }
        poll
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Err(e)) = &poll {
            self.health.close(Some(e));
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        if let Poll::Ready(Err(e)) = &poll {
            self.health.close(Some(e));
        }
        poll
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_shutdown(cx);
        if let Poll::Ready(Err(e)) = &poll {
            self.health.close(Some(e));
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    async fn within<T>(future: impl Future<Output = T>) -> T {
        timeout(TIMEOUT, future).await.expect("timed out")
    }

    async fn eventually(mut condition: impl FnMut() -> bool) {
        within(async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    fn overlong_parent() -> PathBuf {
        PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH)))
    }

    /// A pump between a Unix socket and a duplex pipe, with the two outer ends
    /// standing in for Network.framework and for the server.
    fn relay() -> (UnixStream, DuplexStream, Arc<Health>, DropGuard) {
        let (pumped, network_framework) = UnixStream::pair().unwrap();
        let (transport, server) = duplex(64 * 1024);
        let health = Arc::new(Health::default());
        let stop = spawn_pump(pumped, Box::new(transport), Arc::clone(&health));
        (network_framework, server, health, stop)
    }

    #[tokio::test]
    async fn the_socket_lives_in_a_directory_only_this_user_can_enter() {
        let bridge = Bridge::bind().unwrap();
        let path = bridge.socket_path();
        let dir = path.parent().unwrap();

        assert!(path.as_os_str().len() <= MAX_SOCKET_PATH);
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(DIR_PREFIX)
        );
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[tokio::test]
    async fn dropping_the_bridge_removes_its_directory() {
        let bridge = Bridge::bind().unwrap();
        let dir = bridge.socket_path().parent().unwrap().to_owned();
        assert!(dir.exists());

        drop(bridge);

        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn accepting_removes_the_socket_from_the_file_system() {
        let bridge = Bridge::bind().unwrap();
        let path = bridge.socket_path();
        let _client = UnixStream::connect(&path).await.unwrap();

        let _accepted = within(bridge.accept()).await.unwrap();

        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn another_process_connecting_is_refused() {
        let bridge = Bridge::bind().unwrap();
        let mut intruder = Command::new("nc")
            .arg("-U")
            .arg(bridge.socket_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nc ships with macOS");

        let result = within(bridge.accept()).await;

        let _ = intruder.kill();
        let _ = intruder.wait();
        let error = result.expect_err("the intruder was accepted");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn a_parent_whose_path_is_too_long_is_skipped() {
        let usable = std::env::temp_dir();

        let bridge = Bridge::bind_among([overlong_parent(), usable.clone()]).unwrap();

        assert!(bridge.socket_path().starts_with(&usable));
    }

    #[tokio::test]
    async fn binding_fails_when_no_parent_fits() {
        let error = Bridge::bind_among([overlong_parent()]).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = Bridge::bind_among([]).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[tokio::test]
    async fn binding_fails_when_the_parent_does_not_exist() {
        let missing = PathBuf::from("/mssql-tds-test-no-such-directory");

        let error = Bridge::bind_among([missing]).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn health_starts_open() {
        let health = Health::default();

        assert!(!health.is_closed());
        assert!(health.error().is_none());
    }

    #[test]
    fn health_keeps_the_first_error() {
        let health = Health::default();

        health.close(Some(&io::Error::new(io::ErrorKind::BrokenPipe, "first")));
        health.close(Some(&io::Error::new(io::ErrorKind::TimedOut, "second")));

        let error = health.error().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "first");
        assert!(health.is_closed());
    }

    #[test]
    fn a_clean_end_is_closed_without_an_error() {
        let health = Health::default();

        health.close(None);

        assert!(health.is_closed());
        assert!(health.error().is_none());
    }

    #[tokio::test]
    async fn the_socket_ignores_writes_once_its_peer_is_gone() {
        let (near, far) = UnixStream::pair().unwrap();
        drop(far);
        let mut socket = Socket(near);

        socket.write_all(b"too late").await.unwrap();
        socket.flush().await.unwrap();
        socket.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_plain_socket_reports_a_gone_peer() {
        let (mut near, far) = UnixStream::pair().unwrap();
        drop(far);

        assert!(near.write_all(b"too late").await.is_err());
    }

    #[tokio::test]
    async fn relays_in_both_directions() {
        let (mut network_framework, mut server, _health, _stop) = relay();

        network_framework.write_all(b"to the server").await.unwrap();
        let mut received = [0; 13];
        within(server.read_exact(&mut received)).await.unwrap();
        assert_eq!(&received, b"to the server");

        server.write_all(b"to the client").await.unwrap();
        within(network_framework.read_exact(&mut received))
            .await
            .unwrap();
        assert_eq!(&received, b"to the client");
    }

    #[tokio::test]
    async fn a_transport_that_ends_is_closed_but_not_failed() {
        let (_network_framework, server, health, _stop) = relay();

        drop(server);

        eventually(|| health.is_closed()).await;
        assert!(health.error().is_none());
    }

    #[tokio::test]
    async fn a_transport_that_fails_is_reported_with_its_error() {
        let (mut network_framework, server, health, _stop) = relay();
        drop(server);

        network_framework
            .write_all(b"nobody is listening")
            .await
            .unwrap();

        eventually(|| health.error().is_some()).await;
        assert_eq!(health.error().unwrap().kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn the_server_may_keep_talking_after_the_connection_is_gone() {
        let (network_framework, mut server, health, _stop) = relay();
        drop(network_framework);

        server.write_all(b"a late goodbye").await.unwrap();
        drop(server);

        eventually(|| health.is_closed()).await;
        assert!(health.error().is_none());
    }

    #[tokio::test]
    async fn bytes_in_flight_are_delivered_after_the_pump_is_stopped() {
        let (mut network_framework, mut server, health, stop) = relay();
        network_framework.write_all(b"close_notify").await.unwrap();

        drop(stop);

        let mut received = [0; 12];
        within(server.read_exact(&mut received)).await.unwrap();
        assert_eq!(&received, b"close_notify");
        eventually(|| health.is_closed()).await;
    }
}

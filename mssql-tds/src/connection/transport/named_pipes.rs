// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Named Pipe transport implementation for Windows
//!
//! This module provides Windows-specific functionality for connecting to SQL Server
//! via Named Pipes, including retry logic for busy pipe instances.

use crate::connection::transport::network_transport::Stream;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;
use tokio::net::windows::named_pipe::NamedPipeClient;
use tracing::{debug, info, warn};

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use winapi::shared::winerror::{ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED};
use winapi::um::namedpipeapi::{PeekNamedPipe, SetNamedPipeHandleState};
use winapi::um::winbase::{PIPE_READMODE_BYTE, PIPE_WAIT};

/// Timeout for Named Pipe connection attempts (matching ODBC's NP_OPEN_TIMEOUT)
pub(crate) const NAMED_PIPE_OPEN_TIMEOUT_MS: u32 = 5000;

// Well-known local-host identifiers for pipe-path rewriting.
const LOCAL_LOCALHOST: &str = "localhost";
const LOCAL_IPV4_LOOPBACK: &str = "127.0.0.1";
const LOCAL_IPV6_LOOPBACK: &str = "::1";

/// The pipe the client opens for one SQL Server Browser listed, as SNI
/// opens it (`ParseSsrpString`, ssrp.cpp): the listed path with its machine
/// name replaced by `server`, the server the connection was given, or by
/// `.` when that server is this computer.
///
/// SQL Browser returns paths like `\\COMPUTERNAME\pipe\MSSQL$INST\sql\query`,
/// named by the machine's own name, which this computer may not resolve (a
/// connection by IP address or another DNS name). Opening the local machine
/// as `\\COMPUTERNAME\pipe\...` would also go through SMB (network auth),
/// which often fails with "Access is denied"; `\\.\pipe\...` connects
/// through the local IPC namespace instead. A path that is not
/// `\\machine\...` is kept as listed.
pub(crate) fn pipe_on_server(listed: &str, server: &str) -> String {
    let Some(rest) = listed
        .strip_prefix(r"\\")
        .and_then(|after| after.find('\\').map(|at| &after[at..]))
    else {
        return listed.to_string();
    };
    if is_local_server(server) {
        format!(r"\\.{rest}")
    } else {
        format!(r"\\{server}{rest}")
    }
}

/// Whether `server` names this computer: SNI's `IsLocalHost` (`.`,
/// `(local)`, `localhost` or the computer's name), and the loopback
/// addresses, which reach the same local pipe.
fn is_local_server(server: &str) -> bool {
    server == "."
        || server.eq_ignore_ascii_case("(local)")
        || server.eq_ignore_ascii_case(LOCAL_LOCALHOST)
        || server == LOCAL_IPV4_LOOPBACK
        || server == LOCAL_IPV6_LOOPBACK
        || hostname::get()
            .ok()
            .and_then(|n| n.into_string().ok())
            .is_some_and(|name| server.eq_ignore_ascii_case(&name))
}

/// Opens a named pipe with retry logic to handle ERROR_PIPE_BUSY (231).
///
/// When all instances of a named pipe are busy, Windows returns ERROR_PIPE_BUSY.
/// This function uses WaitNamedPipeW to wait for a pipe instance to become available,
/// then retries the connection. This matches ODBC driver behavior.
///
/// Timeout: NAMED_PIPE_OPEN_TIMEOUT_MS (5000ms by default)
pub(crate) async fn open_named_pipe_with_retry(
    pipe_path: &str,
) -> std::io::Result<NamedPipeClient> {
    use std::time::Instant;
    use tokio::net::windows::named_pipe::ClientOptions;

    info!(pipe_path, "Opening named pipe connection");
    let start_time = Instant::now();
    let timeout_duration = Duration::from_millis(NAMED_PIPE_OPEN_TIMEOUT_MS as u64);

    loop {
        match ClientOptions::new()
            .pipe_mode(tokio::net::windows::named_pipe::PipeMode::Message)
            .open(pipe_path)
        {
            Ok(client) => {
                debug!(pipe_path, elapsed_ms = ?start_time.elapsed().as_millis(), "Named pipe connection established");
                return Ok(client);
            }
            Err(e) => {
                // ERROR_PIPE_BUSY - All pipe instances are busy
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                    let elapsed = start_time.elapsed();
                    warn!(pipe_path, elapsed_ms = ?elapsed.as_millis(), "Named pipe busy, waiting for available instance");
                    if elapsed >= timeout_duration {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "Named pipe connection timed out after {}ms: all pipe instances busy",
                                elapsed.as_millis()
                            ),
                        ));
                    }

                    // Calculate remaining timeout
                    let remaining_ms = timeout_duration
                        .checked_sub(elapsed)
                        .unwrap_or(Duration::from_millis(0))
                        .as_millis() as u32;

                    if remaining_ms == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Named pipe connection timed out: all pipe instances busy",
                        ));
                    }

                    // Wait for pipe to become available (synchronous Windows API call)
                    // Use spawn_blocking to avoid blocking the tokio runtime
                    let pipe_path_owned = pipe_path.to_string();
                    match tokio::task::spawn_blocking(crate::trace_context::propagate(move || {
                        wait_for_named_pipe(&pipe_path_owned, remaining_ms)
                    }))
                    .await
                    {
                        Ok(Ok(())) => {
                            // Pipe should be available now, retry CreateFile
                            debug!("Named pipe became available, retrying connection");
                            continue;
                        }
                        Ok(Err(wait_err)) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                format!(
                                    "Named pipe wait failed after {}ms: {}",
                                    elapsed.as_millis(),
                                    wait_err
                                ),
                            ));
                        }
                        Err(join_err) => {
                            return Err(std::io::Error::other(format!(
                                "Failed to wait for named pipe: {join_err}"
                            )));
                        }
                    }
                } else {
                    // For any other error, fail immediately
                    return Err(e);
                }
            }
        }
    }
}

/// Synchronous helper function that calls WaitNamedPipeW to wait for a pipe instance.
/// This function blocks until a pipe instance is available or the timeout expires.
///
/// # Arguments
/// * `pipe_path` - The full path to the named pipe (e.g., r"\\.\pipe\SQLLocal\MSSQLSERVER")
/// * `timeout_ms` - Timeout in milliseconds
fn wait_for_named_pipe(pipe_path: &str, timeout_ms: u32) -> std::io::Result<()> {
    use winapi::um::namedpipeapi::WaitNamedPipeW;

    debug!(pipe_path, timeout_ms, "Calling WaitNamedPipeW");

    // Convert pipe path to wide string (UTF-16)
    let wide_path: Vec<u16> = OsStr::new(pipe_path)
        .encode_wide()
        .chain(std::iter::once(0)) // Null terminator
        .collect();

    // Call WaitNamedPipeW (synchronous Windows API)
    // Returns:
    //   TRUE (non-zero) if a pipe instance is available
    //   FALSE (0) if timeout expires or error occurs
    let result = unsafe { WaitNamedPipeW(wide_path.as_ptr(), timeout_ms) };

    if result == 0 {
        // WaitNamedPipeW failed or timed out
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// Implementation of Stream trait for NamedPipeClient
impl Stream for NamedPipeClient {
    fn tls_handshake_starting(&mut self) {
        // Named Pipe is already in Message mode for atomic TLS writes
        debug!("TLS handshake starting on Named Pipe (Message mode)");
    }

    fn tls_handshake_completed(&mut self) {
        // Switch from Message mode to Byte mode for streaming reads.
        // Message mode is needed during TLS handshake for atomic writes,
        // but causes issues when reading multi-packet TDS responses because
        // each read() returns one message and then 0 bytes.
        // Byte mode treats the pipe as a stream, allowing proper TDS framing.
        debug!("TLS handshake completed, switching Named Pipe to Byte mode");

        let handle = self.as_raw_handle();
        let mut mode: u32 = PIPE_READMODE_BYTE | PIPE_WAIT;

        let result = unsafe {
            SetNamedPipeHandleState(
                handle as *mut _,
                &mut mode as *mut u32 as *mut _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };

        if result == 0 {
            let error = std::io::Error::last_os_error();
            warn!("Failed to switch Named Pipe to Byte mode: {}", error);
        } else {
            info!("Named Pipe switched to Byte mode for streaming reads");
        }
    }

    fn is_connection_dead(&self) -> bool {
        // Non-destructive liveness probe: PeekNamedPipe reports pipe status
        // without consuming buffered data (unlike a try_read), so it is safe on
        // an idle connection before issuing a command. A broken or disconnected
        // pipe surfaces as ERROR_BROKEN_PIPE / ERROR_PIPE_NOT_CONNECTED; success
        // (with or without buffered bytes) means the pipe is still open.
        let handle = self.as_raw_handle();
        let ok = unsafe {
            PeekNamedPipe(
                handle as *mut _,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            return false;
        }
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const LISTED: &str = r"\\SQL01\pipe\MSSQL$INST\sql\query";

    /// A remote server: the listed pipe on the server as given, whatever
    /// name the Browser's machine gave itself, as SNI opens it.
    #[test]
    fn a_listed_pipe_is_opened_on_the_server_as_given() {
        for server in ["db01.contoso.com", "10.1.2.3", "SQL01", "sql01"] {
            assert_eq!(
                pipe_on_server(LISTED, server),
                format!(r"\\{server}\pipe\MSSQL$INST\sql\query")
            );
        }
        assert_eq!(
            pipe_on_server(r"\\.\pipe\custom\path", "db01"),
            r"\\db01\pipe\custom\path"
        );
    }

    /// This computer, however it was named: the local IPC namespace.
    #[test]
    fn a_listed_pipe_on_this_computer_is_opened_locally() {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .expect("hostname required");
        for server in [
            ".",
            "(local)",
            "(LOCAL)",
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "::1",
            hostname.as_str(),
            &hostname.to_ascii_lowercase(),
        ] {
            assert_eq!(
                pipe_on_server(LISTED, server),
                r"\\.\pipe\MSSQL$INST\sql\query",
                "{server}"
            );
        }
    }

    /// A listed value that is not `\\machine\...` is kept as listed.
    #[test]
    fn a_listed_value_that_is_no_unc_pipe_is_kept() {
        for listed in [r"sql\query", r"\pipe", r"\\server"] {
            assert_eq!(pipe_on_server(listed, "db01"), listed);
        }
    }
}

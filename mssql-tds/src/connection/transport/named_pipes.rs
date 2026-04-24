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
use winapi::shared::winerror::ERROR_PIPE_BUSY;
use winapi::um::namedpipeapi::SetNamedPipeHandleState;
use winapi::um::winbase::{PIPE_READMODE_BYTE, PIPE_WAIT};

/// Timeout for Named Pipe connection attempts (matching ODBC's NP_OPEN_TIMEOUT)
pub(crate) const NAMED_PIPE_OPEN_TIMEOUT_MS: u32 = 5000;

// Well-known local-host identifiers for pipe-path rewriting.
const LOCAL_LOCALHOST: &str = "localhost";
const LOCAL_IPV4_LOOPBACK: &str = "127.0.0.1";
const LOCAL_IPV6_LOOPBACK: &str = "::1";

/// Rewrite the server component of a UNC pipe path to `.` when it refers to
/// the local machine.
///
/// SQL Browser returns paths like `\\COMPUTERNAME\pipe\MSSQL$INST\sql\query`.
/// Opening `\\COMPUTERNAME\pipe\...` goes through SMB (network auth), which
/// often fails with "Access is denied" even on the local machine. Using
/// `\\.\pipe\...` connects through the local IPC namespace instead, matching
/// ODBC/SNI behavior.
pub(crate) fn localize_pipe_path(pipe: &str) -> String {
    // Must start with \\
    if !pipe.starts_with("\\\\") {
        return pipe.to_string();
    }
    let after_prefix = &pipe[2..];
    let sep = match after_prefix.find('\\') {
        Some(pos) => pos,
        None => return pipe.to_string(),
    };
    let server_part = &after_prefix[..sep];

    // Already local
    if server_part == "." {
        return pipe.to_string();
    }

    let is_local = server_part.eq_ignore_ascii_case(LOCAL_LOCALHOST)
        || server_part == LOCAL_IPV4_LOOPBACK
        || server_part == LOCAL_IPV6_LOOPBACK
        || hostname::get()
            .ok()
            .and_then(|n| n.into_string().ok())
            .is_some_and(|name| server_part.eq_ignore_ascii_case(&name));

    if is_local {
        format!("\\\\.{}", &after_prefix[sep..])
    } else {
        pipe.to_string()
    }
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
                    match tokio::task::spawn_blocking(move || {
                        wait_for_named_pipe(&pipe_path_owned, remaining_ms)
                    })
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
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localize_already_local_dot() {
        let path = r"\\.\pipe\sql\query";
        assert_eq!(localize_pipe_path(path), path);
    }

    #[test]
    fn localize_localhost() {
        assert_eq!(
            localize_pipe_path(r"\\localhost\pipe\sql\query"),
            r"\\.\pipe\sql\query"
        );
    }

    #[test]
    fn localize_localhost_case_insensitive() {
        assert_eq!(
            localize_pipe_path(r"\\LOCALHOST\pipe\sql\query"),
            r"\\.\pipe\sql\query"
        );
    }

    #[test]
    fn localize_ipv4_loopback() {
        assert_eq!(
            localize_pipe_path(r"\\127.0.0.1\pipe\sql\query"),
            r"\\.\pipe\sql\query"
        );
    }

    #[test]
    fn localize_ipv6_loopback() {
        assert_eq!(
            localize_pipe_path(r"\\::1\pipe\sql\query"),
            r"\\.\pipe\sql\query"
        );
    }

    #[test]
    fn localize_hostname_match() {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .expect("hostname required");
        let input = format!(r"\\{}\pipe\sql\query", hostname);
        assert_eq!(localize_pipe_path(&input), r"\\.\pipe\sql\query");
    }

    #[test]
    fn localize_remote_server_unchanged() {
        let path = r"\\remoteserver\pipe\sql\query";
        assert_eq!(localize_pipe_path(path), path);
    }

    #[test]
    fn localize_no_prefix_unchanged() {
        assert_eq!(localize_pipe_path("sql\\query"), "sql\\query");
    }

    #[test]
    fn localize_single_backslash_unchanged() {
        assert_eq!(localize_pipe_path(r"\pipe"), r"\pipe");
    }

    #[test]
    fn localize_no_separator_after_prefix() {
        assert_eq!(localize_pipe_path(r"\\server"), r"\\server");
    }
}

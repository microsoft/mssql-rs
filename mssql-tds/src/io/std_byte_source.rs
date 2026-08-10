// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The blocking byte source over an owned `std::net::TcpStream`.
//!
//! [`StdTcpByteSource`] is the production implementor of [`BlockingByteSource`]:
//! the synchronous fetch edge (`TdsSyncClient`) reads TDS packets straight off a
//! blocking socket, with no reactor. It owns the R1 slice-poll edge policy — a
//! bounded blocking read with an atomic cancel-check *between* slices — so
//! timeout and cancellation ride the same receive edge the async
//! [`ReceiveGuard`](crate::io::byte_source::ReceiveGuard) uses, at zero hot-path
//! cost and without a running tokio runtime.

use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::core::TdsResult;
use crate::error::Error::{OperationCancelledError, TimeoutError};
use crate::error::TimeoutErrorType;
use crate::io::byte_source::BlockingByteSource;

/// Duration of one blocking read slice. A stalled read wakes at this cadence so
/// the cancel token and request deadline are re-checked between slices; shorter
/// slices tighten cancel latency at the cost of more wakeups.
const SLICE_TIMEOUT: Duration = Duration::from_millis(100);

/// A [`BlockingByteSource`] over an owned blocking [`TcpStream`].
pub(crate) struct StdTcpByteSource {
    stream: TcpStream,
    /// Cooperative cancel shared with the owning client; observed between read
    /// slices (a blocked `read` cannot itself be interrupted on Windows).
    cancel: Option<CancellationToken>,
    /// Absolute instant the in-flight fetch must complete by, refreshed by the
    /// owning client before each request. `None` waits indefinitely.
    deadline: Option<Instant>,
}

impl StdTcpByteSource {
    /// Wraps an established blocking socket, arming the slice cadence so cancel
    /// and deadline checks interleave with reads.
    pub(crate) fn new(stream: TcpStream, cancel: Option<CancellationToken>) -> TdsResult<Self> {
        stream.set_read_timeout(Some(SLICE_TIMEOUT))?;
        Ok(Self {
            stream,
            cancel,
            deadline: None,
        })
    }

    /// Sets the absolute deadline for subsequent reads (the owning client derives
    /// it from the per-request timeout before each fetch).
    pub(crate) fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// Consumes the source and returns the owned socket, so the client can revert
    /// it to an async tokio stream.
    pub(crate) fn into_stream(self) -> TcpStream {
        self.stream
    }
}

impl BlockingByteSource for StdTcpByteSource {
    fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        loop {
            if let Some(cancel) = &self.cancel
                && cancel.is_cancelled()
            {
                return Err(OperationCancelledError(
                    "blocking receive cancelled".to_string(),
                ));
            }
            if let Some(deadline) = self.deadline
                && Instant::now() >= deadline
            {
                return Err(TimeoutError(TimeoutErrorType::String(
                    "blocking receive deadline elapsed".to_string(),
                )));
            }
            match self.stream.read(buffer) {
                Ok(n) => return Ok(n),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    // Slice expired (or a signal interrupted the read) without new
                    // bytes; loop to re-check cancel/deadline, then read again.
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

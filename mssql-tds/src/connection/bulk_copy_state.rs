// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Timeout state tracking for bulk copy operations.
//!
//! This module provides the `BulkCopyTimeoutState` struct that tracks timeout
//! state during bulk copy operations, including attention packet handling.
//!
//! The timeout mechanism follows the same pattern as Microsoft.Data.SqlClient's
//! SqlBulkCopy implementation:
//! - Operation timeout is tracked from the start of the bulk copy
//! - When timeout expires, an attention packet is sent to cancel the operation
//! - A separate 5-second timeout is used for waiting on the attention ACK
//! - If no attention ACK is received within 5 seconds, the connection is marked broken

use std::time::{Duration, Instant};

use crate::error::Error;

/// Default timeout budget for ATTENTION handling.
///
/// Interrupted token and row reads share this single five-second deadline across
/// sending ATTENTION, completing the in-flight parser, and draining through the
/// terminal acknowledgement. This follows Microsoft.Data.SqlClient and favors
/// prompt, bounded cancellation: an unusually large or slow response may retire
/// a connection that could eventually resynchronize rather than waiting for
/// msodbcsql's longer (up to 120-second) drain budget.
pub const ATTENTION_TIMEOUT_SECONDS: u64 = 5;

/// Tracks timeout state for bulk copy operations.
///
/// This struct manages the timeout deadline and attention packet state during
/// bulk copy operations. It follows the same pattern as SqlClient's timeout
/// handling in TdsParserStateObject.
///
/// # Timeout Flow
///
/// 1. When bulk copy starts, a deadline is set based on `timeout_sec`
/// 2. During writes, `is_expired()` is checked periodically
/// 3. If expired and no attention sent yet, `attention_sent` is set and attention is sent
/// 4. After sending attention, `set_attention_timeout()` sets a 5-second deadline
/// 5. If attention ACK not received within 5 seconds, connection is marked broken
///
/// # Error Preservation
///
/// When attention is sent, existing errors are stored in `pre_attention_errors`
/// and restored after attention processing. This ensures error information
/// is not lost during the attention handling sequence.
#[derive(Debug)]
pub(crate) struct BulkCopyTimeoutState {
    /// When the timeout expires (None = infinite timeout)
    deadline: Option<Instant>,

    /// Whether attention has been sent to the server
    attention_sent: bool,

    /// Whether attention ACK has been received from the server
    attention_received: bool,

    /// Whether we're in the process of sending attention
    attention_sending: bool,

    /// Whether a write timeout occurred during bulk copy
    /// This is set before sending attention to differentiate from other timeout scenarios
    bulk_copy_write_timeout: bool,

    /// Pre-attention errors that need to be preserved during attention processing
    #[allow(dead_code)] // wired in upcoming bulk copy attention flow
    pre_attention_errors: Vec<Error>,
}

impl BulkCopyTimeoutState {
    /// Create a new timeout state with the specified timeout duration.
    ///
    /// # Arguments
    ///
    /// * `timeout` - The timeout duration, or `None` for infinite timeout
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::time::Duration;
    /// use mssql_tds::connection::bulk_copy_state::BulkCopyTimeoutState;
    ///
    /// // 30-second timeout
    /// let state = BulkCopyTimeoutState::new(Some(Duration::from_secs(30)));
    ///
    /// // Infinite timeout
    /// let infinite_state = BulkCopyTimeoutState::new(None);
    /// ```
    pub fn new(timeout: Option<Duration>) -> Self {
        let deadline = timeout.map(|t| Instant::now() + t);
        Self {
            deadline,
            attention_sent: false,
            attention_received: false,
            attention_sending: false,
            bulk_copy_write_timeout: false,
            pre_attention_errors: Vec::new(),
        }
    }

    /// Create a new timeout state from a timeout in seconds.
    ///
    /// A value of 0 means infinite timeout.
    ///
    /// # Arguments
    ///
    /// * `timeout_sec` - The timeout in seconds, or 0 for infinite timeout
    pub fn from_seconds(timeout_sec: u32) -> Self {
        if timeout_sec == 0 {
            Self::new(None)
        } else {
            Self::new(Some(Duration::from_secs(timeout_sec as u64)))
        }
    }

    /// Check if the timeout has expired.
    ///
    /// Returns `true` if the current time is past the deadline.
    /// Returns `false` if no deadline is set (infinite timeout).
    #[inline]
    pub fn is_expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// Get the remaining time in milliseconds until the deadline.
    ///
    /// Returns `None` if no deadline is set (infinite timeout).
    /// Returns `0` if the deadline has already passed.
    pub fn remaining_ms(&self) -> Option<u64> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u64)
    }

    /// Get the remaining time as a Duration.
    ///
    /// Returns `None` if no deadline is set (infinite timeout).
    /// Returns `Duration::ZERO` if the deadline has already passed.
    #[cfg(test)]
    pub fn remaining_duration(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Set the timeout for attention acknowledgment.
    ///
    /// This sets a 5-second deadline for receiving the attention ACK from the server.
    /// This should be called immediately after sending the attention packet.
    ///
    /// If no attention ACK is received within this timeout, the connection should
    /// be marked as broken.
    pub fn set_attention_timeout(&mut self) {
        self.deadline = Some(Instant::now() + Duration::from_secs(ATTENTION_TIMEOUT_SECONDS));
    }

    /// Check if attention has been sent.
    #[inline]
    pub fn is_attention_sent(&self) -> bool {
        self.attention_sent
    }

    /// Check if attention ACK has been received.
    #[cfg(test)]
    #[inline]
    pub fn is_attention_received(&self) -> bool {
        self.attention_received
    }

    /// Check if we're currently sending attention.
    #[cfg(test)]
    #[inline]
    pub fn is_attention_sending(&self) -> bool {
        self.attention_sending
    }

    /// Check if a bulk copy write timeout occurred.
    #[cfg(test)]
    #[inline]
    pub fn is_bulk_copy_write_timeout(&self) -> bool {
        self.bulk_copy_write_timeout
    }

    /// Mark that we're about to send attention.
    pub fn begin_sending_attention(&mut self) {
        self.attention_sending = true;
    }

    /// Mark that attention has been sent.
    ///
    /// This also sets the attention timeout (5 seconds) for receiving the ACK.
    pub fn mark_attention_sent(&mut self) {
        self.attention_sending = false;
        self.attention_sent = true;
        self.set_attention_timeout();
    }

    /// Mark that attention ACK has been received.
    pub fn mark_attention_received(&mut self) {
        self.attention_received = true;
    }

    /// Mark that a bulk copy write timeout occurred.
    ///
    /// This should be called before sending attention to indicate that
    /// the timeout was during a bulk copy write operation.
    pub fn mark_bulk_copy_write_timeout(&mut self) {
        self.bulk_copy_write_timeout = true;
    }

    /// Store errors before attention processing.
    ///
    /// During attention processing, new tokens may be received that could
    /// contain additional errors. This method stores the current errors
    /// so they can be restored after attention processing.
    ///
    /// # Arguments
    ///
    /// * `errors` - The errors to preserve during attention processing
    #[cfg(test)]
    pub fn store_errors_for_attention(&mut self, errors: Vec<Error>) {
        self.pre_attention_errors = errors;
    }

    /// Restore errors after attention processing.
    ///
    /// Returns the errors that were stored before attention processing.
    /// This consumes the stored errors.
    #[cfg(test)]
    pub fn restore_errors_after_attention(&mut self) -> Vec<Error> {
        std::mem::take(&mut self.pre_attention_errors)
    }

    /// Reset attention state for reuse.
    ///
    /// This resets the attention-related flags but preserves the deadline.
    /// Use this after successfully processing an attention ACK.
    #[cfg(test)]
    pub fn reset_attention_state(&mut self) {
        self.attention_sent = false;
        self.attention_received = false;
        self.attention_sending = false;
        self.bulk_copy_write_timeout = false;
        self.pre_attention_errors.clear();
    }

    /// Check if attention handling is complete (sent and received).
    #[cfg(test)]
    pub fn is_attention_complete(&self) -> bool {
        self.attention_sent && self.attention_received
    }
}

impl Default for BulkCopyTimeoutState {
    /// Create a default timeout state with no timeout (infinite).
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_state_creation() {
        let state = BulkCopyTimeoutState::new(Some(Duration::from_secs(30)));
        assert!(!state.is_expired());
        assert!(!state.is_attention_sent());
        assert!(!state.is_attention_received());
        assert!(!state.is_attention_sending());
        assert!(!state.is_bulk_copy_write_timeout());
    }

    #[test]
    fn test_timeout_state_from_seconds() {
        // Non-zero timeout
        let state = BulkCopyTimeoutState::from_seconds(30);
        assert!(!state.is_expired());
        assert!(state.remaining_ms().is_some());

        // Zero means infinite
        let infinite = BulkCopyTimeoutState::from_seconds(0);
        assert!(!infinite.is_expired());
        assert!(infinite.remaining_ms().is_none());
    }

    #[test]
    fn test_infinite_timeout_never_expires() {
        let state = BulkCopyTimeoutState::new(None);
        assert!(!state.is_expired());
        assert!(state.remaining_ms().is_none());
        assert!(state.remaining_duration().is_none());
    }

    #[test]
    fn test_timeout_expiry() {
        // Create a state with a very short timeout
        let state = BulkCopyTimeoutState::new(Some(Duration::from_millis(10)));
        assert!(!state.is_expired());

        // Sleep until timeout expires
        std::thread::sleep(Duration::from_millis(20));
        assert!(state.is_expired());
    }

    #[test]
    fn test_remaining_ms_calculation() {
        let state = BulkCopyTimeoutState::new(Some(Duration::from_secs(10)));
        let remaining = state.remaining_ms().unwrap();
        // Should be close to 10000ms, but allow some tolerance
        assert!(remaining > 9900);
        assert!(remaining <= 10000);
    }

    #[test]
    fn test_attention_timeout_5_seconds() {
        let mut state = BulkCopyTimeoutState::new(None);
        state.set_attention_timeout();

        // Should be set to approximately 5 seconds
        let remaining = state.remaining_ms().unwrap();
        assert!(remaining > 4900);
        assert!(remaining <= 5000);
    }

    #[test]
    fn test_attention_state_transitions() {
        let mut state = BulkCopyTimeoutState::new(Some(Duration::from_secs(30)));

        // Initial state
        assert!(!state.is_attention_sending());
        assert!(!state.is_attention_sent());
        assert!(!state.is_attention_received());

        // Begin sending
        state.begin_sending_attention();
        assert!(state.is_attention_sending());
        assert!(!state.is_attention_sent());

        // Finish sending
        state.mark_attention_sent();
        assert!(!state.is_attention_sending());
        assert!(state.is_attention_sent());

        // Receive ACK
        state.mark_attention_received();
        assert!(state.is_attention_received());
        assert!(state.is_attention_complete());
    }

    #[test]
    fn test_bulk_copy_write_timeout_flag() {
        let mut state = BulkCopyTimeoutState::new(Some(Duration::from_secs(30)));
        assert!(!state.is_bulk_copy_write_timeout());

        state.mark_bulk_copy_write_timeout();
        assert!(state.is_bulk_copy_write_timeout());
    }

    #[test]
    fn test_error_preservation() {
        let mut state = BulkCopyTimeoutState::new(None);

        // Store some errors
        let errors = vec![
            Error::ProtocolError("Error 1".to_string()),
            Error::ProtocolError("Error 2".to_string()),
        ];
        state.store_errors_for_attention(errors);

        // Restore and verify
        let restored = state.restore_errors_after_attention();
        assert_eq!(restored.len(), 2);

        // After restoration, errors should be empty
        let empty = state.restore_errors_after_attention();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_reset_attention_state() {
        let mut state = BulkCopyTimeoutState::new(Some(Duration::from_secs(30)));

        // Set all flags
        state.mark_bulk_copy_write_timeout();
        state.begin_sending_attention();
        state.mark_attention_sent();
        state.mark_attention_received();
        state.store_errors_for_attention(vec![Error::ProtocolError("test".to_string())]);

        // Reset
        state.reset_attention_state();

        // All flags should be cleared
        assert!(!state.is_attention_sending());
        assert!(!state.is_attention_sent());
        assert!(!state.is_attention_received());
        assert!(!state.is_bulk_copy_write_timeout());
        assert!(state.restore_errors_after_attention().is_empty());
    }

    #[test]
    fn test_default_is_infinite() {
        let state = BulkCopyTimeoutState::default();
        assert!(!state.is_expired());
        assert!(state.remaining_ms().is_none());
    }
}

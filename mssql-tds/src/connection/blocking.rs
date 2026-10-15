// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::TdsResult;
use crate::error::Error;

/// Runs `work`, which blocks on name resolution or a system library, on a
/// blocking thread. On the runtime's own thread a stalled call would keep the
/// caller's timeout and cancellation from ever being polled; dropping the
/// returned future instead abandons the call to finish on its own thread.
pub(crate) async fn off_runtime_thread<T: Send + 'static>(
    work: impl FnOnce() -> TdsResult<T> + Send + 'static,
) -> TdsResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::ImplementationError(format!("a blocking task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn a_stalled_call_does_not_keep_the_timeout_from_firing() {
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let work = off_runtime_thread(move || {
            let _ = stalled.recv();
            Ok(())
        });
        let outcome = tokio::time::timeout(Duration::from_millis(100), work).await;
        assert!(
            outcome.is_err(),
            "the timeout must fire while the call is stalled"
        );
        drop(release);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_result_of_the_call_is_returned() {
        let value = off_runtime_thread(|| Ok(7)).await.unwrap();
        assert_eq!(value, 7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicking_call_is_an_error_not_a_crash() {
        let outcome = off_runtime_thread::<()>(|| panic!("boom")).await;
        assert!(matches!(outcome, Err(Error::ImplementationError(_))));
    }
}

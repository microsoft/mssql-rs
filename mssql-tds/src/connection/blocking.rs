// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::TdsResult;
use crate::error::Error;

/// Runs `work`, which blocks on name resolution or a system library, on a
/// blocking thread, with the connection trace context and (on Windows) the
/// impersonated identity of the caller, as they are when this is called. On
/// the runtime's own thread a stalled call would keep the caller's timeout
/// and cancellation from ever being polled; dropping the returned future
/// instead abandons the call to finish on its own thread.
pub(crate) fn off_runtime_thread<T: Send + 'static>(
    work: impl FnOnce() -> TdsResult<T> + Send + 'static,
) -> impl Future<Output = TdsResult<T>> {
    let identity = CallerIdentity::capture();
    let work = crate::trace_context::propagate(work);
    async move {
        let identity = identity?;
        tokio::task::spawn_blocking(move || identity.run(work))
            .await
            .map_err(|error| {
                Error::ImplementationError(format!("a blocking task failed: {error}"))
            })?
    }
}

/// The identity a worker thread takes on for the caller: the caller's
/// impersonation token, when its thread impersonates a client. Credential
/// acquisition (SSPI's default credentials) and LocalDB use the thread's
/// identity, which a pool thread does not otherwise inherit.
#[cfg(windows)]
struct CallerIdentity(Option<impersonation::Token>);

#[cfg(windows)]
impl CallerIdentity {
    fn capture() -> TdsResult<Self> {
        impersonation::Token::of_current_thread().map(Self)
    }

    /// Runs `work` impersonating the caller's token, if any, and reverts
    /// before returning, even if `work` panics.
    fn run<T>(self, work: impl FnOnce() -> TdsResult<T>) -> TdsResult<T> {
        let Some(token) = self.0 else {
            return work();
        };
        let _impersonating = token.impersonate()?;
        work()
    }
}

#[cfg(not(windows))]
struct CallerIdentity;

#[cfg(not(windows))]
impl CallerIdentity {
    fn capture() -> TdsResult<Self> {
        Ok(Self)
    }

    fn run<T>(self, work: impl FnOnce() -> TdsResult<T>) -> TdsResult<T> {
        work()
    }
}

#[cfg(windows)]
mod impersonation {
    use crate::core::TdsResult;
    use crate::security::SecurityError;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_TOKEN, HANDLE};
    use windows_sys::Win32::Security::{RevertToSelf, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, OpenThreadToken, SetThreadToken,
    };

    /// An open thread token handle, closed on drop.
    pub(super) struct Token(HANDLE);

    // SAFETY: a token handle is a kernel object handle, usable from any thread.
    unsafe impl Send for Token {}

    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a handle this owns, opened by `OpenThreadToken`.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn error(action: &str, error: std::io::Error) -> crate::error::Error {
        SecurityError::InternalError(format!("could not {action}: {error}")).into()
    }

    impl Token {
        /// The current thread's impersonation token, or `None` when the
        /// thread does not impersonate. Any other failure is an error, rather
        /// than letting the work run as the process identity.
        pub(super) fn of_current_thread() -> TdsResult<Option<Self>> {
            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: `GetCurrentThread` returns a pseudo-handle valid on this
            // thread; `token` receives an owned handle on success.
            let opened = unsafe {
                OpenThreadToken(
                    GetCurrentThread(),
                    TOKEN_IMPERSONATE | TOKEN_QUERY,
                    1,
                    &mut token,
                )
            };
            if opened != 0 {
                return Ok(Some(Self(token)));
            }
            let os_error = std::io::Error::last_os_error();
            if os_error.raw_os_error() == Some(ERROR_NO_TOKEN as i32) {
                Ok(None)
            } else {
                Err(error(
                    "read the calling thread's impersonation token",
                    os_error,
                ))
            }
        }

        /// Makes the current thread impersonate this token until the returned
        /// guard is dropped.
        pub(super) fn impersonate(&self) -> TdsResult<Reverting> {
            // SAFETY: a null thread pointer means the current thread; `self.0`
            // is an open token handle with TOKEN_IMPERSONATE access.
            if unsafe { SetThreadToken(std::ptr::null(), self.0) } == 0 {
                return Err(error(
                    "impersonate the caller on a worker thread",
                    std::io::Error::last_os_error(),
                ));
            }
            Ok(Reverting)
        }
    }

    /// Reverts the current thread to its own identity when dropped, also
    /// while unwinding.
    pub(super) struct Reverting;

    impl Drop for Reverting {
        fn drop(&mut self) {
            // SAFETY: no arguments; ends this thread's impersonation.
            if unsafe { RevertToSelf() } == 0 {
                // A pool thread left impersonating would run later work as
                // the caller's client; that must never happen.
                std::process::abort();
            }
        }
    }

    /// Whether the current thread impersonates.
    #[cfg(test)]
    pub(super) fn current_thread_impersonates() -> bool {
        Token::of_current_thread().unwrap().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn the_work_sees_the_callers_trace_context() {
        use crate::trace_context::{ConnectionTrace, Context, enable, snapshot};
        use std::sync::Arc;

        enable();
        let connection = Arc::new(ConnectionTrace::default());
        let id = uuid::Uuid::new_v4();
        connection.establish(id);
        let work = {
            let _scope = Context::connection(Arc::clone(&connection)).enter();
            off_runtime_thread(|| Ok(snapshot()))
        };
        let seen = work.await.unwrap();
        assert_eq!(seen.cid, Some(id));
        assert!(seen.dbc.is_some());
    }

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

    #[cfg(windows)]
    mod windows_identity {
        use super::super::impersonation::current_thread_impersonates;
        use super::super::*;
        use windows_sys::Win32::Security::{ImpersonateSelf, RevertToSelf, SecurityImpersonation};

        /// Makes the current thread impersonate its own process identity
        /// until dropped.
        struct Impersonating;

        impl Impersonating {
            fn start() -> Self {
                // SAFETY: no pointers; makes this thread impersonate.
                assert_ne!(unsafe { ImpersonateSelf(SecurityImpersonation) }, 0);
                assert!(current_thread_impersonates());
                Self
            }
        }

        impl Drop for Impersonating {
            fn drop(&mut self) {
                // SAFETY: no arguments; ends this thread's impersonation.
                unsafe { RevertToSelf() };
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn the_work_runs_as_the_impersonating_caller_and_the_worker_reverts() {
            let work = {
                let _caller = Impersonating::start();
                off_runtime_thread(|| Ok(current_thread_impersonates()))
            };
            assert!(!current_thread_impersonates());
            assert!(work.await.unwrap(), "the work did not impersonate");

            let later = tokio::task::spawn_blocking(current_thread_impersonates)
                .await
                .unwrap();
            assert!(!later, "a blocking thread was left impersonating");
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_caller_that_does_not_impersonate_runs_the_work_as_itself() {
            assert!(!current_thread_impersonates());
            let impersonated = off_runtime_thread(|| Ok(current_thread_impersonates()))
                .await
                .unwrap();
            assert!(!impersonated);
        }

        /// On the same thread: the worker reverts after the work, and also
        /// when the work panics.
        #[test]
        fn the_worker_thread_reverts_even_when_the_work_panics() {
            let (first, second) = {
                let _caller = Impersonating::start();
                (
                    CallerIdentity::capture().unwrap(),
                    CallerIdentity::capture().unwrap(),
                )
            };
            std::thread::spawn(move || {
                let during = first.run(|| Ok(current_thread_impersonates())).unwrap();
                assert!(during);
                assert!(!current_thread_impersonates());

                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    second.run::<()>(|| {
                        assert!(current_thread_impersonates());
                        panic!("boom")
                    })
                }));
                assert!(panicked.is_err());
                assert!(!current_thread_impersonates());
            })
            .join()
            .unwrap();
        }
    }
}

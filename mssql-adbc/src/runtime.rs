// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;

use adbc_core::error::{Error, Result, Status};
use tokio::runtime::{Handle, Runtime};

pub(crate) fn process_is_shutting_down() -> bool {
    #[cfg(windows)]
    {
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn RtlDllShutdownInProgress() -> u8;
        }
        // SAFETY: ntdll's loader flag is a no-argument function returning BOOLEAN.
        unsafe { RtlDllShutdownInProgress() != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(crate) struct Executor(Option<Runtime>);

impl Executor {
    pub(crate) fn new() -> Result<Self> {
        Runtime::new()
            .map(|runtime| Self(Some(runtime)))
            .map_err(|error| Error::with_message_and_status(error.to_string(), Status::Internal))
    }

    pub(crate) fn run<F, T>(&self, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send,
        T: Send,
    {
        let runtime = self
            .0
            .as_ref()
            .ok_or_else(|| crate::error::state("runtime closed"))?;
        if Handle::try_current().is_ok() {
            let identity = mssql_tds::CallerIdentity::capture().map_err(crate::error::tds)?;
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        identity
                            .run(|| Ok(runtime.block_on(future)))
                            .map_err(crate::error::tds)?
                    })
                    .join()
                    .map_err(|_| {
                        Error::with_message_and_status(
                            "execution thread panicked",
                            Status::Internal,
                        )
                    })?
            })
        } else {
            runtime.block_on(future)
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            if process_is_shutting_down() {
                // The Windows loader has already terminated threads a runtime drop would join.
                std::mem::forget(runtime);
                return;
            }
            if Handle::try_current().is_ok() {
                std::thread::scope(|scope| {
                    if scope.spawn(|| drop(runtime)).join().is_err() {
                        tracing::error!("ADBC runtime shutdown thread panicked");
                    }
                });
            } else {
                drop(runtime);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_without_an_ambient_runtime() {
        let runtime = Executor::new().unwrap();
        assert_eq!(runtime.run(async { Ok(7) }).unwrap(), 7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_and_drops_inside_current_thread_runtime() {
        let runtime = Executor::new().unwrap();
        assert_eq!(runtime.run(async { Ok(7) }).unwrap(), 7);
    }
}

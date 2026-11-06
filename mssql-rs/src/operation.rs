// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

use std::ops::{Deref, DerefMut};

use mssql_tds::connection::tds_client::TdsClient;

use crate::error::{Error, Result};

pub(crate) trait ConnectionHealth {
    fn is_dead(&self) -> bool;
    fn mark_dead(&mut self);
}

impl ConnectionHealth for TdsClient {
    fn is_dead(&self) -> bool {
        self.is_connection_dead()
    }

    fn mark_dead(&mut self) {
        self.mark_connection_dead();
    }
}

pub(crate) fn ensure_usable(client: &impl ConnectionHealth) -> Result<()> {
    if client.is_dead() {
        return Err(Error::Tds(mssql_tds::error::Error::ConnectionClosed(
            "Connection is known dead; discard this Client and reconnect".into(),
        )));
    }
    Ok(())
}

// Cancellation can abandon native parser/writer state. Drop must not attempt I/O.
pub(crate) struct Operation<'a, C: ConnectionHealth = TdsClient> {
    client: &'a mut C,
    armed: bool,
}

impl<'a, C: ConnectionHealth> Operation<'a, C> {
    pub(crate) fn new(client: &'a mut C) -> Result<Self> {
        ensure_usable(client)?;
        Ok(Self {
            client,
            armed: true,
        })
    }

    pub(crate) fn complete<T>(mut self, result: T) -> T {
        self.armed = false;
        result
    }
}

impl<C: ConnectionHealth> Deref for Operation<'_, C> {
    type Target = C;

    fn deref(&self) -> &C {
        self.client
    }
}

impl<C: ConnectionHealth> DerefMut for Operation<'_, C> {
    fn deref_mut(&mut self) -> &mut C {
        self.client
    }
}

impl<C: ConnectionHealth> Drop for Operation<'_, C> {
    fn drop(&mut self) {
        if self.armed {
            self.client.mark_dead();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use futures_util::{StreamExt, poll};

    use super::*;

    #[derive(Default)]
    struct Health {
        dead: bool,
        marks: usize,
    }

    impl ConnectionHealth for Health {
        fn is_dead(&self) -> bool {
            self.dead
        }

        fn mark_dead(&mut self) {
            self.dead = true;
            self.marks += 1;
        }
    }

    #[tokio::test]
    async fn dropping_pending_operation_marks_dead() {
        let mut health = Health::default();
        let mut future = Box::pin(async {
            let operation = Operation::new(&mut health).expect("connection is healthy");
            pending::<()>().await;
            operation.complete(())
        });
        assert!(poll!(future.as_mut()).is_pending());
        drop(future);
        assert!(health.dead);
        assert_eq!(health.marks, 1);
    }

    #[tokio::test]
    async fn dropping_stream_between_completed_operations_preserves_health() {
        let mut health = Health::default();
        let borrowed = &mut health;
        let mut stream = Box::pin(async_stream::stream! {
            Operation::new(borrowed).expect("connection is healthy").complete(());
            yield 1;
            let operation = Operation::new(borrowed).expect("completed operation preserves health");
            pending::<()>().await;
            operation.complete(());
            yield 2;
        });
        assert_eq!(stream.next().await, Some(1));
        drop(stream);
        assert!(!health.dead);
    }

    #[tokio::test]
    async fn dropping_stream_with_pending_read_marks_dead() {
        let mut health = Health::default();
        let borrowed = &mut health;
        let mut stream = Box::pin(async_stream::stream! {
            Operation::new(borrowed).expect("connection is healthy").complete(());
            yield 1;
            let operation = Operation::new(borrowed).expect("completed operation preserves health");
            pending::<()>().await;
            operation.complete(());
            yield 2;
        });
        assert_eq!(stream.next().await, Some(1));
        assert!(poll!(stream.next()).is_pending());
        drop(stream);
        assert!(health.dead);
        assert_eq!(health.marks, 1);
    }

    #[tokio::test]
    async fn dropping_next_retains_guard_until_stream_resumes() {
        let mut health = Health::default();
        let borrowed = &mut health;
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let mut stream = Box::pin(async_stream::stream! {
            let operation = Operation::new(borrowed).expect("connection is healthy");
            let result = receiver.await;
            operation.complete(result).expect("sender should deliver completion");
            yield 1;
        });
        assert!(poll!(stream.next()).is_pending());
        sender.send(()).expect("receiver is still alive");
        assert_eq!(stream.next().await, Some(1));
        drop(stream);
        assert!(!health.dead);
    }

    #[test]
    fn dropping_unpolled_operation_does_not_mark_dead() {
        let mut health = Health::default();
        let future = async {
            let operation = Operation::new(&mut health).expect("connection is healthy");
            pending::<()>().await;
            operation.complete(())
        };
        drop(future);
        assert!(!health.dead);
    }

    #[test]
    fn completed_success_and_error_do_not_mark_dead() {
        let mut health = Health::default();
        assert_eq!(
            Operation::new(&mut health)
                .expect("connection is healthy")
                .complete(42),
            42
        );
        let result: Result<()> = Operation::new(&mut health)
            .expect("completed operation preserves health")
            .complete(Err(Error::Conversion("test error".into())));
        assert!(matches!(result, Err(Error::Conversion(_))));
        assert!(!health.dead);
        assert_eq!(health.marks, 0);
    }

    #[test]
    fn completion_does_not_revive_native_dead_state() {
        let mut health = Health::default();
        let mut operation = Operation::new(&mut health).expect("connection is healthy");
        operation.mark_dead();
        operation.complete(());
        assert!(health.dead);
    }

    #[test]
    fn dead_connection_is_rejected_without_rearming() {
        let mut health = Health {
            dead: true,
            marks: 0,
        };
        assert!(matches!(
            Operation::new(&mut health),
            Err(Error::Tds(mssql_tds::error::Error::ConnectionClosed(_)))
        ));
        assert_eq!(health.marks, 0);
    }
}

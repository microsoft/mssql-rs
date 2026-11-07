// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::error::{PoolError, PoolResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PoolConfig {
    max_size: NonZeroUsize,
    max_waiting: Option<NonZeroUsize>,
    idle_timeout: Duration,
    acquire_timeout: Duration,
    cleanup_timeout: Duration,
    shutdown_timeout: Duration,
    token_refresh_margin: Duration,
}

impl PoolConfig {
    /// Timeout defaults require agreement; construction always supplies all three.
    pub(crate) fn new(
        acquire_timeout: Duration,
        cleanup_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> PoolResult<Self> {
        validate_duration("acquire_timeout", acquire_timeout, false)?;
        validate_duration("cleanup_timeout", cleanup_timeout, false)?;
        validate_duration("shutdown_timeout", shutdown_timeout, false)?;
        Ok(Self {
            max_size: NonZeroUsize::new(100).expect("positive design default"),
            max_waiting: None,
            idle_timeout: Duration::from_secs(600),
            acquire_timeout,
            cleanup_timeout,
            shutdown_timeout,
            token_refresh_margin: Duration::from_secs(300),
        })
    }

    pub(crate) fn with_max_size(mut self, value: usize) -> PoolResult<Self> {
        if value > Semaphore::MAX_PERMITS {
            return Err(PoolError::InvalidConfig {
                field: "max_size",
                reason: "exceeds the slot scheduler capacity range",
            });
        }
        self.max_size = positive_limit("max_size", value)?;
        Ok(self)
    }

    pub(crate) fn with_max_waiting(mut self, value: Option<usize>) -> PoolResult<Self> {
        self.max_waiting = value
            .map(|value| positive_limit("max_waiting", value))
            .transpose()?;
        Ok(self)
    }

    pub(crate) fn with_idle_timeout(mut self, value: Duration) -> PoolResult<Self> {
        validate_duration("idle_timeout", value, true)?;
        self.idle_timeout = value;
        Ok(self)
    }

    pub(crate) fn with_token_refresh_margin(mut self, value: Duration) -> PoolResult<Self> {
        validate_duration("token_refresh_margin", value, true)?;
        self.token_refresh_margin = value;
        Ok(self)
    }

    pub(crate) fn max_size(&self) -> NonZeroUsize {
        self.max_size
    }
    pub(crate) fn max_waiting(&self) -> Option<NonZeroUsize> {
        self.max_waiting
    }
    pub(crate) fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }
    pub(crate) fn acquire_timeout(&self) -> Duration {
        self.acquire_timeout
    }
    pub(crate) fn cleanup_timeout(&self) -> Duration {
        self.cleanup_timeout
    }
    pub(crate) fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }
    pub(crate) fn token_refresh_margin(&self) -> Duration {
        self.token_refresh_margin
    }
}

fn positive_limit(field: &'static str, value: usize) -> PoolResult<NonZeroUsize> {
    NonZeroUsize::new(value).ok_or(PoolError::InvalidConfig {
        field,
        reason: "must be positive",
    })
}

pub(super) fn validate_duration(
    field: &'static str,
    value: Duration,
    allow_zero: bool,
) -> PoolResult<()> {
    if !allow_zero && value.is_zero() {
        return Err(PoolError::InvalidConfig {
            field,
            reason: "must be positive",
        });
    }
    if Instant::now().checked_add(value).is_none() {
        return Err(PoolError::InvalidConfig {
            field,
            reason: "exceeds the monotonic clock range",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PoolConfig {
        PoolConfig::new(
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(3),
        )
        .unwrap()
    }

    #[test]
    fn defaults_do_not_invent_timeout_budgets() {
        let config = config();
        assert_eq!(config.max_size().get(), 100);
        assert_eq!(config.max_waiting(), None);
        assert_eq!(config.idle_timeout(), Duration::from_secs(600));
        assert_eq!(config.token_refresh_margin(), Duration::from_secs(300));
        assert_eq!(config.acquire_timeout(), Duration::from_secs(1));
        assert_eq!(config.cleanup_timeout(), Duration::from_secs(2));
        assert_eq!(config.shutdown_timeout(), Duration::from_secs(3));
    }

    #[test]
    fn zero_is_field_specific_and_duration_overflow_is_rejected() {
        let good = Duration::from_secs(1);
        for (index, field) in ["acquire_timeout", "cleanup_timeout", "shutdown_timeout"]
            .into_iter()
            .enumerate()
        {
            for invalid in [Duration::ZERO, Duration::MAX] {
                let mut values = [good; 3];
                values[index] = invalid;
                assert!(matches!(
                    PoolConfig::new(values[0], values[1], values[2]),
                    Err(PoolError::InvalidConfig { field: actual, .. }) if actual == field
                ));
            }
        }
        let config = config()
            .with_idle_timeout(Duration::ZERO)
            .unwrap()
            .with_token_refresh_margin(Duration::ZERO)
            .unwrap();
        assert_eq!(config.idle_timeout(), Duration::ZERO);
        assert_eq!(config.token_refresh_margin(), Duration::ZERO);
        assert!(matches!(
            config.clone().with_idle_timeout(Duration::MAX),
            Err(PoolError::InvalidConfig {
                field: "idle_timeout",
                ..
            })
        ));
        assert!(matches!(
            config.with_token_refresh_margin(Duration::MAX),
            Err(PoolError::InvalidConfig {
                field: "token_refresh_margin",
                ..
            })
        ));
    }

    #[test]
    fn queue_limit_is_optional_and_limits_are_strictly_positive() {
        let config = config();
        assert!(matches!(
            config.clone().with_max_size(0),
            Err(PoolError::InvalidConfig {
                field: "max_size",
                ..
            })
        ));
        assert!(matches!(
            config.clone().with_max_waiting(Some(0)),
            Err(PoolError::InvalidConfig {
                field: "max_waiting",
                ..
            })
        ));
        let config = config
            .with_max_size(1)
            .unwrap()
            .with_max_waiting(Some(2))
            .unwrap();
        assert_eq!(config.max_size().get(), 1);
        assert_eq!(config.max_waiting().map(NonZeroUsize::get), Some(2));
        assert_eq!(config.with_max_waiting(None).unwrap().max_waiting(), None);
    }

    #[test]
    fn capacity_outside_the_scheduler_range_is_rejected_before_pool_construction() {
        assert_eq!(
            config()
                .with_max_size(Semaphore::MAX_PERMITS)
                .unwrap()
                .max_size()
                .get(),
            Semaphore::MAX_PERMITS
        );
        for invalid in [Semaphore::MAX_PERMITS + 1, usize::MAX] {
            assert!(matches!(
                config().with_max_size(invalid),
                Err(PoolError::InvalidConfig {
                    field: "max_size",
                    ..
                })
            ));
        }
        assert_eq!(
            config()
                .with_max_waiting(Some(usize::MAX))
                .unwrap()
                .max_waiting()
                .map(NonZeroUsize::get),
            Some(usize::MAX)
        );
    }

    #[test]
    fn builder_changes_preserve_the_original_and_unrelated_budgets() {
        let original = config();
        let configured = original
            .clone()
            .with_max_size(7)
            .unwrap()
            .with_max_waiting(Some(1))
            .unwrap()
            .with_idle_timeout(Duration::from_nanos(1))
            .unwrap()
            .with_token_refresh_margin(Duration::from_secs(17))
            .unwrap();
        assert_eq!(original, config());
        assert_eq!(configured.max_size().get(), 7);
        assert_eq!(configured.max_waiting().map(NonZeroUsize::get), Some(1));
        assert_eq!(configured.idle_timeout(), Duration::from_nanos(1));
        assert_eq!(configured.token_refresh_margin(), Duration::from_secs(17));
        assert_eq!(configured.acquire_timeout(), original.acquire_timeout());
        assert_eq!(configured.cleanup_timeout(), original.cleanup_timeout());
        assert_eq!(configured.shutdown_timeout(), original.shutdown_timeout());
        assert!(configured.clone().with_max_waiting(Some(0)).is_err());
        assert_eq!(configured.max_waiting().map(NonZeroUsize::get), Some(1));
        let reset = configured.clone().with_max_waiting(None).unwrap();
        assert_eq!(reset.max_waiting(), None);
        assert_eq!(reset.with_max_waiting(Some(1)).unwrap(), configured);
    }

    #[test]
    fn smallest_positive_budgets_and_optional_durations_are_preserved() {
        let minimum = Duration::from_nanos(1);
        let config = PoolConfig::new(minimum, minimum, minimum)
            .unwrap()
            .with_idle_timeout(minimum)
            .unwrap()
            .with_token_refresh_margin(minimum)
            .unwrap();
        assert_eq!(config.acquire_timeout(), minimum);
        assert_eq!(config.cleanup_timeout(), minimum);
        assert_eq!(config.shutdown_timeout(), minimum);
        assert_eq!(config.idle_timeout(), minimum);
        assert_eq!(config.token_refresh_margin(), minimum);
    }
}

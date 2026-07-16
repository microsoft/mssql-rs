// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection-scoped cache of `sp_describe_parameter_encryption` results.
//!
//! Always Encrypted requires a `sp_describe_parameter_encryption` round-trip to
//! learn which parameters of a statement must be encrypted and how. Repeating
//! that round-trip on every execution of the same statement is wasteful, so the
//! result is cached and reused across executions — whether the statement runs
//! via `sp_executesql`, a stored-procedure `EXEC`, or the prepared-statement
//! paths (`sp_prepexec` / `sp_prepare` + `sp_execute`).
//!
//! The design mirrors the .NET SqlClient `SqlQueryMetadataCache` and the JDBC
//! `ParameterMetaDataCache`: entries are keyed by the current database plus the
//! query text, bounded to a fixed number of entries, and expire after a fixed
//! time-to-live. Only the *encrypted* CEK wire metadata is cached — plaintext
//! column encryption keys are never stored here; they are re-derived per use
//! through the (separate) CEK cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;

/// A cached describe result together with its expiry instant.
#[derive(Debug)]
struct CacheEntry {
    describe: Arc<DescribeParameterEncryptionResult>,
    expires_at: Instant,
}

/// Connection-scoped, size- and time-bounded cache of
/// `sp_describe_parameter_encryption` results keyed by `(database, query text)`.
///
/// Mirrors SqlClient's `SqlQueryMetadataCache` bounds (2000 entries, a 300-entry
/// trim threshold, and a 10-hour time-to-live).
#[derive(Debug)]
pub(crate) struct QueryMetadataCache {
    entries: HashMap<String, CacheEntry>,
    max_entries: usize,
    trim_threshold: usize,
    ttl: Duration,
}

impl QueryMetadataCache {
    /// Maximum retained entries before trimming (SqlClient `CacheSize`).
    const DEFAULT_MAX_ENTRIES: usize = 2000;
    /// Extra entries tolerated before a trim runs (SqlClient `CacheTrimThreshold`).
    const DEFAULT_TRIM_THRESHOLD: usize = 300;
    /// Entry lifetime — SqlClient's 10-hour metadata cache timeout.
    const DEFAULT_TTL: Duration = Duration::from_secs(10 * 60 * 60);

    /// Creates an empty cache with the default SqlClient-matching bounds.
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: Self::DEFAULT_MAX_ENTRIES,
            trim_threshold: Self::DEFAULT_TRIM_THRESHOLD,
            ttl: Self::DEFAULT_TTL,
        }
    }

    /// Builds the cache key from the current database and the query text.
    ///
    /// The database name's length is prefixed so that a database name containing
    /// the separator sequence cannot collide with a different database/query
    /// split (the same robustness SqlClient gets by padding the database name to
    /// a fixed width).
    pub(crate) fn key(database: &str, sql: &str) -> String {
        format!("{}:{database}:::{sql}", database.len())
    }

    /// Returns the cached describe result for `key` if it is present and has not
    /// expired, evicting it if it has.
    pub(crate) fn get(&mut self, key: &str) -> Option<Arc<DescribeParameterEncryptionResult>> {
        match self.entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(Arc::clone(&entry.describe)),
            Some(_) => {
                self.entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Inserts (or replaces) the describe result for `key`, trimming the cache
    /// first when it has grown past its bound plus the trim threshold.
    pub(crate) fn insert(&mut self, key: String, describe: Arc<DescribeParameterEncryptionResult>) {
        if self.entries.len() >= self.max_entries + self.trim_threshold {
            self.trim();
        }
        self.entries.insert(
            key,
            CacheEntry {
                describe,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    /// Drops expired entries first, then the entries closest to expiry until the
    /// cache is back within its size bound.
    fn trim(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.expires_at > now);
        if self.entries.len() <= self.max_entries {
            return;
        }
        let mut by_expiry: Vec<(Instant, String)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.expires_at, key.clone()))
            .collect();
        by_expiry.sort_by_key(|(expiry, _)| *expiry);
        let to_remove = self.entries.len() - self.max_entries;
        for (_, key) in by_expiry.into_iter().take(to_remove) {
            self.entries.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe() -> Arc<DescribeParameterEncryptionResult> {
        Arc::new(DescribeParameterEncryptionResult::new())
    }

    fn cache_with(max_entries: usize, trim_threshold: usize, ttl: Duration) -> QueryMetadataCache {
        QueryMetadataCache {
            entries: HashMap::new(),
            max_entries,
            trim_threshold,
            ttl,
        }
    }

    #[test]
    fn key_includes_database_length_prefix() {
        assert_eq!(
            QueryMetadataCache::key("master", "SELECT 1"),
            "6:master:::SELECT 1"
        );
        // The length prefix keeps a separator-containing database name from
        // colliding with a different database/query split.
        assert_ne!(
            QueryMetadataCache::key("a:::b", "q"),
            QueryMetadataCache::key("a", ":::b:::q")
        );
    }

    #[test]
    fn get_returns_inserted_entry() {
        let mut cache = QueryMetadataCache::new();
        let key = QueryMetadataCache::key("db", "SELECT 1");
        cache.insert(key.clone(), describe());
        assert!(cache.get(&key).is_some());
        assert!(cache.get("other").is_none());
    }

    #[test]
    fn expired_entry_is_evicted_on_get() {
        // A zero TTL makes every entry expire immediately.
        let mut cache = cache_with(10, 5, Duration::ZERO);
        let key = QueryMetadataCache::key("db", "SELECT 1");
        cache.insert(key.clone(), describe());
        assert!(
            cache.get(&key).is_none(),
            "expired entry must not be returned"
        );
        assert_eq!(cache.entries.len(), 0, "expired entry must be evicted");
    }

    #[test]
    fn insert_keeps_cache_bounded() {
        // Bound 2, threshold 1: the cache trims once it reaches max + threshold
        // (3) entries, so it never grows past that regardless of how many distinct
        // statements are inserted.
        let mut cache = cache_with(2, 1, Duration::from_secs(3600));
        for i in 0..25 {
            cache.insert(QueryMetadataCache::key("db", &format!("q{i}")), describe());
            assert!(
                cache.entries.len() <= 3,
                "cache exceeded its bound: {}",
                cache.entries.len()
            );
        }
        // A trim actually happened (it did not simply keep every entry).
        assert!(cache.entries.len() <= 3);
    }
}

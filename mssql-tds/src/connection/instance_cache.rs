// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::core::TdsResult;
use crate::error::Error;

/// Resolved instance information returned from the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedResolution {
    pub port: Option<u16>,
    pub pipe_path: Option<String>,
}

/// A cached instance resolution entry.
#[derive(Debug, Clone)]
struct CachedInstance {
    port: Option<u16>,
    pipe_path: Option<String>,
}

/// Process-lifetime cache for SSRP instance resolution results.
///
/// Stores both TCP port and named pipe path from SQL Browser, matching
/// ODBC/SNI `LastConnectCache` behavior (format `0:tcp:host,port` /
/// `0:np:pipe_path`).
///
/// Avoids repeated UDP round-trips to SQL Browser when the same instance
/// is connected to multiple times within a process. Entries live for the
/// lifetime of the process and are only removed via explicit invalidation.
///
/// Cache keys are lowercased `"server\instance"` strings since SQL Server
/// instance names are case-insensitive.
#[derive(Debug)]
pub(crate) struct InstanceCache {
    entries: RwLock<HashMap<String, CachedInstance>>,
}

static GLOBAL_CACHE: OnceLock<InstanceCache> = OnceLock::new();

impl InstanceCache {
    fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Returns the global process-lifetime cache.
    pub(crate) fn global() -> &'static InstanceCache {
        GLOBAL_CACHE.get_or_init(InstanceCache::new)
    }

    /// Build a normalized cache key from server and instance names.
    fn cache_key(server: &str, instance: &str) -> String {
        format!("{}\\{}", server.to_lowercase(), instance.to_lowercase())
    }

    /// Look up cached resolution for a server\instance pair. Returns `None`
    /// on miss.
    ///
    /// Returns `ImplementationError` if the internal lock is poisoned (a thread
    /// panicked while holding it). Callers should treat this as a fatal
    /// connection-setup failure — the cache state is unknown.
    pub(crate) fn get(&self, server: &str, instance: &str) -> TdsResult<Option<CachedResolution>> {
        let key = Self::cache_key(server, instance);
        let entries = self.entries.read().map_err(|e| {
            Error::ImplementationError(format!("instance cache lock poisoned: {e}"))
        })?;
        let resolution = entries.get(&key).map(|entry| CachedResolution {
            port: entry.port,
            pipe_path: entry.pipe_path.clone(),
        });
        Ok(resolution)
    }

    /// Store resolved instance info (port and/or pipe path).
    pub(crate) fn insert(
        &self,
        server: &str,
        instance: &str,
        port: Option<u16>,
        pipe_path: Option<String>,
    ) -> TdsResult<()> {
        let key = Self::cache_key(server, instance);
        let mut entries = self.entries.write().map_err(|e| {
            Error::ImplementationError(format!("instance cache lock poisoned: {e}"))
        })?;
        entries.insert(key, CachedInstance { port, pipe_path });
        Ok(())
    }

    /// Remove an entry (e.g. after a connection failure with a cached port).
    #[cfg(test)]
    pub(crate) fn invalidate(&self, server: &str, instance: &str) -> TdsResult<()> {
        let key = Self::cache_key(server, instance);
        let mut entries = self.entries.write().map_err(|e| {
            Error::ImplementationError(format!("instance cache lock poisoned: {e}"))
        })?;
        entries.remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_is_lowercase() {
        let key = InstanceCache::cache_key("MyServer", "SQLEXPRESS");
        assert_eq!(key, "myserver\\sqlexpress");
    }

    #[test]
    fn test_insert_and_get() {
        let cache = InstanceCache::new();
        cache.insert("server", "inst", Some(1444), None).unwrap();
        let res = cache.get("server", "inst").unwrap().unwrap();
        assert_eq!(res.port, Some(1444));
        assert_eq!(res.pipe_path, None);
    }

    #[test]
    fn test_insert_and_get_with_pipe() {
        let cache = InstanceCache::new();
        cache
            .insert(
                "server",
                "inst",
                Some(1444),
                Some(r"\\server\pipe\MSSQL$INST\sql\query".to_string()),
            )
            .unwrap();
        let res = cache.get("server", "inst").unwrap().unwrap();
        assert_eq!(res.port, Some(1444));
        assert_eq!(
            res.pipe_path.as_deref(),
            Some(r"\\server\pipe\MSSQL$INST\sql\query")
        );
    }

    #[test]
    fn test_insert_pipe_only() {
        let cache = InstanceCache::new();
        cache
            .insert(
                "server",
                "inst",
                None,
                Some(r"\\server\pipe\sql\query".to_string()),
            )
            .unwrap();
        let res = cache.get("server", "inst").unwrap().unwrap();
        assert_eq!(res.port, None);
        assert!(res.pipe_path.is_some());
    }

    #[test]
    fn test_case_insensitive_lookup() {
        let cache = InstanceCache::new();
        cache.insert("MyServer", "INST", Some(1444), None).unwrap();
        assert_eq!(
            cache.get("myserver", "inst").unwrap().unwrap().port,
            Some(1444)
        );
        assert_eq!(
            cache.get("MYSERVER", "Inst").unwrap().unwrap().port,
            Some(1444)
        );
    }

    #[test]
    fn test_miss_returns_none() {
        let cache = InstanceCache::new();
        assert_eq!(cache.get("no", "such").unwrap(), None);
    }

    #[test]
    fn test_invalidate() {
        let cache = InstanceCache::new();
        cache.insert("server", "inst", Some(1444), None).unwrap();
        cache.invalidate("server", "inst").unwrap();
        assert_eq!(cache.get("server", "inst").unwrap(), None);
    }

    #[test]
    fn test_overwrite_replaces_entry() {
        let cache = InstanceCache::new();
        cache.insert("server", "inst", Some(1444), None).unwrap();
        cache.insert("server", "inst", Some(1555), None).unwrap();
        assert_eq!(
            cache.get("server", "inst").unwrap().unwrap().port,
            Some(1555)
        );
    }
}

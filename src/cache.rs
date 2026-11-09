//! In-memory TTL + LRU cache.
//!
//! Serves two goals at once:
//!  - Performance: identical repeated queries are answered from memory.
//!  - Quota protection: cache hits do not consume the Google daily quota.
//!
//! Entries expire after `ttl_ms`; when the store is full the least-recently-used
//! entry is evicted. `IndexMap` preserves insertion/access order for O(n) LRU
//! bookkeeping at this cache's size (default 500 entries).

use indexmap::IndexMap;
use serde::Serialize;
use std::sync::{Arc, Mutex};

type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

struct Entry {
    value: String,
    expires_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub expirations: u64,
    pub size: usize,
    pub max_entries: usize,
}

#[derive(Default)]
struct Inner {
    store: IndexMap<String, Entry>,
    hits: u64,
    misses: u64,
    evictions: u64,
    expirations: u64,
}

pub struct TtlLruCache {
    inner: Mutex<Inner>,
    max_entries: usize,
    ttl_ms: u64,
    now: NowFn,
}

impl TtlLruCache {
    pub fn new(max_entries: usize, ttl_ms: u64, now: NowFn) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_entries,
            ttl_ms,
            now,
        }
    }

    pub fn with_system_clock(max_entries: usize, ttl_ms: u64) -> Self {
        Self::new(max_entries, ttl_ms, Arc::new(crate::util::now_ms))
    }

    /// Whether caching is effectively disabled.
    fn disabled(&self) -> bool {
        self.max_entries == 0 || self.ttl_ms == 0
    }

    pub fn get(&self, key: &str) -> Option<String> {
        if self.disabled() {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        let Some(entry) = inner.store.get(key) else {
            inner.misses += 1;
            return None;
        };
        if entry.expires_at <= (self.now)() {
            inner.store.shift_remove(key);
            inner.expirations += 1;
            inner.misses += 1;
            return None;
        }
        // Refresh recency: re-insert to move to the most-recent position.
        if let Some(entry) = inner.store.shift_remove(key) {
            let value = entry.value.clone();
            inner.store.insert(key.to_string(), entry);
            inner.hits += 1;
            return Some(value);
        }
        None
    }

    pub fn set(&self, key: &str, value: String) {
        if self.disabled() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.store.shift_remove(key);
        inner.store.insert(
            key.to_string(),
            Entry {
                value,
                expires_at: (self.now)() + self.ttl_ms as i64,
            },
        );
        while inner.store.len() > self.max_entries {
            if let Some(oldest) = inner.store.keys().next().cloned() {
                inner.store.shift_remove(&oldest);
                inner.evictions += 1;
            } else {
                break;
            }
        }
    }

    /// Remove expired entries eagerly (e.g. on a timer). Returns count removed.
    pub fn prune(&self) -> usize {
        if self.disabled() {
            return 0;
        }
        let now = (self.now)();
        let mut inner = self.inner.lock().unwrap();
        let expired: Vec<String> = inner
            .store
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            inner.store.shift_remove(key);
        }
        inner.expirations += expired.len() as u64;
        expired.len()
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().store.clear();
    }

    pub fn stats(&self) -> CacheStats {
        let inner = self.inner.lock().unwrap();
        CacheStats {
            hits: inner.hits,
            misses: inner.misses,
            evictions: inner.evictions,
            expirations: inner.expirations,
            size: inner.store.len(),
            max_entries: self.max_entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn cache_with_clock(max: usize, ttl: u64) -> (TtlLruCache, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(1_000));
        let handle = clock.clone();
        let cache = TtlLruCache::new(max, ttl, Arc::new(move || handle.load(Ordering::SeqCst)));
        (cache, clock)
    }

    #[test]
    fn hit_miss_and_expiry() {
        let (cache, clock) = cache_with_clock(10, 100);
        assert_eq!(cache.get("k"), None);
        cache.set("k", "v".into());
        assert_eq!(cache.get("k"), Some("v".into()));
        clock.store(1_101, Ordering::SeqCst);
        assert_eq!(cache.get("k"), None);
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.expirations, 1);
    }

    #[test]
    fn evicts_least_recently_used() {
        let (cache, _) = cache_with_clock(2, 1_000);
        cache.set("a", "1".into());
        cache.set("b", "2".into());
        assert_eq!(cache.get("a"), Some("1".into()));
        cache.set("c", "3".into());
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a"), Some("1".into()));
        assert_eq!(cache.get("c"), Some("3".into()));
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn prune_removes_expired() {
        let (cache, clock) = cache_with_clock(10, 100);
        cache.set("a", "1".into());
        cache.set("b", "2".into());
        clock.store(1_200, Ordering::SeqCst);
        assert_eq!(cache.prune(), 2);
        assert_eq!(cache.stats().size, 0);
        assert_eq!(cache.stats().expirations, 2);
    }

    #[test]
    fn disabled_when_zero() {
        let (cache, _) = cache_with_clock(0, 100);
        cache.set("k", "v".into());
        assert_eq!(cache.get("k"), None);
        let (cache, _) = cache_with_clock(10, 0);
        cache.set("k", "v".into());
        assert_eq!(cache.get("k"), None);
    }
}

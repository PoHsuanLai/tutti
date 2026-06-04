//! LRU disk cache for audio files.
//!
//! Bounded cache with least-recently-used eviction.

use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tutti_core::Wave;

/// Bounded LRU cache with entry count and byte limits.
pub struct LruCache {
    cache: DashMap<PathBuf, CacheEntry>,
    max_entries: usize,
    max_bytes: u64,
    current_bytes: AtomicU64,
}

struct CacheEntry {
    wave: Arc<Wave>,
    last_access: AtomicU64,
    size_bytes: u64,
}

impl LruCache {
    pub fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            cache: DashMap::new(),
            max_entries,
            max_bytes,
            current_bytes: AtomicU64::new(0),
        }
    }

    pub fn get(&self, path: &PathBuf) -> Option<Arc<Wave>> {
        self.cache.get(path).map(|entry| {
            entry.last_access.store(now_ms(), Ordering::Relaxed);
            entry.wave.clone()
        })
    }

    /// Evicts LRU entries if necessary.
    pub fn insert(&self, path: PathBuf, wave: Arc<Wave>) {
        let size = wave.len() as u64 * wave.channels() as u64 * 4;

        if let Some(existing) = self.cache.get(&path) {
            existing.last_access.store(now_ms(), Ordering::Relaxed);
            return;
        }

        while self.cache.len() >= self.max_entries
            || (self.current_bytes.load(Ordering::Relaxed) + size > self.max_bytes
                && !self.cache.is_empty())
        {
            if !self.evict_lru() {
                break;
            }
        }

        self.cache.insert(
            path,
            CacheEntry {
                wave,
                last_access: AtomicU64::new(now_ms()),
                size_bytes: size,
            },
        );
        self.current_bytes.fetch_add(size, Ordering::Relaxed);
    }

    fn evict_lru(&self) -> bool {
        let oldest_path = self
            .cache
            .iter()
            .min_by_key(|entry| entry.value().last_access.load(Ordering::Relaxed))
            .map(|entry| entry.key().clone());

        if let Some(path) = oldest_path {
            if let Some((_, entry)) = self.cache.remove(&path) {
                self.current_bytes
                    .fetch_sub(entry.size_bytes, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Number of cached entries. Test observability for eviction behavior.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Total cached bytes. Test observability for byte-budget eviction.
    #[cfg(test)]
    pub fn byte_len(&self) -> u64 {
        self.current_bytes.load(Ordering::Relaxed)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data = vec![0.0f32; samples];
        Arc::new(Wave::from_samples(44100.0, &data))
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = LruCache::new(10, 1024 * 1024);
        let path = PathBuf::from("/test/file.wav");
        let wave = make_wave(100);

        assert!(cache.get(&path).is_none());
        cache.insert(path.clone(), wave);

        let retrieved = cache.get(&path).expect("just inserted");
        assert_eq!(retrieved.len(), 100);
    }

    #[test]
    fn test_cache_max_entries_eviction() {
        let cache = LruCache::new(3, u64::MAX);

        for i in 0..3 {
            cache.insert(PathBuf::from(format!("/test/file{}.wav", i)), make_wave(10));
        }
        assert_eq!(cache.len(), 3);

        let path4 = PathBuf::from("/test/file3.wav");
        cache.insert(path4.clone(), make_wave(10));

        assert_eq!(cache.len(), 3);
        assert!(cache.get(&path4).is_some());
    }

    #[test]
    fn test_cache_max_bytes_eviction() {
        let cache = LruCache::new(100, 1000);

        cache.insert(PathBuf::from("/test/a.wav"), make_wave(100));
        cache.insert(PathBuf::from("/test/b.wav"), make_wave(100));
        assert_eq!(cache.len(), 2);

        cache.insert(PathBuf::from("/test/c.wav"), make_wave(100));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_byte_accounting() {
        let cache = LruCache::new(10, 10000);
        cache.insert(PathBuf::from("/test/a.wav"), make_wave(100));
        cache.insert(PathBuf::from("/test/b.wav"), make_wave(200));

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.byte_len(), 1200);
    }

    #[test]
    fn test_duplicate_insert_no_double_count() {
        let cache = LruCache::new(10, 10000);
        let path = PathBuf::from("/test/a.wav");

        cache.insert(path.clone(), make_wave(100));
        let bytes_after_first = cache.byte_len();

        cache.insert(path, make_wave(100));
        let bytes_after_second = cache.byte_len();

        assert_eq!(bytes_after_first, bytes_after_second);
        assert_eq!(cache.len(), 1);
    }
}

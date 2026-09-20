//! LRU disk cache for audio files.
//!
//! Bounded cache with least-recently-used eviction.
//!
//! The *read* half (`get`, `pin`) is unconditional, but the sole writer is the
//! decode arm of `io::refill::load_wave`, which needs `Wave::load` and so is
//! gated on the codec features. A build with none of them on therefore reaches
//! `insert` from nowhere, and `over_budget` / `evict_lru` / the budget fields
//! die with it — a feature-matrix artifact, not rot. CI builds that config, so
//! the allow is scoped to it rather than blanket, keeping real dead-code
//! detection live everywhere a codec is on.
#![cfg_attr(
    not(any(
        feature = "wav",
        feature = "flac",
        feature = "mp3",
        feature = "ogg",
        test
    )),
    allow(dead_code)
)]

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tutti_core::Wave;

/// Bounded LRU cache with entry count and byte limits.
pub struct LruCache {
    cache: DashMap<PathBuf, CacheEntry>,
    max_entries: usize,
    max_bytes: u64,
    current_bytes: AtomicU64,
    /// Monotonic access counter — the source of every `last_access` stamp.
    ///
    /// "Least recently used" is a statement about access *order*, so the stamp
    /// is a sequence number rather than a reading of a clock. This replaced
    /// `SystemTime::now()` in milliseconds, which got the order wrong two ways:
    ///
    /// - **Resolution.** Cache accesses are memory-speed; several land in the
    ///   same millisecond routinely, and every one of those was a *tie*.
    ///   `min_by_key` breaks a tie by iteration order, which for a `DashMap` is
    ///   shard order — so the victim was effectively arbitrary, and it changed
    ///   between runs. A tick increments per touch, so no two accesses can tie.
    /// - **Monotonicity.** `SystemTime` is a wall clock: NTP correction, a
    ///   manual set, or a DST-adjacent jump moves it *backwards*, which makes a
    ///   just-touched entry look like the oldest one in the cache and evicts it
    ///   next. A counter has no such failure mode.
    ///
    /// `u64` cannot realistically wrap: at one touch per nanosecond it takes
    /// ~584 years.
    access_tick: AtomicU64,
}

struct CacheEntry {
    wave: Arc<Wave>,
    /// The value of [`LruCache::access_tick`] at this entry's last touch.
    /// Ordering-only — the number has no meaning beyond comparing against
    /// another entry's.
    last_access: AtomicU64,
    size_bytes: u64,
    /// Active stream count. Non-zero while a stream is reading this wave; the
    /// LRU refuses to evict a pinned entry (a fully-buffered stream would go
    /// cold and get evicted out from under its reader otherwise).
    pins: AtomicU32,
}

/// RAII guard that keeps a cache entry pinned (unevictable) for the lifetime of
/// a stream. Acquired by [`LruCache::pin`], released on `Drop`. Held inside the
/// butler's stream link so it lives exactly as long as the stream.
pub struct StreamPin {
    cache: Arc<LruCache>,
    path: PathBuf,
}

impl Drop for StreamPin {
    fn drop(&mut self) {
        if let Some(entry) = self.cache.cache.get(&self.path) {
            entry.pins.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl LruCache {
    /// An empty cache bounded by `max_entries` resident waves and `max_bytes`
    /// total.
    ///
    /// Both bounds are targets, not guarantees: an insert that can find no
    /// unpinned victim is admitted over-budget rather than evicting a wave a
    /// live stream is still reading.
    pub fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            cache: DashMap::new(),
            max_entries,
            max_bytes,
            current_bytes: AtomicU64::new(0),
            access_tick: AtomicU64::new(0),
        }
    }

    /// The next access stamp, consuming a tick.
    ///
    /// `Relaxed` is enough: the counter is only ever compared against other
    /// stamps from this same counter, and `fetch_add` is atomic regardless of
    /// ordering, so two concurrent touches get two distinct values. Nothing
    /// reads a tick to synchronize access to other memory.
    fn next_tick(&self) -> u64 {
        self.access_tick.fetch_add(1, Ordering::Relaxed)
    }

    /// The resident wave for `path`, if any, marking it most-recently-used.
    ///
    /// `None` is a plain miss — every caller treats it as "skip this region",
    /// never as an error.
    pub fn get(&self, path: &Path) -> Option<Arc<Wave>> {
        self.cache.get(path).map(|entry| {
            entry.last_access.store(self.next_tick(), Ordering::Relaxed);
            entry.wave.clone()
        })
    }

    /// Admit `wave` under `path`, evicting least-recently-used **unpinned**
    /// entries until it fits.
    ///
    /// Re-inserting a resident path only refreshes its access time — the bytes
    /// are not counted twice. When nothing evictable remains the wave is
    /// admitted over budget, which is the deliberate trade: exceeding a byte
    /// target is recoverable, pulling a wave out from under a playing stream is
    /// not.
    pub fn insert(&self, path: PathBuf, wave: Arc<Wave>) {
        let size = wave.len() as u64 * wave.channels() as u64 * 4;

        if let Some(existing) = self.cache.get(&path) {
            existing
                .last_access
                .store(self.next_tick(), Ordering::Relaxed);
            return;
        }

        while self.over_budget(size) {
            if !self.evict_lru() {
                break;
            }
        }

        self.cache.insert(
            path,
            CacheEntry {
                wave,
                last_access: AtomicU64::new(self.next_tick()),
                size_bytes: size,
                pins: AtomicU32::new(0),
            },
        );
        self.current_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Pin the entry at `path` so the LRU will not evict it while the returned
    /// [`StreamPin`] is alive. Held by an active stream (in `ChannelPlan::Link`)
    /// so a fully-buffered — hence cold — stream is never evicted mid-read.
    ///
    /// No-op-on-drop is safe if the entry is absent (e.g. never admitted): the
    /// pin count only exists on a live entry, so the guard's `Drop` simply finds
    /// nothing to decrement. Callers pin *after* insertion in practice.
    pub fn pin(self: &Arc<Self>, path: &Path) -> StreamPin {
        if let Some(entry) = self.cache.get(path) {
            entry.pins.fetch_add(1, Ordering::Relaxed);
        }
        StreamPin {
            cache: Arc::clone(self),
            path: path.to_path_buf(),
        }
    }

    /// Whether inserting `incoming` bytes would exceed the entry-count or
    /// byte budget (byte check ignores an empty cache so a single oversized
    /// wave can still be admitted).
    fn over_budget(&self, incoming: u64) -> bool {
        self.cache.len() >= self.max_entries
            || (self.current_bytes.load(Ordering::Relaxed) + incoming > self.max_bytes
                && !self.cache.is_empty())
    }

    fn evict_lru(&self) -> bool {
        // Only unpinned entries are eviction candidates; a pinned entry backs an
        // active stream and must stay resident. If every remaining entry is
        // pinned this returns `None`, `insert`'s loop breaks, and the wave is
        // admitted over-budget — the same escape the empty-cache case uses.
        let oldest_path = self
            .cache
            .iter()
            .filter(|entry| entry.value().pins.load(Ordering::Relaxed) == 0)
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

    /// Two accesses with no delay between them still order — the property a
    /// wall clock could not give.
    ///
    /// Both inserts land in the same millisecond (they are memory writes), so
    /// under `SystemTime::now().as_millis()` they carried the *identical*
    /// stamp. `min_by_key` then broke the tie by `DashMap` iteration order,
    /// i.e. shard order, so which entry got evicted was arbitrary and could
    /// differ run to run. No sleep here on purpose: a sleep would paper over
    /// exactly the defect being asserted.
    #[test]
    fn same_instant_accesses_still_order_deterministically() {
        // Budget for two of three waves, forcing one eviction on the third.
        let cache = LruCache::new(2, u64::MAX);
        let a = PathBuf::from("/test/a.wav");
        let b = PathBuf::from("/test/b.wav");
        let c = PathBuf::from("/test/c.wav");

        cache.insert(a.clone(), make_wave(10));
        cache.insert(b.clone(), make_wave(10));
        cache.insert(c.clone(), make_wave(10));

        // `a` was touched first, so `a` is the victim — every time.
        assert!(cache.get(&a).is_none(), "first-touched entry is evicted");
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some());
        assert_eq!(cache.len(), 2);

        // And it is not luck: the same sequence gives the same verdict on
        // every repetition. A tie broken by shard order would vary across
        // these, since each cache is a fresh `DashMap`.
        for _ in 0..64 {
            let cache = LruCache::new(2, u64::MAX);
            cache.insert(a.clone(), make_wave(10));
            cache.insert(b.clone(), make_wave(10));
            cache.insert(c.clone(), make_wave(10));
            assert!(
                cache.get(&a).is_none() && cache.get(&b).is_some() && cache.get(&c).is_some(),
                "the victim must be a on every run, not an arbitrary entry"
            );
        }
    }

    /// Eviction follows *access* order, not insertion order: a `get` on the
    /// oldest entry promotes it, and the next-oldest becomes the victim.
    #[test]
    fn eviction_follows_access_order_not_insertion_order() {
        let cache = LruCache::new(2, u64::MAX);
        let a = PathBuf::from("/test/a.wav");
        let b = PathBuf::from("/test/b.wav");
        let c = PathBuf::from("/test/c.wav");

        cache.insert(a.clone(), make_wave(10));
        cache.insert(b.clone(), make_wave(10));

        // Touch `a`. It was inserted first, but it is now the most recently
        // *used*, so `b` inherits the victim slot.
        assert!(cache.get(&a).is_some(), "touch promotes a");

        cache.insert(c.clone(), make_wave(10));

        assert!(
            cache.get(&a).is_some(),
            "promoted by the get, so it survives"
        );
        assert!(cache.get(&b).is_none(), "least recently used is evicted");
        assert!(cache.get(&c).is_some());
    }

    /// Re-inserting a resident path promotes it too — `insert`'s early-return
    /// arm refreshes the stamp rather than leaving it stale.
    #[test]
    fn reinsert_of_a_resident_path_promotes_it() {
        let cache = LruCache::new(2, u64::MAX);
        let a = PathBuf::from("/test/a.wav");
        let b = PathBuf::from("/test/b.wav");
        let c = PathBuf::from("/test/c.wav");

        cache.insert(a.clone(), make_wave(10));
        cache.insert(b.clone(), make_wave(10));
        cache.insert(a.clone(), make_wave(10));

        cache.insert(c.clone(), make_wave(10));

        assert!(cache.get(&a).is_some(), "re-insert refreshed a's stamp");
        assert!(cache.get(&b).is_none(), "b is now least recently used");
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
    fn test_pin_prevents_eviction_of_lru_victim() {
        // Byte budget fits exactly two waves of 100 samples (400 bytes each).
        let cache = Arc::new(LruCache::new(100, 900));
        let a = PathBuf::from("/test/a.wav");
        let b = PathBuf::from("/test/b.wav");
        let c = PathBuf::from("/test/c.wav");

        // `a` is inserted first, so it is the LRU victim. Pin it: an active
        // stream is reading it.
        cache.insert(a.clone(), make_wave(100));
        let _pin = cache.pin(&a);
        cache.insert(b.clone(), make_wave(100));

        // Inserting `c` would evict the LRU (`a`), but `a` is pinned. The evictor
        // must skip it and take `b` instead (next-oldest, unpinned).
        cache.insert(c.clone(), make_wave(100));

        assert!(cache.get(&a).is_some(), "pinned LRU victim must survive");
        assert!(cache.get(&c).is_some(), "new entry admitted");
        assert!(
            cache.get(&b).is_none(),
            "unpinned next-oldest evicted instead"
        );
    }

    #[test]
    fn test_all_pinned_admits_over_budget() {
        let cache = Arc::new(LruCache::new(100, 900));
        let a = PathBuf::from("/test/a.wav");
        let b = PathBuf::from("/test/b.wav");
        let c = PathBuf::from("/test/c.wav");

        cache.insert(a.clone(), make_wave(100));
        let _pa = cache.pin(&a);
        cache.insert(b.clone(), make_wave(100));
        let _pb = cache.pin(&b);

        // Both resident entries are pinned; the newcomer can't evict either, so
        // it is admitted over-budget rather than dropping an active stream.
        cache.insert(c.clone(), make_wave(100));

        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some(), "over-budget admit when all pinned");
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_unpin_on_drop_restores_evictability() {
        // Budget holds exactly one wave (400 bytes); the second insert must evict
        // the first — unless it's pinned. This makes the eviction target
        // unambiguous (no LRU tie between same-millisecond entries).
        let cache = Arc::new(LruCache::new(100, 400));
        let a = PathBuf::from("/test/a.wav");
        let b = PathBuf::from("/test/b.wav");

        cache.insert(a.clone(), make_wave(100));

        // While pinned, inserting `b` cannot evict `a`; it is admitted
        // over-budget instead (the all-pinned escape). Keep `b` pinned too so
        // that once `a`'s pin drops, `a` is the *only* eviction candidate — no
        // LRU tie between same-millisecond entries.
        let pin_a = cache.pin(&a);
        cache.insert(b.clone(), make_wave(100));
        let _pin_b = cache.pin(&b);
        assert!(
            cache.get(&a).is_some(),
            "pinned `a` survives the over-budget insert"
        );
        drop(pin_a);

        // With `a`'s pin gone it is the sole unpinned entry: the next over-budget
        // insert reclaims it (`b` stays put, pinned).
        let c = PathBuf::from("/test/c.wav");
        cache.insert(c.clone(), make_wave(100));
        assert!(cache.get(&a).is_none(), "dropped pin → LRU can evict `a`");
        assert!(cache.get(&b).is_some(), "still-pinned `b` untouched");
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

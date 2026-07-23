//! Bevy-free cache core: decode-once dedup keyed by `(path, mtime, size)`.
//!
//! [`WaveCacheCore`] owns the entry map and all the cache policy (revalidation
//! window, stale-stamp reload, sticky failures). It knows nothing about *how*
//! decodes run off-thread — the caller drives that:
//!
//! - [`WaveCacheCore::get_or_load`] returns a [`LoadAction`]: either a resident
//!   [`WaveState`], or [`LoadAction::Spawn`] telling the caller to start a decode
//!   for `(key, path)`.
//! - When that decode finishes, the caller hands the result back via
//!   [`WaveCacheCore::complete`].
//!
//! The `bevy` layer wires this to `AsyncComputeTaskPool`; the [`ThreadedWaveCache`]
//! wrapper (below, always available) wires it to `std::thread` so a non-Bevy host
//! gets a working async cache with no framework.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_core::Wave;

/// Filesystem identity of a decoded wave. A change to mtime or size
/// invalidates the cached decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    mtime_ns: u128,
    size: u64,
}

impl FileStamp {
    fn of(path: &str) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Some(Self {
            mtime_ns,
            size: meta.len(),
        })
    }
}

/// The state of a file in the cache, returned by [`WaveCacheCore::get_or_load`].
#[derive(Clone)]
pub enum WaveState {
    /// Decode is in flight; poll again next frame.
    Loading,
    /// Decoded and resident. Cheap to clone (shared `Arc`).
    Ready(Arc<Wave>),
    /// Decode failed (missing file, unsupported/disabled format, corrupt data).
    /// Carries the error string; the caller should stop waiting on this path.
    Failed(Arc<str>),
}

/// What [`WaveCacheCore::get_or_load`] decided.
pub enum LoadAction {
    /// The path is already resident / in-flight / stuck-failed — use this state.
    State(WaveState),
    /// The caller must start a decode of `path` and report it back via
    /// [`WaveCacheCore::complete`] with this `key`.
    Spawn { key: Arc<str>, path: String },
}

enum Entry {
    Loading {
        stamp: FileStamp,
    },
    Ready {
        stamp: FileStamp,
        wave: Arc<Wave>,
        /// When this entry's on-disk stamp was last validated. Lets a hot
        /// `get_or_load` skip the `fs::metadata` syscall (see [`REVALIDATE_AFTER`]).
        checked: Instant,
        /// Resident payload size in bytes (`len * channels * 4`). Charged against
        /// the byte budget; recomputing it on eviction would need the `Arc<Wave>`.
        bytes: u64,
        /// Monotonic recency stamp (see [`WaveCacheCore::recency`]). The lowest
        /// value is the least-recently-used `Ready` entry, evicted first.
        used: u64,
    },
    Failed {
        /// The stamp that failed (`None` if the file was missing). Retry only
        /// when the on-disk stamp differs — so a corrupt/unsupported file stays
        /// failed instead of re-decoding every frame.
        stamp: Option<FileStamp>,
        err: Arc<str>,
        checked: Instant,
    },
}

/// How long a `Ready`/`Failed` entry trusts its last on-disk stamp before
/// re-`stat`ing. `get_or_load` is called per-clip per-frame by the spectral
/// render driver; without this it would `fs::metadata` every clip every frame
/// (a multi-ms hitch). Within this window a hot call is a pure hashmap lookup;
/// an on-disk change is still picked up within ~1s.
const REVALIDATE_AFTER: Duration = Duration::from_secs(1);

/// Default resident byte budget for `Ready` waves (`bytes = len * channels * 4`).
/// At f32 samples this is ~512 MB of decoded audio before LRU eviction kicks in.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Default cap on the number of `Ready` waves held at once, independent of the
/// byte budget — bounds entry churn even when every wave is tiny.
pub const DEFAULT_MAX_ENTRIES: usize = 4096;

/// Resident payload size of a decoded wave, in bytes (`len * channels * 4`,
/// f32 samples). Mirrors the butler `LruCache` accounting.
fn wave_bytes(wave: &Wave) -> u64 {
    wave.len() as u64 * wave.channels() as u64 * 4
}

/// Decode-once cache of `Arc<Wave>`, keyed by file path. Framework-free: the
/// caller owns the off-thread decode mechanism (see module docs).
///
/// `Ready` waves are held under an LRU byte budget + entry cap: completing a
/// decode evicts the least-recently-used `Ready` entries until the resident set
/// fits (see [`Self::with_budget`]). Recency uses a monotonic counter, not
/// wall-clock time — a `peek`/`get_or_load` hit bumps the entry to most-recent.
/// `Loading`/`Failed` entries carry no payload and are never evicted for budget.
pub struct WaveCacheCore {
    entries: HashMap<Arc<str>, Entry>,
    /// Sum of `bytes` across all `Ready` entries. Kept in step with the map so
    /// eviction never has to walk every entry to know the resident total.
    resident_bytes: u64,
    max_bytes: u64,
    max_entries: usize,
    /// Monotonic recency source. Bumped on every `Ready` touch; the entry with
    /// the smallest `used` is the LRU victim. Never wraps in practice (u64).
    clock: u64,
}

impl Default for WaveCacheCore {
    fn default() -> Self {
        Self::with_budget(DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES)
    }
}

impl WaveCacheCore {
    /// Build a cache with an explicit resident byte budget and entry cap for
    /// `Ready` waves. See [`DEFAULT_MAX_BYTES`] / [`DEFAULT_MAX_ENTRIES`] for the
    /// values [`Default`] uses.
    pub fn with_budget(max_bytes: u64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            resident_bytes: 0,
            max_bytes,
            max_entries,
            clock: 0,
        }
    }

    /// Next monotonic recency value. The caller stamps a touched `Ready` entry
    /// with this; the entry holding the smallest value is the LRU victim.
    fn recency(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Decide what to do for `path`: return a resident [`WaveState`], or ask the
    /// caller to [`LoadAction::Spawn`] a decode. Re-decodes if the file's
    /// mtime/size changed since it was cached.
    pub fn get_or_load(&mut self, path: &str) -> LoadAction {
        // Fast path: hashmap lookup *before* any filesystem syscall. A
        // `Ready`/`Failed` entry validated within `REVALIDATE_AFTER` is trusted
        // as-is — the hot per-frame case is then a pure lookup, no `fs::metadata`.
        // A `Loading` entry is always returned without a stat (the decode in
        // flight already pinned a stamp). Only a missing/stale entry stats.
        //
        // A `Ready` hit here needs a recency bump, so it can't ride the shared
        // borrow of `entries.get`; handle it after computing the fresh stamp.
        match self.entries.get(path) {
            Some(Entry::Loading { .. }) => return LoadAction::State(WaveState::Loading),
            Some(Entry::Ready { checked, .. }) if checked.elapsed() < REVALIDATE_AFTER => {
                let used = self.recency();
                if let Some(Entry::Ready { wave, used: u, .. }) = self.entries.get_mut(path) {
                    *u = used;
                    return LoadAction::State(WaveState::Ready(wave.clone()));
                }
                unreachable!("entry was Ready above");
            }
            Some(Entry::Failed { err, checked, .. }) if checked.elapsed() < REVALIDATE_AFTER => {
                return LoadAction::State(WaveState::Failed(err.clone()));
            }
            _ => {}
        }

        // Stale or absent: stat the file and reconcile against the entry.
        let stamp = FileStamp::of(path);
        let used = self.recency();
        match self.entries.get_mut(path) {
            // Fresh `Ready` (stamp matches): bump recency + revalidation and serve.
            // A changed stamp falls through to the re-load below.
            Some(Entry::Ready { stamp: s, wave, checked, used: u, .. }) if Some(*s) == stamp => {
                *checked = Instant::now();
                *u = used;
                return LoadAction::State(WaveState::Ready(wave.clone()));
            }
            // Stay failed unless the on-disk stamp changed (file replaced/fixed).
            // A corrupt/unsupported file keeps the same stamp → no re-decode spin.
            Some(Entry::Failed { stamp: s, err, checked }) if *s == stamp => {
                *checked = Instant::now();
                return LoadAction::State(WaveState::Failed(err.clone()));
            }
            _ => {}
        }

        // Needs a fresh decode. Whatever entry sat here (a stale `Ready`) is
        // being replaced; drop its byte charge first so the account stays exact.
        self.drop_entry_bytes(path);
        let Some(stamp) = stamp else {
            let err: Arc<str> = Arc::from(format!("file not found: {path}"));
            self.entries.insert(
                Arc::from(path),
                Entry::Failed {
                    stamp: None,
                    err: err.clone(),
                    checked: Instant::now(),
                },
            );
            return LoadAction::State(WaveState::Failed(err));
        };
        let key: Arc<str> = Arc::from(path);
        self.entries
            .insert(key.clone(), Entry::Loading { stamp });
        LoadAction::Spawn {
            key,
            path: path.to_string(),
        }
    }

    /// If `path` currently holds a `Ready` entry, subtract its bytes from the
    /// resident total. Called before overwriting an entry so the byte account
    /// never drifts. No-op for `Loading`/`Failed`/absent.
    fn drop_entry_bytes(&mut self, path: &str) {
        if let Some(Entry::Ready { bytes, .. }) = self.entries.get(path) {
            self.resident_bytes = self.resident_bytes.saturating_sub(*bytes);
        }
    }

    /// Report a finished decode for `key` (from a [`LoadAction::Spawn`]). Flips
    /// the entry to `Ready`/`Failed`, preserving the stamp pinned at spawn time.
    /// A stale key (evicted / superseded) is ignored.
    pub fn complete(&mut self, key: &Arc<str>, result: Result<Wave, String>) {
        let stamp = match self.entries.get(key) {
            Some(Entry::Loading { stamp, .. }) => *stamp,
            _ => return,
        };
        let entry = match result {
            Ok(wave) => {
                let bytes = wave_bytes(&wave);
                let used = self.recency();
                self.resident_bytes += bytes;
                Entry::Ready {
                    stamp,
                    wave: Arc::new(wave),
                    checked: Instant::now(),
                    bytes,
                    used,
                }
            }
            Err(e) => Entry::Failed {
                stamp: Some(stamp),
                err: Arc::from(e),
                checked: Instant::now(),
            },
        };
        self.entries.insert(key.clone(), entry);
        self.evict_to_budget();
    }

    /// Evict least-recently-used `Ready` entries until the resident set fits the
    /// byte budget and entry cap. Mirrors the butler `LruCache`: an entry cap
    /// counts every entry, while the byte budget admits a single oversized wave
    /// (never evicts down to zero for it). Only `Ready` entries are candidates —
    /// `Loading`/`Failed` carry no payload.
    fn evict_to_budget(&mut self) {
        loop {
            let ready_count = self
                .entries
                .values()
                .filter(|e| matches!(e, Entry::Ready { .. }))
                .count();

            let over_entries = ready_count > self.max_entries;
            let over_bytes = self.resident_bytes > self.max_bytes && ready_count > 1;
            if !over_entries && !over_bytes {
                return;
            }

            let victim = self
                .entries
                .iter()
                .filter_map(|(k, e)| match e {
                    Entry::Ready { used, .. } => Some((*used, k.clone())),
                    _ => None,
                })
                .min_by_key(|(used, _)| *used)
                .map(|(_, k)| k);

            let Some(victim) = victim else { return };
            self.drop_entry_bytes(&victim);
            self.entries.remove(&victim);
        }
    }

    /// Return the resident wave for `path`, or `None` if it isn't `Ready`.
    /// Never spawns a decode and never blocks.
    pub fn peek(&self, path: &str) -> Option<Arc<Wave>> {
        match self.entries.get(path) {
            Some(Entry::Ready { wave, .. }) => Some(wave.clone()),
            _ => None,
        }
    }

    /// Cheap metadata probe (frame count, sample rate, channels) without
    /// decoding audio. Synchronous but fast (container header only).
    pub fn probe(&self, path: &str) -> Option<tutti_core::WaveMetadata> {
        Wave::probe_metadata(path).ok()
    }

    /// Unload every entry whose path `keep` rejects, freeing its `Arc<Wave>`
    /// (Zrythm's `reload_clip_frame_bufs` model — drop waves no clip references
    /// anymore). A path dropped here re-decodes on its next `get_or_load`. All
    /// entry states are offered to `keep`; the byte account is kept exact.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let mut freed: u64 = 0;
        self.entries.retain(|k, e| {
            if keep(k) {
                return true;
            }
            if let Entry::Ready { bytes, .. } = e {
                freed += *bytes;
            }
            false
        });
        self.resident_bytes = self.resident_bytes.saturating_sub(freed);
    }

    /// Number of tracked entries (any state). Diagnostic / test helper.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total resident bytes across `Ready` entries (`len * channels * 4`).
    /// Diagnostic / test helper for byte-budget eviction.
    pub fn resident_bytes(&self) -> u64 {
        self.resident_bytes
    }
}

/// Decode `path` synchronously. The off-thread mechanism (Bevy task pool or a
/// std thread) wraps this; kept here so both paths decode identically.
pub(crate) fn decode(path: &str) -> Result<Wave, String> {
    Wave::load(path).map_err(|e| format!("{e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2-channel wave of `frames` samples on each channel. Its resident cost
    /// is `frames * 2 * 4` bytes.
    fn wave(frames: usize) -> Wave {
        let mut w = Wave::new(2, 44100.0);
        for i in 0..frames {
            let s = i as f32;
            w.push((s, -s));
        }
        w
    }

    /// Drive an entry to `Ready` the way the wrappers do: `get_or_load` (which
    /// stats the file, so use a non-existent path) then `complete`. Here we skip
    /// the filesystem by injecting a `Loading` entry directly, then completing.
    fn insert_ready(core: &mut WaveCacheCore, path: &str, frames: usize) {
        let key: Arc<str> = Arc::from(path);
        core.entries.insert(
            key.clone(),
            Entry::Loading {
                stamp: FileStamp {
                    mtime_ns: 0,
                    size: 0,
                },
            },
        );
        core.complete(&key, Ok(wave(frames)));
    }

    #[test]
    fn eviction_respects_byte_budget() {
        // Budget for two 100-frame waves (100 * 2 * 4 = 800 bytes each).
        let mut core = WaveCacheCore::with_budget(1600, usize::MAX);
        insert_ready(&mut core, "a", 100);
        insert_ready(&mut core, "b", 100);
        assert_eq!(core.len(), 2);
        assert_eq!(core.resident_bytes(), 1600);

        // A third wave overflows the budget → LRU victim ("a") is evicted.
        insert_ready(&mut core, "c", 100);
        assert_eq!(core.len(), 2);
        assert!(core.resident_bytes() <= 1600);
        assert!(core.peek("a").is_none(), "LRU 'a' evicted");
        assert!(core.peek("b").is_some());
        assert!(core.peek("c").is_some());
    }

    #[test]
    fn entry_cap_evicts_beyond_max_entries() {
        let mut core = WaveCacheCore::with_budget(u64::MAX, 3);
        for name in ["a", "b", "c"] {
            insert_ready(&mut core, name, 10);
        }
        assert_eq!(core.len(), 3);
        insert_ready(&mut core, "d", 10);
        assert_eq!(core.len(), 3, "entry cap holds at 3");
        assert!(core.peek("a").is_none(), "oldest evicted");
        assert!(core.peek("d").is_some());
    }

    #[test]
    fn oversized_single_wave_is_admitted() {
        // One wave larger than the whole budget must still be resident (mirrors
        // the butler: never evict down to zero for a single oversized wave).
        let mut core = WaveCacheCore::with_budget(100, usize::MAX);
        insert_ready(&mut core, "big", 100); // 800 bytes > 100 budget
        assert_eq!(core.len(), 1);
        assert!(core.peek("big").is_some());
    }

    #[test]
    fn recency_ordering_evicts_least_recently_used() {
        let mut core = WaveCacheCore::with_budget(u64::MAX, 2);
        insert_ready(&mut core, "a", 10);
        insert_ready(&mut core, "b", 10);

        // Touch "a" via the real hot path so it becomes most-recent. A stale
        // stamp (mtime 0 vs the real absent file) would re-stat, so instead bump
        // recency directly the way `get_or_load`'s fast path does.
        if let Some(Entry::Ready { used, .. }) = core.entries.get_mut("a") {
            *used = core.clock + 1;
            core.clock += 1;
        }

        // Inserting "c" overflows the entry cap → LRU victim is "b", not "a".
        insert_ready(&mut core, "c", 10);
        assert_eq!(core.len(), 2);
        assert!(core.peek("a").is_some(), "recently touched 'a' survives");
        assert!(core.peek("b").is_none(), "LRU 'b' evicted");
        assert!(core.peek("c").is_some());
    }

    #[test]
    fn retain_unloads_rejected_paths_and_frees_bytes() {
        let mut core = WaveCacheCore::with_budget(u64::MAX, usize::MAX);
        insert_ready(&mut core, "keep", 100);
        insert_ready(&mut core, "drop", 100);
        assert_eq!(core.resident_bytes(), 1600);

        core.retain(|p| p == "keep");
        assert_eq!(core.len(), 1);
        assert!(core.peek("keep").is_some());
        assert!(core.peek("drop").is_none());
        assert_eq!(core.resident_bytes(), 800, "dropped wave's bytes freed");
    }

    #[test]
    fn evicted_then_rerequested_reloads() {
        // A real end-to-end path: write a wav, load it through the ThreadedWaveCache
        // is covered elsewhere. Here, at the core level, drop an entry via retain
        // (an eviction) and confirm the next `get_or_load` asks to spawn again.
        let (_dir, path) = write_wav(500);
        let mut core = WaveCacheCore::with_budget(u64::MAX, usize::MAX);

        // First request spawns a decode.
        let key = match core.get_or_load(&path) {
            LoadAction::Spawn { key, .. } => key,
            other => panic!("expected Spawn, got {:?}", DebugAction(&other)),
        };
        core.complete(&key, Ok(wave(500)));
        assert!(core.peek(&path).is_some());

        // Evict it, then re-request: must Spawn again (reload), not serve stale.
        core.retain(|_| false);
        assert!(core.peek(&path).is_none());
        assert!(
            matches!(core.get_or_load(&path), LoadAction::Spawn { .. }),
            "evicted path reloads on re-request"
        );
    }

    fn write_wav(frames: usize) -> (tempfile::TempDir, String) {
        let mut w = Wave::new(2, 44100.0);
        for i in 0..frames {
            let s = (i as f32 / frames as f32) - 0.5;
            w.push((s, -s));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        w.save_wav16(&path).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    // Minimal Debug shim for `LoadAction` (it isn't `Debug`) used in a panic msg.
    struct DebugAction<'a>(&'a LoadAction);
    impl std::fmt::Debug for DebugAction<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                LoadAction::State(_) => write!(f, "State"),
                LoadAction::Spawn { .. } => write!(f, "Spawn"),
            }
        }
    }
}

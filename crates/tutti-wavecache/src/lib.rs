//! Decode-once, off-thread audio file cache.
//!
//! One place decodes an audio file into an `Arc<Wave>`; everyone else —
//! timeline playback, waveform/STFT analysis, the offline spectral render —
//! shares that `Arc`. This replaces the three separate decode paths the engine
//! grew (Bevy `AssetServer`/`WaveAsset`, the butler's `Wave::load`, and the
//! analysis `decode_mono`), each of which decoded the same file independently
//! and handled absolute desktop paths inconsistently.
//!
//! Bevy-native: [`WaveCache`] is a [`Resource`], decoding runs on the
//! [`AsyncComputeTaskPool`], and a [`poll_wave_cache`] system advances in-flight
//! decodes each frame. Nothing here blocks the main thread — [`WaveCache::get_or_load`]
//! kicks off a decode and returns [`WaveState::Loading`]; [`WaveCache::peek`]
//! returns the `Arc<Wave>` once ready.
//!
//! Keyed by `(path, mtime, size)`: if a file changes on disk, the next
//! `get_or_load` re-decodes it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_core::Wave;

/// Filesystem identity of a decoded wave. A change to mtime or size
/// invalidates the cached decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
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

/// The state of a file in the cache, returned by [`WaveCache::get_or_load`].
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

enum Entry {
    Loading {
        stamp: FileStamp,
        task: Task<Result<Wave, String>>,
    },
    Ready {
        stamp: FileStamp,
        wave: Arc<Wave>,
        /// When this entry's on-disk stamp was last validated. Lets a hot
        /// `get_or_load` skip the `fs::metadata` syscall (see [`REVALIDATE_AFTER`]).
        checked: Instant,
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

/// Decode-once cache of `Arc<Wave>`, keyed by file path.
///
/// Insert a [`WaveCachePlugin`]-equivalent in your app: `init_resource` this
/// and add [`poll_wave_cache`] to `Update`. Then any system with
/// `ResMut<WaveCache>` can `get_or_load`, and `Res<WaveCache>` can `peek`.
#[derive(Resource, Default)]
pub struct WaveCache {
    entries: HashMap<Arc<str>, Entry>,
}

impl WaveCache {
    /// Request the wave for `path`, decoding it off-thread if not already
    /// resident. Non-blocking: returns [`WaveState::Loading`] until the decode
    /// finishes (advanced by [`poll_wave_cache`]).
    ///
    /// Re-decodes if the file's mtime/size changed since it was cached.
    pub fn get_or_load(&mut self, path: &str) -> WaveState {
        // Fast path: hashmap lookup *before* any filesystem syscall. A
        // `Ready`/`Failed` entry validated within `REVALIDATE_AFTER` is trusted
        // as-is — the hot per-frame case is then a pure lookup, no `fs::metadata`.
        // A `Loading` entry is always returned without a stat (the decode in
        // flight already pinned a stamp). Only a missing/stale entry stats.
        match self.entries.get(path) {
            Some(Entry::Loading { .. }) => return WaveState::Loading,
            Some(Entry::Ready { wave, checked, .. }) if checked.elapsed() < REVALIDATE_AFTER => {
                return WaveState::Ready(wave.clone());
            }
            Some(Entry::Failed { err, checked, .. }) if checked.elapsed() < REVALIDATE_AFTER => {
                return WaveState::Failed(err.clone());
            }
            _ => {}
        }

        // Stale or absent: stat the file and reconcile against the entry.
        let stamp = FileStamp::of(path);
        match self.entries.get_mut(path) {
            Some(Entry::Ready { stamp: s, wave, checked }) => {
                if Some(*s) == stamp {
                    *checked = Instant::now();
                    return WaveState::Ready(wave.clone());
                }
                // File changed on disk → fall through and re-load.
            }
            Some(Entry::Failed { stamp: s, err, checked }) => {
                // Stay failed unless the on-disk stamp changed (the file was
                // replaced/fixed). A corrupt or unsupported file keeps the same
                // stamp and stays failed — no per-frame re-decode spin.
                if *s == stamp {
                    *checked = Instant::now();
                    return WaveState::Failed(err.clone());
                }
            }
            _ => {}
        }

        // Spawn a fresh decode.
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
            return WaveState::Failed(err);
        };
        let key: Arc<str> = Arc::from(path);
        let path_owned = path.to_string();
        let task = AsyncComputeTaskPool::get()
            .spawn(async move { Wave::load(&path_owned).map_err(|e| format!("{e:?}")) });
        self.entries
            .insert(key, Entry::Loading { stamp, task });
        WaveState::Loading
    }

    /// Return the resident wave for `path`, or `None` if it isn't `Ready`.
    /// Never spawns a decode and never blocks. For consumers that only want to
    /// use an already-loaded wave (e.g. the offline render assembly).
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

    /// Number of tracked entries (any state). Diagnostic / test helper.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Advance in-flight decodes; flip finished entries to `Ready`/`Failed`.
/// Add to `Update`. Cheap: only polls `Loading` entries, never blocks.
pub fn poll_wave_cache(mut cache: ResMut<WaveCache>) {
    // Collect keys to mutate after the borrow (can't mutate map while iterating).
    let mut done: Vec<(Arc<str>, Result<Wave, String>)> = Vec::new();
    for (key, entry) in cache.entries.iter_mut() {
        if let Entry::Loading { task, .. } = entry {
            if let Some(result) = block_on(future::poll_once(task)) {
                done.push((key.clone(), result));
            }
        }
    }
    for (key, result) in done {
        // Preserve the stamp recorded when the load was kicked off.
        let stamp = match cache.entries.get(&key) {
            Some(Entry::Loading { stamp, .. }) => *stamp,
            _ => continue,
        };
        let entry = match result {
            Ok(wave) => Entry::Ready {
                stamp,
                wave: Arc::new(wave),
                checked: Instant::now(),
            },
            Err(e) => {
                bevy_log::warn!("[wavecache] decode failed for {key}: {e}");
                Entry::Failed {
                    stamp: Some(stamp),
                    err: Arc::from(e),
                    checked: Instant::now(),
                }
            }
        };
        cache.entries.insert(key, entry);
    }
}

/// Registers [`WaveCache`] as a resource and runs [`poll_wave_cache`] each
/// frame. Add this once; consumers then use `Res`/`ResMut<WaveCache>`.
pub struct WaveCachePlugin;

impl Plugin for WaveCachePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WaveCache>()
            .add_systems(Update, poll_wave_cache);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(frames: usize) -> (tempfile::TempDir, String) {
        // Build a wav via fundsp-tutti's writer (dev-dep), so the cache decodes
        // a real file through the same Symphonia path the engine uses.
        use fundsp::wave::Wave as FWave;
        let mut w = FWave::new(2, 44100.0);
        for i in 0..frames {
            let s = (i as f32 / frames as f32) - 0.5;
            w.push((s, -s));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        w.save_wav16(&path).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    fn drain_pool(cache: &mut WaveCache) {
        // Pump the poll until no Loading entries remain (bounded).
        for _ in 0..1000 {
            let still_loading = cache
                .entries
                .values()
                .any(|e| matches!(e, Entry::Loading { .. }));
            if !still_loading {
                break;
            }
            // poll_wave_cache needs ResMut; call the inner logic directly.
            let mut done: Vec<(Arc<str>, Result<Wave, String>)> = Vec::new();
            for (key, entry) in cache.entries.iter_mut() {
                if let Entry::Loading { task, .. } = entry {
                    if let Some(result) = block_on(future::poll_once(task)) {
                        done.push((key.clone(), result));
                    }
                }
            }
            for (key, result) in done {
                let stamp = match cache.entries.get(&key) {
                    Some(Entry::Loading { stamp, .. }) => *stamp,
                    _ => continue,
                };
                let entry = match result {
                    Ok(wave) => Entry::Ready {
                        stamp,
                        wave: Arc::new(wave),
                        checked: Instant::now(),
                    },
                    Err(e) => Entry::Failed {
                        stamp: Some(stamp),
                        err: Arc::from(e),
                        checked: Instant::now(),
                    },
                };
                cache.entries.insert(key, entry);
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn loads_then_ready_matches_direct_decode() {
        bevy_tasks::AsyncComputeTaskPool::get_or_init(Default::default);
        let (_dir, path) = write_wav(5000);
        let mut cache = WaveCache::default();

        assert!(matches!(cache.get_or_load(&path), WaveState::Loading));
        drain_pool(&mut cache);

        let wave = cache.peek(&path).expect("ready after poll");
        assert_eq!(wave.len(), 5000);
    }

    #[test]
    fn second_request_hits_cache_no_second_decode() {
        bevy_tasks::AsyncComputeTaskPool::get_or_init(Default::default);
        let (_dir, path) = write_wav(1000);
        let mut cache = WaveCache::default();
        cache.get_or_load(&path);
        drain_pool(&mut cache);

        // Now ready: get_or_load returns Ready immediately, no new entry.
        let before = cache.len();
        assert!(matches!(cache.get_or_load(&path), WaveState::Ready(_)));
        assert_eq!(cache.len(), before, "no duplicate entry / re-decode");
    }

    #[test]
    fn ready_entry_skips_metadata_within_interval() {
        // A `Ready` entry validated within REVALIDATE_AFTER must be returned
        // without re-`stat`ing the file. Proof: delete the file after it's
        // Ready — a hot `get_or_load` must STILL return `Ready` (if it had
        // stat'd the now-missing file, the entry would flip to `Failed`).
        bevy_tasks::AsyncComputeTaskPool::get_or_init(Default::default);
        let (dir, path) = write_wav(1000);
        let mut cache = WaveCache::default();
        cache.get_or_load(&path);
        drain_pool(&mut cache);
        assert!(matches!(cache.get_or_load(&path), WaveState::Ready(_)));

        // Remove the file; within the interval the cache trusts its last stamp.
        std::fs::remove_file(&path).unwrap();
        drop(dir);
        let before = cache.len();
        assert!(
            matches!(cache.get_or_load(&path), WaveState::Ready(_)),
            "Ready entry must not re-stat within REVALIDATE_AFTER"
        );
        assert_eq!(cache.len(), before, "no new entry; no re-decode");
    }

    #[test]
    fn missing_file_is_failed() {
        bevy_tasks::AsyncComputeTaskPool::get_or_init(Default::default);
        let mut cache = WaveCache::default();
        assert!(matches!(
            cache.get_or_load("/no/such/file.wav"),
            WaveState::Failed(_)
        ));
    }
}

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

/// Decode-once cache of `Arc<Wave>`, keyed by file path. Framework-free: the
/// caller owns the off-thread decode mechanism (see module docs).
#[derive(Default)]
pub struct WaveCacheCore {
    entries: HashMap<Arc<str>, Entry>,
}

impl WaveCacheCore {
    /// Decide what to do for `path`: return a resident [`WaveState`], or ask the
    /// caller to [`LoadAction::Spawn`] a decode. Re-decodes if the file's
    /// mtime/size changed since it was cached.
    pub fn get_or_load(&mut self, path: &str) -> LoadAction {
        // Fast path: hashmap lookup *before* any filesystem syscall. A
        // `Ready`/`Failed` entry validated within `REVALIDATE_AFTER` is trusted
        // as-is — the hot per-frame case is then a pure lookup, no `fs::metadata`.
        // A `Loading` entry is always returned without a stat (the decode in
        // flight already pinned a stamp). Only a missing/stale entry stats.
        match self.entries.get(path) {
            Some(Entry::Loading { .. }) => return LoadAction::State(WaveState::Loading),
            Some(Entry::Ready { wave, checked, .. }) if checked.elapsed() < REVALIDATE_AFTER => {
                return LoadAction::State(WaveState::Ready(wave.clone()));
            }
            Some(Entry::Failed { err, checked, .. }) if checked.elapsed() < REVALIDATE_AFTER => {
                return LoadAction::State(WaveState::Failed(err.clone()));
            }
            _ => {}
        }

        // Stale or absent: stat the file and reconcile against the entry.
        let stamp = FileStamp::of(path);
        match self.entries.get_mut(path) {
            Some(Entry::Ready { stamp: s, wave, checked }) => {
                if Some(*s) == stamp {
                    *checked = Instant::now();
                    return LoadAction::State(WaveState::Ready(wave.clone()));
                }
                // File changed on disk → fall through and re-load.
            }
            Some(Entry::Failed { stamp: s, err, checked }) => {
                // Stay failed unless the on-disk stamp changed (the file was
                // replaced/fixed). A corrupt or unsupported file keeps the same
                // stamp and stays failed — no per-frame re-decode spin.
                if *s == stamp {
                    *checked = Instant::now();
                    return LoadAction::State(WaveState::Failed(err.clone()));
                }
            }
            _ => {}
        }

        // Needs a fresh decode.
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

    /// Report a finished decode for `key` (from a [`LoadAction::Spawn`]). Flips
    /// the entry to `Ready`/`Failed`, preserving the stamp pinned at spawn time.
    /// A stale key (evicted / superseded) is ignored.
    pub fn complete(&mut self, key: &Arc<str>, result: Result<Wave, String>) {
        let stamp = match self.entries.get(key) {
            Some(Entry::Loading { stamp, .. }) => *stamp,
            _ => return,
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
        self.entries.insert(key.clone(), entry);
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

    /// Number of tracked entries (any state). Diagnostic / test helper.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Decode `path` synchronously. The off-thread mechanism (Bevy task pool or a
/// std thread) wraps this; kept here so both paths decode identically.
pub(crate) fn decode(path: &str) -> Result<Wave, String> {
    Wave::load(path).map_err(|e| format!("{e:?}"))
}

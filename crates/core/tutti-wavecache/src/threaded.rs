//! Framework-free async cache: [`WaveCacheCore`] driven by `std::thread`.
//!
//! A non-Bevy host uses this directly. `get_or_load` spawns a detached decode
//! thread that pushes its result onto an mpsc channel; [`ThreadedWaveCache::poll`]
//! drains finished decodes into the core. Non-blocking, no external runtime.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use tutti_core::Wave;

use crate::core::{decode, LoadAction, WaveCacheCore, WaveState};

/// [`WaveCacheCore`] with a `std::thread` decode pool. Call [`get_or_load`] to
/// request a wave and [`poll`] once per tick (or before a batch of `peek`s) to
/// advance finished decodes.
///
/// [`get_or_load`]: ThreadedWaveCache::get_or_load
/// [`poll`]: ThreadedWaveCache::poll
pub struct ThreadedWaveCache {
    core: WaveCacheCore,
    tx: Sender<(Arc<str>, Result<Wave, String>)>,
    rx: Receiver<(Arc<str>, Result<Wave, String>)>,
}

impl Default for ThreadedWaveCache {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            core: WaveCacheCore::default(),
            tx,
            rx,
        }
    }
}

impl ThreadedWaveCache {
    /// Request the wave for `path`, decoding it on a background thread if not
    /// already resident. Non-blocking: returns [`WaveState::Loading`] until a
    /// later [`poll`](Self::poll) picks up the finished decode.
    pub fn get_or_load(&mut self, path: &str) -> WaveState {
        match self.core.get_or_load(path) {
            LoadAction::State(state) => state,
            LoadAction::Spawn { key, path } => {
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send((key, decode(&path)));
                });
                WaveState::Loading
            }
        }
    }

    /// Drain finished decodes into the cache. Cheap; call once per tick.
    pub fn poll(&mut self) {
        while let Ok((key, result)) = self.rx.try_recv() {
            self.core.complete(&key, result);
        }
    }

    /// Resident wave for `path`, or `None` if not `Ready`. Never blocks.
    pub fn peek(&self, path: &str) -> Option<Arc<Wave>> {
        self.core.peek(path)
    }

    /// Cheap metadata probe without decoding audio.
    pub fn probe(&self, path: &str) -> Option<tutti_core::WaveMetadata> {
        self.core.probe(path)
    }

    pub fn len(&self) -> usize {
        self.core.len()
    }

    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(frames: usize) -> (tempfile::TempDir, String) {
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

    /// Poll until `path` leaves the `Loading` state (bounded).
    fn drain_until_ready(cache: &mut ThreadedWaveCache, path: &str) {
        for _ in 0..1000 {
            cache.poll();
            if cache.peek(path).is_some() {
                return;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn loads_then_ready_matches_direct_decode() {
        let (_dir, path) = write_wav(5000);
        let mut cache = ThreadedWaveCache::default();
        assert!(matches!(cache.get_or_load(&path), WaveState::Loading));
        drain_until_ready(&mut cache, &path);
        let wave = cache.peek(&path).expect("ready after poll");
        assert_eq!(wave.len(), 5000);
    }

    #[test]
    fn second_request_hits_cache_no_second_decode() {
        let (_dir, path) = write_wav(1000);
        let mut cache = ThreadedWaveCache::default();
        cache.get_or_load(&path);
        drain_until_ready(&mut cache, &path);
        let before = cache.len();
        assert!(matches!(cache.get_or_load(&path), WaveState::Ready(_)));
        assert_eq!(cache.len(), before, "no duplicate entry / re-decode");
    }

    #[test]
    fn missing_file_is_failed() {
        let mut cache = ThreadedWaveCache::default();
        assert!(matches!(
            cache.get_or_load("/no/such/file.wav"),
            WaveState::Failed(_)
        ));
    }
}

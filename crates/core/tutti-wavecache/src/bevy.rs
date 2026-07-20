//! Bevy ECS layer: [`WaveCacheCore`] as a [`Resource`], decoding on the
//! [`AsyncComputeTaskPool`].
//!
//! [`WaveCache::get_or_load`] kicks off a decode and returns
//! [`WaveState::Loading`]; [`poll_wave_cache`] advances in-flight decodes each
//! frame; [`WaveCache::peek`] returns the `Arc<Wave>` once ready. This mirrors
//! the framework-free [`ThreadedWaveCache`](crate::ThreadedWaveCache), swapping
//! `std::thread` for the Bevy task pool.

use std::collections::HashMap;
use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_core::Wave;

use crate::core::{decode, LoadAction, WaveCacheCore, WaveState};

/// Decode-once cache of `Arc<Wave>`, keyed by file path, as a Bevy resource.
///
/// `init_resource` this and add [`poll_wave_cache`] to `Update` (or just add
/// [`WaveCachePlugin`]). Then any system with `ResMut<WaveCache>` can
/// `get_or_load`, and `Res<WaveCache>` can `peek`.
#[derive(Resource, Default)]
pub struct WaveCache {
    core: WaveCacheCore,
    /// In-flight decodes, keyed by the same `Arc<str>` the core pinned.
    tasks: HashMap<Arc<str>, Task<Result<Wave, String>>>,
}

impl WaveCache {
    /// Request the wave for `path`, decoding it off-thread if not already
    /// resident. Non-blocking: returns [`WaveState::Loading`] until the decode
    /// finishes (advanced by [`poll_wave_cache`]).
    ///
    /// Re-decodes if the file's mtime/size changed since it was cached.
    pub fn get_or_load(&mut self, path: &str) -> WaveState {
        match self.core.get_or_load(path) {
            LoadAction::State(state) => state,
            LoadAction::Spawn { key, path } => {
                let task = AsyncComputeTaskPool::get().spawn(async move { decode(&path) });
                self.tasks.insert(key, task);
                WaveState::Loading
            }
        }
    }

    /// Return the resident wave for `path`, or `None` if it isn't `Ready`.
    /// Never spawns a decode and never blocks.
    pub fn peek(&self, path: &str) -> Option<Arc<Wave>> {
        self.core.peek(path)
    }

    /// Cheap metadata probe (frame count, sample rate, channels) without
    /// decoding audio. Synchronous but fast (container header only).
    pub fn probe(&self, path: &str) -> Option<tutti_core::WaveMetadata> {
        self.core.probe(path)
    }

    /// Number of tracked entries (any state). Diagnostic / test helper.
    pub fn len(&self) -> usize {
        self.core.len()
    }

    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }
}

/// Advance in-flight decodes; flip finished entries to `Ready`/`Failed`.
/// Add to `Update`. Cheap: only polls `Loading` tasks, never blocks.
pub fn poll_wave_cache(mut cache: ResMut<WaveCache>) {
    let mut done: Vec<(Arc<str>, Result<Wave, String>)> = Vec::new();
    cache.tasks.retain(|key, task| {
        if let Some(result) = block_on(future::poll_once(task)) {
            done.push((key.clone(), result));
            false // task finished — drop it
        } else {
            true
        }
    });
    for (key, result) in done {
        if let Err(e) = &result {
            bevy_log::warn!("[wavecache] decode failed for {key}: {e}");
        }
        cache.core.complete(&key, result);
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
        for _ in 0..1000 {
            if cache.tasks.is_empty() {
                break;
            }
            poll_once(cache);
            std::thread::yield_now();
        }
    }

    // `poll_wave_cache` needs `ResMut`; call the same logic on a bare handle.
    fn poll_once(cache: &mut WaveCache) {
        let mut done: Vec<(Arc<str>, Result<Wave, String>)> = Vec::new();
        cache.tasks.retain(|key, task| {
            if let Some(result) = block_on(future::poll_once(task)) {
                done.push((key.clone(), result));
                false
            } else {
                true
            }
        });
        for (key, result) in done {
            cache.core.complete(&key, result);
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

        let before = cache.len();
        assert!(matches!(cache.get_or_load(&path), WaveState::Ready(_)));
        assert_eq!(cache.len(), before, "no duplicate entry / re-decode");
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

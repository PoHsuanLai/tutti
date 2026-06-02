//! SoundFont file management with lock-free caching.

use crate::error::{Error, Result};
use core::sync::atomic::{AtomicUsize, Ordering};
use dashmap::DashMap;
use rustysynth::{SoundFont, SynthesizerSettings};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use tutti_core::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SoundFontHandle(usize);

impl SoundFontHandle {
    pub fn id(&self) -> usize {
        self.0
    }
}

pub struct SoundFontSystem {
    sample_rate: u32,
    soundfonts: DashMap<usize, Arc<SoundFont>>,
    path_to_handle: DashMap<PathBuf, SoundFontHandle>,
    next_handle: AtomicUsize,
}

impl SoundFontSystem {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            soundfonts: DashMap::new(),
            path_to_handle: DashMap::new(),
            next_handle: AtomicUsize::new(0),
        }
    }

    pub fn load(&self, path: impl AsRef<Path>) -> Result<SoundFontHandle> {
        let path = path.as_ref().to_path_buf();

        if let Some(handle) = self.path_to_handle.get(&path) {
            return Ok(*handle);
        }

        let file = File::open(&path)?;

        let mut reader = BufReader::new(file);
        let soundfont = Arc::new(SoundFont::new(&mut reader).map_err(|e| {
            Error::SoundFont(format!(
                "Failed to parse SoundFont file '{}': {}",
                path.display(),
                e
            ))
        })?);

        let handle_id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let handle = SoundFontHandle(handle_id);

        self.soundfonts.insert(handle_id, soundfont);
        self.path_to_handle.insert(path, handle);

        Ok(handle)
    }

    pub fn get(&self, handle: &SoundFontHandle) -> Option<Arc<SoundFont>> {
        self.soundfonts
            .get(&handle.0)
            .map(|entry| entry.value().clone())
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn default_settings(&self) -> SynthesizerSettings {
        SynthesizerSettings::new(self.sample_rate as i32)
    }

    pub fn len(&self) -> usize {
        self.soundfonts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.soundfonts.is_empty()
    }

    pub fn handles(&self) -> Vec<SoundFontHandle> {
        self.soundfonts
            .iter()
            .map(|entry| SoundFontHandle(*entry.key()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_soundfont_manager_creation() {
        let manager = SoundFontSystem::new(44100);
        assert_eq!(manager.sample_rate(), 44100);
        assert!(manager.is_empty());
        assert_eq!(manager.len(), 0);
    }

    // Note: Loading tests would require actual .sf2 files
    // These should be added in integration tests with test fixtures
}

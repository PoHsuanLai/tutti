//! LRU memory cache with optional disk persistence for waveform thumbnails.

use crate::waveform::MultiResolutionSummary;
use lru::LruCache;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

pub struct ThumbnailCache {
    memory_cache: LruCache<u64, MultiResolutionSummary>,
    disk_path: Option<PathBuf>,
}

impl ThumbnailCache {
    pub fn new(max_entries: usize) -> Self {
        let capacity =
            NonZeroUsize::new(max_entries.max(1)).expect("BUG: max(1) should always be non-zero");
        Self {
            memory_cache: LruCache::new(capacity),
            disk_path: None,
        }
    }

    pub fn with_disk_cache(max_memory_entries: usize, disk_path: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&disk_path)?;

        let capacity = NonZeroUsize::new(max_memory_entries.max(1))
            .expect("BUG: max(1) should always be non-zero");
        Ok(Self {
            memory_cache: LruCache::new(capacity),
            disk_path: Some(disk_path),
        })
    }

    /// Checks memory, then disk, then calls `compute`.
    pub fn get_or_compute<F>(&mut self, hash: u64, compute: F) -> &MultiResolutionSummary
    where
        F: FnOnce() -> MultiResolutionSummary,
    {
        if self.memory_cache.contains(&hash) {
            return self
                .memory_cache
                .get(&hash)
                .expect("BUG: entry should exist (checked with contains)");
        }

        if let Some(ref disk_path) = self.disk_path {
            if let Some(summary) = self.load_from_disk(disk_path, hash) {
                self.memory_cache.put(hash, summary);
                return self
                    .memory_cache
                    .get(&hash)
                    .expect("BUG: entry should exist (just put)");
            }
        }

        let summary = compute();

        if let Some(ref disk_path) = self.disk_path {
            let _ = self.save_to_disk(disk_path, hash, &summary);
        }

        self.memory_cache.put(hash, summary);
        self.memory_cache
            .get(&hash)
            .expect("BUG: entry should exist (just put)")
    }

    pub fn get(&mut self, hash: u64) -> Option<&MultiResolutionSummary> {
        if self.memory_cache.contains(&hash) {
            return self.memory_cache.get(&hash);
        }

        if let Some(ref disk_path) = self.disk_path {
            if let Some(summary) = self.load_from_disk(disk_path, hash) {
                self.memory_cache.put(hash, summary);
                return self.memory_cache.get(&hash);
            }
        }

        None
    }

    pub fn put(&mut self, hash: u64, summary: MultiResolutionSummary) {
        if let Some(ref disk_path) = self.disk_path {
            let _ = self.save_to_disk(disk_path, hash, &summary);
        }

        self.memory_cache.put(hash, summary);
    }

    pub fn remove(&mut self, hash: u64) {
        self.memory_cache.pop(&hash);

        if let Some(ref disk_path) = self.disk_path {
            let path = self.disk_cache_path(disk_path, hash);
            let _ = fs::remove_file(path);
        }
    }

    pub fn clear(&mut self) {
        self.memory_cache.clear();

        if let Some(ref disk_path) = self.disk_path {
            if let Ok(entries) = fs::read_dir(disk_path) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "thumb") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.memory_cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.memory_cache.is_empty()
    }

    fn disk_cache_path(&self, base: &Path, hash: u64) -> PathBuf {
        base.join(format!("{:016x}.thumb", hash))
    }

    fn load_from_disk(&self, base: &Path, hash: u64) -> Option<MultiResolutionSummary> {
        let path = self.disk_cache_path(base, hash);
        let mut file = File::open(path).ok()?;

        let mut data = Vec::new();
        file.read_to_end(&mut data).ok()?;

        // Simple binary format: [num_levels: u32] [for each level: [num_blocks: u32] [blocks...]]
        self.deserialize_summary(&data)
    }

    fn save_to_disk(
        &self,
        base: &Path,
        hash: u64,
        summary: &MultiResolutionSummary,
    ) -> io::Result<()> {
        let path = self.disk_cache_path(base, hash);
        let mut file = File::create(path)?;

        let data = self.serialize_summary(summary);
        file.write_all(&data)?;

        Ok(())
    }

    fn serialize_summary(&self, summary: &MultiResolutionSummary) -> Vec<u8> {
        let mut data = Vec::new();

        data.push(1u8); // version
        data.extend_from_slice(&(summary.base_samples_per_block as u32).to_le_bytes());
        data.extend_from_slice(&(summary.levels.len() as u32).to_le_bytes());

        for level in &summary.levels {
            data.extend_from_slice(&(level.samples_per_block as u32).to_le_bytes());
            data.extend_from_slice(&(level.total_samples as u64).to_le_bytes());
            data.extend_from_slice(&(level.blocks.len() as u32).to_le_bytes());

            for block in &level.blocks {
                data.extend_from_slice(&block.min.to_le_bytes());
                data.extend_from_slice(&block.max.to_le_bytes());
                data.extend_from_slice(&block.rms.to_le_bytes());
            }
        }

        data
    }

    fn deserialize_summary(&self, data: &[u8]) -> Option<MultiResolutionSummary> {
        if data.is_empty() {
            return None;
        }

        let mut pos = 0;

        let version = data.get(pos)?;
        if *version != 1 {
            return None;
        }
        pos += 1;

        let base_samples_per_block =
            u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;

        let num_levels = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;

        let mut levels = Vec::with_capacity(num_levels);

        for _ in 0..num_levels {
            let samples_per_block =
                u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
            pos += 4;

            let total_samples =
                u64::from_le_bytes(data.get(pos..pos + 8)?.try_into().ok()?) as usize;
            pos += 8;

            let num_blocks = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
            pos += 4;

            let mut blocks = Vec::with_capacity(num_blocks);

            for _ in 0..num_blocks {
                let min = f32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
                pos += 4;
                let max = f32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
                pos += 4;
                let rms = f32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
                pos += 4;

                blocks.push(crate::waveform::WaveformBlock { min, max, rms });
            }

            levels.push(crate::waveform::WaveformSummary {
                blocks,
                samples_per_block,
                total_samples,
            });
        }

        Some(MultiResolutionSummary {
            levels,
            base_samples_per_block,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waveform::{WaveformBlock, WaveformSummary};

    fn create_test_summary() -> MultiResolutionSummary {
        MultiResolutionSummary {
            levels: vec![
                WaveformSummary {
                    blocks: vec![
                        WaveformBlock {
                            min: -0.5,
                            max: 0.5,
                            rms: 0.3,
                        },
                        WaveformBlock {
                            min: -0.8,
                            max: 0.8,
                            rms: 0.5,
                        },
                    ],
                    samples_per_block: 512,
                    total_samples: 1024,
                },
                WaveformSummary {
                    blocks: vec![WaveformBlock {
                        min: -0.8,
                        max: 0.8,
                        rms: 0.4,
                    }],
                    samples_per_block: 1024,
                    total_samples: 1024,
                },
            ],
            base_samples_per_block: 512,
        }
    }

    #[test]
    fn test_memory_cache() {
        let mut cache = ThumbnailCache::new(10);

        let summary = create_test_summary();
        cache.put(12345, summary.clone());

        assert_eq!(cache.len(), 1);

        let retrieved = cache.get(12345);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().levels.len(), 2);

        // Non-existent key
        assert!(cache.get(99999).is_none());
    }

    #[test]
    fn test_get_or_compute() {
        let mut cache = ThumbnailCache::new(10);

        let mut compute_count = 0;

        // First call should compute
        let _ = cache.get_or_compute(12345, || {
            compute_count += 1;
            create_test_summary()
        });
        assert_eq!(compute_count, 1);

        // Second call should use cache
        let _ = cache.get_or_compute(12345, || {
            compute_count += 1;
            create_test_summary()
        });
        assert_eq!(compute_count, 1); // Should still be 1
    }

    #[test]
    fn test_lru_eviction() {
        let mut cache = ThumbnailCache::new(2);

        cache.put(1, create_test_summary());
        cache.put(2, create_test_summary());
        cache.put(3, create_test_summary()); // Should evict 1

        assert_eq!(cache.len(), 2);
        assert!(cache.get(1).is_none()); // Should be evicted
        assert!(cache.get(2).is_some());
        assert!(cache.get(3).is_some());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let cache = ThumbnailCache::new(10);
        let original = create_test_summary();

        let serialized = cache.serialize_summary(&original);
        let deserialized = cache.deserialize_summary(&serialized);

        assert!(deserialized.is_some());
        let restored = deserialized.unwrap();

        assert_eq!(restored.levels.len(), original.levels.len());
        assert_eq!(
            restored.base_samples_per_block,
            original.base_samples_per_block
        );

        for (orig_level, rest_level) in original.levels.iter().zip(restored.levels.iter()) {
            assert_eq!(orig_level.blocks.len(), rest_level.blocks.len());
            assert_eq!(orig_level.samples_per_block, rest_level.samples_per_block);

            for (orig_block, rest_block) in orig_level.blocks.iter().zip(rest_level.blocks.iter()) {
                assert!((orig_block.min - rest_block.min).abs() < 0.0001);
                assert!((orig_block.max - rest_block.max).abs() < 0.0001);
                assert!((orig_block.rms - rest_block.rms).abs() < 0.0001);
            }
        }
    }

    #[test]
    fn test_clear() {
        let mut cache = ThumbnailCache::new(10);

        cache.put(1, create_test_summary());
        cache.put(2, create_test_summary());
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn test_remove() {
        let mut cache = ThumbnailCache::new(10);

        cache.put(1, create_test_summary());
        cache.put(2, create_test_summary());

        cache.remove(1);

        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_some());
        assert_eq!(cache.len(), 1);
    }
}

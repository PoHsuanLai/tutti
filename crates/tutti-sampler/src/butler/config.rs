//! Butler thread configuration.

#[derive(Debug, Clone, Copy)]
pub struct BufferConfig {
    /// Default: 16384 (aligned to 16KB)
    pub chunk_size: usize,
    /// Default: 8192
    pub flush_threshold: usize,
    /// Default: 64
    pub cache_max_entries: usize,
    /// Default: 1GB
    pub cache_max_bytes: u64,
    /// Default: 512 (~12ms @ 44.1kHz)
    pub seek_crossfade_samples: usize,
    /// When true, multiple streams are refilled concurrently via rayon.
    pub parallel_io: bool,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            chunk_size: 16384,
            flush_threshold: 8192,
            cache_max_entries: 64,
            cache_max_bytes: 1024 * 1024 * 1024, // 1GB
            seek_crossfade_samples: 512,
            parallel_io: true,
        }
    }
}

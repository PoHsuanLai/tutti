//! Host-local configuration for a plugin bridge. Not wire data —
//! controls how *this* process connects to and manages a plugin-server.

use crate::protocol::SampleFormat;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub socket_path: PathBuf,
    pub shm_prefix: String,
    pub max_buffer_size: usize,
    pub timeout_ms: u64,
    #[serde(default)]
    pub preferred_format: SampleFormat,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            socket_path: std::env::temp_dir().join("tutti-bridge.sock"),
            shm_prefix: "tutti_audio_".to_string(),
            max_buffer_size: 8192,
            timeout_ms: 5000,
            preferred_format: SampleFormat::Float32,
        }
    }
}

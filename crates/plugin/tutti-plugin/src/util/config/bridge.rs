//! Host-local configuration for a plugin bridge. Not wire data —
//! controls how *this* process connects to and manages a plugin-server.

use crate::protocol::SampleFormat;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A socket path no other bridge in this process will pick.
///
/// Every bridge needs its own: the path is a rendezvous point for exactly one
/// host/server pair, and `bind` unlinks whatever is already there. Two bridges
/// sharing one path means the second unlinks the first's *live* socket, and
/// then either `ProcessGuard::drop` deletes the survivor's socket out from
/// under it. The symptom is a plugin that dies when an unrelated plugin is
/// unloaded — far from the cause.
///
/// The pid keeps it unique across concurrent hosts; the counter keeps it unique
/// within one. Public because this is the only correct way to name a bridge
/// socket — hand-rolling one is how the collision above arises.
pub fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tutti-bridge-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

/// How this process connects to and manages one plugin-server subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Unix socket this bridge's host/server pair rendezvous on. Must be unique
    /// per bridge — see `unique_socket_path`.
    pub socket_path: PathBuf,
    /// Prefix for the shared-memory audio slab's OS name.
    pub shm_prefix: String,
    /// Largest block, in **frames**, the plugin will be asked to process. Sizes
    /// the slab, so a later block may not exceed it.
    pub max_buffer_size: usize,
    /// How long to wait on a subprocess reply before erroring, in milliseconds.
    pub timeout_ms: u64,
    /// Sample format to request; the subprocess replies with what it negotiated.
    #[serde(default)]
    pub preferred_format: SampleFormat,
}

impl Default for BridgeConfig {
    /// **`socket_path` is unique per call, not a constant.**
    ///
    /// A fixed path would make every `..BridgeConfig::default()` a latent
    /// collision — the second bridge to bind unlinks the first's live socket,
    /// and the symptom is a plugin dying when an unrelated plugin is unloaded.
    /// A default that is safe only when overridden is not a default: deriving it
    /// here means a caller must go out of its way to *create* a collision rather
    /// than to avoid one.
    fn default() -> Self {
        Self {
            socket_path: unique_socket_path(),
            shm_prefix: "tutti_audio_".to_string(),
            max_buffer_size: 8192,
            timeout_ms: 5000,
            preferred_format: SampleFormat::Float32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserted on `default()` itself rather than on `unique_socket_path`: the
    /// property that matters is that a *default* config does not collide, and a
    /// test of the helper alone passes even when `default()` stops calling it.
    #[test]
    fn each_default_config_gets_its_own_socket_path() {
        let a = BridgeConfig::default();
        let b = BridgeConfig::default();
        assert_ne!(
            a.socket_path, b.socket_path,
            "two default configs share a socket path: the second bridge to bind \
             will unlink the first's live socket"
        );
    }

    /// A partial-update construction is the exact shape `to_bridge_config` uses,
    /// and the shape through which a shared path would propagate.
    #[test]
    fn partial_update_from_default_still_gets_a_unique_socket() {
        let a = BridgeConfig {
            timeout_ms: 1,
            ..BridgeConfig::default()
        };
        let b = BridgeConfig {
            timeout_ms: 2,
            ..BridgeConfig::default()
        };
        assert_ne!(a.socket_path, b.socket_path);
    }
}

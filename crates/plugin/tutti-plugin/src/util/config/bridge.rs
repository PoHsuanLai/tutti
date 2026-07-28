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
/// socket, and callers that hand-rolled it are how the collision arose.
pub fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tutti-bridge-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

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
    /// **`socket_path` is unique per call, not a constant.** It used to be a
    /// fixed `tutti-bridge.sock`, which made every `..BridgeConfig::default()`
    /// a latent collision: `Plugins::load` inherited it through
    /// `CatalogConfig::to_bridge_config`, so loading a second plugin unlinked
    /// the first one's live socket. The sites that worked did so only because
    /// they happened to override the field.
    ///
    /// A default that is safe only when overridden is not a default. Deriving
    /// it here means a caller must go out of its way to create a collision
    /// rather than go out of its way to avoid one.
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

    /// The property the catalog needed and did not have. Asserted on `default()`
    /// itself rather than on the helper, because the bug was that a *default*
    /// config collided — testing only `unique_socket_path` would have passed
    /// while the collision remained.
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
    /// and the shape that inherited the shared path.
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

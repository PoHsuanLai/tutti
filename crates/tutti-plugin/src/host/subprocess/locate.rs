//! Locate the `plugin-server` binary.
//!
//! Search order: `TUTTI_PLUGIN_SERVER` env → next to current exe →
//! parent dir (for `examples/` subdir) → `PATH`.

use crate::error::{BridgeError, Result};
use std::path::PathBuf;

pub(super) fn find_plugin_server() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("TUTTI_PLUGIN_SERVER") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        tracing::warn!("TUTTI_PLUGIN_SERVER={p} does not exist, falling back to search");
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("plugin-server");
            if candidate.exists() {
                return Ok(candidate);
            }
            if let Some(parent) = exe_dir.parent() {
                let candidate = parent.join("plugin-server");
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(sep) {
            let candidate = PathBuf::from(dir).join("plugin-server");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err(BridgeError::ServerNotFound)
}

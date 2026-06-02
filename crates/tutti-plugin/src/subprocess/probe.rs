//! One-shot metadata probe — spawn a plugin-server in probe-only mode,
//! query the plugin's factory info, kill the subprocess.

use super::locate::find_plugin_server;
use crate::error::{BridgeError, Result};
use crate::protocol::{BridgeMessage, HostMessage, PluginInfo};
use crate::transport::control::{self as ipc, ControlStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_ATTEMPTS: u32 = 20;
const CONNECT_BACKOFF: Duration = Duration::from_millis(100);

pub fn probe_metadata(plugin_path: &Path) -> Result<PluginInfo> {
    let server_path = find_plugin_server()?;
    let socket_path = next_socket_path();

    let _ = std::fs::remove_file(&socket_path);

    let mut child = Command::new(&server_path)
        .arg(&socket_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("TUTTI_PROBE_MODE", "1")
        .spawn()
        .map_err(BridgeError::Io)?;

    let result = run_probe(&socket_path, plugin_path);

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&socket_path);

    result
}

fn run_probe(socket_path: &Path, plugin_path: &Path) -> Result<PluginInfo> {
    let stream = connect_with_retry(socket_path)?;
    probe_exchange(stream, plugin_path)
}

fn connect_with_retry(socket: &Path) -> Result<ControlStream> {
    for _ in 0..CONNECT_ATTEMPTS {
        if let Ok(stream) = ipc::connect(socket) {
            return Ok(stream);
        }
        thread::sleep(CONNECT_BACKOFF);
    }
    Err(BridgeError::Timeout {
        operation: "connect to plugin-server".into(),
        duration_ms: (CONNECT_ATTEMPTS * CONNECT_BACKOFF.as_millis() as u32) as u64,
    })
}

fn probe_exchange(mut stream: ControlStream, plugin_path: &Path) -> Result<PluginInfo> {
    match ipc::recv_within(&mut stream, PROBE_TIMEOUT)? {
        BridgeMessage::Ready => {}
        ref other => return Err(BridgeError::unexpected_message("Ready", other)),
    }

    ipc::send(
        &mut stream,
        &HostMessage::ProbePlugin {
            path: plugin_path.to_path_buf(),
        },
    )?;

    match ipc::recv_within(&mut stream, PROBE_TIMEOUT)? {
        BridgeMessage::PluginLoaded { metadata, .. } => Ok(*metadata),
        BridgeMessage::Error { message } => {
            Err(BridgeError::load_from_server(plugin_path, message))
        }
        ref other => Err(BridgeError::unexpected_message("PluginLoaded", other)),
    }
}

fn next_socket_path() -> PathBuf {
    static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tutti-probe-{}-{}.sock",
        std::process::id(),
        PROBE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

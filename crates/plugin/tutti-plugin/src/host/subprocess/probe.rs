//! One-shot metadata probe — spawn a plugin-server in probe-only mode,
//! query the plugin's factory info, kill the subprocess.

use super::locate::ServerLocator;
use crate::error::{BridgeError, Result};
use crate::protocol::{BridgeMessage, HostMessage, PluginDescriptor};
use crate::util::transport::control::{self as ipc, ControlStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_ATTEMPTS: u32 = 20;
const CONNECT_BACKOFF: Duration = Duration::from_millis(100);

pub fn probe_metadata(plugin_path: &Path) -> Result<PluginDescriptor> {
    probe_metadata_with(&ServerLocator::from_env(), plugin_path)
}

/// [`probe_metadata`], with the server binary named explicitly rather than
/// searched for.
///
/// The same seam `launch_with` provides, for the same reason: the choice of
/// binary is an argument so that substituting one does not require writing to
/// `TUTTI_PLUGIN_SERVER`, which every thread in the process shares.
///
/// `#[cfg_attr(not(test), allow(dead_code))]` rather than deletion: probe and
/// launch are the *two* entry points that spawn a server, and leaving only one
/// of them injectable is how the next test reaches for `set_var` again.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn probe_metadata_with(
    locator: &ServerLocator,
    plugin_path: &Path,
) -> Result<PluginDescriptor> {
    let server_path = locator.resolve()?;
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

fn run_probe(socket_path: &Path, plugin_path: &Path) -> Result<PluginDescriptor> {
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

fn probe_exchange(mut stream: ControlStream, plugin_path: &Path) -> Result<PluginDescriptor> {
    match ipc::recv_within(&mut stream, PROBE_TIMEOUT)? {
        BridgeMessage::Ready { protocol_version } => {
            crate::protocol::check_protocol_version(protocol_version)?;
        }
        ref other => return Err(BridgeError::unexpected_message("Ready", other)),
    }

    ipc::send(
        &mut stream,
        &HostMessage::ProbePlugin {
            path: plugin_path.to_path_buf(),
        },
    )?;

    match ipc::recv_within(&mut stream, PROBE_TIMEOUT)? {
        BridgeMessage::PluginLoaded { descriptor, .. } => Ok(*descriptor),
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

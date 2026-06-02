//! Full launch sequence for a plugin-server subprocess:
//! spawn → handshake → load plugin → setup shared memory → return
//! a [`LaunchedServer`] ready for the RT bridge thread to connect.

use super::locate::find_plugin_server;
use crate::config::BridgeConfig;
use crate::error::{BridgeError, Result};
use crate::protocol::{BridgeMessage, HostMessage, PluginInfo, SampleFormat};
use crate::transport::control::{self as ipc, ControlStream};
use crate::transport::shm::{AudioSlab, SlabLayout};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long to let the plugin-server bind its socket before we connect.
const STARTUP_DELAY: Duration = Duration::from_millis(500);

pub struct LaunchedServer {
    pub process: Child,
    pub metadata: PluginInfo,
    pub format: SampleFormat,
    pub audio_buffer: Arc<AudioSlab>,
}

pub fn launch(
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: f64,
) -> Result<LaunchedServer> {
    let process = spawn_process(config)?;
    let mut stream = handshake(config)?;

    let shm_name = next_shm_name();
    let (metadata, format) =
        load_plugin(&mut stream, config, plugin_path, sample_rate, &shm_name)?;
    let audio_buffer = setup_shm(&mut stream, config, &metadata, format, shm_name)?;

    Ok(LaunchedServer {
        process,
        metadata: *metadata,
        format,
        audio_buffer,
    })
}

fn spawn_process(config: &BridgeConfig) -> Result<Child> {
    let server_path = find_plugin_server()?;
    tracing::debug!("spawning plugin-server: {}", server_path.display());
    Command::new(server_path)
        .arg(&config.socket_path)
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(BridgeError::Io)
}

fn handshake(config: &BridgeConfig) -> Result<ControlStream> {
    thread::sleep(STARTUP_DELAY);
    let mut stream = ipc::connect(&config.socket_path)?;
    let timeout = Duration::from_millis(config.timeout_ms);
    match ipc::recv_within(&mut stream, timeout)? {
        BridgeMessage::Ready => Ok(stream),
        ref other => Err(BridgeError::unexpected_message("Ready", other)),
    }
}

fn load_plugin(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: f64,
    shm_name: &str,
) -> Result<(Box<PluginInfo>, SampleFormat)> {
    ipc::send(
        stream,
        &HostMessage::LoadPlugin {
            path: plugin_path.to_path_buf(),
            sample_rate,
            block_size: config.max_buffer_size,
            preferred_format: config.preferred_format,
            shm_name: shm_name.to_string(),
        },
    )?;

    let timeout = Duration::from_millis(config.timeout_ms);
    match ipc::recv_within(stream, timeout)? {
        BridgeMessage::PluginLoaded {
            metadata,
            negotiated_format,
        } => Ok((metadata, negotiated_format)),
        BridgeMessage::Error { message } => {
            Err(BridgeError::load_from_server(plugin_path, message))
        }
        ref other => Err(BridgeError::unexpected_message("PluginLoaded", other)),
    }
}

fn setup_shm(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    metadata: &PluginInfo,
    format: SampleFormat,
    shm_name: String,
) -> Result<Arc<AudioSlab>> {
    // Carry the plugin's per-bus layout on the slab so both processes agree
    // how the flat channel range maps to buses. Negotiated once here at load;
    // empty when the plugin reported no multi-bus layout (single-bus legacy).
    let buses = metadata.buses.clone();
    let channels = if buses.is_empty() {
        // Legacy single-bus: one flat channel set shared in-place by both
        // directions, sized to the wider of the two.
        metadata
            .audio_io
            .inputs
            .max(metadata.audio_io.outputs)
            .max(2)
    } else {
        // Multi-bus: input and output directions occupy disjoint flat ranges so
        // sidechain inputs survive the output write (see `SlabLayout::output_base`).
        let total_in: usize = metadata.input_bus_channels().iter().sum();
        let total_out: usize = metadata.output_bus_channels().iter().sum();
        (total_in + total_out).max(2)
    };
    let layout = SlabLayout {
        channels,
        samples_per_channel: config.max_buffer_size,
        format,
        buses,
    };
    let audio_buffer = Arc::new(AudioSlab::create(shm_name.clone(), layout.clone())?);

    ipc::send(stream, &HostMessage::SetupSharedMemory { shm_name, layout })?;

    let timeout = Duration::from_millis(config.timeout_ms);
    match ipc::recv_within(stream, timeout)? {
        BridgeMessage::SharedMemoryReady => Ok(audio_buffer),
        ref other => Err(BridgeError::unexpected_message("SharedMemoryReady", other)),
    }
}

fn next_shm_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SHM_COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "tutti_plugin_{}_{}",
        std::process::id(),
        SHM_COUNTER.fetch_add(1, Ordering::Relaxed),
    )
}

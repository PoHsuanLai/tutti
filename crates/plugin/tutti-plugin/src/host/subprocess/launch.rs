//! Full launch sequence for a plugin-server subprocess:
//! spawn → handshake → load plugin → setup shared memory → return
//! a [`LaunchedServer`] ready for the RT bridge thread to connect.

use super::locate::find_plugin_server;
use crate::util::config::BridgeConfig;
use crate::error::{BridgeError, Result};
use crate::protocol::{
    BridgeMessage, BusChannels, HostMessage, LoadedPlugin, PluginDescriptor, SampleFormat,
};
use crate::util::transport::control::{self as ipc, ControlStream};
use crate::util::transport::shm::{AudioSlab, SlabLayout};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long to let the plugin-server bind its socket before we connect.
const STARTUP_DELAY: Duration = Duration::from_millis(500);

pub struct LaunchedServer {
    pub process: Child,
    /// Catalog identity (name, vendor, class, editor).
    pub descriptor: PluginDescriptor,
    /// Engine-wiring data from instantiation (bus widths, latency, f64).
    pub loaded: LoadedPlugin,
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
    let (descriptor, loaded, format) =
        load_plugin(&mut stream, config, plugin_path, sample_rate, &shm_name)?;
    let audio_buffer = setup_shm(&mut stream, config, &loaded, format, shm_name)?;

    Ok(LaunchedServer {
        process,
        descriptor: *descriptor,
        loaded,
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

#[allow(clippy::type_complexity)]
fn load_plugin(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    plugin_path: &Path,
    sample_rate: f64,
    shm_name: &str,
) -> Result<(Box<PluginDescriptor>, LoadedPlugin, SampleFormat)> {
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
            descriptor,
            loaded,
            negotiated_format,
        } => Ok((descriptor, loaded, negotiated_format)),
        BridgeMessage::Error { message } => {
            Err(BridgeError::load_from_server(plugin_path, message))
        }
        ref other => Err(BridgeError::unexpected_message("PluginLoaded", other)),
    }
}

fn setup_shm(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    loaded: &LoadedPlugin,
    format: SampleFormat,
    shm_name: String,
) -> Result<Arc<AudioSlab>> {
    // Carry the plugin's per-bus layout on the slab so both processes agree how
    // the flat channel range maps to buses. Loaders always populate at least the
    // main bus, so a single-bus plugin reports `inputs = [main_in]` /
    // `outputs = [main_out]`. A truly empty list means "unknown" (filename
    // fallback) and collapses to the legacy single shared flat range.
    let single_bus = loaded.inputs.len() <= 1 && loaded.outputs.len() <= 1;
    let channels = if single_bus {
        // One main bus per direction: a single flat channel set shared in-place,
        // sized to the wider of the two directions.
        loaded.total_inputs().max(loaded.total_outputs()).max(2)
    } else {
        // Multi-bus: input and output directions occupy disjoint flat ranges so
        // sidechain inputs survive the output write (see `SlabLayout::output_base`).
        (loaded.total_inputs() + loaded.total_outputs()).max(2)
    };
    // Only carry the per-bus partition on the slab when it's genuinely multi-bus;
    // a single main bus per direction stays the legacy in-place flat layout.
    let (inputs, outputs) = if single_bus {
        (BusChannels::new(), BusChannels::new())
    } else {
        (loaded.inputs.clone(), loaded.outputs.clone())
    };
    let layout = SlabLayout {
        channels,
        samples_per_channel: config.max_buffer_size,
        format,
        inputs,
        outputs,
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

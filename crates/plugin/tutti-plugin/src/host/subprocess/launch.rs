//! Full launch sequence for a plugin-server subprocess:
//! spawn → handshake → load plugin → setup shared memory → return
//! a [`LaunchedServer`] ready for the RT bridge thread to connect.

use super::locate::find_plugin_server;
use crate::error::{BridgeError, Result};
use crate::host::node::BATCH_SIZE;
use crate::protocol::{
    BridgeMessage, BusChannels, ChannelLayout, HostMessage, LoadedPlugin, PluginDescriptor,
    SampleFormat,
};
use crate::util::config::BridgeConfig;
use crate::util::transport::control::{self as ipc, ControlStream};
use crate::util::transport::shm::{AudioSlab, SlabLayout, RING_SLOTS};
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
        BridgeMessage::Ready { protocol_version } => {
            crate::protocol::check_protocol_version(protocol_version)?;
            Ok(stream)
        }
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

/// The slab shape for a freshly loaded plugin: how many flat channels, how they
/// partition into buses, and how many samples per channel cross the boundary.
///
/// Split out of [`setup_shm`] because it is the whole of the decision and none
/// of the I/O — every input is a plain value, so it is unit-testable, whereas
/// `setup_shm` needs a live subprocess on the other end of `stream`. That
/// matters more than it looks: the only production construction site for a
/// `SlabLayout` used to be unreachable from a test, so the in-place aliasing
/// decision below could only be checked by launching a real plugin.
fn slab_layout_for(
    loaded: &LoadedPlugin,
    format: SampleFormat,
    max_buffer_size: usize,
) -> SlabLayout {
    // Carry the plugin's per-bus layout so both processes agree how each
    // direction's channels partition into buses. Loaders normally populate at
    // least the main bus; an empty list means the plugin's shape was never
    // determined (filename-fallback discovery), and it used to be reinterpreted
    // as "one flat range shared in place by both directions" — the aliasing that
    // made the out-of-process bypass silent. Empty is now invalid, so an unknown
    // shape gets an explicit stereo default instead.
    let inputs = default_if_empty(&loaded.inputs);
    let outputs = default_if_empty(&loaded.outputs);

    SlabLayout {
        // Sized to the largest block that can actually arrive, NOT to
        // `config.max_buffer_size`. The two are different quantities that were
        // being conflated: `max_buffer_size` (8192 by default) is what the
        // *plugin* is told to size its own buffers for on `LoadPlugin`, whereas
        // this is what crosses the shared region per block — and fundsp never
        // hands a node more than `BATCH_SIZE` at a time (see the constant's
        // docs; `BigBlockAdapter` chunks anything larger upstream).
        //
        // At the default that is a 128x over-allocation: 64 KiB of untouched
        // mapped pages per stereo plugin instead of 512 B. `min` rather than a
        // bare `BATCH_SIZE` so a host that deliberately configures a *smaller*
        // buffer still gets a slab it cannot overrun.
        samples_per_channel: max_buffer_size.min(BATCH_SIZE),
        format,
        slots: RING_SLOTS as u32,
        inputs,
        outputs,
    }
}

/// A plugin whose bus layout is unknown gets one stereo bus rather than nothing.
///
/// Stereo because it is what the overwhelming majority of plugins present and
/// what the previous code's `.max(2)` floor already assumed; the difference is
/// that the assumption is now stated as a bus, so no later code has to guess
/// what an empty list meant.
fn default_if_empty(buses: &BusChannels) -> BusChannels {
    if buses.is_empty() {
        BusChannels::from_slice(&[ChannelLayout::Stereo])
    } else {
        buses.clone()
    }
}

fn setup_shm(
    stream: &mut ControlStream,
    config: &BridgeConfig,
    loaded: &LoadedPlugin,
    format: SampleFormat,
    shm_name: String,
) -> Result<Arc<AudioSlab>> {
    let layout = slab_layout_for(loaded, format, config.max_buffer_size);
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

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::SmallVec;

    fn loaded(inputs: &[ChannelLayout], outputs: &[ChannelLayout]) -> LoadedPlugin {
        LoadedPlugin {
            inputs: SmallVec::from_slice(inputs),
            outputs: SmallVec::from_slice(outputs),
            ..Default::default()
        }
    }

    /// The default `BridgeConfig::max_buffer_size`. Named here so the sizing
    /// tests below read as "the shipped default", not as a magic number.
    const DEFAULT_MAX_BUFFER: usize = 8192;

    /// **The exact shape that regressed**, now asserting the inverse of what it
    /// used to. A plain stereo-in/stereo-out plugin with one bus per direction
    /// is the *common* case, and it is the case the old code collapsed onto a
    /// single shared region: `output_base == 0`, so the host's own input write
    /// landed where it later read the plugin's output. The two directions must
    /// now be separately sized and separately addressed.
    #[test]
    fn stereo_one_bus_each_direction_gets_disjoint_regions() {
        let layout = slab_layout_for(
            &loaded(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]),
            SampleFormat::Float32,
            DEFAULT_MAX_BUFFER,
        );

        assert_eq!(layout.input_channels(), 2);
        assert_eq!(layout.output_channels(), 2);
        assert_eq!(layout.input_ring_bytes(), layout.output_ring_bytes());
        // Both directions are counted. Under the old `max`-for-single-bus rule
        // this total would have been half as large — that halving *was* the
        // aliasing.
        assert_eq!(
            layout.byte_size_with_header(0),
            layout.input_ring_bytes() * 2
        );
    }

    /// A sidechain widens only the input direction.
    #[test]
    fn sidechain_input_widens_only_its_own_ring() {
        let layout = slab_layout_for(
            &loaded(
                &[ChannelLayout::Stereo, ChannelLayout::Mono],
                &[ChannelLayout::Stereo],
            ),
            SampleFormat::Float32,
            DEFAULT_MAX_BUFFER,
        );

        assert_eq!(layout.input_channels(), 3);
        assert_eq!(layout.output_channels(), 2);
        assert!(layout.input_ring_bytes() > layout.output_ring_bytes());
    }

    /// A plugin whose bus layout was never determined (filename-fallback
    /// discovery) gets an explicit stereo bus per direction.
    ///
    /// Not merely a convenience: an empty list is now *invalid* at the slab, and
    /// it used to be the encoding for "share one region in place". Defaulting
    /// here is what keeps an undiscoverable plugin loadable without resurrecting
    /// that meaning.
    #[test]
    fn unknown_buses_default_to_stereo_per_direction() {
        let layout = slab_layout_for(&loaded(&[], &[]), SampleFormat::Float32, DEFAULT_MAX_BUFFER);
        assert_eq!(layout.inputs.len(), 1);
        assert_eq!(layout.outputs.len(), 1);
        assert_eq!(layout.input_channels(), 2);
        assert_eq!(layout.output_channels(), 2);
    }

    /// Every layout this function produces must be one the slab will accept.
    /// Cheaper to assert here than to discover at plugin-load time on a user's
    /// machine.
    #[test]
    fn every_produced_layout_is_addressable() {
        let cases = [
            loaded(&[], &[]),
            loaded(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]),
            loaded(&[ChannelLayout::Mono], &[ChannelLayout::Stereo]),
            loaded(
                &[ChannelLayout::Stereo, ChannelLayout::Mono],
                &[ChannelLayout::Stereo],
            ),
        ];
        for (i, l) in cases.iter().enumerate() {
            let layout = slab_layout_for(l, SampleFormat::Float32, DEFAULT_MAX_BUFFER);
            assert!(!layout.inputs.is_empty(), "case {i}: empty input buses");
            assert!(!layout.outputs.is_empty(), "case {i}: empty output buses");
            assert_eq!(layout.slots as usize, RING_SLOTS, "case {i}");
        }
    }

    /// The slab is sized to the block that can actually arrive, not to
    /// `max_buffer_size`. At the shipped default that is a 128x difference —
    /// enough that it more than pays for the ring this change introduces.
    #[test]
    fn slab_is_sized_to_the_real_block_not_the_configured_maximum() {
        let l = loaded(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]);
        let layout = slab_layout_for(&l, SampleFormat::Float32, DEFAULT_MAX_BUFFER);

        assert_eq!(layout.samples_per_channel, BATCH_SIZE);
        assert_eq!(
            DEFAULT_MAX_BUFFER / BATCH_SIZE,
            128,
            "if this ratio changes the comment in `slab_layout_for` is stale"
        );

        // The headline claim of step 1: even with a 2-slot ring in BOTH
        // directions, right-sizing leaves the mapping smaller than the old
        // single-buffer, single-direction slab.
        let old_bytes = 2 * DEFAULT_MAX_BUFFER * 4;
        assert!(
            layout.byte_size_with_header(0) < old_bytes,
            "ringed layout ({} B) should still beat the old oversized one ({old_bytes} B)",
            layout.byte_size_with_header(0)
        );
    }

    /// A host configured *below* the batch size gets a slab it cannot overrun —
    /// the reason this is a `min` and not a bare `BATCH_SIZE`.
    #[test]
    fn a_smaller_configured_buffer_wins() {
        let l = loaded(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]);
        let layout = slab_layout_for(&l, SampleFormat::Float32, 32);
        assert_eq!(layout.samples_per_channel, 32);
    }

    /// f64 negotiation doubles the region; the channel/bus decisions are
    /// unaffected by sample format.
    #[test]
    fn f64_doubles_the_byte_size_only() {
        let l = loaded(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]);
        let f32_layout = slab_layout_for(&l, SampleFormat::Float32, DEFAULT_MAX_BUFFER);
        let f64_layout = slab_layout_for(&l, SampleFormat::Float64, DEFAULT_MAX_BUFFER);

        assert_eq!(f64_layout.inputs, f32_layout.inputs);
        assert_eq!(
            f64_layout.samples_per_channel,
            f32_layout.samples_per_channel
        );
        assert_eq!(
            f64_layout.byte_size_with_header(0),
            f32_layout.byte_size_with_header(0) * 2
        );
    }
}

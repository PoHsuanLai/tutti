//! Engine construction: one ordered, fallible RT-wiring transaction that
//! publishes every subsystem as a Bevy resource.
//!
//! This is the irreducible core of bevy-tutti. The order is load-bearing:
//! `AudioEngine::new` opens CPAL (yielding sample rate + channels), the shared
//! managers and graph are built from those, `net.backend()` is taken once, the
//! RT processor is assembled, `audio_engine.start()` makes the callback live
//! (once), then the sampler / soundfont / analysis handles are built sharing
//! the same managers. The shared manager *instances* never escape — only the
//! finished per-subsystem resources are inserted into the `App`.
//!
//! There is no public `TuttiEngine` bundle: construction inserts directly, so
//! the only intermediate is the local bindings in [`build_into`].

use bevy_app::App;

use crate::engine::audio_io::{AudioCallbackState, AudioEngine};
use crate::engine::{Result, TuttiDriver, AudioGraph};
use tutti_core::dsp::An;
use tutti_core::processor::GraphProcessor;
use tutti_core::Arc;
use tutti_core::{
    ClickNode, ClickSettings, MeteringHandle, MeteringManager, PdcManager, TransportClock,
    TransportHandle, TransportManager, GraphNet,
};

// Each subsystem owns its own transient `PendingX` (defined next to its plugin).
// `build_into` fills them; the subsystem's plugin `build()` claims each into the
// subsystem's `*Res` (synchronously, before frame 1).
use tutti_core::graph::{AudioConfig, PendingGraph};
use tutti_core::metering::PendingMetering;
use tutti_core::transport::PendingTransport;

#[cfg(feature = "midi")]
use tutti_core::processor::MidiProcessor;
#[cfg(feature = "midi")]
use tutti_midi_io::PendingMidi;
#[cfg(feature = "midi-hardware")]
use tutti_midi_io::MidiIo;
#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiBus;
// `MidiRoutingTable` is a plain type (from tutti-midi-types, a hard dep) that
// `AudioGraph` always carries — import it unconditionally so the `from_parts`
// call shape doesn't depend on the `midi` feature.
use tutti_midi_types::MidiRoutingTable;

#[cfg(feature = "sampler")]
use tutti_sampler::{PendingSampler, Sampler};

#[cfg(feature = "analysis")]
use tutti_analysis::PendingAnalysis;

/// The audio processor type that runs on the RT callback thread.
///
/// - With `midi`: `MidiProcessor<GraphProcessor>` — splits buffers on MIDI
///   events, routes them through the caller-supplied queue, then ticks the graph.
/// - Without `midi`: `GraphProcessor` — just ticks the graph.
#[cfg(feature = "midi")]
pub type DefaultProcessor = MidiProcessor<GraphProcessor>;
#[cfg(not(feature = "midi"))]
pub type DefaultProcessor = GraphProcessor;

/// Build the engine from a [`TuttiPlugin`](crate::TuttiPlugin) config and insert
/// every subsystem resource into `app`. The audio callback is live on return.
///
/// On `Err`, nothing is inserted — the `engine_ready` run-condition gates all
/// engine-dependent systems, so the app proceeds without audio.
pub fn build_into(plugin: &crate::TuttiPlugin, app: &mut App) -> Result<()> {
    // OS MIDI ports are opened iff the `midi-hardware` feature is compiled.
    // Built first so we can hand the port manager to the processor's MIDI input.
    // (Software MIDI fan-out via `MidiBus` is always present under `midi`.)
    #[cfg(feature = "midi-hardware")]
    let midi_io = {
        let port_manager = Arc::new(tutti_midi_io::MidiPortManager::new(256));
        Some(MidiIo::new(port_manager))
    };

    let mut audio_engine = AudioEngine::new(plugin.output_device)?;
    let sample_rate = audio_engine.sample_rate();
    let channels = audio_engine.channels();

    let inputs = plugin.inputs;
    let outputs = if plugin.outputs == 0 { 2 } else { plugin.outputs };

    let transport_mgr = Arc::new(TransportManager::new(sample_rate));
    let metering_mgr = Arc::new(MeteringManager::new(sample_rate));
    let click_settings = Arc::new(ClickSettings::new());
    let pdc = PdcManager::new(outputs, 0);
    let pdc_snapshot = pdc.snapshot_arc();

    let mut net = GraphNet::new(inputs, outputs);

    // Transport clock — infrastructure node writing back beat position via atomics.
    let clock = TransportClock::new(
        transport_mgr.tempo().clone(),
        transport_mgr.paused().clone(),
        sample_rate,
    )
    .with_seek(
        transport_mgr.seek_target().clone(),
        transport_mgr.seek_pending().clone(),
    )
    .with_loop(
        transport_mgr.loop_enabled_flag().clone(),
        transport_mgr.loop_start_beat_atomic().clone(),
        transport_mgr.loop_end_beat_atomic().clone(),
    )
    .with_position_writeback(transport_mgr.current_beat().clone());
    net.inner_mut().push(Box::new(clock));

    // Metronome — mixed into master output.
    let click_transport = TransportHandle::new(transport_mgr.clone(), click_settings.clone());
    let click = ClickNode::new(click_transport, click_settings.clone(), sample_rate);
    let click_id = net.inner_mut().push(Box::new(An(click)));
    net.inner_mut().pipe_output(click_id);

    let backend = net.backend();

    let midi_route = MidiRoutingTable::new();
    #[cfg(feature = "midi")]
    let midi_bus = MidiBus::new();

    let graph_processor = GraphProcessor::new(transport_mgr.clone(), backend);

    // Clock master — outbound MIDI Beat Clock / MTC generator. Reads the
    // transport, pushes into its own output ring (independent of the routing
    // path, so System Real-Time reaches hardware-out). Ticked once per block by
    // the RT processor; its consumer is drained to the OS by the frontend pump.
    // Starts disabled — no output until the UI connects a device + enables it.
    #[cfg(feature = "midi")]
    let (clock_master, clock_out_consumer) = {
        let (producer, consumer) = tutti_midi_runtime::midi_output_channel_with_capacity(1024);
        let clock_transport =
            TransportHandle::new(transport_mgr.clone(), click_settings.clone());
        let master = Arc::new(tutti_midi_runtime::ClockMaster::new(
            Arc::new(clock_transport),
            sample_rate,
            producer,
        ));
        (master, consumer)
    };

    #[cfg(feature = "midi")]
    let processor: DefaultProcessor = {
        let mut midi_proc = MidiProcessor::new(graph_processor, midi_route.snapshot_arc());
        midi_proc.set_queue(Arc::new(midi_bus.clone()));
        midi_proc.set_clock(clock_master.clone());

        // Hardware MIDI input only exists under `midi-hardware`.
        #[cfg(feature = "midi-hardware")]
        if let Some(ref io) = midi_io {
            midi_proc.set_input(io.port_manager().clone());
        }

        midi_proc
    };

    #[cfg(not(feature = "midi"))]
    let processor: DefaultProcessor = graph_processor;

    let callback_state = Arc::new(AudioCallbackState::new(processor, metering_mgr.clone()));
    audio_engine.start(callback_state.clone())?;

    #[cfg(feature = "sampler")]
    let sampler = Sampler::new(
        sample_rate,
        tutti_sampler::SamplerConfig {
            pdc: Some(pdc_snapshot.clone()),
            ..Default::default()
        },
    )?;
    // Silence the unused warning in the non-sampler config.
    #[cfg(not(feature = "sampler"))]
    let _ = &pdc_snapshot;

    let graph = AudioGraph::from_parts(
        net,
        pdc,
        midi_route,
        sample_rate,
        channels,
    );

    let driver = TuttiDriver::from_parts(audio_engine, callback_state);

    let transport = TransportHandle::new(transport_mgr, click_settings);

    #[cfg(feature = "analysis")]
    let analysis = tutti_analysis::AnalysisRes::new(sample_rate, metering_mgr.clone());

    let metering = MeteringHandle::new(metering_mgr);

    // --- Hand each subsystem its transient `PendingX` (claimed in each
    // subsystem plugin's `build()`). The non-send CPAL driver has no subsystem
    // plugin, so it's inserted directly. On `Err` earlier, none of this runs —
    // `engine_ready` stays an exact proxy. ---
    let config = AudioConfig {
        sample_rate,
        channels,
    };
    app.insert_resource(PendingGraph(Some((graph, config))));
    app.insert_non_send_resource(driver);
    app.insert_resource(PendingTransport(Some(transport)));
    app.insert_resource(PendingMetering(Some(metering)));

    #[cfg(feature = "midi")]
    app.insert_resource(PendingMidi {
        bus: Some(midi_bus),
        #[cfg(feature = "midi-hardware")]
        io: midi_io,
        clock_out: Some(tutti_midi_io::ClockMasterRes::new(
            clock_master,
            clock_out_consumer,
        )),
    });

    #[cfg(feature = "sampler")]
    app.insert_resource(PendingSampler(Some(sampler)));

    #[cfg(feature = "analysis")]
    app.insert_resource(PendingAnalysis(Some(analysis)));

    Ok(())
}

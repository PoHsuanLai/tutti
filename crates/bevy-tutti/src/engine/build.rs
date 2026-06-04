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
use crate::engine::{Result, TuttiDriver, TuttiGraph};
use tutti_core::dsp::An;
use tutti_core::processor::GraphProcessor;
use tutti_core::Arc;
use tutti_core::{
    ClickNode, ClickSettings, MeteringHandle, MeteringManager, PdcManager, TransportClock,
    TransportHandle, TransportManager, TuttiNet,
};

use tutti_core::ecs::{AudioConfig, MeteringRes, TransportRes, TuttiGraphRes};

#[cfg(feature = "midi")]
use tutti_core::midi::MidiProcessor;
#[cfg(feature = "midi")]
use tutti_midi_io::ecs::MidiBusRes;
#[cfg(feature = "midi-hardware")]
use tutti_midi_io::MidiIo;
#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiBus;
#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiRoutingTable;
#[cfg(feature = "midi-hardware")]
use tutti_midi_io::ecs::MidiIoRes;

#[cfg(feature = "sampler")]
use tutti_sampler::ecs::SamplerRes;
#[cfg(feature = "sampler")]
use tutti_sampler::Sampler;

#[cfg(feature = "analysis")]
use tutti_analysis::ecs::AnalysisRes;

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

    let mut net = TuttiNet::new(inputs, outputs);

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

    #[cfg(feature = "midi")]
    let midi_route = MidiRoutingTable::new();
    #[cfg(feature = "midi")]
    let midi_bus = MidiBus::new();

    let graph_processor = GraphProcessor::new(transport_mgr.clone(), backend);

    #[cfg(feature = "midi")]
    let processor: DefaultProcessor = {
        let mut midi_proc = MidiProcessor::new(graph_processor, midi_route.snapshot_arc());
        midi_proc.set_queue(Arc::new(midi_bus.clone()));

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
    let sampler = Arc::new(
        Sampler::builder(sample_rate)
            .pdc(pdc_snapshot.clone())
            .build()?,
    );
    // Silence the unused warning in the non-sampler config.
    #[cfg(not(feature = "sampler"))]
    let _ = &pdc_snapshot;

    let graph = TuttiGraph::from_parts(
        net,
        pdc,
        #[cfg(feature = "midi")]
        midi_route,
        sample_rate,
        channels,
    );

    let driver = TuttiDriver::from_parts(audio_engine, callback_state);

    let transport = TransportHandle::new(transport_mgr, click_settings);

    #[cfg(feature = "analysis")]
    let analysis = tutti_analysis::AnalysisHandle::with_metering(sample_rate, metering_mgr.clone());

    let metering = MeteringHandle::new(metering_mgr);
    // Enable amplitude + CPU metering by default (consumers read
    // `MeteringRes::amplitude()` / `cpu()` directly).
    metering.inner().enable_amp();
    metering.inner().cpu().enable();

    // --- Publish every subsystem as a resource (no intermediate bundle) ---
    app.insert_resource(AudioConfig {
        sample_rate,
        channels,
    });
    app.insert_resource(TuttiGraphRes(graph));
    app.insert_non_send_resource(driver);
    app.insert_resource(TransportRes(transport));
    app.insert_resource(MeteringRes(metering));

    #[cfg(feature = "midi")]
    app.insert_resource(MidiBusRes(midi_bus));
    #[cfg(feature = "midi-hardware")]
    if let Some(io) = midi_io {
        app.insert_resource(MidiIoRes(io));
    }

    #[cfg(feature = "sampler")]
    {
        app.insert_resource(tutti_sampler::ecs::init_auditioner(&sampler));
        app.insert_resource(SamplerRes(sampler));
    }

    #[cfg(feature = "analysis")]
    app.insert_resource(AnalysisRes(analysis));

    Ok(())
}

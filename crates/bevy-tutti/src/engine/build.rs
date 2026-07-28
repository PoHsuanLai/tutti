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

use crate::engine::Result;
use tutti_core::dsp::An;
use tutti_core::engine::Engine;
use tutti_core::Arc;
use tutti_core::{
    dsp::Net, AudioTap, ClickNode, ClickSettings, MasterMeter, Transport, TransportClock,
};
use tutti_cpal::{AudioCallbackState, AudioEngine, TuttiDriver};

use crate::graph::{
    AudioConfig, AudioGraphRes, MeteringRes, MetronomeRes, TransportClockNode, TransportRes,
};

#[cfg(feature = "midi-hardware")]
use crate::midi::MidiIoRes;
#[cfg(feature = "midi")]
use crate::midi::{ClockMasterRes, MidiBusRes, MidiRoutingRes};
#[cfg(feature = "midi-hardware")]
use tutti_midi_io::MidiIo;
#[cfg(feature = "midi")]
use tutti_midi_runtime::{MidiBus, MidiPreBlock};
#[cfg(feature = "midi")]
use tutti_midi_types::MidiRoutingTable;

#[cfg(feature = "sampler")]
use crate::sampler::SamplerRes;
#[cfg(feature = "sampler")]
use tutti_sampler::DiskStreamer;

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
        let port_manager = Arc::new(tutti_midi_io::HardwareMidiInputs::new(256));
        Some(MidiIo::new(port_manager))
    };

    let mut audio_engine = AudioEngine::new(plugin.output_device)?;
    let sample_rate = audio_engine.sample_rate();
    let channels = audio_engine.channels();

    let inputs = plugin.inputs;
    let outputs = if plugin.outputs == 0 {
        2
    } else {
        plugin.outputs
    };

    let transport = Transport::new(sample_rate);
    let meter = MasterMeter::new();
    let tap = AudioTap::new();
    let click_settings = Arc::new(ClickSettings::new());

    // Per-channel pre-roll for sources outside the graph. Stays empty unless the
    // app adds `LatencyCompensationPlugin`, which owns publishing into it; the
    // sampler subscribes here so the wiring exists either way.
    let compensation = crate::latency::ChannelCompensation::default();

    let mut net = Net::new(inputs, outputs);

    // Transport clock — emits the beat on two ports and writes it back to the
    // manager's atomic. Its NodeId is retained so beat-driven nodes can wire an
    // edge to it (published below as `TransportClockNode`).
    let clock = TransportClock::new(transport.clock_links(), sample_rate);
    let clock_id = net.push(Box::new(clock));

    // Metronome — mixed into master output. It only READS the transport
    // (beat + rolling/recording), so it takes a read view, not a control handle.
    let click = ClickNode::with_transport(transport.clone(), click_settings.clone(), sample_rate);
    let click_id = net.push(Box::new(An(click)));
    net.pipe_output(click_id);

    let backend = net.backend();

    // The routing table is a MIDI-subsystem concern, not a graph one: it maps a
    // MIDI channel to a destination unit's mailbox, with no fundsp edge behind
    // it. Built here only because the RT `MidiPreBlock` needs its snapshot at
    // assembly time; the writer half is handed to `TuttiMidiPlugin` below.
    #[cfg(feature = "midi")]
    let midi_route = MidiRoutingTable::new();
    #[cfg(feature = "midi")]
    let midi_bus = MidiBus::new();

    let engine = Engine::new(transport.motion.clone(), backend);

    // Clock master — outbound MIDI Beat Clock / MTC generator. Reads the
    // transport, pushes into its own output ring (independent of the routing
    // path, so System Real-Time reaches hardware-out). Ticked once per block by
    // the RT processor; its consumer is drained to the OS by the frontend pump.
    // Starts disabled — no output until the UI connects a device + enables it.
    #[cfg(feature = "midi")]
    let (clock_master, clock_out_consumer) = {
        let (sender, receiver) = tutti_midi_runtime::MidiMailbox::pair(
            tutti_midi_runtime::tutti_midi_types::MidiUnitId::next(),
        );
        let master = Arc::new(tutti_midi_runtime::ClockMaster::new(
            Arc::new(transport.clone()),
            sample_rate,
            sender,
        ));
        (master, receiver)
    };

    // MIDI runs as a once-per-block producer before the graph render: it ticks
    // the clock, polls hardware, and routes events into node inboxes (which time
    // each event by its `frame_offset`). See `MidiPreBlock`.
    #[cfg(feature = "midi")]
    let pre_block = {
        let mut pre_block = MidiPreBlock::new(midi_route.snapshot_arc());
        pre_block.set_queue(Arc::new(midi_bus.clone()));
        pre_block.set_clock(clock_master.clone());

        // Input-edge translation: assemble (N)RPN runs, then rewrite classic-MPE
        // channel-spread into native per-note messages, so downstream synths see
        // only native MIDI-2. MPE mode comes from the app's `MpeModeConfig`
        // (inserted before the engine builds); default `Disabled` = passthrough.
        pre_block
            .set_translator(tutti_midi_runtime::tutti_midi_types::Midi1ToMidi2Translator::new());
        let mpe_mode = app
            .world()
            .get_resource::<crate::midi::MpeModeConfig>()
            .map(|c| c.0)
            .unwrap_or(tutti_midi_io::MpeMode::Disabled);
        pre_block.set_mpe_ingest(tutti_midi_runtime::MpeIngest::new(mpe_mode));

        // Hardware MIDI input only exists under `midi-hardware`.
        #[cfg(feature = "midi-hardware")]
        if let Some(ref io) = midi_io {
            pre_block.set_input(io.port_manager().clone());
        }

        pre_block
    };

    let callback_state = {
        let state = AudioCallbackState::new(engine, meter.clone(), tap.clone());
        #[cfg(feature = "midi")]
        let state = state.with_pre_block(pre_block);
        Arc::new(state)
    };
    audio_engine.start(callback_state.clone())?;

    #[cfg(feature = "sampler")]
    let sampler = DiskStreamer::new(
        sample_rate,
        tutti_sampler::DiskStreamerConfig {
            pdc: Some(Arc::clone(&compensation.0)),
            ..Default::default()
        },
    )?;

    // The net *is* the graph — its backend was taken above, and the sample rate
    // and channel count it already carries are what `AudioConfig` publishes.
    let graph = net;

    let driver = TuttiDriver::from_parts(audio_engine, callback_state);

    // The metronome resource is just the shared click settings — callers set
    // volume/mode via `ClickState`'s atomic setters directly.
    let metronome = click_settings;

    // --- Publish every subsystem. On `Err` earlier none of this runs, so
    // `engine_ready` stays an exact proxy for "the callback is live". ---
    let config = AudioConfig {
        sample_rate,
        channels,
    };
    app.insert_resource(AudioGraphRes(graph));
    app.insert_resource(config);
    // Inserted whether or not the app opts into compensation: the sampler already
    // holds a clone of this Arc, so the resource must be *this* one, not a fresh
    // default. `LatencyCompensationPlugin` uses `init_resource`, which leaves it.
    app.insert_resource(compensation);
    app.insert_non_send(driver);
    app.insert_resource(TransportRes(transport));
    app.insert_resource(MetronomeRes(metronome));
    app.insert_resource(TransportClockNode(clock_id));
    // Consumers read `MeteringRes::get()` directly, so the meter has to be
    // measuring from the start.
    meter.enable();
    app.insert_resource(MeteringRes(meter));

    #[cfg(feature = "midi")]
    {
        // Both must be the very values the pre-block above shares — a freshly
        // built one publishes where the audio thread never reads.
        app.insert_resource(MidiBusRes::new(midi_bus));
        app.insert_resource(MidiRoutingRes(midi_route));
        app.insert_resource(ClockMasterRes::new(clock_master, clock_out_consumer));
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = midi_io {
            app.insert_resource(MidiIoRes(io));
        }
    }

    #[cfg(feature = "sampler")]
    app.insert_resource(SamplerRes(sampler));

    Ok(())
}

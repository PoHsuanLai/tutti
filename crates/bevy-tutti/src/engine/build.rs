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
use tutti_core::Arc;
use tutti_core::{
    dsp::Net, AudioTap, ClickNode, ClickSettings, MasterMeter, Transport, TransportClock,
};
use tutti_core::{Engine, MAX_ROOT_CHANNELS};
use tutti_cpal::{AudioCallbackState, AudioEngine, TuttiDriver};

use crate::graph::{
    AudioConfig, AudioGraphRes, AudioTapRes, EngineNodes, MeteringRes, MetronomeRes, TransportRes,
};

#[cfg(feature = "midi-hardware")]
use crate::midi::MidiIoRes;
#[cfg(feature = "midi")]
use crate::midi::{ClockMasterRes, MidiBusRes, MidiOutSinkRes, MidiRoutingRes};
#[cfg(feature = "midi-hardware")]
use tutti_midi_hardware::MidiSession;
#[cfg(feature = "midi")]
use tutti_midi_runtime::{MidiBus, MidiPostBlock, MidiPreBlock};
#[cfg(feature = "midi")]
use tutti_midi_types::MidiRoutingTable;

#[cfg(feature = "sampler")]
use crate::sampler::DiskStreamerRes;
#[cfg(feature = "sampler")]
use tutti_sampler::DiskStreamer;

/// Build the engine from a [`TuttiPlugin`](crate::TuttiPlugin) config and insert
/// every subsystem resource into `app`. The audio callback is live on return.
///
/// On `Err`, nothing is inserted — the `engine_ready` run-condition gates all
/// engine-dependent systems, so the app proceeds without audio.
pub fn build_into(plugin: &crate::TuttiPlugin, app: &mut App) -> Result<()> {
    // OS MIDI ports are opened iff the `midi-hardware` feature is compiled.
    // Built first so the port manager can be handed to the processor's MIDI
    // input. (Software fan-out via `MidiBus` is always present under `midi`.)
    #[cfg(feature = "midi-hardware")]
    let midi_io = {
        let port_manager = Arc::new(tutti_midi_hardware::HardwareMidiInputs::new(256));
        Some(MidiSession::new(port_manager))
    };

    let mut audio_engine = AudioEngine::new(plugin.output_device)?;
    let sample_rate = audio_engine.sample_rate();
    let channels = audio_engine.channels();

    // The port manager times each inbound event by turning a wall-clock delta
    // into a `frame_offset`, which takes the device's real rate. It is built
    // above — before the device exists — at a placeholder 44100, so at any
    // other rate every hardware event lands at the wrong offset (~8.8% early
    // at 48 kHz). Set here, the first moment the rate is known and well before
    // `audio_engine.start()` makes the callback live, which is the contract
    // `HardwareMidiInputs::set_sample_rate` documents.
    #[cfg(feature = "midi-hardware")]
    if let Some(ref io) = midi_io {
        io.ports().set_sample_rate(sample_rate);
    }

    let inputs = plugin.inputs;
    let outputs = root_width(plugin.outputs, channels);

    let transport = Transport::new(sample_rate);
    let meter = MasterMeter::new();
    let tap = AudioTap::new();
    let click_settings = Arc::new(ClickSettings::new());

    // Per-channel pre-roll for sources outside the graph. Stays empty unless the
    // app adds `LatencyCompensationPlugin`, which owns publishing into it; the
    // sampler subscribes here so the wiring exists either way.
    let compensation = crate::graph::latency::ChannelCompensation::default();

    let mut net = Net::new(inputs, outputs);

    // Transport clock — emits the beat on two ports and writes it back to the
    // manager's atomic. Beat-driven nodes take those ports as inputs, so the
    // clock needs a name a host can address; it gets an entity below, like every
    // other node in the graph.
    let clock = TransportClock::new(transport.clock_links(), sample_rate);
    let clock_id = net.push(Box::new(clock));

    // Metronome. It only READS the transport (beat + rolling/recording), so it
    // takes a read view, not a control handle.
    //
    // It is NOT wired to the output here, and must not be. `pipe_output` reads
    // like "mix the click into master" and is not what it does — it overwrites
    // every global output edge, so the first soundfont to load silently
    // disconnects the metronome. What the click feeds is the host's
    // declaration, like every other node; see `graph::wire`.
    let click = ClickNode::with_transport(transport.clone(), click_settings.clone(), sample_rate);
    let click_id = net.push(Box::new(An(click)));

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
        let (sender, receiver) =
            tutti_midi_runtime::MidiMailbox::pair(tutti_midi_types::MidiUnitId::next());
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
        pre_block.set_translator(tutti_midi_types::Midi1ToMidi2Translator::new());
        let mpe_mode = app
            .world()
            .get_resource::<crate::midi::MpeModeConfig>()
            .map(|c| c.0)
            .unwrap_or(tutti_midi_types::MpeMode::Disabled);
        pre_block.set_mpe_ingest(tutti_midi_runtime::MpeIngest::new(mpe_mode));
        // The live handle, so MPE stays configurable after build rather than
        // being fixed here. `MpeModeConfig` above is the *seed*; a host that
        // stores zone setup in a document overwrites it through this.
        app.insert_resource(crate::midi::MpeModeRes(pre_block.mpe_mode_handle()));

        // Hardware MIDI input only exists under `midi-hardware`.
        #[cfg(feature = "midi-hardware")]
        if let Some(ref io) = midi_io {
            pre_block.set_input(io.ports().clone());
        }

        pre_block
    };

    // The outbound half, run *after* the graph render: it fans out whatever the
    // graph emitted (a hosted plugin's MIDI-out) into the same unit inboxes
    // inbound events reach.
    //
    // `tutti-cpal` holds an `Option<MidiPostBlock>` and calls `run()` in the
    // callback; assembling one needs the routing table and the bus, which are
    // this adapter's to own, so it belongs here rather than in the device layer.
    // The engine-side path itself needs no adapter — `tutti-midi-runtime`'s
    // `outbound_block_path` test assembles the whole round trip with no Bevy in
    // scope, and exists to keep that true.
    //
    // `midi_route.snapshot_arc()` is deliberately the *same* handle the pre-block
    // took: a node's MIDI-out is routed by exactly the rules a hardware input is,
    // and two tables would let the two directions disagree about where a channel
    // goes.
    #[cfg(feature = "midi")]
    let post_block = {
        let mut post_block = MidiPostBlock::new(midi_route.snapshot_arc());
        post_block.set_queue(Arc::new(midi_bus.clone()));
        post_block
    };

    // The collection point emitting nodes push into. Taken before the post-block
    // moves into the callback state, and published as `MidiOutSinkRes` for a host
    // to hand to whatever emits (`plugin.set_midi_out(sink.handle())`). Nothing
    // is installed automatically — `midi::out_sink` states why.
    #[cfg(feature = "midi")]
    let midi_out_sink = post_block.sink();

    let callback_state = {
        let state = AudioCallbackState::new(engine, meter.clone(), tap.clone());
        #[cfg(feature = "midi")]
        let state = state.with_pre_block(pre_block).with_post_block(post_block);
        Arc::new(state)
    };
    audio_engine.start(callback_state.clone())?;

    #[cfg(feature = "sampler")]
    let disk_streamer = DiskStreamer::new(
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
    // The two engine-built nodes get entities like everything else in the graph,
    // and `EngineNodes` publishes them. Spawned here, before any host system
    // runs, they are otherwise unreachable: both carry `AudioNode` and nothing
    // else, so a query cannot tell them apart — and `PortSources` names sources
    // by `Entity`, so an unnameable node is an unwirable one. The clock exists
    // precisely to be wired to, and the click is left unwired *so that* the host
    // declares where it lands.
    let clock_entity = app.world_mut().spawn(tutti_core::AudioNode(clock_id)).id();
    let click_entity = app.world_mut().spawn(tutti_core::AudioNode(click_id)).id();
    app.insert_resource(EngineNodes {
        clock: clock_entity,
        click: click_entity,
    });
    // Consumers read `MeteringRes::get()` directly, so the meter has to be
    // measuring from the start. `disable()` through the `Deref` turns it back
    // off; see `graph::metering` for what that does and does not save.
    meter.enable();
    app.insert_resource(MeteringRes(meter));
    // Deliberately NOT opened: while closed, `AudioTap::push` on the audio
    // thread is one atomic load and a return, so a host that never analyses
    // pays nothing. `AudioTapRes::open()` is the switch, and it hands back a
    // consumer the caller owns.
    app.insert_resource(AudioTapRes(tap));

    #[cfg(feature = "midi")]
    {
        // Both must be the very values the pre-block above shares — a freshly
        // built one publishes where the audio thread never reads.
        app.insert_resource(MidiBusRes::new(midi_bus));
        app.insert_resource(MidiRoutingRes::new(midi_route));
        app.insert_resource(ClockMasterRes::new(clock_master, clock_out_consumer));
        // The outbound collection point, from the post-block now living in the
        // callback state. A host hands `handle()` to whatever emits; nothing is
        // installed automatically — see `midi::out_sink` for why.
        app.insert_resource(MidiOutSinkRes::new(midi_out_sink));
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = midi_io {
            app.insert_resource(MidiIoRes(io));
        }
    }

    #[cfg(feature = "sampler")]
    app.insert_resource(DiskStreamerRes(disk_streamer));

    Ok(())
}

/// How wide the graph root is built, given what the project asks for and what
/// the device presents.
///
/// **These are two independent numbers, and neither may narrow the other.**
///
/// `plugin_outputs` is the *project's* width. A 5.1 project on a stereo laptop
/// must still render six channels — `Engine::process_segment` folds them into
/// the device buffer through the shared ITU matrices every block. Clamping the
/// root down to the device would make that fold a no-op that hides real channel
/// loss, which is precisely the bug the fold exists to prevent.
///
/// The device is a **floor** because a root narrower than the device leaves the
/// extra device channels permanently silent: `fold_frame` zero-fills rather
/// than upmixing, deliberately, so nothing downstream can recover them.
///
/// A device *narrower* than the root needs nothing done here — that is the case
/// the engine already handles, and the whole point of the root fold. "Clamp to
/// the device" is the obvious wrong instinct.
///
/// The [`MAX_ROOT_CHANNELS`] clamp is applied **here, at construction**, rather
/// than left to `process_segment`'s runtime clamp: without it `Net::outputs()`
/// reports a width whose upper channels are declarable — a sink can name them,
/// the wiring resolves — but never rendered, because the render scratch is
/// bounded. Offline export is unaffected: it clones the net and uses the
/// offline `set_output_arity`, which has no such cap.
fn root_width(plugin_outputs: usize, device: tutti_core::ChannelLayout) -> usize {
    plugin_outputs
        .max(device.count() as usize)
        .clamp(1, MAX_ROOT_CHANNELS)
}

#[cfg(test)]
mod tests {
    use super::root_width;
    use tutti_core::ChannelLayout;
    use tutti_core::MAX_ROOT_CHANNELS;

    /// `root_width` is `max(project, device)` clamped to `1..=MAX_ROOT_CHANNELS`,
    /// and each row below is one of the four ways that rule is load-bearing.
    ///
    /// One table rather than five functions: every case is the same two-argument
    /// call against one expected width, so the only thing five names bought was
    /// five stack traces for the same assertion.
    #[test]
    fn root_width_takes_the_wider_side_within_the_scratch_bounds() {
        let cases: &[(usize, ChannelLayout, usize, &str)] = &[
            (
                2,
                ChannelLayout::from(6u16),
                6,
                "a wider device widens the root: a stereo project on a 5.1 device \
                 must render all six, or the top four are permanently silent",
            ),
            (
                6,
                ChannelLayout::STEREO,
                6,
                "a wider project is kept and folded, not clamped: a 5.1 project on \
                 a stereo device keeps its width; the root fold narrows it at the \
                 device edge, and clamping here would hide the loss",
            ),
            (
                0,
                ChannelLayout::from(6u16),
                6,
                "an unset project width falls through to the device",
            ),
            (
                0,
                ChannelLayout::STEREO,
                2,
                "an unset project width falls through to the device",
            ),
            (
                64,
                ChannelLayout::from(32u16),
                MAX_ROOT_CHANNELS,
                "nothing exceeds the render scratch",
            ),
            (
                0,
                ChannelLayout::EMPTY,
                1,
                "the root is never zero wide: a zero-output root would render \
                 nothing at all",
            ),
        ];

        for &(project, device, expected, why) in cases {
            assert_eq!(
                root_width(project, device),
                expected,
                "root_width({project}, {} channels): {why}",
                device.count()
            );
        }
    }
}

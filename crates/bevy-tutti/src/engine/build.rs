//! Engine construction: one ordered, fallible RT-wiring transaction that
//! publishes every subsystem as a Bevy resource.
//!
//! This is the irreducible core of bevy-tutti. The order is load-bearing:
//! `AudioEngine::new` opens CPAL (yielding sample rate + channels), the shared
//! managers and graph are built from those, the graph's audio side is taken once, the
//! RT processor is assembled, `audio_engine.start()` makes the callback live
//! (once), then the sampler / soundfont / analysis handles are built sharing
//! the same managers. The shared manager *instances* never escape — only the
//! finished per-subsystem resources are inserted into the `App`.
//!
//! There is no public `TuttiEngine` bundle: construction inserts directly, so
//! the only intermediate is the local bindings in [`build_into`].

use bevy_app::App;

use crate::engine::Result;
use tutti_core::Arc;
use tutti_core::{AudioNode, AudioTap, ClickNode, ClickSettings, MasterMeter, Transport};
use tutti_core::{Engine, SampleRate, MAX_ROOT_CHANNELS};
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
    build_on(
        plugin,
        app,
        AudioEngine::new(plugin.output_device)?,
        |audio_engine, state| audio_engine.start(state),
    )
}

/// [`build_into`] over an `audio_engine` already opened, started by `start`:
/// the device-free build, when `audio_engine` is `AudioEngine::from_spec`
/// and `start` runs it on a `ManualStreamDriver` — every subsystem built and
/// published as a device would have them, with no sound card.
pub(crate) fn build_on(
    plugin: &crate::TuttiPlugin,
    app: &mut App,
    mut audio_engine: AudioEngine,
    start: impl FnOnce(&mut AudioEngine, Arc<AudioCallbackState>) -> tutti_cpal::Result<()>,
) -> Result<()> {
    // OS MIDI ports are opened iff the `midi-hardware` feature is compiled.
    // Built first so the port manager can be handed to the processor's MIDI
    // input. (Software fan-out via `MidiBus` is always present under `midi`.)
    #[cfg(feature = "midi-hardware")]
    let midi_io = {
        let port_manager = Arc::new(tutti_midi_hardware::HardwareMidiInputs::new(256));
        Some(MidiSession::new(port_manager))
    };

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

    // Per-channel pre-roll for sources outside the graph, which `commit_graph`
    // publishes with every plan; the sampler subscribes to this one.
    let compensation = crate::graph::latency::ChannelCompensation::default();

    let Assembled {
        graph,
        engine,
        clock: clock_node,
        click: click_node,
    } = assemble(sample_rate, inputs, outputs, &transport, &click_settings)?;

    // The routing table is a MIDI-subsystem concern, not a graph one: it maps a
    // MIDI channel to a destination unit's mailbox, with no fundsp edge behind
    // it. Built here only because the RT `MidiPreBlock` needs its snapshot at
    // assembly time; the writer half is handed to `TuttiMidiPlugin` below.
    #[cfg(feature = "midi")]
    let midi_route = MidiRoutingTable::new();
    #[cfg(feature = "midi")]
    let midi_bus = MidiBus::new();

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
    start(&mut audio_engine, callback_state.clone())?;

    #[cfg(feature = "sampler")]
    let disk_streamer = DiskStreamer::new(
        sample_rate,
        tutti_sampler::DiskStreamerConfig {
            pdc: Some(Arc::clone(&compensation.0)),
            ..Default::default()
        },
    )?;

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
    app.insert_resource(graph);
    app.insert_resource(config);
    // The sampler already holds a clone of this Arc, so the resource must be
    // *this* one, not a fresh default. `GraphReconcilePlugin` (and
    // `LatencyCompensationPlugin`) use `init_resource`, which leaves it.
    app.insert_resource(compensation);
    app.insert_non_send(driver);
    app.insert_resource(TransportRes(transport));
    app.insert_resource(MetronomeRes(metronome));
    // The two engine-built nodes get entities like everything else in the graph,
    // and `EngineNodes` publishes them. Spawned here, before any host system
    // runs, they are otherwise unreachable: both carry `AudioNode` and nothing
    // else, so a query cannot tell them apart — and `PortSources` names sources
    // by `Entity`, so an unnameable node is an unwirable one. The clock exists
    // precisely to be wired to, and the click's *output* is left unwired so that
    // the host declares where it lands.
    //
    // The click's *inputs* are declared here: it takes the beat from the clock's
    // two ports, per sample, so each click starts on the frame its beat lands on
    // rather than on a block boundary. Declared rather than `set_source`d so
    // the reconciler owns the edge like every other one — and the edge is also
    // what orders the clock before the click, which two unconnected nodes do not
    // get.
    let clock_entity = app.world_mut().spawn(clock_node).id();
    let click_entity = app
        .world_mut()
        .spawn((
            click_node,
            crate::graph::PortSources::stereo_from(clock_entity),
        ))
        .id();
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

/// The graph, the engine over its audio side, and the two nodes the engine
/// builds into it.
struct Assembled {
    graph: AudioGraphRes,
    engine: Engine,
    /// What the beat ports come from.
    clock: AudioNode,
    /// The metronome, its inputs still undeclared.
    click: AudioNode,
}

/// Build the graph, add the beat clock and the metronome, and build the
/// engine over its audio side. The device-free half of [`build_into`], so the
/// engine can be rendered in a test.
///
/// **The graph runs at the device's `rate`.** Every unit inserted is prepared
/// at the graph's rate (`Legacy`'s prepare), so a graph left at its default
/// 44.1 kHz would run every node — the beat clock included — at 44.1 kHz on a
/// 48 kHz device: the clock and every oscillator about 8.8% slow. (That is
/// what this builder did on `Net` before the rate was passed here.)
///
/// The clock is an `EnvClock` ([`AudioGraphRes::insert_beat_clock`]): a
/// graph engine drives its own `TransportClock` and forbids a second in the
/// graph. It emits the beat on a `TransportClock`'s two ports, and the
/// metronome is wired to them by the `PortSources` [`build_into`] declares.
fn assemble(
    rate: SampleRate,
    inputs: usize,
    outputs: usize,
    transport: &Transport,
    click_settings: &Arc<ClickSettings>,
) -> Result<Assembled> {
    let mut graph = AudioGraphRes::with_rate(inputs, outputs, rate);

    // The beat clock — emits the beat on two ports. Beat-driven nodes take
    // those ports as inputs, so the clock needs a name a host can address; it
    // gets an entity in `build_into`, like every other node in the graph.
    let clock = graph.insert_beat_clock();

    // Metronome. It only READS the transport (rolling/recording), so it takes a
    // read view, not a control handle; the beat itself arrives on its two input
    // ports from the clock, declared once both have entities.
    //
    // It is NOT wired to the output here, and must not be. `pipe_output` reads
    // like "mix the click into master" and is not what it does — it overwrites
    // every global output edge, so the first soundfont to load silently
    // disconnects the metronome. What the click feeds is the host's
    // declaration, like every other node; see `graph::wire`.
    let click = graph.insert(ClickNode::with_transport(
        transport.clone(),
        click_settings.clone(),
        rate,
    ));

    let engine = graph.engine(transport)?;
    Ok(Assembled {
        graph,
        engine,
        clock,
        click,
    })
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
/// than left to the engine: `Engine::with_graph` bounds the editor to
/// [`MAX_ROOT_CHANNELS`] global outputs (the render scratch is bounded), so a
/// wider root would be refused at its first commit, and the graph would never
/// play. Offline export is unaffected: a fork is prepared on its own, with no
/// such cap.
pub(crate) fn root_width(plugin_outputs: usize, device: tutti_core::ChannelLayout) -> usize {
    plugin_outputs
        .max(device.count() as usize)
        .clamp(1, MAX_ROOT_CHANNELS)
}

#[cfg(test)]
mod tests {
    use super::root_width;
    use tutti_core::ChannelLayout;
    use tutti_core::MAX_ROOT_CHANNELS;

    /// **The engine publishes the tap its callback feeds**, not a fresh
    /// disconnected one: a host opens `AudioTapRes` and sees the frames the
    /// callback renders. Built device-free (`build_on` over a
    /// `ManualStreamDriver`), so it runs in CI; it replaces
    /// `graph_reconcile`'s `the_engine_publishes_its_tap`, which opened a real
    /// device, was ignored, and asserted only that the resource existed.
    ///
    /// Mutation (run): `build_on` inserting `AudioTapRes(AudioTap::new())`
    /// instead of the callback's `tap` → the opened consumer sees nothing.
    #[test]
    fn the_engine_publishes_the_tap_its_callback_feeds() {
        use crate::graph::AudioTapRes;
        use tutti_core::SampleRate;
        use tutti_cpal::{AudioEngine, ManualStreamDriver, OutputSpec};

        let mut app = bevy_app::App::new();
        let (driver, stream) = ManualStreamDriver::new();
        super::build_on(
            &crate::TuttiPlugin::default(),
            &mut app,
            AudioEngine::from_spec(OutputSpec::new(
                SampleRate(48_000.0),
                ChannelLayout::STEREO,
                tutti_cpal::cpal::SampleFormat::F32,
            )),
            |engine, state| engine.start_with(state, driver),
        )
        .expect("builds with no device");
        let mut consumer = app
            .world()
            .resource::<AudioTapRes>()
            .open()
            .expect("a fresh tap opens");
        stream.render_block(64).expect("the stream is open");
        let mut frames = 0;
        while consumer.try_pop().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 64, "the callback's block reached the published tap");
    }

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

/// The engine [`assemble`] builds, rendered from the builder's own wiring (the
/// beat clock it picks, the click it builds against it), against the engine
/// this builder made before design doc 013's PR 13: fundsp's `Net` with a
/// `TransportClock` in it, over `Engine::new`.
///
/// **The `Net`-era engine is a test oracle here, and nothing else.** The
/// adapter lost its `Net` arm in PR 13; these tests keep the assertions it
/// used to run on both arms ("the native engine renders the `Net` engine's
/// samples"), with the `Net` side rebuilt by hand from tutti-core
/// ([`net_era`]) exactly as `assemble` built it on `Net`. It goes when
/// `Engine::new(NetBackend)` does (doc 013, PR 15), and these become checks
/// against the analytic figures each test also asserts.
#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::graph::GraphSource;
    use tutti_core::{AudioUnit, ChannelLayout, InterleavedMut, MetronomeMode, MotionEvent};

    const RATE: f64 = 48_000.0;

    /// The engine `assemble` built on `GraphBackend::Net` before PR 13, and
    /// the graph it renders: a `Net` at `RATE`, the `TransportClock` pushed
    /// first, then the click (`AudioGraphRes::insert_beat_clock` and
    /// `insert` on the `Net` arm), and `Engine::new` over its backend. `wire`
    /// edits the net as a test edits the adapter's graph, and its edits are
    /// committed as the adapter's `commit` committed them.
    fn net_era(
        transport: &Transport,
        settings: &Arc<ClickSettings>,
        wire: impl FnOnce(&mut tutti_core::dsp::Net, tutti_core::dsp::NodeId, tutti_core::dsp::NodeId),
    ) -> (Engine, tutti_core::dsp::Net) {
        let rate = SampleRate(RATE);
        let mut net = tutti_core::dsp::Net::new(0, 2);
        net.set_sample_rate(rate);
        let clock = net.add(tutti_core::TransportClock::new(
            transport.clock_links(),
            rate,
        ));
        let click = net.push(Box::new(ClickNode::with_transport(
            transport.clone(),
            settings.clone(),
            rate,
        )));
        let engine = Engine::new(transport.motion.clone(), net.backend());
        wire(&mut net, clock, click);
        net.commit_output_arity_change();
        (engine, net)
    }

    /// `frames` stereo frames of `engine` in `block`-frame device blocks,
    /// the transport rolling from the first block, and — with `seek` — a
    /// locate to beat 0.75 between two blocks past the first 60 000 frames.
    fn run(
        engine: &Engine,
        transport: &Transport,
        frames: usize,
        block: usize,
        seek: bool,
    ) -> Vec<f32> {
        transport.motion.try_send(MotionEvent::Play).expect("room");
        let mut out = Vec::with_capacity(frames * 2);
        let mut buf = vec![0.0f32; block * 2];
        let mut located = !seek;
        while out.len() < frames * 2 {
            // A seek between two blocks, past the first second: the beat
            // clock has to follow the transport, not just count frames.
            if !located && out.len() >= 2 * 60_000 {
                located = true;
                transport
                    .motion
                    .try_send(MotionEvent::Locate {
                        beat: tutti_core::Beat(0.75),
                        fade: tutti_core::FadeOut::Immediate,
                        then: tutti_core::Then::Keep,
                    })
                    .expect("room");
            }
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
            out.extend_from_slice(&buf);
        }
        out.truncate(frames * 2);
        out
    }

    /// The click at full volume on every beat.
    fn loud_click() -> Arc<ClickSettings> {
        let settings = Arc::new(ClickSettings::new());
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);
        settings
    }

    /// An engine from [`assemble`] at `RATE`, the click fed by the beat clock
    /// and on both global outputs, rolling from the first block. `frames`
    /// stereo frames rendered in 512-frame device blocks, with a seek past
    /// the first second.
    ///
    /// The click's inputs are wired here the way `build_into` declares them
    /// (`PortSources::stereo_from(clock)`), without an `App`.
    fn render_click(frames: usize) -> Vec<f32> {
        let transport = Transport::new(SampleRate(RATE));
        let settings = loud_click();
        let Assembled {
            mut graph,
            engine,
            clock,
            click,
        } = assemble(SampleRate(RATE), 0, 2, &transport, &settings).expect("builds");
        for port in 0..2 {
            graph.set_source(click, port, GraphSource::Node(clock, port));
            graph.set_output_source(port, GraphSource::Node(click, port));
        }
        assert!(graph.commit(), "the first commit goes through");
        run(&engine, &transport, frames, 512, true)
    }

    /// [`render_click`] on the `Net`-era engine ([`net_era`]).
    fn render_click_net_era(frames: usize) -> Vec<f32> {
        let transport = Transport::new(SampleRate(RATE));
        let settings = loud_click();
        let (engine, net) = net_era(&transport, &settings, |net, clock, click| {
            for port in 0..2 {
                net.set_source(click, port, tutti_core::dsp::Source::Local(clock, port));
                net.set_output_source(port, tutti_core::dsp::Source::Local(click, port));
            }
        });
        let out = run(&engine, &transport, frames, 512, true);
        // The net outlives the render: `Engine::new` renders the backend its
        // frontend feeds.
        drop(net);
        out
    }

    /// Frames on which the left channel starts sounding.
    fn onsets(stereo: &[f32]) -> Vec<usize> {
        let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
        (1..left.len())
            .filter(|&f| left[f] != 0.0 && left[f - 1] == 0.0)
            .chain((left.first().is_some_and(|&x| x != 0.0)).then_some(0))
            .collect()
    }

    /// **The builder's engine clicks the samples the `Net`-era engine
    /// clicked.** The click reads an `EnvClock` (the graph engine drives its
    /// own `TransportClock` and forbids a second); on `Net` it read a
    /// `TransportClock` in the graph, wired to the click the same way. The
    /// transport starts before the first block, so `ClickNode`'s play gate —
    /// read once per 64-frame chunk, from the live flag, which on the graph
    /// engine is already the whole block's (doc 013, gap 5) — opens on the
    /// same frame on both. A start *inside* a block would open it a block
    /// early; that is `ClickNode`'s gate, not the beat, and is pinned in
    /// `tutti-core`'s `env_clock` suite.
    ///
    /// A seek between two blocks is in the render, because that is where a
    /// wrong clock shows: a steady transport is counted alike by any clock.
    /// The onsets are also pinned on their own, so the test does not rest on
    /// the oracle alone: at 120 BPM a beat is 24 000 frames, the seek lands on
    /// the 118th 512-frame block (frame 60 416) at beat 0.75, and the next
    /// beat is a quarter beat (6 000 frames) later; a click is heard from the
    /// frame after its beat. The locate itself clicks too (60 417): it moves
    /// the whole beat from 2 to 0, which the click takes for a new beat — as
    /// it did on `Net`, and as the oracle below agrees.
    ///
    /// Mutations (run):
    /// - `AudioGraphRes::insert_beat_clock` inserting a `TransportClock`
    ///   beside the engine's → two clocks consume the one transport's seek,
    ///   the click's misses it, and the renders part after it;
    /// - inserting a silent two-port node in its place → no clicks at all.
    #[test]
    fn the_click_is_bit_identical_to_the_net_era_engine() {
        let frames = 3 * RATE as usize;
        let net = render_click_net_era(frames);
        let native = render_click(frames);
        let on = onsets(&native);
        assert_eq!(
            on,
            vec![1, 24_001, 48_001, 60_417, 66_417, 90_417, 114_417, 138_417],
            "a click on every beat, across the seek"
        );
        assert_eq!(onsets(&net), on, "onset frames");
        let parted = net
            .iter()
            .zip(&native)
            .position(|(a, b)| a.to_bits() != b.to_bits());
        assert_eq!(parted, None, "the same samples (first difference at)");
    }

    /// **The graph runs at the device's rate.** At 120 BPM and 48 kHz the
    /// click lands every 24 000 frames. Every unit inserted is prepared at
    /// the graph's rate, so a graph left at its 44.1 kHz default — which is
    /// what `build_into` built on `Net` before the rate was passed — runs the
    /// beat clock 8.8% fast and clicks every 22 050 frames.
    ///
    /// Mutation (run): `assemble` building the graph with
    /// `AudioGraphRes::headless` (the 44.1 kHz default) instead of at `rate`
    /// → clicks at 22 050 and fails.
    #[test]
    fn the_graph_runs_at_the_device_rate() {
        // The click's first sample is `sin(0)`, so it is heard from the
        // frame after its beat; the spacing is what the rate decides.
        let on = onsets(&render_click(50_000));
        assert_eq!(on, vec![1, 24_001, 48_001]);
    }

    // ---- a clip reader through the builder's engine -------------------------

    /// The tone's frame `i`: 440 Hz at `RATE`.
    #[cfg(feature = "sampler")]
    fn tone_at(i: usize) -> f32 {
        (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin()
    }

    /// One sampler voice on a second of the tone, placed at beat 0 on
    /// `transport` and pitched by `cents`.
    #[cfg(feature = "sampler")]
    fn voice(transport: &Transport, cents: f32) -> tutti_sampler::VoicePool {
        use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
        let mut wave = tutti_io::Wave::new(1, RATE);
        for i in 0..RATE as usize {
            wave.push_frame(&[tone_at(i)]);
        }
        let source = MemorySource::with_transport(
            Arc::new(wave),
            Arc::new(transport.clone()) as Arc<dyn tutti_core::Timeline>,
            tutti_core::Beat(0.0),
            None,
        );
        let (mut pool, _handle) = VoicePool::new();
        pool.insert_voice(
            SlotId(1),
            Voice {
                source: VoiceSource::Memory(source),
                play: Playback {
                    pitch: tutti_core::Cents::new(cents),
                    ..Default::default()
                },
                channel_index: None,
            },
        );
        pool
    }

    /// An engine from [`assemble`] with one sampler voice ([`voice`]) on both
    /// global outputs, the transport rolling from the first block. `frames`
    /// stereo frames in `block`-frame device blocks.
    ///
    /// The voice is a clip reader: it polls the transport (its
    /// `Arc<dyn Timeline>`) on every 64-frame call. That call is `Legacy`'s,
    /// and it reads the right beat because the engine renders a plan holding
    /// a `Legacy` unit chunk-major, 64 frames across every node with the
    /// playhead published after each (doc 013, "chunk-major `Legacy`
    /// compatibility mode").
    #[cfg(feature = "sampler")]
    fn render_voice(cents: f32, frames: usize, block: usize) -> Vec<f32> {
        let transport = Transport::new(SampleRate(RATE));
        let settings = Arc::new(ClickSettings::new());
        let Assembled {
            mut graph, engine, ..
        } = assemble(SampleRate(RATE), 0, 2, &transport, &settings).expect("builds");
        let voice = graph.insert(voice(&transport, cents));
        for port in 0..2 {
            graph.set_output_source(port, GraphSource::Node(voice, port));
        }
        assert!(graph.commit(), "the first commit goes through");
        run(&engine, &transport, frames, block, false)
    }

    /// [`render_voice`] on the `Net`-era engine ([`net_era`]). The builder
    /// pushed its `TransportClock` before any voice; the net then runs the
    /// voice first in each 64-frame chunk, and it reads the beat the clock
    /// published at the end of the previous chunk: this chunk's first frame,
    /// what the graph engine's chunk-major render gives it.
    #[cfg(feature = "sampler")]
    fn render_voice_net_era(cents: f32, frames: usize, block: usize) -> Vec<f32> {
        let transport = Transport::new(SampleRate(RATE));
        let settings = Arc::new(ClickSettings::new());
        let (engine, net) = net_era(&transport, &settings, |net, _, _| {
            let v = net.push(Box::new(voice(&transport, cents)));
            for port in 0..2 {
                net.set_output_source(port, tutti_core::dsp::Source::Local(v, port));
            }
        });
        let out = run(&engine, &transport, frames, block, false);
        drop(net);
        out
    }

    /// **A sampler voice plays in time through the builder's engine**, at
    /// `block`-frame device blocks with a rolling transport: the dry voice is
    /// the tone it plays, frame for frame, and dry and a fifth up it renders
    /// the `Net`-era engine's samples bit for bit (which also checks the
    /// oracle against the tone, so the two cannot agree on a wrong answer).
    ///
    /// `net_parity.rs` renders under a stopped transport, where a clip reader
    /// sounds nothing; this is the adapter's path with the clock moving (doc
    /// 013, the #32 follow-up).
    ///
    /// Mutation (run): the engine rendering whole device blocks with a
    /// `Legacy` unit present (`GraphRender::settle` ignoring `has_legacy`)
    /// → the native render parts from the tone and from `Net`'s at frame 64,
    /// at both block sizes.
    #[cfg(feature = "sampler")]
    fn a_voice_plays_in_time(block: usize) {
        let frames = 24_000;
        for cents in [0.0f32, 700.0] {
            let net = render_voice_net_era(cents, frames, block);
            let native = render_voice(cents, frames, block);
            if cents == 0.0 {
                for (what, out) in [("native", &native), ("Net era", &net)] {
                    for (i, s) in out.as_chunks::<2>().0.iter().enumerate() {
                        let want = tone_at(i);
                        assert!(
                            (s[0] - want).abs() < 1e-3,
                            "{block}-frame blocks, {what}: frame {i} read {}, the tone is {want}",
                            s[0]
                        );
                    }
                }
            }
            let peak = native.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.5, "{block}-frame blocks, {cents} cents: silent");
            if let Some(i) = net
                .iter()
                .zip(&native)
                .position(|(a, b)| a.to_bits() != b.to_bits())
            {
                panic!(
                    "{block}-frame blocks, {cents} cents: the render parts from the Net era's \
                     at frame {} (channel {}): net {} native {}",
                    i / 2,
                    i % 2,
                    net[i],
                    native[i]
                );
            }
        }
    }

    #[cfg(feature = "sampler")]
    #[test]
    fn a_voice_plays_in_time_at_256_frame_blocks() {
        a_voice_plays_in_time(256);
    }

    #[cfg(feature = "sampler")]
    #[test]
    fn a_voice_plays_in_time_at_512_frame_blocks() {
        a_voice_plays_in_time(512);
    }
}

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
use crate::midi::{ClockMasterRes, MidiEngineNodes, MpeModeRes};
#[cfg(feature = "midi-hardware")]
use tutti_midi_hardware::MidiSession;
#[cfg(feature = "midi")]
use tutti_midi_runtime::{ClockNode, MidiInputNode, MidiOutNode};

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
    // Built first so the port manager can be handed to the graph's MIDI input
    // node.
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
        #[cfg_attr(not(feature = "midi"), allow(unused_mut))]
        mut graph,
        engine,
        click: click_node,
    } = assemble(
        sample_rate,
        audio_engine.spec().quantum,
        inputs,
        outputs,
        &transport,
        &click_settings,
        plugin.render_workers,
    )?;

    // The engine's MIDI, as graph nodes (doc 013, rewrite item 5): the
    // hardware input (translating, and ingesting MPE in the app's
    // `MpeModeConfig`, inserted before the engine builds; default `Disabled`),
    // the clock (Beat Clock / MTC, disabled until a host enables it) and the
    // hardware out the clock is wired to, which the frontend pump drains to
    // the OS. Bound to entities below.
    #[cfg(feature = "midi")]
    let midi_nodes = {
        let mpe_mode = app
            .world()
            .get_resource::<crate::midi::MpeModeConfig>()
            .map(|c| c.0)
            .unwrap_or(tutti_midi_types::MpeMode::Disabled);
        #[cfg(feature = "midi-hardware")]
        let wire: Option<Arc<dyn tutti_midi_types::MidiIn>> = midi_io
            .as_ref()
            .map(|io| Arc::clone(io.ports()) as Arc<dyn tutti_midi_types::MidiIn>);
        #[cfg(not(feature = "midi-hardware"))]
        let wire: Option<Arc<dyn tutti_midi_types::MidiIn>> = None;
        let (input_node, input) = graph.insert(MidiInputNode::new(wire).with_mpe(mpe_mode));
        let (clock_node, master) = graph.insert(ClockNode::new());
        let (out_node, out) = graph.insert(MidiOutNode::new());
        (input_node, input, clock_node, master, out_node, out)
    };

    let callback_state = Arc::new(AudioCallbackState::new(engine, meter.clone(), tap.clone()));
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
    // The engine-built node gets an entity like everything else in the graph,
    // and `EngineNodes` publishes it. Spawned here, before any host system
    // runs, it is otherwise unreachable — and `PortSources` names sources by
    // `Entity`, so an unnameable node is an unwirable one. The click's output
    // is left unwired so that the host declares where it lands.
    //
    // The click has no inputs: it reads the beat of every frame from its
    // block's `Env`, so each click starts on the frame its beat lands on with
    // nothing wired into it.
    let click_entity = app.world_mut().spawn(click_node).id();
    app.insert_resource(EngineNodes {
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
        // The MIDI nodes get entities like every node, so a host can wire to
        // (and from) them by entity: the route rules feed from the input's
        // ports, and anything declared on the hardware out reaches the wire
        // beside the clock.
        let (input_node, input, clock_node, master, out_node, out) = midi_nodes;
        let input_entity = app.world_mut().spawn(input_node).id();
        let clock_entity = app.world_mut().spawn(clock_node).id();
        let out_entity = app.world_mut().spawn(out_node).id();
        app.world_mut()
            .get_resource_or_init::<crate::graph::EventFeeds>()
            .set(out_entity, "clock", vec![clock_node.into()]);
        app.world_mut()
            .get_resource_or_init::<crate::graph::GraphDirty>()
            .0 = true;
        app.insert_resource(MidiEngineNodes {
            input: input_entity,
            input_node,
            clock: clock_entity,
            hardware_out: out_entity,
        });
        app.insert_resource(MpeModeRes(input));
        app.insert_resource(ClockMasterRes::new(master, out));
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = midi_io {
            app.insert_resource(MidiIoRes(io));
        }
    }

    #[cfg(feature = "sampler")]
    app.insert_resource(DiskStreamerRes(disk_streamer));

    Ok(())
}

/// The graph, the engine over its audio side, and the node the engine
/// builds into it.
struct Assembled {
    graph: AudioGraphRes,
    engine: Engine,
    /// The metronome (no inputs: it reads its block's `Env`).
    click: AudioNode,
}

/// Build the graph, add the metronome, and build the engine over its audio
/// side. The device-free half of [`build_into`], so the engine can be
/// rendered in a test.
///
/// **The graph runs at the device's `rate`.** Every node inserted is prepared
/// at the graph's rate, so a graph left at its default 44.1 kHz would run
/// every node at 44.1 kHz on a 48 kHz device: every oscillator about 8.8%
/// slow. (That is what this builder did on `Net` before the rate was passed
/// here.)
///
/// The graph holds no beat clock: a graph engine drives its own
/// `TransportClock`, and every node that follows the beat reads it from its
/// block's `Env`.
fn assemble(
    rate: SampleRate,
    quantum: Option<tutti_core::Samples>,
    inputs: usize,
    outputs: usize,
    transport: &Transport,
    click_settings: &Arc<ClickSettings>,
    render_workers: usize,
) -> Result<Assembled> {
    let mut graph = AudioGraphRes::for_device(inputs, outputs, rate, quantum);
    graph.set_render_workers(render_workers);

    // Metronome. The beat, play and record state come from its
    // block's `Env`, and the count-in flag off the transport it is built over
    // (read only). Its controls are the shared click settings, which
    // `MetronomeRes` already holds.
    //
    // It is NOT wired to the output here, and must not be. `pipe_output` reads
    // like "mix the click into master" and is not what it does — it overwrites
    // every global output edge, so the first soundfont to load silently
    // disconnects the metronome. What the click feeds is the host's
    // declaration, like every other node; see `graph::wire`.
    let (click, _settings) = graph.insert(ClickNode::new(transport, Arc::clone(click_settings)));

    let engine = graph.engine(transport)?;
    Ok(Assembled {
        graph,
        engine,
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
/// than left to the engine: `Engine::new` bounds the editor to
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
/// click it builds).
///
/// From design doc 013's PR 13 to PR 15 these compared the engine with the
/// one this builder made before PR 13 (fundsp's `Net` with a
/// `TransportClock` in it, over the engine's `Net` backend, rebuilt by hand
/// as `net_era`), bit for bit. PR 15 removed that backend. Each test now
/// stands on the analytic figures it also asserted (onset frames, the tone a
/// dry voice reads), on what does not depend on the oracle (a voice's
/// render at two block sizes whose 64-frame chunks coincide), and, for the
/// samples themselves, on a golden digest recorded from this engine on the
/// commit that retired the oracle, which rendered the `Net` era's samples
/// bit for bit (asserted there). The click and the pitched voice call `sin`
/// (and the vocoder's FFT), libm quality-of-implementation that differs in
/// the last ulp between C runtimes, so the digests are asserted on
/// Linux/glibc only, where they were recorded ([`GOLDEN_HERE`]).
#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::graph::GraphSource;
    use tutti_core::{ChannelLayout, InterleavedMut, MetronomeMode, MotionEvent};

    const RATE: f64 = 48_000.0;

    /// Whether this target is the one the golden digests were recorded on:
    /// Linux with glibc's libm (see the module docs).
    const GOLDEN_HERE: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

    /// FNV-1a over the samples' little-endian `f32` bits.
    fn digest(samples: &[f32]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in samples.iter().flat_map(|s| s.to_le_bytes()) {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Assert `got` is the digest recorded as `want`, where the goldens hold.
    fn assert_golden(what: &str, got: u64, want: u64) {
        if GOLDEN_HERE {
            assert_eq!(
                got, want,
                "{what}: digest {got:#018x}, recorded {want:#018x}"
            );
        }
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

    /// An engine from [`assemble`] at `RATE`, the click on both global
    /// outputs, rolling from the first block. `frames` stereo frames rendered
    /// in 512-frame device blocks, with a seek past the first second.
    ///
    /// The click has no inputs: it reads its block's `Env`.
    fn render_click(frames: usize) -> Vec<f32> {
        let transport = Transport::new(SampleRate(RATE));
        let settings = loud_click();
        let Assembled {
            mut graph,
            engine,
            click,
            ..
        } = assemble(SampleRate(RATE), None, 0, 2, &transport, &settings, 1).expect("builds");
        for port in 0..2 {
            graph.set_output_source(port, GraphSource::Node(click, port));
        }
        assert!(graph.commit(), "the first commit goes through");
        run(&engine, &transport, frames, 512, true)
    }

    /// Frames on which the left channel starts sounding.
    fn onsets(stereo: &[f32]) -> Vec<usize> {
        let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
        (1..left.len())
            .filter(|&f| left[f] != 0.0 && left[f - 1] == 0.0)
            .chain((left.first().is_some_and(|&x| x != 0.0)).then_some(0))
            .collect()
    }

    /// **The builder's engine clicks on every beat, across a seek**, and
    /// (Linux/glibc) clicks the samples the `Net`-era engine clicked. The
    /// click reads the beat of every frame from its block's `Env` (the
    /// transport the engine's own `TransportClock` reports; the graph holds
    /// no second one); on `Net` it read a `TransportClock` in the graph,
    /// wired to its inputs, and until its `Node` port an `EnvClock` wired the
    /// same way. The transport starts before the first block, so the play
    /// gate opens on the first frame (a start inside a block opens it on its
    /// frame: `ClickNode`'s own tests pin that).
    ///
    /// A seek between two blocks is in the render, because that is where a
    /// wrong clock shows: a steady transport is counted alike by any clock.
    /// The onsets, analytically: at 120 BPM a beat is 24 000 frames, the seek
    /// lands on the 118th 512-frame block (frame 60 416) at beat 0.75, and
    /// the next beat is a quarter beat (6 000 frames) later; a click is heard
    /// from the frame after its beat (its first sample is `sin(0)`). The
    /// locate itself clicks too (60 417): it moves the whole beat from 2 to
    /// 0, which the click takes for a new beat — as it did on `Net`.
    ///
    /// Until doc 013 PR 15 the samples were compared, bit for bit, with the
    /// `Net`-era engine (`net_era`); the digest is what that render was.
    ///
    /// Mutations (run):
    /// - the click reading the block's first beat for the whole block
    ///   (`piece_beats` emitting the piece's first beat) → the onsets land on
    ///   block starts and move;
    /// - the click's volume at 0.99 → the digest moves.
    #[test]
    fn the_click_sounds_on_every_beat_across_the_seek() {
        let frames = 3 * RATE as usize;
        let rendered = render_click(frames);
        assert_eq!(
            onsets(&rendered),
            vec![1, 24_001, 48_001, 60_417, 66_417, 90_417, 114_417, 138_417],
            "a click on every beat, across the seek"
        );
        assert_golden("the click", digest(&rendered), 0x9ac0_787e_520a_8085);
    }

    /// **The graph runs at the device's rate.** At 120 BPM and 48 kHz the
    /// click lands every 24 000 frames. Every unit inserted is prepared at
    /// the graph's rate, so a graph left at its 44.1 kHz default — which is
    /// what `build_into` built on `Net` before the rate was passed — renders
    /// the click's waveform at 44.1 kHz (the click's own `prepare`) and
    /// clicked every 22 050 frames on `Net`.
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

    /// One sampler voice on a second of the tone, placed at beat 0 and
    /// pitched by `cents`. It reads the transport from its blocks' `Env`.
    #[cfg(feature = "sampler")]
    fn voice(cents: f32) -> tutti_sampler::VoicePool {
        use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
        let mut wave = tutti_io::Wave::new(1, RATE);
        for i in 0..RATE as usize {
            wave.push_frame(&[tone_at(i)]);
        }
        let source = MemorySource::placed(Arc::new(wave), tutti_core::Beat(0.0), None);
        let mut pool = VoicePool::new();
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
    /// The voice is a clip reader, a graph node: it reads the playhead from
    /// each block's `Env`, per frame, seating its read on the first frame of
    /// each 64-frame piece it renders. (Until the sampler's nodes ported it
    /// polled the transport out of band from a `Legacy` call, and read the
    /// right beat only because the engine rendered a plan holding one
    /// chunk-major.)
    #[cfg(feature = "sampler")]
    fn render_voice(cents: f32, frames: usize, block: usize) -> Vec<f32> {
        let transport = Transport::new(SampleRate(RATE));
        let settings = Arc::new(ClickSettings::new());
        let Assembled {
            mut graph, engine, ..
        } = assemble(SampleRate(RATE), None, 0, 2, &transport, &settings, 1).expect("builds");
        let (voice, _handle) = graph.insert(voice(cents));
        for port in 0..2 {
            graph.set_output_source(port, GraphSource::Node(voice, port));
        }
        assert!(graph.commit(), "the first commit goes through");
        run(&engine, &transport, frames, block, false)
    }

    #[cfg(feature = "sampler")]
    /// The dominant frequency of `x[from..]` between 500 and 900 Hz: the peak of
    /// its Hann-windowed spectrum, scanned in quarter-hertz steps. A tolerance
    /// check on it is portable (a last-ulp libm difference moves no peak).
    ///
    /// Not zero crossings: the vocoder's output carries low-level phase
    /// artefacts that add crossings, and a crossing count read the fifth-up
    /// voice 2% sharp (672.7 Hz) where its spectrum peaks at 658.75 Hz.
    fn dominant_frequency(x: &[f32], from: usize, rate: f64) -> f64 {
        use std::f64::consts::TAU;
        let w = &x[from..];
        let n = w.len() as f64;
        let mut best = (0.0, 0.0);
        let mut f = 500.0;
        while f < 900.0 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &s) in w.iter().enumerate() {
                let hann = 0.5 - 0.5 * (TAU * i as f64 / n).cos();
                let p = TAU * f * i as f64 / rate;
                re += f64::from(s) * hann * p.cos();
                im -= f64::from(s) * hann * p.sin();
            }
            let m = re * re + im * im;
            if m > best.1 {
                best = (f, m);
            }
            f += 0.25;
        }
        best.0
    }

    /// **A sampler voice plays in time through the builder's engine**, at
    /// `block`-frame device blocks with a rolling transport: the dry voice is
    /// the tone it plays, to the bit, on both channels; the voice a fifth up
    /// renders the same bits as at 64-frame blocks, and (Linux/glibc) the
    /// `Net`-era engine's.
    ///
    /// `scene_render.rs` renders under a stopped transport, where a clip
    /// reader sounds nothing; this is the adapter's path with the clock
    /// moving (doc 013, the #32 follow-up). Until doc 013 PR 15 both voices
    /// were compared, bit for bit, with the `Net`-era engine (`net_era`).
    ///
    /// Mutation (run): `Cents::to_pitch_ratio` dividing by 1 100 cents to
    /// the octave → the pitched voice is not a fifth up, on every target.
    /// Mutation (run): the voice seating each 64-frame piece at its block's
    /// first beat (`interp::place` reading `run.beat_at(0)`) → both block
    /// sizes fail. (It passed here while the assembled graph held a `Legacy`
    /// unit and the engine rendered it chunk-major, 64 frames a block, so
    /// every piece was a block's first; it is also pinned by tutti-sampler's
    /// `block_render.rs` and tutti-export's `graph_source.rs` and
    /// `sampler_to_export.rs`.) (The `Net`-era
    /// mutation here, the engine publishing its playhead before the render,
    /// has no counterpart: the voice reads the playhead from its `Env`, not
    /// the published position.)
    #[cfg(feature = "sampler")]
    fn a_voice_plays_in_time(block: usize) {
        let frames = 24_000;
        // The dry voice is pinned to the tone itself; only the pitched one,
        // which has no closed form, carries a digest.
        for (cents, want) in [(0.0f32, None), (700.0, Some(0x5681_b9dc_0fbe_0695u64))] {
            let rendered = render_voice(cents, frames, block);
            let peak = rendered.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.5, "{block}-frame blocks, {cents} cents: silent");
            if cents == 0.0 {
                for (i, s) in rendered.as_chunks::<2>().0.iter().enumerate() {
                    let want = tone_at(i).to_bits();
                    assert!(
                        s[0].to_bits() == want && s[1].to_bits() == want,
                        "{block}-frame blocks: frame {i} read {s:?}, the tone is {}",
                        tone_at(i)
                    );
                }
            } else {
                // Portable, where the digest is not: a fifth up from 440 Hz
                // is 440 · 2^(7/12) ≈ 659.26 Hz. Past the vocoder's first few
                // thousand frames, within 1% (a semitone is 6%).
                let left: Vec<f32> = rendered.iter().step_by(2).copied().collect();
                let want = 440.0 * 2f64.powf(7.0 / 12.0);
                let got = dominant_frequency(&left, 4_096, RATE);
                assert!(
                    (got - want).abs() < want * 1e-2,
                    "{block}-frame blocks: {got} Hz, a fifth up is {want} Hz"
                );
                let by64 = render_voice(cents, frames, 64);
                if let Some(i) = by64
                    .iter()
                    .zip(&rendered)
                    .position(|(a, b)| a.to_bits() != b.to_bits())
                {
                    panic!(
                        "{block}-frame blocks, {cents} cents: parts from 64-frame blocks at \
                         frame {} (channel {}): {} vs {}",
                        i / 2,
                        i % 2,
                        rendered[i],
                        by64[i]
                    );
                }
            }
            if let Some(want) = want {
                assert_golden(&format!("voice, {cents} cents"), digest(&rendered), want);
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

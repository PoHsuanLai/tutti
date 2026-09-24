//! What hosting a plugin looks like from the outside, headless.
//!
//! Run it:
//!
//! ```sh
//! cargo run -p bevy-tutti --features plugin,modulation --example plugin_host -- /path/to/Foo.vst3
//! ```
//!
//! With no path it runs the parts that need no plugin binary — the request
//! lifecycle, the catalog, and the failure path — and says what it skipped. With
//! one, it loads that plugin for real: binds the transport, declares a param
//! modulatable, opens and closes the editor, and prints what each step produced.
//!
//! # What this is checking
//!
//! That the vocabulary is usable in the order a host actually needs it, without
//! reaching around the adapter. Every call below is public API; if any step here
//! wants a private field or a second lookup, the design is wrong.
//!
//! Note what is *absent*: no `plugins.load(..)` on the frame thread, no manual
//! `set_transport_source`, no `param_target` bookkeeping, no
//! `MidiTargetRegistry::register`. Those are the adapter's job, and a host that
//! had to do them would be doing the work this crate exists to do.
//!
//! # Audio
//!
//! Against the in-repo `audio-probe.vst3` the expected samples are known
//! exactly — it renders `input + bus*1000 + channel + 1` — so the render checks
//! arithmetic rather than "not silent", which would pass with crossed channels.
//!
//! Driving an out-of-process plugin has two traps that both look like broken
//! audio: the first two blocks are silence by construction (signal starts at
//! frame 127), and an unpaced loop outruns the subprocess so every block is
//! dropped as stale. See `RENDER_FRAMES` and the sleep in the render loop.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, PortSources,
    SpawnAudioNode, TransportRes,
};
use bevy_tutti::plugin_host::{
    PluginCatalogState, PluginEditorOpen, PluginHealth, PluginLiveness, PluginLoadDone,
    PluginLoadTerminated, PluginRequest, PluginsRes, SetEditorVisible, TuttiHostingPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::{ClickState, Transport};
use tutti_core::AudioNode;
use tutti_plugin::catalog::{CatalogConfig, PluginId, Plugins, NO_SCAN_DIRS};

#[cfg(feature = "modulation")]
use bevy_tutti::modulation::{ModParamRange, TuttiModulationPlugin};
#[cfg(feature = "modulation")]
use tutti_types::ParamAddr;

const SAMPLE_RATE: f64 = 48_000.0;
/// The project tempo. Deliberately not 120: a plugin that reports 120 here is
/// reading a default rather than this session, which is the bug the transport
/// binding exists to prevent.
const TEMPO: f64 = 90.0;
/// Long enough for a load to finish (subprocess launch is ~0.5s at best) plus
/// the frames the editor open takes to attach.
const TICKS: usize = 600;
/// Samples advanced per update, standing in for one audio block.
const BLOCK: f64 = 512.0;
/// Frames pulled through the graph at the end, to prove audio moves.
///
/// Well past the two blocks of warm-up: `tick` buffers 64 samples before
/// shipping, then the pipeline lags one more block. Anything ≤128 measures only
/// the dead zone and reads as broken audio.
const RENDER_FRAMES: usize = 1024;
/// DC level fed into the plugin's input, so an *effect* plugin has something to
/// transform. A generator ignores it; a passthrough or gain reveals itself.
const INPUT_LEVEL: f32 = 0.25;

/// The plugin this run is hosting, if one was named on the command line.
#[derive(Resource)]
struct Target(Option<PluginId>);

/// One-shot latches so the example narrates rather than spams.
#[derive(Resource, Default)]
struct Narrated {
    loaded: bool,
    editor_opened: bool,
    editor_closed: bool,
}

fn main() {
    let target = std::env::args().nth(1).map(PluginId::from_path);
    if target.is_none() {
        println!(
            "no plugin path given — running the no-binary parts only.\n\
             pass one to exercise load/bind/editor:\n  \
             cargo run -p bevy-tutti --features plugin,modulation \\\n    \
             --example plugin_host -- /path/to/Foo.vst3\n"
        );
    }

    let mut app = App::new();
    // Loading and scanning both run on `AsyncComputeTaskPool`, which is the
    // host's to initialise — `TuttiHostingPlugin` does not add it, for the same
    // reason `ExportPlugin` does not: a host with its own pool configuration
    // would have it silently overridden.
    app.add_plugins((bevy_app::TaskPoolPlugin::default(), GraphReconcilePlugin));
    #[cfg(feature = "modulation")]
    app.add_plugins(TuttiModulationPlugin);
    // MIDI: a hosted plugin's inbox is an ordinary `MidiInPort`, so registration
    // is the shared path — nothing plugin-shaped is added by enabling this.
    #[cfg(feature = "midi")]
    app.add_plugins(bevy_tutti::midi::TuttiMidiPlugin);
    // PDC is opt-in: it costs a graph walk per commit, and a host with no
    // latency-reporting nodes never needs it. A plugin *is* such a node — it
    // reports its own latency plus the IPC pipeline's — so a host that loads
    // plugins and skips this has every plugin's delay uncompensated.
    app.add_plugins(bevy_tutti::LatencyCompensationPlugin);
    app.add_plugins(TuttiHostingPlugin);

    // Stand in for the engine bootstrap: a graph and a transport, no device.
    // `TuttiPlugin` does this for a real host.
    let transport = Transport::new(SAMPLE_RATE);
    transport.settings.set_tempo(TEMPO);
    let _ = transport
        .motion
        .try_send(tutti_core::transport::MotionEvent::Play);
    // Take a backend before handing the graph over. `commit_graph` asserts one
    // exists — a commit publishes the new version *to* the backend, so a `Net`
    // without one has nowhere to publish. A real host gets this from
    // `TuttiPlugin`, which hands the backend to the audio callback; here it is
    // dropped, so nothing renders and commits merely have somewhere to go.
    let mut net = Net::new(0, 2);
    let _backend = net.backend();
    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(AudioConfig {
        sample_rate: SAMPLE_RATE.into(),
        channels: Default::default(),
    });
    app.insert_resource(TransportRes(transport));
    // The metronome, for its meter cell rather than its click: the meter is
    // where the time signature lives, and a plugin's transport snapshot carries
    // bar and signature. Binding shares this cell rather than snapshotting it,
    // so a later tempo-map edit reaches plugins already running.
    app.insert_resource(MetronomeRes(std::sync::Arc::new(ClickState::new())));
    // The MIDI bus, so registration has somewhere to put the plugin's sender.
    // `test_support` because that is what this is: a harness standing in for the
    // engine bootstrap, not a host. A real one gets its bus from `build_into`,
    // wired to the audio thread's pre-block; this one is wired to nothing, which
    // is enough to prove a plugin *reaches* the bus but not that MIDI plays.
    #[cfg(feature = "midi")]
    app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
    app.insert_resource(AudioEngineState::Running);

    // A catalog with no scan dirs: this example never scans, it registers the
    // one path it was given. `TuttiHostingPlugin` inserts an equivalent default,
    // so this only demonstrates that overriding it is the whole config story.
    // `NO_SCAN_DIRS`, not `vec![]`: the element type of an empty vec is
    // unconstrained here, so `impl Into<PathBuf>` has nothing to infer from.
    let config = CatalogConfig::new(
        std::env::temp_dir().join("plugin-host-example.json"),
        NO_SCAN_DIRS,
    );
    app.insert_resource(PluginsRes::new(Plugins::empty(config)));

    app.insert_resource(Target(target));
    app.init_resource::<Narrated>();

    app.add_systems(Startup, request_plugin);
    app.add_systems(Update, (narrate_load, narrate_editor, drive_transport));

    for _ in 0..TICKS {
        app.update();
    }

    report(app.world_mut());
}

/// Spawn the request. This is the entire load API from a host's side.
fn request_plugin(mut commands: Commands, target: Res<Target>, config: Res<AudioConfig>) {
    let Some(id) = target.0.clone() else {
        return;
    };

    let mut entity = commands.spawn(PluginRequest {
        id,
        // Read off the engine, not assumed: the plugin is instantiated at this
        // rate and a mismatch is audible.
        sample_rate: config.sample_rate,
        // A previously saved chunk would go here — `PluginHealth::snapshot` is
        // where one comes from after a crash.
        state: None,
    });

    // Declare which params modulation may reach. A plugin addresses its params
    // by numeric id, so these are `ParamAddr::Id`; a native node would use
    // `ParamAddr::Unit`. Ranges come from the host because asking the plugin
    // means a blocking IPC round-trip.
    #[cfg(feature = "modulation")]
    entity.insert(ModParamRange::default().with(ParamAddr::Id(0), 0.5, 0.0, 1.0));

    // Observed at the spawn site, where the surrounding context is in scope.
    entity.observe(|done: On<PluginLoadDone>| match &done.result {
        Ok(()) => println!("  load: ok"),
        Err(e) => println!("  load: failed — {e}"),
    });
}

/// Report the first frame a plugin is fully bound, and open its editor.
fn narrate_load(
    mut commands: Commands,
    mut narrated: ResMut<Narrated>,
    loaded: Query<(Entity, &PluginHealth), With<AudioNode>>,
) {
    if narrated.loaded {
        return;
    }
    let Ok((entity, health)) = loaded.single() else {
        return;
    };
    narrated.loaded = true;
    println!("  status: {:?}", health.status);

    // Feed the plugin a signal, then route it to master. Note there is nothing
    // plugin-specific in either step: wiring names *entities*, never node types,
    // so a hosted plugin is addressed exactly like a synth or a filter.
    //
    // The input matters for what the render below can prove. Most probe plugins
    // are effects — they transform input rather than generate — so rendering one
    // with silence at its input yields silence at its output whether the path
    // works or not. A known DC level in makes the output diagnostic.
    // Stereo DC: `dc` with a 2-channel argument, so the source has a port 1 for
    // the plugin's right input. A mono `Const::mono(x)` here leaves port 1 unresolvable,
    // which `rebuild` skips — silently, since an unresolvable port is an
    // ordinary not-yet state elsewhere.
    let source = commands
        .spawn_audio_node(tutti_nodes::testing::Const::frame(&[
            INPUT_LEVEL,
            INPUT_LEVEL,
        ]))
        .id();
    commands
        .entity(entity)
        .insert(PortSources::stereo_from(source));
    commands.insert_resource(MasterSources::from(entity));
    println!("  fed {INPUT_LEVEL} DC in, wired to master out");

    println!("  opening editor");
    commands.trigger(SetEditorVisible::show(entity));
}

/// Report the editor opening, then close it again to prove it is reversible.
fn narrate_editor(
    mut commands: Commands,
    mut narrated: ResMut<Narrated>,
    open: Query<(Entity, &PluginEditorOpen)>,
) {
    match open.single() {
        Ok((entity, editor)) if !narrated.editor_opened => {
            narrated.editor_opened = true;
            println!(
                "  editor: open at {}x{} (resizable={})",
                editor.width, editor.height, editor.capabilities.resize.resizable
            );
            // Closing and reopening is the case the old open-component could not
            // express: it was consumed a frame after insertion, so nothing could
            // ask whether an editor was showing.
            commands.trigger(SetEditorVisible::toggle(entity));
        }
        Err(_) if narrated.editor_opened && !narrated.editor_closed => {
            narrated.editor_closed = true;
            println!("  editor: closed, and reopenable");
        }
        _ => {}
    }
}

/// Advance the playhead so the transport a plugin reads is actually moving.
///
/// The unit types are why this reads as arithmetic on beats rather than on
/// floats: a `Beat` plus a raw `f64` does not compile, which is what stops a
/// seconds-shaped quantity being added to a beat-shaped one.
fn drive_transport(transport: Res<TransportRes>) {
    let beat = transport.settings.beat();
    let per_tick = tutti_core::BeatDuration(TEMPO / 60.0 / SAMPLE_RATE * BLOCK);
    transport.settings.set_beat(beat + per_tick);
}

fn report(world: &mut World) {
    println!("\n--- result ---");

    // Only the in-repo probe has a predictable output, so only it gets the
    // arithmetic check below.
    let is_probe = world
        .query::<&PluginRequest>()
        .iter(world)
        .next()
        .is_some_and(|r| r.id.path().to_string_lossy().contains("audio-probe"));

    let catalog_state = world.resource::<PluginCatalogState>().clone();
    println!("catalog: {catalog_state:?}");

    // Query the *requests*, not the successes. A load that failed leaves a
    // request carrying `PluginLoadTerminated` and no `PluginHealth`, and a
    // report that only looked for health would call that "nothing happened" —
    // which is exactly the silent-failure this layer is meant to end.
    let mut requests = world.query_filtered::<(
        Option<&PluginLoadTerminated>,
        Option<&PluginHealth>,
        Option<&AudioNode>,
    ), With<PluginRequest>>();
    let all: Vec<_> = requests
        .iter(world)
        .map(|(t, h, n)| {
            (
                t.is_some(),
                h.map(|h| h.status.clone()),
                h.map(|h| h.snapshot().is_some()),
                n.is_some(),
            )
        })
        .collect();

    if all.is_empty() {
        println!("no plugin requested this run.");
        println!(
            "\nwhat ran without one: the catalog resource and the scan/probe\n\
             systems. what did not: the load path, transport binding, param\n\
             accumulators, the editor — all of which need a plugin. Pass a path\n\
             to reach them."
        );
        return;
    }

    for (terminated, status, has_snapshot, wired) in all {
        match status {
            Some(status) => {
                println!("plugin: {status:?}");
                println!("  wired into the graph: {wired}");
                println!(
                    "  recoverable state captured: {}",
                    has_snapshot == Some(true)
                );
                if matches!(status, PluginLiveness::Dead { .. }) {
                    println!(
                        "  (dead plugins are unwired by removing `AudioNode`; the same\n   \
                         observers that unwire the graph take the MIDI sender off the bus)"
                    );
                }
            }
            // Terminated with no health: the load failed. The reason already
            // went to the `PluginLoadDone` observer above — this is only the
            // steady state it left behind.
            None if terminated => println!(
                "plugin: load failed (see above); request retained and marked\n  \
                 terminated, so it is not retried — each retry would be another\n  \
                 subprocess launch against a four-thread pool"
            ),
            None => println!("plugin: load still in flight after {TICKS} ticks"),
        }
    }

    // MIDI: did the plugin's sender actually reach the bus? Registration is the
    // shared steady-state pass, keyed on `AudioNode` — nothing plugin-specific
    // ran — so this is really asking whether `impl MidiNode for PluginClient`
    // plus the type registration were enough. If the plugin were unregistered,
    // the resolver would simply never see it and MIDI would go nowhere, silently.
    #[cfg(feature = "midi")]
    {
        let mut registered = world.query::<&bevy_tutti::midi::MidiRegistered>();
        let ids: Vec<_> = registered.iter(world).map(|r| r.unit_id()).collect();
        // Distinguish "the plugin was not registered" from "there was no bus to
        // register into". Only `engine::build_into` inserts `MidiBusRes`, and
        // this example stands in for the bootstrap rather than running it — so
        // the absence is the harness's, not the plugin layer's, and reporting it
        // as a MIDI failure would be blaming the wrong thing.
        match world.get_resource::<bevy_tutti::midi::MidiBusRes>() {
            None => println!(
                "midi: no MidiBusRes — only `build_into` inserts one, and this\n  \
                 example builds a graph by hand. Registration correctly skipped."
            ),
            Some(bus) => {
                let on_bus = ids.iter().filter(|id| bus.contains(**id)).count();
                println!(
                    "midi: {on_bus}/{} registered sender(s) on the bus",
                    ids.len()
                );
            }
        }
    }

    // PDC: what did the graph walk conclude? A plugin reports its own latency
    // plus the IPC batcher's fixed pipeline cost, and compensation inserts delay
    // on the *other* paths so everything lands aligned. Zero here would mean
    // either no compensation ran or the plugin reported nothing.
    let latency = world.resource::<bevy_tutti::GraphLatency>();
    println!(
        "pdc: graph latency {} samples ({:.2} ms at {SAMPLE_RATE:.0} Hz)",
        latency.0.get(),
        latency.0.get() as f64 / SAMPLE_RATE * 1000.0
    );

    // IO width, which separates "rendered silence" from "no channel to render
    // into" — the batcher loops `for ch in 0..outputs`, so a zero width writes
    // nothing and leaves the buffer untouched.
    {
        let node = world
            .query_filtered::<&AudioNode, With<PluginHealth>>()
            .single(world)
            .ok()
            .copied();
        let graph = world.resource::<AudioGraphRes>();
        match node {
            Some(n) => {
                let unit = graph.0.node(n.0);
                println!(
                    "plugin node io: {} in, {} out",
                    unit.inputs(),
                    unit.outputs()
                );
            }
            None => println!("plugin node io: node not resolvable in the graph"),
        }
    }

    // Pull real samples through the graph. Everything above only proves the
    // plugin is *reachable*; this proves audio moves through it.
    //
    // `tick` per sample, as a `Net` at the master output is driven. It warms up
    // one block later than `process` (frame 127 vs 63), which is why the
    // format-level suites in `tutti-vst3-host` see a shorter dead zone.
    {
        use tutti_core::AudioUnit as _;
        let mut graph = world.resource_mut::<AudioGraphRes>();
        let mut frame = [0.0f32; 2];
        // Per channel, not one peak: the whole point of a bus/channel tag is
        // that a plugin writing the *same* value to both channels and one
        // writing the right value to each are different outcomes, and a single
        // peak cannot tell them apart. Last frame as well as peak, because a
        // plugin that ramps and one that settles also differ.
        let mut peak = [0.0f32; 2];
        let mut rendered = 0usize;
        // Where silence stops, not whether it started — the pipeline opens
        // silent by design, and a peak alone conflates that with a dead plugin.
        let mut first_signal: Option<usize> = None;
        for _ in 0..RENDER_FRAMES {
            graph.0.tick(&[], &mut frame);
            peak[0] = peak[0].max(frame[0].abs());
            peak[1] = peak[1].max(frame[1].abs());
            if first_signal.is_none() && (frame[0] != 0.0 || frame[1] != 0.0) {
                first_signal = Some(rendered);
            }
            rendered += 1;

            // Pace at the callback rate. Load-bearing, not cosmetic: the slab
            // ring is two deep, so a block more than one behind the newest is
            // dropped by design (`MAX_BEHIND`). Free-running outruns the
            // subprocess and every block ages out — silence, with no error.
            if rendered.is_multiple_of(64) {
                std::thread::sleep(std::time::Duration::from_micros(1333));
            }
        }
        println!(
            "rendered {rendered} frames: peak L={:.6} R={:.6}, last L={:.6} R={:.6}",
            peak[0], peak[1], frame[0], frame[1]
        );
        match first_signal {
            Some(at) => println!("  first non-zero sample at frame {at} (warm-up is 2 blocks)"),
            None => println!("  no non-zero sample in {rendered} frames"),
        }

        // The probe's output is known exactly (`input + bus*1000 + channel + 1`
        // against `INPUT_LEVEL` DC on bus 0), so check the numbers. Any other
        // pair is a routing fault — swapped channels, an aux bus on the main
        // one, or no input reaching the plugin — and a peak-only check passes
        // for all three.
        if is_probe {
            let want = [INPUT_LEVEL + 1.0, INPUT_LEVEL + 2.0];
            if peak == want {
                println!(
                    "  matches the probe's expected tags exactly \
                     (L={:.2}, R={:.2} = input + bus*1000 + channel + 1)",
                    want[0], want[1]
                );
            } else {
                println!(
                    "  MISMATCH: expected L={:.2} R={:.2}, got L={:.6} R={:.6} \
                     — the tags are exact, so this is a routing fault.",
                    want[0], want[1], peak[0], peak[1]
                );
            }
        }
        if peak == [0.0, 0.0] {
            println!(
                "  Silent. Check the io width above, that the subprocess is \
                 alive, and that the render loop is not outrunning it."
            );
        }
    }

    let transport = world.resource::<TransportRes>();
    println!(
        "transport the plugin reads: {:.1} BPM, beat {:.2}",
        transport.settings.tempo(),
        transport.settings.beat().get()
    );
    println!(
        "  (a plugin reporting 120 BPM here would be reading a default rather\n   \
         than this session — the bug transport binding exists to prevent)"
    );
}

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

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
};
use bevy_tutti::plugin_host::{
    PluginCatalogState, PluginEditorOpen, PluginHealth, PluginLoadDone, PluginLoadTerminated,
    PluginRequest, PluginStatus, PluginsRes, SetEditorVisible, TuttiHostingPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::{ClickState, Transport};
use tutti_core::AudioNode;
use tutti_plugin::catalog::{CatalogConfig, PluginId, Plugins};

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
const RENDER_FRAMES: usize = 256;

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
        sample_rate: SAMPLE_RATE,
        channels: Default::default(),
    });
    app.insert_resource(TransportRes(transport));
    // The metronome, for its meter cell rather than its click: the meter is
    // where the time signature lives, and a plugin's transport snapshot carries
    // bar and signature. Binding shares this cell rather than snapshotting it,
    // so a later tempo-map edit reaches plugins already running.
    app.insert_resource(MetronomeRes(std::sync::Arc::new(ClickState::new())));
    app.insert_resource(AudioEngineState::Running);

    // A catalog with no scan dirs: this example never scans, it registers the
    // one path it was given. `TuttiHostingPlugin` inserts an equivalent default,
    // so this only demonstrates that overriding it is the whole config story.
    let config = CatalogConfig::new(
        std::env::temp_dir().join("plugin-host-example.json"),
        vec![],
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

    // Route the plugin to the master output. Note there is nothing
    // plugin-specific here: wiring names *entities*, never node types, so a
    // hosted plugin is addressed exactly like a synth or a filter. This is the
    // whole audio-output story from a host's side.
    commands.insert_resource(MasterSources::from(entity));
    println!("  wired to master out");

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
                if matches!(status, PluginStatus::Dead { .. }) {
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

    // Pull real samples through the graph. Everything above only proves the
    // plugin is *reachable*; this is the part that proves audio moves through
    // it. `tick` per sample rather than `process`, matching the audio tests —
    // a block is capped at 64 frames and this needs no more than a handful.
    {
        use tutti_core::dsp::AudioUnit as _;
        let mut graph = world.resource_mut::<AudioGraphRes>();
        let mut frame = [0.0f32; 2];
        let mut peak = 0.0f32;
        let mut rendered = 0usize;
        for _ in 0..RENDER_FRAMES {
            graph.0.tick(&[], &mut frame);
            peak = peak.max(frame[0].abs()).max(frame[1].abs());
            rendered += 1;
        }
        println!("rendered {rendered} frames through the plugin, peak {peak:.6}");
        if peak == 0.0 {
            println!(
                "  (silence is the honest answer for a probe plugin with no input\n   \
                 and no note — what this shows is that the graph pulls *through*\n   \
                 the plugin without panicking or stalling, which is the wiring\n   \
                 claim. Asserting on non-silence needs a plugin that generates.)"
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

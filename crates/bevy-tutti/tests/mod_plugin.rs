//! Modulation lanes on hosted plugins' parameters (doc 014 §8.2, §12
//! "PR 4b"): a plugin target is a lane on the plugin's event input, its knob
//! its own, started at the plugin's value and moved by the plugin's GUI.
//!
//! The plugins are the reference ones: the CLAP probe through a real
//! `plugin-server` (build it first: `cargo build -p tutti-plugin-server`),
//! and the VST2 probe in process.

#![cfg(all(feature = "plugin", feature = "modulation"))]

#[macro_use]
mod common;

use std::time::{Duration, Instant};

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
};
use bevy_tutti::modulation::{ParamRef, TuttiModulationPlugin};
use bevy_tutti::plugin_host::{PluginLoadTerminated, PluginRequest, TuttiHostingPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::{ClickState, Transport};
use tutti_core::AudioNode;
use tutti_plugin::catalog::PluginId;
use tutti_types::SampleRate;

const RATE: f64 = 48_000.0;

/// Set a switch a plugin (or its server) reads when it starts.
fn env(key: &str, value: impl ToString) {
    // SAFETY: nextest runs each test in its own process, and nothing in it
    // reads the environment concurrently.
    unsafe { std::env::set_var(key, value.to_string()) };
}

/// A headless app (stereo, 48 kHz) with the graph, hosting and modulation
/// plugins.
fn app() -> App {
    let mut graph = AudioGraphRes::headless(0, 2);
    graph.set_sample_rate(SampleRate(RATE));
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(graph);
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(RATE)));
    app.insert_resource(MetronomeRes(std::sync::Arc::new(ClickState::new())));
    app.add_plugins((
        GraphReconcilePlugin,
        TuttiHostingPlugin,
        TuttiModulationPlugin,
    ));
    app
}

/// Loads the plugin at `path` on the master and waits until its node is in
/// the graph and its values are read.
fn load(app: &mut App, path: std::path::PathBuf) -> Entity {
    let entity = app
        .world_mut()
        .spawn(PluginRequest {
            id: PluginId::from_path(path),
            sample_rate: SampleRate(RATE),
            ..Default::default()
        })
        .id();
    app.insert_resource(MasterSources::from(entity));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let world = app.world();
        let loaded = world.get::<PluginLoadTerminated>(entity).is_some()
            && world.get::<AudioNode>(entity).is_some_and(|&node| {
                world
                    .resource::<AudioGraphRes>()
                    .with_plugin(node, |p| p.param_values().is_some_and(|m| m.is_ready()))
                    == Some(true)
            });
        if loaded {
            break;
        }
        assert!(Instant::now() < deadline, "the plugin never loaded");
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    entity
}

/// Frames until `target` has a lane, and its knob.
fn knob_of(app: &mut App, target: ParamRef) -> f32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = app.world().resource::<AudioGraphRes>().modulation();
        assert_eq!(state.refusal(), None, "the declarations apply");
        if let Some((_, lane)) = state.lane(target) {
            return lane.knob().expect("a plugin lane has a knob");
        }
        assert!(Instant::now() < deadline, "no lane on {target:?}");
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **Export == live on a plugin's parameter (M3).** The reference CLAP
/// plugin renders a constant tag scaled by its gain (id 77, applied
/// sample-accurately at each parameter event's frame); a 3 Hz sine at depth
/// 0.1 modulates the gain's lane, whose knob starts at the plugin's own
/// value (0 dB: 60/72 of `-60..=12`). Live renders rolling 1024-frame
/// blocks from the play start, each once the server answered the chunk
/// before it (`PluginControls::chunk_in_flight_answered`), and fails by name
/// if one was played unanswered (`missed_chunks`); the export
/// forks the graph (the plugin by state transfer, the lane by clone) and
/// renders the same span at its 1024-frame chunk. Equal bit for bit.
///
/// On main (7bc1d6f3) this failed: the plugin's gain was a per-block
/// automation binding the export did not fork, so the export rendered the
/// authored value (`0.0631`, the base 0.5) while live rendered the
/// modulated one (`0.5012`).
///
/// Mutations (run), each fails: the knob seeded at the range's bottom
/// (the session's `ModHostOver` answering `Some(Some(0.0))` for a plugin
/// parameter: the knob check);
/// a lane handing the editor no fork source (`fork: None` in
/// `ControlLaneNode::into_parts`: the export is refused as not forkable);
/// a lane's fork starting its knob at 0, not the live knob (`fork_fresh`:
/// frame 1024 reads 1 live, 0.001 exported); the wait for the answer
/// removed (the blocks rendered back to back: "live block 1 late or
/// missed", not a silent-vs-signal compare). With the old fixed 25 ms
/// pacing a slow server under a loaded run left a block silent (2 of 33
/// full-suite runs: "frame 4096: live 0 vs export 2.169").
#[cfg(feature = "export")]
#[test]
fn an_export_is_the_live_render_on_a_clap_gain() {
    use bevy_tutti::export::{
        ExportClock, ExportDone, ExportOutput, ExportPlugin, ExportRequest, ExportSource,
        ExportTarget,
    };
    use bevy_tutti::modulation::{Lfo, ModRange, ModRoute, ModSource};
    use std::sync::{Arc, Mutex};
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_export::{AudioFormat, BitDepth, EncodeConfig, ExportConfig, RenderConfig};
    use tutti_types::{Beat, Bpm, Depth, Hz};

    const BLOCK: usize = 1024;
    const BLOCKS: usize = 24;

    env(
        "TUTTI_PLUGIN_SERVER",
        common::plugin::plugin_server().display(),
    );
    env("TUTTI_CLAP_PROBE_RENDER_MODE", 2);
    env("TUTTI_CLAP_PROBE_APPLY_GAIN", 1);
    let mut app = app();
    app.add_plugins(ExportPlugin);
    app.insert_resource(bevy_tutti::graph::AudioConfig {
        sample_rate: SampleRate(RATE),
        channels: tutti_core::ChannelLayout::STEREO,
    });
    let probe = load(&mut app, common::plugin::clap_probe());
    let gain = ParamRef::plugin(probe, 77);
    let world = app.world_mut();
    world.spawn(ModRange::new(gain, 0.0, 1.0));
    let lfo = world.spawn(ModSource::new(Lfo::sine().hz(Hz(3.0)))).id();
    world.spawn(ModRoute::new(lfo, gain).with_depth(Depth(0.1)));
    let knob = knob_of(&mut app, gain);
    assert!(
        (knob - 60.0 / 72.0).abs() < 1e-4,
        "the knob starts at the plugin's 0 dB: {knob}"
    );
    for _ in 0..3 {
        app.update();
    }

    let total = BLOCK * BLOCKS;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: Bpm(120.0),
        sample_rate: SampleRate(RATE),
        loop_range: None,
    }));
    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(RATE),
            duration_seconds: total as f64 / RATE,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: BitDepth::Float32,
            channels: tutti_core::ChannelLayout::STEREO,
        },
        ..Default::default()
    };

    // Live: rolling from beat 0 in device-sized blocks, a frame between
    // each as a device's callbacks would leave. The plugin's pipeline plays
    // each chunk's answer one block later; a block played before its answer
    // landed is silence, which would read as a mismatch below. So before each
    // block, wait for the chunk in flight to be answered (a slow server, a
    // loaded machine), and after it, require that none was played unanswered:
    // a late block is waited for, a missed one fails here, by name.
    let controls = {
        let world = app.world();
        let node = *world.get::<AudioNode>(probe).expect("loaded");
        world
            .resource::<AudioGraphRes>()
            .with_plugin(node, |p| p.controls().cloned())
            .flatten()
            .expect("an out-of-process plugin has controls")
    };
    let missed_before = controls.missed_chunks();
    let mut side = app
        .world_mut()
        .resource_mut::<AudioGraphRes>()
        .take_audio_side();
    let mut live = [Vec::new(), Vec::new()];
    let mut out = vec![Vec::new(), Vec::new()];
    for b in 0..BLOCKS {
        let beat = (b * BLOCK) as f64 * 2.0 / RATE;
        side.set_transport(tutti_graph::Transport::new(
            true,
            Bpm(120.0),
            Beat(beat),
            None,
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !controls.chunk_in_flight_answered() {
            assert!(
                Instant::now() < deadline,
                "live block {b}: the chunk before it was never answered (10 s)"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        side.render(BLOCK, BLOCK, &mut out);
        assert_eq!(
            controls.missed_chunks(),
            missed_before,
            "live block {b} late or missed: the plugin's chunk was played unanswered"
        );
        for (l, o) in live.iter_mut().zip(&out) {
            l.extend_from_slice(o);
        }
        app.update();
    }

    let slot: Arc<Mutex<Option<Vec<Vec<f32>>>>> = Arc::default();
    let seen = Arc::clone(&slot);
    app.world_mut()
        .spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::Buffers,
            config,
            ExportClock::timeline(timeline),
        ))
        .observe(move |done: On<ExportDone>| match &done.result {
            Ok(ExportOutput::Buffers(r)) => *seen.lock().unwrap() = Some(r.planes.clone()),
            other => panic!("the export failed: {other:?}"),
        });
    let mut planes = None;
    for _ in 0..6_000 {
        app.update();
        if let Some(p) = slot.lock().unwrap().take() {
            planes = Some(p);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let export = planes.expect("the export reported");
    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(e.len() >= total, "channel {c}: {} frames", e.len());
        let moving = l.iter().filter(|&&v| v != 0.0).collect::<Vec<_>>();
        assert!(
            moving.windows(2).any(|w| w[0] != w[1]),
            "channel {c}: the gain moves"
        );
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f}: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
}

/// **An edit in a plugin's GUI becomes its lane's knob** (doc 014 §8.2, one
/// writer per knob). The reference VST2 plugin, in process, reports
/// `audioMasterAutomate(0, 0.5)` from inside `process` (as a GUI drag
/// does); the host's drain (`editor_idle`) hands it to the parameter
/// callback, and the next frame writes it into the lane's knob on
/// parameter 0, leaving parameter 1's knob where the plugin started it.
///
/// Mutations (run), each fails: the session's `Phase::Plugins` not writing
/// the knob (the mirror alone: parameter 0's knob stays 0); writing every
/// edit to parameter 1's knob (parameter 0's stays 0).
#[cfg(feature = "vst2")]
#[test]
fn a_plugin_gui_edit_becomes_the_knob() {
    use bevy_tutti::modulation::{Macro, ModRange, ModRoute, ModSource};
    use bevy_tutti::plugin_host::PluginEmitter;

    let path = common::plugin::vst2_probe();
    let burst = |count: u32| {
        // SAFETY: the path is the cdylib this crate's dev-dependency built;
        // the probe exports this as `extern "C" fn(u32)`.
        let lib = unsafe { libloading::Library::new(&path) }.expect("re-open the probe");
        {
            let f: libloading::Symbol<extern "C" fn(u32)> =
                unsafe { lib.get(b"tutti_vst2_probe_set_automate_burst\0") }
                    .expect("the burst switch");
            f(count);
        }
        // Never unloaded: the host still runs the image.
        std::mem::forget(lib);
    };

    let mut app = app();
    let plugin = load(&mut app, path.clone());
    let (p0, p1) = (ParamRef::plugin(plugin, 0), ParamRef::plugin(plugin, 1));
    let world = app.world_mut();
    world.spawn(ModRange::new(p0, 0.0, 1.0));
    world.spawn(ModRange::new(p1, 0.0, 1.0));
    let zero = world.spawn(ModSource::new(Macro(0.0))).id();
    world.spawn(ModRoute::new(zero, p0));
    world.spawn(ModRoute::new(zero, p1));
    // The probe's parameter `i` starts at `i / 16`.
    assert_eq!(knob_of(&mut app, p0), 0.0);
    assert_eq!(knob_of(&mut app, p1), 1.0 / 16.0);

    burst(1);
    let mut out = [0.0f32; 2];
    app.world_mut()
        .resource_mut::<AudioGraphRes>()
        .render_frame_at(
            &tutti_graph::Transport::new(
                false,
                tutti_types::Bpm(120.0),
                tutti_types::Beat(0.0),
                None,
            ),
            &mut out,
        );
    burst(0);
    app.world()
        .get::<PluginEmitter>(plugin)
        .expect("the plugin's handle")
        .handle
        .editor_idle();
    app.update();

    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert_eq!(state.lane(p0).expect("a lane").1.knob(), Some(0.5));
    assert_eq!(state.lane(p1).expect("a lane").1.knob(), Some(1.0 / 16.0));
    let node = *app.world().get::<AudioNode>(plugin).expect("bound");
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .with_plugin(node, |p| p.param_values().expect("the mirror").get(0)),
        Some(Some(Some(0.5))),
        "and the mirror"
    );
}

/// **A plugin's MIDI wiring changing keeps its parameter lanes.** The
/// reference VST2 plugin (in process) has a lane on parameter 0 (a macro's
/// `+0.25` on a knob of 0), on its one event input. A MIDI source is then
/// declared on that input (`EventSources`) and taken away again: each
/// rewrite of the port's declared sources leaves the lane's edge where it
/// was (`tutti_session::Graph::set_event_sources` keeps the edges it did not
/// make), so `event_sources` still lists the lane and the plugin still
/// reads `0.25`.
///
/// Mutation (run): `set_event_sources` dropping the edges it did not make
/// (the `others` re-add removed) → the lane is gone from the port after the
/// first rewrite → fails.
#[cfg(feature = "vst2")]
#[test]
fn a_plugins_midi_rewiring_keeps_its_lanes() {
    use bevy_tutti::graph::{EventSources, SpawnAudioNode};
    use bevy_tutti::modulation::{Macro, ModRange, ModRoute, ModSource};
    use bevy_tutti::plugin_host::PluginEmitter;
    use tutti_graph::ForkByClone;
    use tutti_plugin::handles::ParamAddress;
    use tutti_types::Depth;

    let mut app = app();
    let plugin = load(&mut app, common::plugin::vst2_probe());
    let p0 = ParamRef::plugin(plugin, 0);
    let world = app.world_mut();
    world.spawn(ModRange::new(p0, 0.0, 1.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, p0).with_depth(Depth(0.25)));
    knob_of(&mut app, p0);
    let lane = AudioNode(
        app.world()
            .resource::<AudioGraphRes>()
            .modulation()
            .lane(p0)
            .expect("a lane")
            .0,
    );
    let node = *app.world().get::<AudioNode>(plugin).expect("bound");
    let clip = app
        .world_mut()
        .commands()
        .spawn_audio_node(ForkByClone(common::probe::Held { on: u64::MAX }))
        .id();
    app.world_mut().flush();
    app.update();
    let clip_node = *app.world().get::<AudioNode>(clip).expect("bound");
    let mut side = app
        .world_mut()
        .resource_mut::<AudioGraphRes>()
        .take_audio_side();
    let value = |app: &App| {
        app.world()
            .get::<PluginEmitter>(plugin)
            .expect("the handle")
            .handle
            .params()
            .parameter_value(ParamAddress::Index(0))
            .expect("a value")
    };
    let mut out = vec![Vec::new(), Vec::new()];
    let mut check = |app: &mut App, want: &[AudioNode], what: &str| {
        let mut want: Vec<_> = want.iter().map(|n| n.0).collect();
        want.sort();
        for _ in 0..2 {
            app.update();
        }
        side.render(1_024, 128, &mut out);
        let mut sources: Vec<_> = app
            .world()
            .resource::<AudioGraphRes>()
            .event_sources(node, 0)
            .into_iter()
            .map(|s| s.node.0)
            .collect();
        sources.sort();
        assert_eq!(sources, want, "{what}: the port's sources");
        assert_eq!(value(app), 0.25, "{what}: the lane reaches the plugin");
    };
    check(&mut app, &[lane], "before");
    app.world_mut()
        .entity_mut(plugin)
        .insert(EventSources::from(clip));
    check(&mut app, &[lane, clip_node], "a MIDI source added");
    app.world_mut()
        .entity_mut(plugin)
        .insert(EventSources(Vec::new()));
    check(&mut app, &[lane], "the MIDI source removed");
}

/// **A plugin echoing the lane's writes does not ratchet its knob.** The
/// reference VST2 plugin (in process) with its echo on reports every value
/// `setParameter` is given back through `audioMasterAutomate`, from inside
/// the call. Its parameter 0 has a lane: a knob of 0 and a macro's `+0.25`,
/// so the lane writes 0.25. Taken as a GUI edit, that echo would become the
/// knob (0.25), the lane would then write 0.5, and so on to the clamp; the
/// VST2 host drops a report made inside its own `setParameter` call
/// (`tutti-vst2-host`'s `host_write`), so after four rounds of render,
/// drain and frame the knob is still 0 and the plugin still reads 0.25.
///
/// Mutation (run): `HostState::automate` without the `IN_HOST_WRITE` check
/// → the knob reads 0.25 after the first round → fails.
#[cfg(feature = "vst2")]
#[test]
fn a_plugin_echoing_the_lane_keeps_its_knob() {
    use bevy_tutti::modulation::{Macro, ModRange, ModRoute, ModSource};
    use bevy_tutti::plugin_host::PluginEmitter;
    use tutti_plugin::handles::ParamAddress;
    use tutti_types::Depth;

    let path = common::plugin::vst2_probe();
    let echo = |on: bool| {
        // SAFETY: the path is the cdylib this crate's dev-dependency built;
        // the probe exports this as `extern "C" fn(bool)`.
        let lib = unsafe { libloading::Library::new(&path) }.expect("re-open the probe");
        {
            let f: libloading::Symbol<extern "C" fn(bool)> =
                unsafe { lib.get(b"tutti_vst2_probe_set_echo_set_parameter\0") }
                    .expect("the echo switch");
            f(on);
        }
        // Never unloaded: the host still runs the image.
        std::mem::forget(lib);
    };

    let mut app = app();
    let plugin = load(&mut app, path.clone());
    let p0 = ParamRef::plugin(plugin, 0);
    let world = app.world_mut();
    world.spawn(ModRange::new(p0, 0.0, 1.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, p0).with_depth(Depth(0.25)));
    assert_eq!(knob_of(&mut app, p0), 0.0);
    echo(true);

    let mut side = app
        .world_mut()
        .resource_mut::<AudioGraphRes>()
        .take_audio_side();
    let mut out = vec![Vec::new(), Vec::new()];
    for round in 0..4 {
        side.render(1_024, 128, &mut out);
        let handle = app
            .world()
            .get::<PluginEmitter>(plugin)
            .expect("the plugin's handle")
            .handle
            .clone();
        handle.editor_idle();
        app.update();
        let state = app.world().resource::<AudioGraphRes>().modulation();
        assert_eq!(
            state.lane(p0).expect("a lane").1.knob(),
            Some(0.0),
            "round {round}: the knob"
        );
        assert_eq!(
            handle.params().parameter_value(ParamAddress::Index(0)),
            Some(0.25),
            "round {round}: the plugin reads the knob plus the offset"
        );
    }
    echo(false);
}

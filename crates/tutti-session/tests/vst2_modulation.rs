//! Modulation lanes on an in-process VST2 plugin's parameters (doc 014 §8.2,
//! §12.5): a plugin target is a lane on the plugin's event input, its knob
//! its own, started at the plugin's value and moved by the plugin's GUI.
//!
//! These are the session halves of `bevy-tutti`'s `mod_plugin` VST2 tests
//! (`a_plugin_gui_edit_becomes_the_knob`,
//! `a_plugins_midi_rewiring_keeps_its_lanes`,
//! `a_plugin_echoing_the_lane_keeps_its_knob`), moved here when the adapter
//! was parked: the `Session` and `Graph` calls stand for the adapter's
//! `ModRoute` entities and `EventSources` component. The plugin is the
//! reference VST2 probe, in process.

use std::time::{Duration, Instant};

use tutti_graph::NodeKey;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_plugin::catalog::PluginId;
use tutti_plugin_types::{Normalized, ParamId};
use tutti_session::{Feeder, PHASES, ParamRef, PhaseCx, PluginLoadRequest, Session, SessionEvent};
use tutti_types::{ChannelLayout, Depth, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);

fn vst2_probe() -> std::path::PathBuf {
    tutti_fixtures::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin")
}

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// A detached stereo session at 48 kHz with the probe loaded in process, on
/// the outputs, its parameter values read.
fn session_with_probe() -> (Session, NodeKey) {
    let mut s = Session::builder()
        .outputs_floor(2)
        .build_detached(RATE)
        .expect("a detached session builds");
    let node = s
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest {
                id: PluginId::from_path(vst2_probe()),
                state: None,
            },
        )
        .expect("the in-process VST2 loads");
    s.graph().set_outputs_from(node);
    s.update();
    assert!(
        s.plugin(node)
            .and_then(|p| p.param_values())
            .is_some_and(|m| m.is_ready()),
        "an in-process plugin's values are read as it opens"
    );
    (s, node)
}

/// One update at `now`, every phase, as a frame of a host.
fn update(s: &mut Session, now: Instant) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
    }
    out
}

/// Updates until `target` has a lane, and its knob.
fn knob_of(s: &mut Session, target: ParamRef) -> f32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let now = Instant::now();
    loop {
        assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
        if let Some((_, lane)) = s.mod_state().lane(target.key()) {
            return lane.knob().expect("a plugin lane has a knob");
        }
        assert!(Instant::now() < deadline, "no lane on {target:?}");
        update(s, now);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Calls the one-argument switch `name` the probe exports, in the image the
/// host loaded (never unloaded: the host still runs it).
fn probe_switch<T>(name: &[u8], arg: T) {
    let path = vst2_probe();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(&path) }.expect("re-open the probe");
    {
        // SAFETY: the probe exports these as `extern "C" fn(T)`.
        let f: libloading::Symbol<extern "C" fn(T)> =
            unsafe { lib.get(name) }.expect("the probe's switch");
        f(arg);
    }
    std::mem::forget(lib);
}

/// **An edit in a plugin's GUI becomes its lane's knob** (doc 014 §8.2, one
/// writer per knob). The reference VST2 plugin, in process, reports
/// `audioMasterAutomate(0, 0.5)` from inside `process` (as a GUI drag does);
/// the host's drain (`editor_idle`) hands it to the parameter callback, and
/// the next update writes it into the lane's knob on parameter 0, leaving
/// parameter 1's knob where the plugin started it.
///
/// Mutations (each run, each failed): the session's `Phase::Plugins` not
/// writing the knob (the mirror alone: parameter 0's knob stays 0); writing
/// every edit to parameter 1's knob (parameter 0's stays 0).
#[test]
fn a_plugin_gui_edit_becomes_the_knob() {
    let (mut s, node) = session_with_probe();
    let (p0, p1) = (ParamRef::plugin(node, 0), ParamRef::plugin(node, 1));
    {
        let mut m = s.mods();
        m.range(p0, 0.0, 1.0);
        m.range(p1, 0.0, 1.0);
        let zero = m.macro_(0.0);
        m.route(zero, p0, Depth(1.0));
        m.route(zero, p1, Depth(1.0));
    }
    // The probe's parameter `i` starts at `i / 16`.
    assert_eq!(knob_of(&mut s, p0), 0.0);
    assert_eq!(knob_of(&mut s, p1), 1.0 / 16.0);

    probe_switch::<u32>(b"tutti_vst2_probe_set_automate_burst\0", 1);
    let mut out = [0.0f32; 2];
    s.render_frame_at(
        &tutti_graph::Transport::new(
            false,
            tutti_types::Bpm::of(120.0),
            tutti_types::Beat(0.0),
            None,
        ),
        &mut out,
    );
    probe_switch::<u32>(b"tutti_vst2_probe_set_automate_burst\0", 0);
    s.plugin(node)
        .expect("the plugin's handle")
        .handle()
        .editor_idle(&main_thread());
    update(&mut s, Instant::now());

    let state = s.mod_state();
    assert_eq!(state.lane(p0.key()).expect("a lane").1.knob(), Some(0.5));
    assert_eq!(
        state.lane(p1.key()).expect("a lane").1.knob(),
        Some(1.0 / 16.0)
    );
    assert_eq!(
        s.plugin(node)
            .and_then(|p| p.param_values())
            .expect("the mirror")
            .get(ParamId::new(0)),
        Some(Some(Normalized::of(0.5))),
        "and the mirror"
    );
}

/// A node with one event output and nothing else: a source of event edges
/// (the clip a MIDI feed names).
#[derive(Clone)]
struct EventSource1;

impl Node for EventSource1 {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A plugin's MIDI wiring changing keeps its parameter lanes.** The
/// reference VST2 plugin (in process) has a lane on parameter 0 (a macro's
/// `+0.25` on a knob of 0), on its one event input. A MIDI source is then
/// declared on that input (`Feeder::Declared`) and taken away again: each
/// rewrite of the port's declared sources leaves the lane's edge where it was
/// (`Graph::set_event_sources` keeps the edges it did not make), so
/// `event_sources` still lists the lane and the plugin still reads `0.25`.
///
/// Mutation (run): `write_event_sources` dropping the edges it did not make
/// (the `others` re-add removed) -> the lane is gone from the port after the
/// first rewrite -> fails.
#[test]
fn a_plugins_midi_rewiring_keeps_its_lanes() {
    let (mut s, node) = session_with_probe();
    let p0 = ParamRef::plugin(node, 0);
    {
        let mut m = s.mods();
        m.range(p0, 0.0, 1.0);
        let offset = m.macro_(1.0);
        m.route(offset, p0, Depth(0.25));
    }
    knob_of(&mut s, p0);
    let lane = s.mod_state().lane(p0.key()).expect("a lane").0;
    let (clip, _) = s.graph().add(tutti_graph::ForkByClone(EventSource1));
    s.update();
    let mut side = s.graph().take_audio_side();
    let value = |s: &Session| {
        s.plugin(node)
            .expect("the handle")
            .handle()
            .any_thread()
            .parameter_value(ParamId::new(0))
            .expect("a value")
    };
    let mut out = vec![Vec::new(), Vec::new()];
    let mut check = |s: &mut Session, want: &[NodeKey], what: &str| {
        let mut want = want.to_vec();
        want.sort();
        for _ in 0..2 {
            update(s, Instant::now());
        }
        side.render(1_024, 128, &mut out);
        let mut sources: Vec<_> = s
            .graph_ref()
            .event_sources(node, 0)
            .into_iter()
            .map(|e| e.node)
            .collect();
        sources.sort();
        assert_eq!(sources, want, "{what}: the port's sources");
        assert_eq!(
            value(s),
            Normalized::of(0.25),
            "{what}: the lane reaches the plugin"
        );
    };
    check(&mut s, &[lane], "before");
    s.graph()
        .set_feed(node, Feeder::Declared, vec![clip.into()]);
    check(&mut s, &[lane, clip], "a MIDI source added");
    s.graph().set_feed(node, Feeder::Declared, Vec::new());
    check(&mut s, &[lane], "the MIDI source removed");
}

/// **A plugin echoing the lane's writes does not ratchet its knob.** The
/// reference VST2 plugin (in process) with its echo on reports every value
/// `setParameter` is given back through `audioMasterAutomate`, from inside the
/// call. Its parameter 0 has a lane: a knob of 0 and a macro's `+0.25`, so
/// the lane writes 0.25. Taken as a GUI edit, that echo would become the knob
/// (0.25), the lane would then write 0.5, and so on to the clamp; the VST2
/// host drops a report made inside its own `setParameter` call
/// (`tutti-vst2-host`'s `host_write`), so after four rounds of render, drain
/// and update the knob is still 0 and the plugin still reads 0.25.
///
/// Mutation (run): `HostState::automate` without the `IN_HOST_WRITE` check ->
/// the knob reads 0.25 after the first round -> fails.
#[test]
fn a_plugin_echoing_the_lane_keeps_its_knob() {
    let (mut s, node) = session_with_probe();
    let p0 = ParamRef::plugin(node, 0);
    {
        let mut m = s.mods();
        m.range(p0, 0.0, 1.0);
        let offset = m.macro_(1.0);
        m.route(offset, p0, Depth(0.25));
    }
    assert_eq!(knob_of(&mut s, p0), 0.0);
    probe_switch::<bool>(b"tutti_vst2_probe_set_echo_set_parameter\0", true);

    let mut side = s.graph().take_audio_side();
    let mut out = vec![Vec::new(), Vec::new()];
    for round in 0..4 {
        side.render(1_024, 128, &mut out);
        let handle = s
            .plugin(node)
            .expect("the plugin's handle")
            .handle()
            .clone();
        handle.editor_idle(&main_thread());
        update(&mut s, Instant::now());
        assert_eq!(
            s.mod_state().lane(p0.key()).expect("a lane").1.knob(),
            Some(0.0),
            "round {round}: the knob"
        );
        assert_eq!(
            handle.any_thread().parameter_value(ParamId::new(0)),
            Some(Normalized::of(0.25)),
            "round {round}: the plugin reads the knob plus the offset"
        );
    }
    probe_switch::<bool>(b"tutti_vst2_probe_set_echo_set_parameter\0", false);
}

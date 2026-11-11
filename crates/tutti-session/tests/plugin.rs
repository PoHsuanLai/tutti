//! Hosted plugins and exports in the session (design 015 §4.3, §9.6),
//! end to end over the reference CLAP plugin in a real `plugin-server`.
//!
//! The render mode a plugin was told is read back through its latency: with
//! `TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY=24` the probe reports 24 frames
//! more while it renders offline, and says so through `clap.latency`, which
//! the host carries to the plugin's latency cell (`PluginControls::latency`).
//! The live plugin is the one these read: an export's fork is a second
//! instance, in its own server.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! it is not a dev-dependency (a cycle through `tutti-plugin`), and without
//! it every test here fails, naming that command. They must fail rather than
//! skip.

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::{clap_probe, plugin_server};
use tutti_export::{ExportConfig, RenderConfig};
use tutti_graph::ForkByClone;
use tutti_graph::NodeKey;
use tutti_plugin::RenderMode;
use tutti_plugin::catalog::PluginId;
use tutti_plugin_host::PluginStatus;
use tutti_session::{
    ExportClock, ExportRequest, ExportSource, ExportTarget, Phase, PhaseCx, PluginLiveness,
    PluginLoadRequest, Session, SessionEvent,
};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 512;
/// What the probe adds to its latency while it renders offline.
const OFFLINE_EXTRA: usize = 24;

/// Sets an environment variable for the plugin servers this process spawns.
fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: nextest runs each test in its own process, and every variable is
    // set before the first server is spawned, from the test's own thread:
    // nothing reads the environment concurrently.
    unsafe { std::env::set_var(key, value) };
}

/// An offline stereo session at 48 kHz, its servers' environment set:
/// the server's path, the offline extra, and `extra`.
fn session(extra: &[(&str, &str)]) -> Session {
    session_built(extra, |b| b)
}

/// [`session`] with the builder `build` shapes.
fn session_built(
    extra: &[(&str, &str)],
    build: impl FnOnce(tutti_session::SessionBuilder) -> tutti_session::SessionBuilder,
) -> Session {
    set_env("TUTTI_PLUGIN_SERVER", plugin_server());
    set_env(
        "TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY",
        OFFLINE_EXTRA.to_string(),
    );
    for (key, value) in extra {
        set_env(key, value);
    }
    build(Session::builder())
        .build_offline(RATE, ChannelLayout::STEREO, Samples(BLOCK))
        .expect("an offline session builds")
}

/// Loads the probe into `s` (blocking), unwired.
fn add_probe(s: &mut Session) -> NodeKey {
    s.add_plugin_blocking(
        &main_thread(),
        PluginLoadRequest {
            id: PluginId::from_path(clap_probe()),
            state: None,
        },
    )
    .expect("the reference CLAP plugin loads")
}

/// The probe's own latency (without the pipeline's chunk), as its node's
/// latency cell holds it.
fn latency(s: &Session, node: NodeKey) -> Samples {
    s.plugin(node)
        .expect("hosted")
        .controls()
        .expect("out of process")
        .latency()
}

/// One control round trip to the probe's server. The server sends the
/// `LatencyChanged` a `render.set` caused after that call's reply, and the
/// bridge thread reads it off the socket only while it waits for the next
/// reply; live, any later control call (a snapshot, a parameter read) carries
/// it, and here this one does.
fn round_trip(s: &Session, node: NodeKey) {
    let handle = s.plugin(node).expect("hosted").handle();
    let _ = handle.current_preset(&main_thread());
}

/// Waits (up to 10 s) for the probe's latency to read `want`: the
/// `clap.latency` callback reaches the cell from the server's bridge thread.
fn await_latency(s: &Session, node: NodeKey, want: Samples, why: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while {
        round_trip(s, node);
        latency(s, node) != want
    } {
        assert!(
            Instant::now() < deadline,
            "{why}: the probe's latency stayed {:?}, want {want:?}",
            latency(s, node)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Holds for 200 ms that the probe's latency reads `want`: what an
/// announcement that did not happen looks like.
fn stays_latency(s: &Session, node: NodeKey, want: Samples, why: &str) {
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        round_trip(s, node);
        assert_eq!(latency(s, node), want, "{why}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A short render of `source` into memory.
fn request(source: ExportSource) -> ExportRequest {
    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: tutti_types::Seconds(0.05),
            ..Default::default()
        },
        ..Default::default()
    };
    ExportRequest::new(source, ExportTarget::Buffers, config, ExportClock::frozen())
}

/// The probe on the outputs, its render mode announced (Realtime) by an
/// update: returns its node and its realtime latency.
fn wired_probe(s: &mut Session) -> (NodeKey, Samples) {
    let node = add_probe(s);
    s.graph().set_outputs_from(node);
    s.update();
    // Told Realtime by that update (it was never told): the latency it
    // reports realtime.
    let base = latency(s, node);
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    (node, base)
}

/// **An export announces Offline before its first block, and Realtime after
/// it.** `export` runs `Phase::RenderMode` before it returns: the probe
/// renders offline (its latency moves by the extra) while the job is still
/// unrun, so before the fork can render a block. `finish_export` releases
/// the token and runs the phase again: back to realtime, with no update.
///
/// Mutations (each run, each failed): `Session::export` not running the
/// phase (the session still says Realtime after `export`); `finish_export`
/// not running it (still Offline after it); `render_mode_phase` recording
/// the mode without calling `set_render_mode` (the latency stays realtime
/// while the job waits).
#[test]
fn an_export_announces_offline_before_its_first_block_and_realtime_after() {
    let mut s = session(&[]);
    let (node, base) = wired_probe(&mut s);
    let offline = Samples(base.0 + OFFLINE_EXTRA);

    let job = s
        .export(request(ExportSource::Mix))
        .expect("the export starts");
    assert_eq!(s.render_mode(), RenderMode::Offline);
    await_latency(&s, node, offline, "told Offline before the job runs");
    assert!(s.export_in_flight());

    let done = job.run();
    let output = s
        .finish_export(done)
        .expect("the export renders")
        .finished();
    assert!(matches!(
        output,
        Some(tutti_session::ExportOutput::Buffers(_))
    ));
    assert!(!s.export_in_flight());
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    await_latency(&s, node, base, "told Realtime by finish_export");
}

/// **A dropped job, or a dropped return, restores Realtime on the next
/// update** (§4.3: the render mode is a level). Neither reaches
/// `finish_export`; each drops the export's token, and the update after
/// finds none alive. Until that update the session still says Offline:
/// nothing is announced from a drop (it may be a worker's).
///
/// Mutations (each run, each failed): announcing Realtime only in
/// `finish_export` (the `Phase::RenderMode` arm of `run_phase` emptied) →
/// the session still says Offline after the update; `render_mode_phase`
/// recording the mode without calling `set_render_mode` → the latency never
/// moves offline.
#[test]
fn a_dropped_export_job_restores_realtime_on_the_next_update() {
    let mut s = session(&[]);
    let (node, base) = wired_probe(&mut s);
    let offline = Samples(base.0 + OFFLINE_EXTRA);

    // The job, dropped unrun (a cancelled task).
    let job = s.export(request(ExportSource::Mix)).expect("starts");
    await_latency(&s, node, offline, "the job's export is in flight");
    drop(job);
    assert_eq!(
        s.render_mode(),
        RenderMode::Offline,
        "nothing told on a drop"
    );
    s.update();
    assert!(!s.export_in_flight());
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    await_latency(&s, node, base, "the update after a dropped job");

    // Its return, dropped unfinished (a worker that ran it, then went).
    let job = s.export(request(ExportSource::Mix)).expect("starts again");
    await_latency(&s, node, offline, "the second export is in flight");
    let done = std::thread::spawn(move || job.run()).join().unwrap();
    assert!(s.export_in_flight(), "the return holds the token");
    drop(done);
    s.update();
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    await_latency(&s, node, base, "the update after a dropped return");
}

/// **A plugin added mid-export is told Offline** on its first update:
/// `needs_announcement`'s `!told` half, end to end. The mode has not changed
/// since the export announced it, so only "never told" reaches the new
/// plugin.
///
/// Mutation (run, failed): `render_mode_phase` asking
/// `needs_announcement(changed, true)` (every plugin counted as told) → the
/// late plugin keeps its realtime latency.
#[test]
fn a_plugin_added_mid_export_is_told_offline() {
    let mut s = session(&[]);
    let (dc, _) = s.graph().insert(tutti_nodes::testing::Const::mono(0.0));
    s.graph().set_outputs_from(dc);
    s.update();
    let job = s.export(request(ExportSource::Mix)).expect("starts");
    assert_eq!(s.render_mode(), RenderMode::Offline);

    let late = add_probe(&mut s);
    let base = latency(&s, late);
    stays_latency(&s, late, base, "not told before its first update");
    s.update();
    await_latency(
        &s,
        late,
        Samples(base.0 + OFFLINE_EXTRA),
        "a plugin added mid-export, after its first update",
    );

    let out = s.finish_export(job.run()).expect("renders");
    assert!(out.finished().is_some(), "not cancelled");
    await_latency(&s, late, base, "told Realtime once the export ends");
}

/// **With recovery off, a plugin dead for the window is removed, and reported
/// once.** The probe
/// aborts on its first block; once the engine latches the death, the
/// debounce counts unhealthy observations at the `now`s the test passes:
/// `Liveness::DEATHS` of them inside the window (50 ms) are still Failing,
/// with the node in place; the first past the window is Dead, the node is
/// removed and `PluginDied` names it, with the engine's cause; no update
/// after reports it again.
///
/// Mutations (each run, each failed): the phase observing every plugin
/// healthy (never Failing); the `Dead` arm not removing the node (still in
/// the graph); `Graph::remove` keeping the plugin's record, with the phase
/// not skipping a dead one (the record check). "Reported once" is then held
/// by the record's removal: no mutation leaves the record and passes the
/// record check, so the two last asserts guard a regression in that order.
#[test]
fn a_plugin_dead_for_the_window_is_removed_and_reported_once() {
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1")], |b| {
        b.plugin_recovery(None)
    });
    let (node, _) = wired_probe(&mut s);

    // Render until the engine latches the death (the server aborted on its
    // first block; the bridge notices on a later one).
    let mut out = vec![0.0f32; BLOCK * 2];
    let deadline = Instant::now() + Duration::from_secs(10);
    while matches!(
        s.plugin(node).unwrap().handle().status(),
        PluginStatus::Alive
    ) {
        assert!(Instant::now() < deadline, "the probe never died");
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
        std::thread::sleep(Duration::from_millis(5));
    }

    let t0 = Instant::now();
    let plugins_at = |s: &mut Session, ms: u64| {
        let mut out = Vec::new();
        s.run_phase(
            Phase::Plugins,
            PhaseCx::at(t0 + Duration::from_millis(ms)),
            &mut out,
        );
        out.into_iter()
            .filter_map(|e| match e {
                SessionEvent::PluginDied { node, cause } => Some((node, cause)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for (i, ms) in [0, 10, 20].into_iter().enumerate() {
        assert!(plugins_at(&mut s, ms).is_empty(), "inside the window");
        assert_eq!(
            s.plugin(node).unwrap().liveness(),
            &PluginLiveness::Failing {
                consecutive: i as u8 + 1
            }
        );
        assert!(s.graph_ref().contains(node), "a failing plugin stays");
    }
    let died = plugins_at(&mut s, 60);
    assert_eq!(died.len(), 1, "one report: {died:?}");
    assert_eq!(died[0].0, node);
    assert!(
        !died[0].1.is_empty(),
        "the engine's cause, not a placeholder"
    );
    assert!(
        !s.graph_ref().contains(node),
        "the dead plugin's node is removed"
    );
    assert!(s.plugin(node).is_none(), "and its record");

    assert!(plugins_at(&mut s, 200).is_empty(), "reported once");
    let again = s
        .update()
        .into_iter()
        .filter(|e| matches!(e, SessionEvent::PluginDied { .. }))
        .count();
    assert_eq!(again, 0, "reported once");
}

/// A node that records the thread it is dropped on: a stand-in for a
/// forked in-process VST2 plugin, whose drop closes its instance and belongs
/// on the main thread. Forked by clone, so the export's copy shares the
/// record.
#[derive(Clone)]
struct DropWitness(Arc<Mutex<Vec<std::thread::ThreadId>>>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(std::thread::current().id());
    }
}

impl tutti_graph::Node for DropWitness {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(&mut self, _: &tutti_graph::Cx<'_>, _: tutti_graph::Io<'_>) -> tutti_graph::Status {
        tutti_graph::Status::Silent
    }
    fn reset(&mut self) {}
}

/// **An export's leftovers drop on the main thread.** The live graph holds a
/// [`DropWitness`] beside the probe, both on the outputs (through a plugin
/// input: the probe's fork comes too), so the export's fork carries a clone
/// of the witness. The job runs on a spawned thread and `finish_export` on
/// this one: every drop of the fork's witness is this thread's.
///
/// Mutation (run, failed): `ExportJob::run` dropping the rendered graph
/// inside `run()` (`graph.take()` dropped before it returns) → the
/// witness's drop is recorded on the worker.
#[test]
fn export_leftovers_drop_on_the_main_thread() {
    let mut s = session(&[]);
    let drops: Arc<Mutex<Vec<std::thread::ThreadId>>> = Arc::default();
    let (witness, _) = s
        .graph()
        .insert(ForkByClone(DropWitness(Arc::clone(&drops))));
    let probe = add_probe(&mut s);
    s.graph()
        .set_source(probe, 0, tutti_session::GraphSource::Node(witness, 0));
    s.graph()
        .set_source(probe, 1, tutti_session::GraphSource::Node(witness, 1));
    s.graph().set_outputs_from(probe);
    s.update();
    drops.lock().unwrap().clear();

    let job = s.export(request(ExportSource::Mix)).expect("starts");
    let done = std::thread::spawn(move || job.run()).join().unwrap();
    let out = s.finish_export(done).expect("renders");
    assert!(out.finished().is_some(), "not cancelled");

    let main = std::thread::current().id();
    let drops = drops.lock().unwrap().clone();
    assert!(!drops.is_empty(), "the export's copy was never dropped");
    assert!(
        drops.iter().all(|&t| t == main),
        "a copy was dropped off the main thread: {drops:?} (main {main:?})"
    );
}

/// **The update frees a hosted plugin's meter cleared while a block held
/// it**: once cleared the node's meter slot follows no project meter, so
/// only the session's sweep of its plugins' meters reaches what the clear
/// retired, on the next update with nothing published again.
///
/// Mutation (run): `Graph::reclaim` leaving out the plugins' meters → the
/// cleared meter is still retired after the update, fails.
#[test]
fn an_update_frees_a_plugin_meter_cleared_while_a_block_held_it() {
    let mut s = session(&[]);
    let probe = add_probe(&mut s);
    s.update();
    let meter = s
        .plugin(probe)
        .expect("hosted")
        .meter()
        .expect("a CLAP node has a meter")
        .clone();
    assert!(meter.has_meter(), "the session installed the project's");

    let mut block = meter.probe_reader();
    {
        let _held = block.lend();
        meter.clear_meter();
        s.update();
        assert_eq!(meter.retired_len(), 1, "held across the clear");
    }
    s.update();
    assert_eq!(meter.retired_len(), 0, "the update freed it");
}

// --- Recovery ---------------------------------------------------------------

/// The reference plugin's applied `Gain` parameter.
fn gain_param(_s: &Session, _node: NodeKey) -> tutti_plugin_types::ParamId {
    tutti_plugin_types::ParamId::new(tutti_clap_test_plugin::params_state::GAIN_PARAM_ID)
}

fn param(s: &Session, node: NodeKey, id: tutti_plugin_types::ParamId) -> Option<f64> {
    s.plugin(node)
        .expect("hosted")
        .handle()
        .any_thread()
        .parameter_value(id)
        .map(|v| v.get())
}

/// Whether a read-back value is the 0.25 that was set (it passes through the
/// plugin's f32 and dB range).
fn is_quarter(v: Option<f64>) -> bool {
    v.is_some_and(|v| (v - 0.25).abs() < 1e-4)
}

/// A marker path for `TUTTI_CLAP_PROBE_ONCE_FILE`, free now: the first server
/// to start fails, replacements do not.
fn once_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().into_owned()
}

/// The reference plugin in `TagPassthrough`: with nothing wired it renders a
/// constant (its tag), so a block of silence is a plugin that is not there.
const PASSTHROUGH: (&str, &str) = ("TUTTI_CLAP_PROBE_RENDER_MODE", "1");

/// The probe applies the parameter writes it is sent (its `Gain`) to its
/// audio, which is also what makes it take them from the host at all.
const APPLY_GAIN: (&str, &str) = ("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");

/// Renders one block and returns whether any sample was non-zero.
fn renders_sound(s: &mut Session) -> bool {
    let mut out = vec![0.0f32; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
    out.iter().any(|&x| x != 0.0)
}

/// A virtual clock for the session's phases, and every event they and the
/// reload jobs reported.
struct Drive {
    now: Instant,
    events: Vec<SessionEvent>,
    /// Run when a reload job is handed out, before it is run.
    on_reload_due: Option<Box<dyn Fn()>>,
}

impl Drive {
    fn new() -> Self {
        Self {
            now: Instant::now(),
            events: Vec::new(),
            on_reload_due: None,
        }
    }

    /// One frame: a block rendered (the server gets real time to act), the
    /// plugins' phase at the clock 20 ms on, and any reload job run and
    /// landed at once, as a host would on a worker.
    fn step(&mut self, s: &mut Session) {
        let _ = renders_sound(s);
        std::thread::sleep(Duration::from_millis(5));
        self.now += Duration::from_millis(20);
        let mut out = Vec::new();
        s.run_phase(Phase::Plugins, PhaseCx::at(self.now), &mut out);
        for event in out {
            if let SessionEvent::PluginReloadDue(job) = event {
                if let Some(check) = &self.on_reload_due {
                    check();
                }
                let landed = s.land_reload(&main_thread(), job.run());
                self.events.extend(landed);
            } else {
                self.events.push(event);
            }
        }
        // The commit that puts a replacement into the executor's plan, and the
        // collect that frees the plan's old unit on this thread.
        s.run_phase(Phase::Commit, PhaseCx::at(self.now), &mut Vec::new());
        s.run_phase(Phase::Collect, PhaseCx::at(self.now), &mut Vec::new());
    }

    /// Steps until an event satisfies `stop` (10 s of wall time at most).
    fn until(&mut self, s: &mut Session, why: &str, stop: impl Fn(&SessionEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.events.iter().any(&stop) {
            assert!(Instant::now() < deadline, "{why}: {:?}", self.events);
            self.step(s);
        }
    }

    fn count(&self, f: impl Fn(&SessionEvent) -> bool) -> usize {
        self.events.iter().filter(|e| f(e)).count()
    }
}

/// **A crashed plugin comes back at its node with its last state.** The
/// probe's first server aborts on its 60th block (a replacement does not: the
/// once-file); before that a parameter is set and a snapshot captured. The
/// death is reported as `PluginReloading` with the node kept, and the load
/// is handed out as a job; landing it puts the replacement at the same node
/// with its wiring (the outputs still read it, so the render is sound again
/// with no re-wiring), the parameter back at the snapshot's value, and
/// `PluginRecovered` saying the state was restored, how old, and that the
/// latency moved by the replacement's extra (the plan's total with it: the
/// commit re-plans PDC).
///
/// Mutations (each run, each failed): `install_reload` not calling
/// `replace_plugin` (the node stays dead: no sound, no `PluginRecovered`);
/// the job built with `state: None` (the parameter reads its default);
/// `install_reload` not putting the snapshot back after the swap, which
/// clears it (no recovery point after the first recovery); the latency pair
/// not read after the swap (`(before, before)`); writing the old mirror's
/// load-time values to the replacement after the swap (the parameter is
/// overwritten again over the blocks that follow).
#[test]
fn a_crashed_plugin_comes_back_at_its_node_with_its_last_state() {
    const EXTRA: usize = 48;
    let once = once_file("recovery-crash.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            APPLY_GAIN,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "60"),
            ("TUTTI_CLAP_PROBE_RESTART_LATENCY", &EXTRA.to_string()),
        ],
        |b| b.snapshot_interval(Duration::from_secs(1)),
    );
    let (node, _) = wired_probe(&mut s);
    // The graph's figure for the node: the probe's own and the pipeline's.
    let base = s.graph_ref().node_latency(node);
    let id = gain_param(&s, node);

    // A value worth restoring, and a snapshot of it.
    s.plugin(node)
        .unwrap()
        .handle()
        .any_thread()
        .set_parameter_value(id, tutti_plugin_types::Normalized::of(0.25));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_quarter(param(&s, node, id)) {
        assert!(
            Instant::now() < deadline,
            "the probe never took the value: {:?}",
            param(&s, node, id)
        );
        // The write rides the next block to the server.
        let _ = renders_sound(&mut s);
        std::thread::sleep(Duration::from_millis(5));
    }
    let t0 = Instant::now();
    for at in [0, 2_000] {
        let mut out = Vec::new();
        s.run_phase(
            Phase::Plugins,
            PhaseCx::at(t0 + Duration::from_millis(at)),
            &mut out,
        );
        for event in out {
            if let SessionEvent::PluginSnapshotDue(job) = event {
                assert!(s.land_snapshot(job.run()), "the snapshot is stored");
            }
        }
    }
    assert!(s.plugin(node).unwrap().snapshot().is_some());
    let total_before = s.latency();

    // Render into the crash, then through the recovery.
    let mut drive = Drive::new();
    drive.now = t0 + Duration::from_secs(3);
    drive.until(&mut s, "recovered", |e| {
        matches!(e, SessionEvent::PluginRecovered { .. })
    });
    let kinds: Vec<&str> = drive
        .events
        .iter()
        .map(|e| match e {
            SessionEvent::PluginReloading { .. } => "reloading",
            SessionEvent::PluginRecovered { .. } => "recovered",
            _ => "other",
        })
        .filter(|k| *k != "other")
        .collect();
    assert_eq!(kinds, ["reloading", "recovered"], "{:?}", drive.events);
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginDied { .. })),
        0
    );
    let Some(SessionEvent::PluginRecovered {
        node: at,
        state,
        latency,
        attempts,
    }) = drive
        .events
        .iter()
        .find(|e| matches!(e, SessionEvent::PluginRecovered { .. }))
    else {
        unreachable!()
    };
    assert_eq!(*at, node, "the same node");
    assert!(
        matches!(state, tutti_session::StateRestore::Restored { .. }),
        "{state:?}"
    );
    assert_eq!(*attempts, 1);
    assert_eq!(latency.0, base);
    assert_eq!(
        latency.1,
        Samples(base.0 + EXTRA),
        "the replacement's latency"
    );
    assert!(s.graph_ref().contains(node));
    assert_eq!(s.plugin(node).unwrap().liveness(), &PluginLiveness::Healthy);
    // The snapshot is still the node's recovery point: a second failure
    // restores it again.
    assert!(s.plugin(node).unwrap().snapshot().is_some());

    // The state came back.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_quarter(param(&s, node, id)) {
        assert!(
            Instant::now() < deadline,
            "the replacement's parameter is {:?}, not the snapshot's 0.25",
            param(&s, node, id)
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // And it stays restored: nothing writes the load-time values over it
    // (a write rides the next blocks to the server, so render some).
    for _ in 0..40 {
        let _ = renders_sound(&mut s);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        is_quarter(param(&s, node, id)),
        "the restored parameter was overwritten: {:?}",
        param(&s, node, id)
    );

    // The commit re-plans PDC for the new latency, and the outputs (never
    // re-wired) read the replacement.
    s.update();
    let total_after = s.latency();
    assert_eq!(
        total_after.0,
        total_before.0 + EXTRA,
        "PDC follows the latency"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !renders_sound(&mut s) {
        assert!(
            Instant::now() < deadline,
            "the recovered plugin renders nothing"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **A plugin that fails every load is given up on, and its node removed.**
/// Every server aborts on its first block, so each reload comes up and dies:
/// two reloads (the policy's budget), then `PluginGaveUp` naming the failures,
/// the node and its record gone, and no `PluginDied`.
///
/// Mutations (each run, each failed): the budget compared past its limit (a
/// third reload); `GiveUp` not removing the node (`contains` still true).
#[test]
fn a_plugin_that_crashes_every_time_is_given_up_on() {
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1")], |b| {
        b.plugin_recovery(Some(tutti_session::RecoveryPolicy {
            max_failures: 2,
            backoff: Duration::from_millis(1),
            ..Default::default()
        }))
    });
    let (node, _) = wired_probe(&mut s);
    let mut drive = Drive::new();
    drive.until(&mut s, "given up", |e| {
        matches!(e, SessionEvent::PluginGaveUp { .. })
    });
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginReloading { .. })),
        2,
        "{:?}",
        drive.events
    );
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginDied { .. })),
        0
    );
    let Some(SessionEvent::PluginGaveUp { node: at, why, .. }) = drive
        .events
        .iter()
        .find(|e| matches!(e, SessionEvent::PluginGaveUp { .. }))
    else {
        unreachable!()
    };
    assert_eq!(*at, node);
    assert_eq!(why.failures, 3);
    assert!(!s.graph_ref().contains(node), "the node is removed");
    assert!(s.plugin(node).is_none(), "and its record");
}

/// **A reload job dropped unrun is a failed attempt**, and the next one is
/// handed out: nothing waits on a job that will never land.
///
/// Mutation (run, failed): the sole-owner check of the job's token removed
/// (no second `PluginReloading`, the first job's slot waits forever).
#[test]
fn a_reload_job_dropped_unrun_is_a_failed_attempt() {
    let once = once_file("recovery-dropped.once");
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
        ],
        |b| {
            b.plugin_recovery(Some(tutti_session::RecoveryPolicy {
                backoff: Duration::from_millis(1),
                ..Default::default()
            }))
        },
    );
    let (node, _) = wired_probe(&mut s);
    let mut now = Instant::now();
    let mut reloadings = 0;
    let mut dropped = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while reloadings < 2 {
        assert!(Instant::now() < deadline, "no second attempt");
        let _ = renders_sound(&mut s);
        std::thread::sleep(Duration::from_millis(5));
        now += Duration::from_millis(20);
        let mut out = Vec::new();
        s.run_phase(Phase::Plugins, PhaseCx::at(now), &mut out);
        for event in out {
            match event {
                SessionEvent::PluginReloading { node: n, .. } => {
                    assert_eq!(n, node);
                    reloadings += 1;
                }
                // Dropped: the host never ran it.
                SessionEvent::PluginReloadDue(job) => {
                    dropped += 1;
                    drop(job);
                }
                _ => {}
            }
        }
    }
    assert_eq!(dropped, 1);
    assert!(matches!(
        s.plugin(node).unwrap().liveness(),
        PluginLiveness::Recovering { .. }
    ));
}

/// Whether the process `pid` is gone (reaped, or dead and awaiting reaping).
#[cfg(target_os = "linux")]
fn process_ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with(['Z', 'X'])),
    }
}

/// A policy that suspects a hang after 16 blocks and 100 ms of the clock the
/// test drives (20 ms a step).
fn quick_hang_policy() -> tutti_session::RecoveryPolicy {
    tutti_session::RecoveryPolicy {
        hang_blocks: 16,
        hang_window: Duration::from_millis(100),
        backoff: Duration::from_millis(1),
        ..Default::default()
    }
}

/// **A plugin that stops answering is ended, then replaced.** The probe's
/// first server parks in `process()` for good (the once-file: a replacement
/// does not): it never crashes, so only the abandoned-block count shows it.
/// The session ends its server (the death reads "stopped answering"), the
/// node is kept, and when the replacement's load is handed out the hung
/// server is already gone: a live server is never left beside its
/// replacement. The replacement renders.
///
/// Mutations (each run, each failed): the hang verdict not calling
/// `terminate` (the hung server is never ended: no `PluginReloading`);
/// `terminate` not latching its cause (the death reads "Bridge process
/// crashed", not "stopped answering"); `terminate` not killing the server
/// (the server is still alive when the load is handed out).
#[cfg(target_os = "linux")]
#[test]
fn a_plugin_that_stops_answering_is_ended_and_replaced() {
    let once = once_file("recovery-hang.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_BLOCK_FROM", "20"),
        ],
        |b| b.plugin_recovery(Some(quick_hang_policy())),
    );
    let (node, _) = wired_probe(&mut s);
    let pid = s
        .plugin(node)
        .unwrap()
        .handle()
        .server_pid()
        .expect("an out-of-process plugin has a server");

    let mut drive = Drive::new();
    drive.on_reload_due = Some(Box::new(move || {
        assert!(
            process_ended(pid),
            "the hung server {pid} was still alive when its replacement was loaded"
        );
    }));
    drive.until(&mut s, "recovered", |e| {
        matches!(e, SessionEvent::PluginRecovered { .. })
    });
    let Some(SessionEvent::PluginReloading { cause, .. }) = drive
        .events
        .iter()
        .find(|e| matches!(e, SessionEvent::PluginReloading { .. }))
    else {
        unreachable!()
    };
    assert!(cause.contains("stopped answering"), "{cause}");
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginDied { .. })),
        0
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !renders_sound(&mut s) {
        assert!(Instant::now() < deadline, "the replacement renders nothing");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **A slow plugin that still answers is not declared hung.** Every block
/// takes the probe 80 ms, far over the bridge's budget for a 512-frame block:
/// blocks are abandoned all the time, and every one's reply comes back late.
/// Abandoned blocks pass the policy's 16 many times over, but replies keep
/// arriving between the readings, so the plugin is left alone for four
/// seconds; both totals did rise (else the test proves nothing).
///
/// Mutation (run, failed): `Channels::note_replied` not counting (no reply
/// is an answer): the abandoned total grows with the replies flat, and the
/// verdict ends the server. (Not counting an owed reply alone survives here:
/// this plugin's replies also reach the bridge as the answer to the next
/// block; the owed path is pinned by `Channels`' own test.)
#[test]
fn a_slow_plugin_that_still_answers_is_not_declared_hung() {
    let mut s = session_built(
        &[PASSTHROUGH, ("TUTTI_CLAP_PROBE_STALL_US", "80000")],
        |b| b.plugin_recovery(Some(quick_hang_policy())),
    );
    let (node, _) = wired_probe(&mut s);
    let mut drive = Drive::new();
    // Four seconds of wall time: at 80 ms a block, with the bridge's budget
    // missed on most, far more than 16 blocks are abandoned over it.
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        drive.step(&mut s);
    }
    let counts = s.plugin(node).unwrap().handle().block_counts();
    assert!(
        counts.abandoned > 16,
        "{counts:?}: the plugin is not slow enough"
    );
    assert!(counts.replies > 0, "{counts:?}: it never answered");
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginReloading { .. })),
        0
    );
    assert_eq!(s.plugin(node).unwrap().liveness(), &PluginLiveness::Healthy);
}

/// **A replacement with other ports is a failed attempt, and the graph stays
/// editable.** The replacement server reports an extra aux bus (an updated
/// binary whose buses changed). The node's edges stay as they are, so
/// swapping it in would leave the topology invalid and refuse every later
/// commit while the session called the plugin healthy. It is refused before
/// the swap: the next attempt is scheduled (the same refusal again, until
/// the budget is spent), the reason reaches `PluginGaveUp`, and an edit made
/// meanwhile commits.
///
/// Mutation (run, failed): the port comparison removed from
/// `install_reload` (the swap happens: the edit stays uncommitted, and the
/// plugin is reported recovered).
#[test]
fn a_replacement_with_other_ports_is_refused_and_the_graph_stays_editable() {
    let once = once_file("recovery-ports.once");
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
            ("TUTTI_CLAP_PROBE_RESTART_PORT_LAYOUT", "1"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1))
                .plugin_recovery(Some(tutti_session::RecoveryPolicy {
                    max_failures: 2,
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                }))
        },
    );
    let (node, _) = wired_probe(&mut s);
    snapshot_now(&mut s, node);
    let kept = s.plugin(node).unwrap().snapshot().unwrap().to_vec();
    let mut drive = Drive::new();
    drive.until(&mut s, "a second attempt", |e| {
        matches!(e, SessionEvent::PluginReloading { attempt: 2, .. })
    });
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginRecovered { .. })),
        0,
        "{:?}",
        drive.events
    );
    // An edit made now commits.
    let (extra, _) = s.graph().insert(ForkByClone(DropWitness(Arc::default())));
    assert!(s.graph_ref().is_edited());
    s.update();
    assert!(
        !s.graph_ref().is_edited(),
        "the commit was refused: the topology is invalid"
    );
    assert!(s.graph_ref().contains(extra));

    drive.until(&mut s, "given up", |e| {
        matches!(e, SessionEvent::PluginGaveUp { .. })
    });
    let Some(SessionEvent::PluginGaveUp { why, snapshot, .. }) = drive
        .events
        .iter()
        .find(|e| matches!(e, SessionEvent::PluginGaveUp { .. }))
    else {
        unreachable!()
    };
    assert!(why.last.contains("ports differ"), "{why}");
    assert_eq!(
        snapshot.as_deref(),
        Some(kept.as_slice()),
        "the user's last state goes out with the event"
    );
    assert!(!s.graph_ref().contains(node));
    s.update();
    assert!(!s.graph_ref().is_edited());
}

/// Captures a snapshot of `node` at once, as a host's worker would (the
/// session's snapshot cadence is 1 s here, driven by the virtual clock).
fn snapshot_now(s: &mut Session, node: NodeKey) {
    let t0 = Instant::now();
    for at in [0, 2_000] {
        let mut out = Vec::new();
        s.run_phase(
            Phase::Plugins,
            PhaseCx::at(t0 + Duration::from_millis(at)),
            &mut out,
        );
        for event in out {
            if let SessionEvent::PluginSnapshotDue(job) = event {
                assert!(s.land_snapshot(job.run()), "the snapshot is stored");
            }
        }
    }
    assert!(s.plugin(node).unwrap().snapshot().is_some());
}

fn recovered_state(drive: &Drive) -> Vec<tutti_session::StateRestore> {
    drive
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::PluginRecovered { state, .. } => Some(state.clone()),
            _ => None,
        })
        .collect()
}

/// **A snapshot that kills the plugin loading it is not tried again.** The
/// first server dies; every replacement dies when it is handed a state. The
/// first attempt (with the snapshot) fails before the plugin is installed,
/// so it is never reported recovered; the second goes without the state,
/// comes up, and says so: `StateRestore::Withheld`, the snapshot gone from
/// the record.
///
/// Mutations (each run, each failed): `withhold_snapshot` not clearing the
/// snapshot (every attempt carries it: budget spent, `PluginGaveUp`, no
/// recovery); the dead-on-arrival check removed from `install_reload` (the
/// dead replacement is installed and reported `Restored`); the withheld
/// bytes not kept (`withheld_snapshot` is `None`); `land_snapshot` replacing
/// them (the default-state snapshot overwrites them).
#[test]
fn a_snapshot_that_kills_the_load_is_withheld_from_the_next_attempt() {
    let once = once_file("recovery-withheld.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            APPLY_GAIN,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "40"),
            ("TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD", "1"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1))
                .plugin_recovery(Some(tutti_session::RecoveryPolicy {
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                }))
        },
    );
    let (node, _) = wired_probe(&mut s);
    let id = gain_param(&s, node);
    // A state that is not the plugin's default.
    s.plugin(node)
        .unwrap()
        .handle()
        .any_thread()
        .set_parameter_value(id, tutti_plugin_types::Normalized::of(0.25));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_quarter(param(&s, node, id)) {
        assert!(Instant::now() < deadline, "the probe never took the value");
        let _ = renders_sound(&mut s);
        std::thread::sleep(Duration::from_millis(5));
    }
    snapshot_now(&mut s, node);
    let users = s.plugin(node).unwrap().snapshot().unwrap().to_vec();
    let mut drive = Drive::new();
    drive.until(&mut s, "recovered", |e| {
        matches!(e, SessionEvent::PluginRecovered { .. })
    });
    let states = recovered_state(&drive);
    assert!(
        matches!(
            states.as_slice(),
            [tutti_session::StateRestore::Withheld { .. }]
        ),
        "{states:?} in {:?}",
        drive.events
    );
    assert_eq!(
        drive.count(|e| matches!(e, SessionEvent::PluginReloading { .. })),
        2,
        "the attempt with the snapshot failed first"
    );
    assert!(s.plugin(node).unwrap().snapshot().is_none());
    assert_eq!(s.plugin(node).unwrap().liveness(), &PluginLiveness::Healthy);
    // The user's state is kept for the host, not thrown away...
    assert_eq!(
        s.plugin(node).unwrap().withheld_snapshot(),
        Some(users.as_slice())
    );
    // ...and the snapshots taken since, of a plugin at its defaults, do not
    // replace it.
    snapshot_now(&mut s, node);
    let fresh = s.plugin(node).unwrap().snapshot().expect("a new snapshot");
    assert_ne!(fresh, users.as_slice(), "the plugin is at its defaults");
    assert_eq!(
        s.plugin(node).unwrap().withheld_snapshot(),
        Some(users.as_slice()),
        "a default-state snapshot replaced the withheld one"
    );
}

/// **A plugin that dies soon after restoring a snapshot loses it for the next
/// load.** Every server aborts on its third block. The first replacement
/// takes the snapshot (`Restored`) and dies within the probation, so the
/// next load goes without it (`Withheld`); a death outside the probation
/// would have used the snapshot again.
///
/// Mutation (run, failed): `probation_until` never set on a restore (the
/// second replacement is `Restored` again, off the same snapshot).
#[test]
fn a_plugin_that_dies_soon_after_restoring_loses_the_snapshot() {
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "3")], |b| {
        b.snapshot_interval(Duration::from_secs(1))
            .plugin_recovery(Some(tutti_session::RecoveryPolicy {
                max_failures: 5,
                backoff: Duration::from_millis(1),
                ..Default::default()
            }))
    });
    let (node, _) = wired_probe(&mut s);
    snapshot_now(&mut s, node);
    let mut drive = Drive::new();
    drive.until(&mut s, "a recovery without the snapshot", |e| {
        matches!(
            e,
            SessionEvent::PluginRecovered {
                state: tutti_session::StateRestore::Withheld { .. },
                ..
            }
        )
    });
    let states = recovered_state(&drive);
    assert!(
        matches!(
            states.as_slice(),
            [
                tutti_session::StateRestore::Restored { .. },
                tutti_session::StateRestore::Withheld { .. }
            ]
        ),
        "{states:?}"
    );
}

/// **An export started while the plugin is being replaced is refused.** The
/// dead plugin cannot hand over its state for the fork, so the gather names
/// the failure; the session and its render mode are left as they were, and an
/// export started after the recovery works.
///
/// Not mutation-tested: it pins what the fork of a dead plugin does today.
#[test]
fn an_export_during_the_recovery_window_is_refused() {
    let once = once_file("recovery-export.once");
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
        ],
        |b| {
            b.plugin_recovery(Some(tutti_session::RecoveryPolicy {
                backoff: Duration::from_secs(3_600),
                ..Default::default()
            }))
        },
    );
    let (node, _) = wired_probe(&mut s);
    // Dead and waiting for a backoff that does not end.
    let mut drive = Drive::new();
    drive.until(&mut s, "reloading", |e| {
        matches!(e, SessionEvent::PluginReloading { .. })
    });
    assert!(matches!(
        s.plugin(node).unwrap().liveness(),
        PluginLiveness::Recovering { .. }
    ));
    let refused = s.export(request(ExportSource::Mix));
    let refusal = format!("{:?}", refused.as_ref().err());
    assert!(
        refusal.contains("SaveState(PluginCrashed)"),
        "the fork of a dead plugin is refused: {refusal}"
    );
    assert!(!s.export_in_flight());
    assert_eq!(s.render_mode(), RenderMode::Realtime);
}

/// Copies stereo through, and cancels the export it renders in when it
/// processes its second block. Forked by clone: the fork shares the handle.
#[derive(Clone)]
struct CancelOnSecondBlock {
    blocks: Arc<std::sync::atomic::AtomicUsize>,
    handle: Arc<std::sync::OnceLock<tutti_session::ExportHandle>>,
}

impl tutti_graph::Node for CancelOnSecondBlock {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            outs.get(c).copy_from_slice(ins.get(c));
        }
        if self
            .blocks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
            == 2
        {
            self.handle.get().expect("the handle was set").cancel();
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// The plugin servers this process has running: its children whose command
/// line names the server binary. `None` off Linux, where `/proc` is not
/// there to read.
fn servers() -> Option<usize> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let me = std::process::id().to_string();
    let server = plugin_server();
    let server = server.file_name().unwrap().to_string_lossy().into_owned();
    let mut n = 0;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let dir = entry.path();
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid ...`: comm may hold spaces, so split after its `)`.
        let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
        if after.split_whitespace().nth(1) != Some(me.as_str()) {
            continue;
        }
        let cmd = std::fs::read(dir.join("cmdline")).unwrap_or_default();
        if String::from_utf8_lossy(&cmd).contains(&server) {
            n += 1;
        }
    }
    Some(n)
}

/// **A cancelled export over a hosted plugin's fork** stops at its second
/// block, reports `Cancelled`, releases the token on `finish_export`, takes
/// its forked `plugin-server` with it (the fork dies with the rendered graph,
/// on the main thread), and the next export, forking a fresh server, succeeds.
/// The server count is read from `/proc`, so that part is Linux only.
///
/// Mutation (run): `ExportHandle::tick` ignoring the stop request -> the
/// render runs to its end and the outcome is `Finished`; fails.
/// Mutation (run): `finish_export_unannounced` forgetting the rendered graph
/// -> the forked server is still running after the finish; fails.
#[test]
fn a_cancelled_export_frees_the_token_and_the_forked_server() {
    use std::sync::atomic::AtomicUsize;
    let mut s = session(&[]);
    let probe = add_probe(&mut s);
    let blocks = Arc::new(AtomicUsize::new(0));
    let handle = Arc::new(std::sync::OnceLock::new());
    let (cancel, _) = s.graph().add(ForkByClone(CancelOnSecondBlock {
        blocks: Arc::clone(&blocks),
        handle: Arc::clone(&handle),
    }));
    s.graph()
        .set_source(cancel, 0, tutti_session::GraphSource::Node(probe, 0));
    s.graph()
        .set_source(cancel, 1, tutti_session::GraphSource::Node(probe, 1));
    s.graph().set_outputs_from(cancel);
    s.update();
    let live = servers();

    let mut long = request(ExportSource::Mix);
    long.config.render.duration_seconds = tutti_types::Seconds(5.0);
    let job = s.export(long).expect("starts");
    handle.set(job.handle()).ok();
    let rendered = job.run();
    assert!(
        blocks.load(std::sync::atomic::Ordering::SeqCst) < 20,
        "the render stopped at the cancel, not at its end of 235 blocks"
    );
    assert!(s.export_in_flight(), "the token lives until the finish");
    if let Some(live) = live {
        assert_eq!(servers(), Some(live + 1), "the export's fork has a server");
    }
    let out = s.finish_export(rendered).expect("a cancel is not an error");
    assert!(out.finished().is_none(), "cancelled");
    assert!(!s.export_in_flight(), "the token is released");
    if let Some(live) = live {
        let deadline = Instant::now() + Duration::from_secs(10);
        while servers() != Some(live) {
            assert!(Instant::now() < deadline, "the fork's server never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let next = s
        .export_blocking(request(ExportSource::Mix))
        .expect("the next export is not refused");
    assert!(next.finished().is_some(), "and renders to its end");
}

/// **A replacement that refused the snapshot is not blamed for its death.**
/// Every server aborts on its third block and refuses any state. The first
/// replacement is `Refused` (it never restored anything), and its death is
/// not the snapshot's doing: the snapshot is offered again (`Refused`
/// again), none is withheld.
///
/// Mutation (run, failed): `probation_until` also set on a refusal (the
/// second replacement is `Withheld` and the snapshot is gone).
#[test]
fn a_death_after_a_refused_restore_does_not_withhold_the_snapshot() {
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "3"),
            ("TUTTI_CLAP_PROBE_REFUSE_STATE_LOAD", "1"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1))
                .plugin_recovery(Some(tutti_session::RecoveryPolicy {
                    max_failures: 5,
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                }))
        },
    );
    let (node, _) = wired_probe(&mut s);
    snapshot_now(&mut s, node);
    let mut drive = Drive::new();
    drive.until(&mut s, "two recoveries", |e| {
        matches!(e, SessionEvent::PluginRecovered { attempts: 2, .. })
    });
    let states = recovered_state(&drive);
    assert!(
        states
            .iter()
            .all(|s| matches!(s, tutti_session::StateRestore::Refused { .. })),
        "{states:?}"
    );
    assert!(s.plugin(node).unwrap().withheld_snapshot().is_none());
    assert!(s.plugin(node).unwrap().snapshot().is_some());
}

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

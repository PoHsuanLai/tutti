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
use tutti_core::{AudioNode, ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_export::{ExportConfig, RenderConfig};
use tutti_graph::ForkByClone;
use tutti_plugin::catalog::PluginId;
use tutti_plugin::handles::PluginStatus;
use tutti_plugin::RenderMode;
use tutti_session::{
    ExportClock, ExportRequest, ExportSource, ExportTarget, Phase, PhaseCx, PluginLiveness,
    PluginLoadRequest, Session, SessionEvent,
};

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
    set_env("TUTTI_PLUGIN_SERVER", plugin_server());
    set_env(
        "TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY",
        OFFLINE_EXTRA.to_string(),
    );
    for (key, value) in extra {
        set_env(key, value);
    }
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, Samples(BLOCK))
        .expect("an offline session builds")
}

/// Loads the probe into `s` (blocking), unwired.
fn add_probe(s: &mut Session) -> AudioNode {
    s.add_plugin_blocking(PluginLoadRequest {
        id: PluginId::from_path(clap_probe()),
        state: None,
    })
    .expect("the reference CLAP plugin loads")
}

/// The probe's own latency (without the pipeline's chunk), as its node's
/// latency cell holds it.
fn latency(s: &Session, node: AudioNode) -> Samples {
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
fn round_trip(s: &Session, node: AudioNode) {
    let handle = s.plugin(node).expect("hosted").handle();
    let _ = handle
        .presets()
        .expect("the subprocess backend has a preset route")
        .current_preset();
}

/// Waits (up to 10 s) for the probe's latency to read `want`: the
/// `clap.latency` callback reaches the cell from the server's bridge thread.
fn await_latency(s: &Session, node: AudioNode, want: Samples, why: &str) {
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
fn stays_latency(s: &Session, node: AudioNode, want: Samples, why: &str) {
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
            duration_seconds: 0.05,
            ..Default::default()
        },
        ..Default::default()
    };
    ExportRequest::new(source, ExportTarget::Buffers, config, ExportClock::frozen())
}

/// The probe on the outputs, its render mode announced (Realtime) by an
/// update: returns its node and its realtime latency.
fn wired_probe(s: &mut Session) -> (AudioNode, Samples) {
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
    let output = s.finish_export(done).expect("the export renders");
    assert!(matches!(output, tutti_session::ExportOutput::Buffers(_)));
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

    s.finish_export(job.run()).expect("renders");
    await_latency(&s, late, base, "told Realtime once the export ends");
}

/// **A plugin dead for the window is removed, and reported once.** The probe
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
    let mut s = session(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1")]);
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
    s.finish_export(done).expect("renders");

    let main = std::thread::current().id();
    let drops = drops.lock().unwrap().clone();
    assert!(!drops.is_empty(), "the export's copy was never dropped");
    assert!(
        drops.iter().all(|&t| t == main),
        "a copy was dropped off the main thread: {drops:?} (main {main:?})"
    );
}

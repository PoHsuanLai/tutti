//! Modulation lanes on a hosted plugin's parameters, through the session
//! (doc 014 §8.2, §12.5): a plugin target is a lane on the plugin's event
//! input, its knob its own and started at the plugin's value, and a lane
//! outlives the plugin behind it being replaced (a crash's recovery, a
//! crossfade) and an export is the live render.
//!
//! These are the session halves of `bevy-tutti`'s `plugin_recovery`
//! (`a_modulation_lane_on_the_plugin_survives_its_replacement`),
//! `plugin_capture` (`a_crossfaded_plugin_is_bound_again`: the lane and
//! the parameter read) and `mod_plugin`
//! (`an_export_is_the_live_render_on_a_clap_gain`), moved here when the
//! adapter was parked. A detached session (no engine, no device) stands for
//! the adapter's headless graph.
//!
//! The reference plugin is the CLAP probe through a real `plugin-server`.
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! without it every test here fails, naming that command. They must fail
//! rather than skip.

mod support;

use std::time::{Duration, Instant};

use support::{clap_probe, plugin_server};
use tutti_graph::NodeKey;
use tutti_modulation::Lfo;
use tutti_plugin::catalog::PluginId;
use tutti_plugin::handles::PluginClient;
use tutti_plugin_types::{Normalized, ParamId};
use tutti_session::{
    Crossfade, PHASES, ParamRef, PhaseCx, PluginLoadRequest, Session, SessionEvent,
};
use tutti_types::{Depth, Hz, SampleRate, Seconds};

const RATE: SampleRate = SampleRate(48_000.0);
/// The plugin's pipeline chunk in a detached session: its `MaxBlock`
/// (`LIVE_MAX_BLOCK`).
const CHUNK: usize = 1_024;
/// The probe's gain parameter, `-60..=12` dB; its default (0 dB) is 60/72 of
/// the range.
const GAIN: u32 = 77;

/// SAFETY: nextest runs each test in its own process, and every variable is
/// set before the first server is spawned, from the test's own thread.
fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    unsafe { std::env::set_var(key, value) };
}

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// A detached stereo session at 48 kHz, the probe's server in the
/// environment.
fn detached() -> Session {
    set_env("TUTTI_PLUGIN_SERVER", plugin_server());
    Session::builder()
        .outputs_floor(2)
        .build_detached(RATE)
        .expect("a detached session builds")
}

/// Loads the probe into `s` and puts it on the outputs.
fn add_probe(s: &mut Session) -> NodeKey {
    let node = s
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest {
                id: PluginId::from_path(clap_probe()),
                state: None,
            },
        )
        .expect("the reference CLAP plugin loads");
    s.graph().set_outputs_from(node);
    node
}

/// A virtual clock for the session's phases, and the events the update
/// events asked the host for run as a host would: a parameter read, a
/// reload.
struct Drive {
    now: Instant,
    told: Vec<&'static str>,
}

impl Drive {
    fn new() -> Self {
        Self {
            now: Instant::now(),
            told: Vec::new(),
        }
    }

    /// One frame: every phase at the clock 20 ms on, the jobs it hands out run
    /// and landed at once.
    fn update(&mut self, s: &mut Session) {
        self.now += Duration::from_millis(20);
        let mut out = Vec::new();
        for &phase in PHASES {
            s.run_phase(phase, PhaseCx::at(self.now), &mut out);
        }
        for event in out {
            match event {
                SessionEvent::PluginParamsDue(job) => {
                    assert!(s.land_params(job.run()), "the read lands");
                }
                SessionEvent::PluginReloadDue(job) => {
                    let landed = s.land_reload(&main_thread(), job.run());
                    self.told_of(&landed);
                }
                other => self.told_of(&[other]),
            }
        }
    }

    fn told_of(&mut self, events: &[SessionEvent]) {
        for event in events {
            match event {
                SessionEvent::PluginReloading { .. } => self.told.push("reloading"),
                SessionEvent::PluginRecovered { .. } => self.told.push("recovered"),
                _ => {}
            }
        }
    }

    /// [`update`](Self::update) and a plugin chunk's worth of frames rendered
    /// from the graph (the pipeline sends the server one block per chunk).
    /// Whether any was non-zero.
    fn frame(&mut self, s: &mut Session) -> bool {
        self.update(s);
        let mut heard = false;
        for _ in 0..CHUNK {
            let mut out = [0.0f32; 2];
            s.render_frame(&mut out);
            heard |= out.iter().any(|&x| x != 0.0);
        }
        std::thread::sleep(Duration::from_millis(5));
        heard
    }
}

/// Updates until the plugin at `node` has its parameter values read.
fn wait_for_values(drive: &mut Drive, s: &mut Session, node: NodeKey) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s
        .plugin(node)
        .and_then(|p| p.param_values())
        .is_some_and(|m| m.is_ready())
    {
        assert!(
            Instant::now() < deadline,
            "the plugin's parameter read never landed"
        );
        drive.update(s);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The node of `target`'s lane, waiting for it while the declarations apply.
fn lane_of(drive: &mut Drive, s: &mut Session, target: ParamRef) -> NodeKey {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
        if let Some((key, _)) = s.mod_state().lane(target.key()) {
            return key;
        }
        assert!(Instant::now() < deadline, "no lane on {target:?}");
        drive.update(s);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn gain_value(s: &Session, node: NodeKey) -> Option<Normalized> {
    s.plugin(node)
        .expect("hosted")
        .handle()
        .any_thread()
        .parameter_value(ParamId::new(GAIN))
}

/// **A modulation lane on the plugin survives the replacement.** The lane
/// (a macro of -0.25 on the gain parameter, id 77) is a node of its own
/// feeding the plugin's event input. The recovery's replacement takes the
/// plugin's node, so the declarations still resolve; the session rebuilds the
/// lane for the new unit (the node's generation moved), as it does for a
/// plugin swapped by hand, and the lane's value reaches the replacement,
/// which loaded at its default (no snapshot was due): the gain reads default
/// minus 0.25 again.
///
/// The reference plugin aborts on its 8th block in the first server only
/// (`TUTTI_CLAP_PROBE_ONCE_FILE`); the second server is the replacement.
///
/// Mutation (run, failed): `install_reload` building the replacement without
/// the old plugin's mirror (`hosted.mirror = None` for the new one): the lane
/// has no knob to start from and never reaches the replacement. (Making the
/// session's lane bookkeeping ignore the generation move, in
/// `Modulation::apply`'s `taken_lanes`, does not fail this: the lane is also
/// rebuilt through the changed bind, so that path is not pinned here.)
#[test]
fn a_modulation_lane_on_the_plugin_survives_its_replacement() {
    let once = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("session-recovery-lane.once");
    let _ = std::fs::remove_file(&once);
    set_env("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
    set_env("TUTTI_CLAP_PROBE_ONCE_FILE", &once);
    set_env("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "8");
    set_env("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);

    let gain = ParamRef::plugin(node, GAIN);
    {
        let mut m = s.mods();
        m.range(gain, 0.0, 1.0);
        let source = m.macro_(1.0);
        m.route(source, gain, Depth(-0.25));
    }
    lane_of(&mut drive, &mut s, gain);
    // The default is 60/72 of the range; the lane moves it down a quarter.
    let lowered = Normalized::clamped(60.0 / 72.0 - 0.25);
    let near = |v: Option<Normalized>| v.is_some_and(|v| (v.get() - lowered.get()).abs() < 1e-3);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node)) {
        assert!(
            Instant::now() < deadline,
            "the lane never reached the plugin: {:?}",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }

    // Through the crash (block 8) and the recovery.
    let deadline = Instant::now() + Duration::from_secs(30);
    while drive.told != ["reloading", "recovered"] {
        assert!(Instant::now() < deadline, "told {:?}", drive.told);
        drive.frame(&mut s);
    }
    // The session rebuilds a lane whose target unit was replaced (the node's
    // generation moved), from the same declarations. (The rebuilt lane may
    // reuse the old lane's key, so the proof is what it does: the new plugin
    // loaded at its default, and the gain is lowered again only by a lane
    // that reaches it.)
    let rebuilt = lane_of(&mut drive, &mut s, gain);
    assert!(s.graph_ref().contains(rebuilt), "the lane is in the graph");
    assert_eq!(
        s.graph_ref().event_sources(node, 0),
        vec![rebuilt.into()],
        "and feeds the replacement's event input"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node)) {
        assert!(
            Instant::now() < deadline,
            "the lane does not reach the replacement: {:?}",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }
}

/// **A crossfaded plugin keeps its gain's lane, and the lane follows the
/// plugin out.** A lane on the probe's gain feeds its event input. The
/// incoming plugin's parameter mirror is read by a job (`PluginParamsDue`,
/// run and landed by the host, never on the update's thread), and until it
/// lands a lane on the plugin would wait; then the lane feeds whichever plugin
/// the node holds now, the incoming one (a crossfade keeps the key and its
/// edges). When the node goes, the lane's target no longer resolves and the
/// lane leaves the graph with it.
///
/// Mutations (each run, each failed): the host dropping the `PluginParamsDue`
/// job instead of running and landing it (the arm in `Drive::update`): the
/// mirror is never ready and the wait fails; `Graph::event_sources` listing no
/// edges (`.filter(|_| false)`): the precondition fails; `Modulation::apply`
/// keeping a lane whose target no longer resolves
/// (`binds.get(t).is_none_or(..)` as `is_some_and`): the lane stays after the
/// plugin goes and the last assertion fails.
#[test]
fn a_crossfaded_plugin_keeps_its_lane_and_a_removed_one_drops_it() {
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::plugin(node, GAIN);
    {
        let mut m = s.mods();
        m.range(gain, 0.0, 1.0);
        let offset = m.macro_(0.0);
        m.route(offset, gain, Depth(1.0));
    }
    let outgoing_lane = lane_of(&mut drive, &mut s, gain);
    assert_eq!(
        s.graph_ref().event_sources(node, 0),
        vec![outgoing_lane.into()],
        "precondition: the lane feeds the outgoing plugin"
    );

    let incoming = PluginClient::new(tutti_plugin::BridgeConfig::default(), clap_probe(), RATE)
        .expect("load a second instance of the reference plugin");
    let landed = s
        .graph()
        .crossfade_plugin(
            node,
            Box::new(incoming),
            Seconds(0.005),
            tutti_graph::CrossfadeCurve::EqualAmplitude,
        )
        .expect("the crossfade is accepted");
    assert!(matches!(landed, Crossfade::Landed(())), "nothing waits");
    // Rebuilt by the crossfade: not ready until the job is run and landed.
    assert!(
        !s.plugin(node)
            .and_then(|p| p.param_values())
            .is_some_and(|m| m.is_ready()),
        "the incoming plugin's mirror starts unread"
    );
    wait_for_values(&mut drive, &mut s, node);

    let lane = lane_of(&mut drive, &mut s, gain);
    drive.update(&mut s);
    assert_eq!(
        s.graph_ref().event_sources(node, 0),
        vec![lane.into()],
        "the gain's lane feeds the incoming plugin"
    );
    assert!(s.graph_ref().contains(lane), "while its plugin is");

    s.remove(node);
    drive.update(&mut s);
    drive.update(&mut s);
    assert!(
        !s.graph_ref().contains(lane),
        "the lane leaves with its plugin"
    );
}

/// **Export == live on a plugin's parameter.** The reference CLAP plugin
/// renders a constant tag scaled by its gain (id 77, applied sample-accurately
/// at each parameter event's frame); a 3 Hz sine at depth 0.1 modulates the
/// gain's lane, whose knob starts at the plugin's own value (0 dB: 60/72 of
/// `-60..=12`). Live renders rolling 1024-frame blocks from the play start,
/// each once the server answered the chunk before it
/// (`PluginControls::chunk_in_flight_answered`), and fails by name if one was
/// played unanswered (`missed_chunks`); the export forks the graph (the
/// plugin by state transfer, the lane by clone) and renders the same span at
/// its 1024-frame chunk. Equal bit for bit.
///
/// Mutations (each run, each failed): the knob seeded at the range's bottom
/// (the session's `ModHostOver` answering `Some(Some(0.0))` for a plugin
/// parameter: the knob check); a lane handing the editor no fork source
/// (`fork: None` in `ControlLaneNode::into_parts`: the export is refused as
/// not forkable); a lane's fork starting its knob at 0, not the live knob
/// (`fork_fresh`: the export's gain differs from live); the wait for the
/// answer removed (the blocks rendered back to back: "live block 1 late or
/// missed", not a silent-vs-signal compare).
#[test]
fn an_export_is_the_live_render_on_a_clap_gain() {
    use std::sync::Arc;
    use tutti_export::{Outcome, Produced};
    use tutti_session::{Bounce, ExportClock, Latency, Range, Tail};
    use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_types::{Beat, Bpm, ChannelLayout};

    const BLOCK: usize = 1_024;
    const BLOCKS: usize = 24;

    set_env("TUTTI_CLAP_PROBE_RENDER_MODE", "2");
    set_env("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);

    let gain = ParamRef::plugin(node, GAIN);
    {
        let mut m = s.mods();
        m.range(gain, 0.0, 1.0);
        let lfo = m.lfo(Lfo::sine().hz(Hz(3.0)));
        m.route(lfo, gain, Depth(0.1));
    }
    lane_of(&mut drive, &mut s, gain);
    let knob = s
        .mod_state()
        .lane(gain.key())
        .and_then(|(_, lane)| lane.knob())
        .expect("a plugin lane has a knob");
    assert!(
        (knob - 60.0 / 72.0).abs() < 1e-4,
        "the knob starts at the plugin's 0 dB: {knob}"
    );
    for _ in 0..3 {
        drive.update(&mut s);
    }

    let total = BLOCK * BLOCKS;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: Bpm::of(120.0),
        sample_rate: RATE,
        loop_range: None,
    }));
    let config = Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(
            Seconds(0.0),
            Seconds(total as f64 / RATE.get()),
        ))
        .sample_rate(RATE)
        .latency(Latency::Keep)
        .tail(Tail::None);

    // Live: rolling from beat 0 in device-sized blocks, a frame between each
    // as a device's callbacks would leave. The plugin's pipeline plays each
    // chunk's answer one block later; a block played before its answer
    // landed is silence, which would read as a mismatch below. So before each
    // block, wait for the chunk in flight to be answered (a slow server, a
    // loaded machine), and after it, require that none was played
    // unanswered: a late block is waited for, a missed one fails here, by
    // name.
    let controls = s
        .plugin(node)
        .expect("hosted")
        .controls()
        .expect("an out-of-process plugin has controls")
        .clone();
    let missed_before = controls.missed_chunks();
    let mut side = s.graph().take_audio_side();
    let mut live = [Vec::new(), Vec::new()];
    let mut out = vec![Vec::new(), Vec::new()];
    for b in 0..BLOCKS {
        let beat = (b * BLOCK) as f64 * 2.0 / RATE.get();
        side.set_transport(tutti_graph::Transport::new(
            true,
            Bpm::of(120.0),
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
        drive.update(&mut s);
    }

    let request = config.clock(ExportClock::timeline(timeline));
    let Outcome::Finished(bounced) = s.bounce(request).expect("the export renders") else {
        panic!("the export finishes into memory")
    };
    let Produced::Buffers(export) = bounced.into_master() else {
        panic!("the export finishes into memory")
    };
    for (c, (l, e)) in live.iter().zip(export.planes()).enumerate() {
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

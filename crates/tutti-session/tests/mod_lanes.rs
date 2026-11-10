//! Modulation on control lanes, through the session (doc 014 §8, §12.5).
//!
//! The session halves of `bevy-tutti`'s `mod_lanes` suite, which declared its
//! modulation as ECS components and asserted on what the graph renders. Every
//! test here declares through [`Session::mods`] (or a host's [`ModInput`]),
//! runs the session's updates, and asserts on what the graph renders: the
//! nodes are [`ProbeUnit`]s, which write the value their param port reads to
//! their output, so a render is the delivered value frame by frame. The audio
//! side is taken (`Graph::take_audio_side`) and rendered between updates, as
//! a device would.
//!
//! What stayed in `bevy-tutti` is the adapter's: a host-defined source kind
//! (`ModSourceKind`), and the components that declare. Tests that exist in
//! `modulation.rs` (the §8.1 render, a lane built by a re-apply publishing
//! its latency, a target placed an update late) are not repeated here.
//!
//! # Mutations run against this file
//!
//! Each fails the test it names (the per-test docs keep the `bevy-tutti`
//! era's as well): `Mods::remove_source` not dropping its routes
//! (`a_removed_source_..`); `Take::is_done` ignoring a touch take's release
//! (`a_take_suspends_..`); `ModRefusal::route` without `Cycle`
//! (`a_follower_downstream_..`); `declare_targets` letting a second range
//! replace the first, or a second automation merge (`two_ranges_or_..`);
//! `Phase::AfterCommit` not sending the resyncs (`a_held_note_..`) and not
//! re-applying (`a_node_crossfaded_..`); `Session::publish_sent` a no-op
//! (`a_host_edit_committed_..`); `Mods::route` minting one key
//! (`two_routes_..`); `Mods::remove_route` a no-op
//! (`removing_the_last_route_..`); `Pan` out of `STRIP_PARAMS`, the chorus
//! without its ports, and the synth's `ParamSet` with a fresh volume cell
//! (the three node-lane cases); the test's own knob writes dropped (the knob
//! and export cases).
//!
//! Gated like the modulation suite (`modulation`), with `export` for the
//! export-equals-live case and `synth` for the synth-as-target case.

mod common;

use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;

use tutti_graph::NodeKey;
use tutti_graph::{
    CrossfadeCurve, Cx, Event, ForkByClone, Io, Node, Offset, Prepare, Shape, Status,
};
use tutti_modulation::{
    Adsr, CompileError, Curve, Declarations, Follower, Lfo, NoteEnvelope, RouteDecl, RouteKey,
    RouteTarget, Source, SourceKey, TargetKey,
};
use tutti_nodes::automation::{AutomationEnvelope, AutomationPoint, RecordMode};
use tutti_nodes::{Feed, ReconcileError};
use tutti_session::{
    AudioSide, GraphSource, ModInput, ModKeys, ModRefusal, PHASES, ParamRef, PhaseCx, Session,
    SessionEvent, Take, declare_targets,
};
use tutti_types::{
    Amplitude, Beat, Bpm, ChannelLayout, Depth, Hz, SampleRate, Samples, Seconds, UnitParam,
};

use common::probe::{ProbeUnit, Pulse};

const RATE: f64 = 48_000.0;
const TEMPO: Bpm = Bpm::of(120.0);
/// A lane's control step (`tutti_nodes::automation::CONTROL_STEP`).
const STEP: usize = 32;
/// A port's connect crossfade (`PARAM_DECLICK`).
const DECLICK: usize = 256;

/// A detached session of `outputs` channels at 48 kHz, its audio side taken:
/// the executor on this side, played in device-sized blocks.
fn rig(outputs: usize) -> (Session, AudioSide) {
    let mut s = Session::builder()
        .outputs_floor(outputs)
        .build_detached(SampleRate::DEFAULT)
        .expect("builds");
    s.graph().set_sample_rate(SampleRate(RATE));
    let side = s.graph().take_audio_side();
    (s, side)
}

/// Output channel `c` from `nodes[c]`'s port 0.
fn master(s: &mut Session, nodes: &[NodeKey]) {
    for (c, &n) in nodes.iter().enumerate() {
        s.graph().set_output_source(c, GraphSource::Node(n, 0));
    }
}

/// Updates until nothing is left to resolve: nodes committed, then lanes.
fn settle(s: &mut Session) {
    for _ in 0..4 {
        s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert_eq!(
        s.mod_state().unresolved_targets(),
        &[],
        "every target resolved"
    );
}

/// `frames` frames of `channels` outputs, stopped, in `block`-frame blocks.
fn render_n(side: &mut AudioSide, channels: usize, frames: usize, block: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); channels];
    side.set_transport(tutti_graph::Transport::new(false, TEMPO, Beat(0.0), None));
    side.render(frames, block, &mut out);
    out
}

/// [`render_n`] of one output.
fn render(side: &mut AudioSide, frames: usize, block: usize) -> Vec<Vec<f32>> {
    render_n(side, 1, frames, block)
}

/// Doc 014 §5.3's bound for a linear sine at `rate` on the 32-frame grid,
/// times the swing, plus `f32` rounding of values near 1000.
fn lfo_tolerance(depth: f64, rate: f64) -> f64 {
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    h * h / 8.0 * 1000.0 * depth + 1e-3
}

/// The value probe 0 settles to, past the connect's declick: frames
/// `DECLICK + STEP..` of a 1024-frame render must all read it.
fn assert_holds(out: &[f32], want: f32, what: &str) {
    for (f, &v) in out.iter().enumerate().skip(DECLICK + STEP) {
        assert!(
            (v - want).abs() < 1e-3,
            "{what}: frame {f} reads {v}, not {want}"
        );
    }
}

/// A note-on (note 60, velocity 100) at executor frame `on`, never
/// released, on its one event output.
#[derive(Clone)]
struct Held {
    on: u64,
}

impl Node for Held {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (base, n) = (cx.env.frame.get(), cx.env.block_len);
        if let Some(o) = self
            .on
            .checked_sub(base)
            .and_then(|o| Offset::new(o as usize, n))
        {
            io.event_out(0)
                .push(Event::midi(o, [0x2090_3C64, 0, 0, 0]))
                .expect("room");
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A source declaring `LATE` frames of latency: a look-ahead's bookkeeping.
#[derive(Clone)]
struct Late;

const LATE: usize = 100;

impl Node for Late {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_latency(tutti_types::Latency::new(Samples(LATE)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

// --------------------------------------------------------- export --

/// **Export == live, on a native param.** A 3 Hz sine on one probe's cutoff
/// and a 5 Hz triangle on another's volume, their knobs moved after the lanes
/// are built (a knob is the node's cell, which a fork reads); live plays from
/// a stop into rolling ragged blocks, the export (buffers, over the fork of
/// the graph) renders from its first frame. Equal bit for bit from the play
/// start.
///
/// Mutation (run in `bevy-tutti`): the modulation host handing the lane a
/// copy of the knob cell, not the node's own (the live lane keeps the value
/// it was built with, the export forks the node's: frame 0 reads 623.5 live
/// against 450 exported) fails. Run here: the knob move (`set_param(a, Cutoff,
/// 450)`) dropped, so both sides play 500 and the final analytic check, at
/// 450, fails (the live-equals-export loop alone could not see it).
#[cfg(feature = "export")]
#[test]
fn an_export_is_the_live_render_on_a_native_param() {
    use tutti_export::{EncodeConfig, ExportConfig, Format, Outcome, Pcm, RenderConfig};
    use tutti_session::{ExportClock, ExportOutput, ExportRequest, ExportSource, ExportTarget};
    use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};

    let (mut s, mut side) = rig(2);
    let (a, _) = s.graph().add(ProbeUnit::new(500.0));
    let (b, _) = s.graph().add(ProbeUnit::of(UnitParam::Volume, 0.5));
    master(&mut s, &[a, b]);
    let cutoff = ParamRef::unit(a, UnitParam::Cutoff);
    let volume = ParamRef::unit(b, UnitParam::Volume);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    m.range(volume, 0.0, 1.0);
    let sine = m.lfo(Lfo::sine().hz(Hz(3.0)));
    let tri = m.lfo(Lfo::triangle().hz(Hz(5.0)));
    m.route(sine, cutoff, Depth(0.3));
    m.route(tri, volume, Depth(0.2));
    settle(&mut s);
    // Past the connect's declick, stopped; then the knobs move.
    render_n(&mut side, 2, 1_024, 128);
    s.graph().set_param(a, UnitParam::Cutoff, 450.0);
    s.update();
    // The same write lands on the live side before it plays.
    render_n(&mut side, 2, 128, 128);

    let total = 24_000;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: TEMPO,
        sample_rate: SampleRate(RATE),
        loop_range: None,
    }));
    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(RATE),
            duration_seconds: tutti_types::Seconds(total as f64 / RATE),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Float32),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    };
    let request = ExportRequest::new(
        ExportSource::Mix,
        ExportTarget::Buffers,
        config,
        ExportClock::timeline(timeline),
    );
    let Outcome::Finished(ExportOutput::Buffers(rendered)) =
        s.export_blocking(request).expect("the export runs")
    else {
        panic!("the export finishes into memory")
    };
    let export = rendered.planes;

    // Live: rolling from beat 0 (the play clock restarts there), in
    // ragged blocks.
    let mut live = [Vec::new(), Vec::new()];
    let mut out = vec![Vec::new(), Vec::new()];
    let mut done = 0;
    for n in [1usize, 128, 7, 64, 127, 33, 2, 100].iter().cycle() {
        if done >= total {
            break;
        }
        let n = (*n).min(total - done);
        let beat = done as f64 * TEMPO.get() / 60.0 / RATE;
        side.set_transport(tutti_graph::Transport::new(true, TEMPO, Beat(beat), None));
        side.render(n, n, &mut out);
        for (l, o) in live.iter_mut().zip(&out) {
            l.extend_from_slice(o);
        }
        done += n;
    }

    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(e.len() >= total, "channel {c}: {} frames", e.len());
        assert!(l.iter().any(|&v| v != l[0]), "channel {c} moves");
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
    // The knob written after the lanes were built is what both play.
    let want = 450.0 + 300.0 * (std::f64::consts::TAU * 3.0 * 12_000.0 / RATE).sin();
    assert!((f64::from(export[0][12_000]) - want).abs() < lfo_tolerance(0.3, 3.0));
}

// ------------------------------------------ removal and the knob --

/// **A removed source leaves no node and no layer.** A follower and an LFO
/// on one cutoff; the follower's source is removed (and the route from it).
/// Its node leaves the graph, the lane's program reads no input and keeps the
/// LFO's layer, and the session holds nothing for it.
///
/// Mutation (run in `bevy-tutti`): the declarations keeping a route whose
/// source is gone fails (the compile refuses an unknown source, so nothing
/// changes and the node stays). Run here: `Mods::remove_source` keeping the
/// routes from it (`retain` dropped) fails the same way.
#[test]
fn a_removed_source_leaves_no_node_and_no_layer() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    let (kick, _) = s.graph().add(ForkByClone(Pulse {
        from: 0,
        to: 48_000,
        level: 0.5,
    }));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let lfo = m.lfo(Lfo::sine().hz(Hz(2.0)));
    let follower = m.follower(Follower::default(), kick);
    m.route(lfo, cutoff, Depth(0.1));
    m.route(follower, cutoff, Depth(-0.2));
    settle(&mut s);
    render(&mut side, 1_024, 128);
    let node = s
        .mod_state()
        .source_node(follower)
        .expect("the follower has a node");
    assert!(s.graph_ref().contains(node));
    let (_, lane) = s.mod_state().lane(cutoff.key()).expect("a lane");
    assert_eq!(
        lane.program().inputs().len(),
        1,
        "precondition: it reads the follower"
    );

    assert!(s.mods().remove_source(follower));
    settle(&mut s);
    render(&mut side, 1_024, 128);

    assert_eq!(
        s.mod_state().source_node(follower),
        None,
        "nothing held for it"
    );
    assert!(!s.graph_ref().contains(node), "its node is gone");
    let (_, lane) = s
        .mod_state()
        .lane(cutoff.key())
        .expect("the LFO keeps the lane");
    let program = lane.program();
    assert!(program.inputs().is_empty(), "no input layer");
    assert_eq!(program.layers().len(), 1, "the LFO's layer");
}

/// **A knob written after a route edit is the knob.** The cutoff's knob is
/// the node's own cell, written through `Graph::set_param`; a depth edit
/// republishes the lane and writes no knob, so the knob written before it and
/// the one written after it are each the base the LFO rides on.
///
/// Mutations (run in `bevy-tutti`), each failed: the apply writing each
/// range's bottom into its knob (`set_knob(min)` after every apply: the
/// cutoff rides on 0, frame 1056 reads 54.6); the param write dropped (the
/// knob stays 500). Run here: the second `set_param` (700) dropped: the
/// lane's knob reads 300 and the second half rides on the wrong base.
#[test]
fn a_knob_written_after_a_route_edit_is_the_knob() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let lfo = m.lfo(Lfo::sine().hz(Hz(2.0)));
    let route = m.route(lfo, cutoff, Depth(0.3));
    settle(&mut s);
    render(&mut side, 1_024, 128);

    s.graph().set_param(probe, UnitParam::Cutoff, 300.0);
    s.update();
    s.mods().route_mut(route).expect("the route").depth = Depth(0.2);
    s.update();
    let a = render(&mut side, 2_048, 128);
    s.graph().set_param(probe, UnitParam::Cutoff, 700.0);
    s.update();
    let b = render(&mut side, 2_048, 128);

    assert_eq!(
        s.mod_state().lane(cutoff.key()).expect("a lane").1.knob(),
        Some(700.0)
    );
    let tol = lfo_tolerance(0.2, 2.0);
    for (base, from, got) in [(300.0, 1_024, &a[0]), (700.0, 3_072, &b[0])] {
        for (i, &v) in got.iter().enumerate().skip(STEP) {
            let p = (from + i) as f64;
            let want = base + 200.0 * (std::f64::consts::TAU * 2.0 * p / RATE).sin();
            assert!(
                (f64::from(v) - want).abs() < tol,
                "knob {base}, frame {p}: {v} vs {want}"
            );
        }
    }
}

// ------------------------------------------------------- envelope --

/// **A held note envelope newly routed reads its level at once.** A note held
/// from frame 10 into a note envelope (sustain 0.4) routed to probe 1
/// (`0..=1`, knob 0.2, depth 0.5); probe 2 (knob 0.1) has a lane reading a
/// macro of 0. A route from the envelope to probe 2 is added in the sustain:
/// from the first grid piece after the frame it lands in, probe 2 reads
/// `0.1 + 0.5 · 0.4`, not its knob until the next note.
///
/// Mutation (run in `bevy-tutti`): `Phase::AfterCommit` not sending the
/// resyncs owed (`ModSession::resync` skipped) fails: the resync stays owed.
#[test]
fn a_held_note_envelope_newly_routed_reads_its_level_at_once() {
    let (mut s, mut side) = rig(2);
    let (one, _) = s.graph().add(ProbeUnit::new(0.2));
    let (two, _) = s.graph().add(ProbeUnit::new(0.1));
    let (clip, _) = s.graph().add(ForkByClone(Held { on: 10 }));
    master(&mut s, &[one, two]);
    let (c1, c2) = (
        ParamRef::unit(one, UnitParam::Cutoff),
        ParamRef::unit(two, UnitParam::Cutoff),
    );
    let adsr = Adsr {
        attack: Seconds(0.002),
        decay: Seconds(0.003),
        sustain: Amplitude(0.4),
        release: Seconds(0.2),
    };
    let mut m = s.mods();
    m.range(c1, 0.0, 1.0);
    m.range(c2, 0.0, 1.0);
    let env = m.note_envelope(NoteEnvelope::new(adsr), clip, 0);
    let zero = m.macro_(0.0);
    m.route(env, c1, Depth(0.5));
    m.route(zero, c2, Depth(0.5));
    settle(&mut s);
    let got = render_n(&mut side, 2, 4_096, 128);
    assert!(
        (got[0][4_095] - 0.4).abs() < 1e-6,
        "probe 1 in the sustain: {}",
        got[0][4_095]
    );
    assert!((got[1][4_095] - 0.1).abs() < 1e-7, "probe 2 at its knob");

    s.mods().route(env, c2, Depth(0.5));
    s.update();
    assert_eq!(
        s.mod_state().resyncs_owed(),
        0,
        "the resync went with the update's commit"
    );
    let got = render_n(&mut side, 2, 512, 128);
    for (f, &v) in got[1].iter().enumerate().skip(STEP) {
        assert!((v - 0.3).abs() < 1e-6, "frame {f}: {v}");
    }
}

// ----------------------------------------------------------- takes --

/// **A take suspends the automation, then restores it.** A cutoff (knob 300)
/// automated to 800: the automation plays. A touch take opens on the
/// automation's target: the knob plays (the gesture drives the param). The
/// take records 650 and is released: its envelope becomes the automation, and
/// 650 plays.
///
/// Mutations (run in `bevy-tutti`), each failed: the declarations ignoring
/// the take (the automation plays through it: 800, not 300); the closing of
/// takes leaving the curve (the old 800 is back after the release, not 650);
/// `Take::is_done` ignoring a touch take's release (the take stays open:
/// 300).
#[test]
fn a_take_suspends_the_automation_then_restores_it() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(300.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut lane = AutomationEnvelope::new(0.0).with_range(0.0, 1_000.0);
    lane.add_point(AutomationPoint::new(0.0, 800.0));
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    m.automate(cutoff, Arc::new(lane));
    settle(&mut s);
    let last = |out: &[Vec<f32>]| *out[0].last().expect("rendered");
    assert_eq!(
        last(&render(&mut side, 1_024, 128)),
        800.0,
        "the automation plays"
    );

    let mut m = s.mods();
    assert!(m.record(cutoff, Take::record(RecordMode::Touch, 0.0, 1_000.0)));
    m.take_mut(cutoff)
        .expect("the take is open")
        .recorder_mut()
        .touch(Beat(0.0), 650.0);
    s.update();
    assert_eq!(
        last(&render(&mut side, 512, 128)),
        300.0,
        "the knob plays under the take"
    );

    s.mods()
        .take_mut(cutoff)
        .expect("still open")
        .recorder_mut()
        .release(Beat(1.0), 650.0);
    s.update();
    assert!(s.mods().take_mut(cutoff).is_none(), "the take closed");
    assert_eq!(
        last(&render(&mut side, 512, 128)),
        650.0,
        "the take's envelope plays"
    );
}

// ---------------------------------------------------------- refusal --

/// The refusals of one `update`, in order.
fn refusals(events: Vec<SessionEvent>) -> Vec<ModRefusal> {
    events
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::ModulationRefused(r) => Some(r),
            _ => None,
        })
        .collect()
}

/// **A follower downstream of its target is refused by route, and the error
/// is surfaced.** A follower of the probe's own output routed into the
/// probe's cutoff would be a cycle: the declaration is refused with the
/// route's key, as a `SessionEvent::ModulationRefused` and in
/// `ModState::refusal`, and nothing is built. Dropping the route applies the
/// rest, and clears the refusal.
///
/// Mutations (run in `bevy-tutti`), each failed: `ModRefusal::route` mapping
/// `Cycle` to `None`; a refusal reported every update (`ModState::refuse`
/// comparing to nothing: two messages across two updates).
#[test]
fn a_follower_downstream_of_its_target_is_refused_by_route() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let offset = m.macro_(0.5);
    m.route(offset, cutoff, Depth(0.2));
    settle(&mut s);

    let mut m = s.mods();
    let follower = m.follower(Follower::default(), probe);
    let route = m.route(follower, cutoff, Depth(0.5));
    let mut refused = refusals(s.update());
    refused.extend(refusals(s.update()));
    assert_eq!(refused.len(), 1, "one message for one refusal: {refused:?}");
    assert_eq!(refused[0].route(), Some(route), "refused by route");
    assert!(matches!(
        refused[0],
        ModRefusal::Apply(ReconcileError::Cycle { .. })
    ));
    assert!(matches!(
        s.mod_state().refusal(),
        Some(ModRefusal::Apply(ReconcileError::Cycle { .. }))
    ));
    assert_eq!(s.mod_state().source_node(follower), None, "nothing built");
    let (_, lane) = s
        .mod_state()
        .lane(cutoff.key())
        .expect("the lane before the refusal");
    assert!(lane.program().inputs().is_empty(), "unchanged");
    // The macro's `+100` still plays.
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 600.0);

    assert!(s.mods().remove_route(route));
    settle(&mut s);
    assert_eq!(s.mod_state().refusal(), None);
}

/// A host's keys: the target key `bits` names `node`; its one source has no
/// feed.
struct HostKeys {
    bits: u64,
    node: Cell<Option<NodeKey>>,
}

impl ModKeys for HostKeys {
    fn node(&self, target: u64) -> Option<NodeKey> {
        (target == self.bits).then(|| self.node.get()).flatten()
    }
    fn feed(&self, _: SourceKey) -> Option<Feed> {
        None
    }
}

/// One update of every phase with the host's declarations: a macro at 1
/// (source 1) routed at depth 0.1 (route 2) onto `target`, whose targets are
/// `targets`.
fn host_update(
    s: &mut Session,
    keys: &HostKeys,
    target: TargetKey,
    targets: &[(TargetKey, tutti_modulation::TargetDecl)],
) -> Vec<SessionEvent> {
    let sources = [(SourceKey(1), Source::Macro { value: 1.0 })];
    let routes = [(
        RouteKey(2),
        RouteDecl {
            source: SourceKey(1),
            to: RouteTarget::Param(target),
            depth: Depth(0.1),
            polarity: tutti_modulation::Polarity::Bipolar,
            curve: tutti_modulation::CurveType::Linear,
        },
    )];
    let input = ModInput {
        declarations: Declarations {
            sources: &sources,
            routes: &routes,
            targets,
        },
        keys,
    };
    let cx = PhaseCx::at(Instant::now()).with_mods(input);
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, cx, &mut out);
    }
    out
}

/// **Two ranges, or two automations, on one param are refused by target.** A
/// host's macro `+100` on a cutoff probe (knob 500, range `0..=1000`). A
/// second range on the cutoff (`0..=2000`, which would make it `+200`) is
/// refused as `DuplicateTarget` naming the cutoff, once, and nothing changes:
/// the probe still plays 600. Without the refusal the two would merge, the
/// winner being whichever the query met last. Dropping it applies again; then
/// two automations on the cutoff are refused the same way.
///
/// Mutations (run in `bevy-tutti`, against `declare_targets`), each failed
/// with no message: a second range replacing the first (merged); a second
/// automation merged into the first (`automated.insert`'s answer ignored).
/// Run here: the same two.
#[test]
fn two_ranges_or_automations_on_one_param_are_refused() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff).key();
    let keys = HostKeys {
        bits: cutoff.node,
        node: Cell::new(Some(probe)),
    };
    let one = declare_targets([(cutoff, 0.0, 1_000.0)], []);
    for _ in 0..4 {
        host_update(&mut s, &keys, cutoff, &one);
    }
    assert_eq!(s.mod_state().refusal(), None);
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 600.0);

    let refused_by_cutoff = |s: &mut Session, targets: &[_], what: &str| {
        let mut refused = refusals(host_update(s, &keys, cutoff, targets));
        refused.extend(refusals(host_update(s, &keys, cutoff, targets)));
        assert_eq!(refused.len(), 1, "{what}: one message: {refused:?}");
        assert_eq!(refused[0].target(), Some(cutoff), "{what}: by target");
        assert!(
            matches!(
                s.mod_state().refusal(),
                Some(ModRefusal::Apply(ReconcileError::Compile(
                    CompileError::DuplicateTarget(_)
                )))
            ),
            "{what}: held"
        );
    };

    let two_ranges = declare_targets([(cutoff, 0.0, 1_000.0), (cutoff, 0.0, 2_000.0)], []);
    refused_by_cutoff(&mut s, &two_ranges, "two ranges");
    assert_eq!(
        *render(&mut side, 1_024, 128)[0].last().unwrap(),
        600.0,
        "nothing changed"
    );
    for _ in 0..4 {
        host_update(&mut s, &keys, cutoff, &one);
    }
    assert_eq!(s.mod_state().refusal(), None, "dropping it applies again");

    let curve = || -> Option<Arc<dyn Curve>> {
        Some(Arc::new(
            AutomationEnvelope::new(0.0).with_range(0.0, 1_000.0),
        ))
    };
    let two_automations = declare_targets(
        [(cutoff, 0.0, 1_000.0)],
        [(cutoff, curve()), (cutoff, curve())],
    );
    refused_by_cutoff(&mut s, &two_automations, "two automations");
}

// ----------------------------------------------------------- rebind --

/// **A node crossfaded to one with another param list keeps modulating.** A
/// macro's `+250` on a cutoff probe (knob 500); the node is crossfaded to a
/// probe that declares the cutoff and a Q (another param list, knob 400).
/// After the re-apply that follows the commit (doc 014 §8.2: the rebound
/// target is unresolved until its node is in a plan) the new node plays
/// `400 + 250`, and a range edit after it lands on the new node (`+500`).
///
/// Mutations (run in `bevy-tutti`, against `Phase::AfterCommit`), each failed:
/// not re-applying while a target is unresolved (`ModSession::reapply`
/// returning `false`: the new node's first frame plays its bare knob, 400);
/// the re-apply's commit skipped (`self.commit()` deleted: "modulated from
/// its first frame" fails).
#[test]
fn a_node_crossfaded_to_another_param_list_keeps_modulating() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let offset = m.macro_(1.0);
    m.route(offset, cutoff, Depth(0.25));
    settle(&mut s);
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 750.0, "precondition: modulated");

    s.graph()
        .crossfade(
            probe,
            ProbeUnit::new(400.0).with_extra(UnitParam::Q),
            Seconds(0.005),
            CrossfadeCurve::EqualAmplitude,
        )
        .expect("the crossfade starts");
    // The update that commits the new node re-applies after its commit: the
    // first block it plays is modulated.
    s.update();
    let out = render(&mut side, 1_024, 128);
    assert_eq!(
        *out[0].last().unwrap(),
        650.0,
        "the new node, modulated from its first frame"
    );
    settle(&mut s);
    let out = render(&mut side, 4_096, 128);
    assert_eq!(*out[0].last().unwrap(), 650.0, "the new node, modulated");

    s.mods().range(cutoff, 0.0, 2_000.0);
    s.update();
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 900.0, "a later range edit lands");
}

// ------------------------------------------------- a host's commit --

/// **A host's staged edit committed on an update with an unresolved target
/// publishes its PDC figures on that update.** A host inserts, between
/// updates, a latent node on the outputs and a probe it then modulates: the
/// probe's target is unresolved when the update applies. The update's commit
/// sends the host's edit with the lanes, and the session's latency is that
/// plan's on that update; the re-apply after it resolves the target.
///
/// Mutation (run in `bevy-tutti`): the figures mirrored by neither system
/// failed (`GraphLatency` read 0). Run here: `Session::publish_sent`
/// returning at once leaves `latency()` at 0.
#[test]
fn a_host_edit_committed_for_a_target_publishes_its_latency() {
    let (mut s, mut side) = rig(1);
    settle(&mut s);
    let (late, _) = s.graph().add(ForkByClone(Late));
    s.graph().set_outputs_from(late);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let offset = m.macro_(1.0);
    m.route(offset, cutoff, Depth(0.1));

    s.update();
    assert_eq!(
        s.latency(),
        Samples(LATE),
        "the plan the update sent, published"
    );
    render(&mut side, 128, 128);
    settle(&mut s);
    assert!(s.mod_state().lane(cutoff.key()).is_some());
}

// ------------------------------------- ported from the old driver --
//
// Doc 014 §10 deleted the frame-rate driver and its suites. The properties
// below were theirs and still hold of the declarations; each is asserted on
// the lanes now.

/// **Two routes onto one param sum, and removing one takes only its share.**
/// Two macros at 1 on a cutoff (knob 500 in `0..=1000`) at depths 0.1 and 0.2
/// add 100 and 200: 800. Removing the second route leaves 600 (the session
/// has no `enabled` flag: `bevy-tutti`'s disabled route declared nothing,
/// which is this); declaring it again, 800.
///
/// Mutation (run in `bevy-tutti`): the lane's program keeping only its last
/// layer (`ControlProgram::new` given `layers.split_off(len - 1)`: 700 for
/// 800) failed.
#[test]
fn two_routes_on_one_param_sum_and_removing_one_takes_its_share() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let a = m.macro_(1.0);
    let b = m.macro_(1.0);
    m.route(a, cutoff, Depth(0.1));
    let second = m.route(b, cutoff, Depth(0.2));
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "both routes");

    assert!(s.mods().remove_route(second));
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 600.0, "one removed");

    s.mods().route(b, cutoff, Depth(0.2));
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "declared again");
}

/// **Removing the last route returns the param to its knob.** A macro at
/// depth 0.1 on a cutoff (knob 500 in `0..=1000`) adds 100: 600. With the
/// route removed the target has nothing left to drive it, so its lane is gone
/// and the probe reads the knob, 500, past the disconnect's declick.
///
/// Mutation (run in `bevy-tutti`): the lane sweep keeping a lane whose target
/// has no bind left → the lane stays on the port with its stale program →
/// failed.
#[test]
fn removing_the_last_route_returns_the_param_to_its_knob() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let offset = m.macro_(1.0);
    let route = m.route(offset, cutoff, Depth(0.1));
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 600.0, "the route");
    assert!(
        s.mod_state().lane(cutoff.key()).is_some(),
        "the route has its lane"
    );

    assert!(s.mods().remove_route(route));
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 500.0, "no route left");
    assert!(s.mod_state().lane(cutoff.key()).is_none(), "and no lane");
}

/// **A bus strip's volume and pan take lanes.** The strip declares its params
/// itself (`STRIP_PARAMS`), so a route onto either resolves to a lane on its
/// port, with nothing refused.
///
/// Mutation (run in `bevy-tutti`): `Pan` dropped from `STRIP_PARAMS` → the
/// route is refused (`NoSuchParam`) → failed.
#[test]
fn a_bus_strips_volume_and_pan_take_lanes() {
    let (mut s, _side) = rig(2);
    let (strip, _) = s.graph().add(tutti_nodes::BusStripNode::with_channels(
        ChannelLayout::STEREO,
    ));
    let mut m = s.mods();
    let offset = m.macro_(0.5);
    for param in [UnitParam::Volume, UnitParam::Pan] {
        let at = ParamRef::unit(strip, param);
        m.range(at, 0.0, 1.0);
        m.route(offset, at, Depth(0.5));
    }
    settle(&mut s);
    for param in [UnitParam::Volume, UnitParam::Pan] {
        assert!(
            s.mod_state()
                .lane(ParamRef::unit(strip, param).key())
                .is_some(),
            "{param:?} has its lane"
        );
    }
}

/// **A chorus's rate and a phaser's mix take lanes.** The nodes declare these
/// params as ports (`MOD_DELAY_PARAMS`, `PHASER_PARAMS`), so a route onto
/// each resolves to a lane, with nothing refused. What a lane on them sounds
/// like is pinned where the nodes live (`tutti-nodes`'
/// `a_lane_modulates_a_chorus_rate_around_its_own_cell` and
/// `each_fed_param_is_the_one_the_dsp_reads`).
///
/// Mutation (run in `bevy-tutti`): `ModDelayNode` not declaring its ports
/// (the `.with_params(&MOD_DELAY_PARAMS)` dropped) → the chorus route is
/// refused (`NoSuchParam`) → failed.
#[test]
fn a_chorus_rate_and_a_phaser_mix_take_lanes() {
    let (mut s, _side) = rig(2);
    let (chorus, _) = s
        .graph()
        .add(tutti_nodes::ModDelayNode::chorus(ChannelLayout::STEREO));
    let (phaser, _) = s.graph().add(tutti_nodes::PhaserNode::with_channels(
        ChannelLayout::STEREO,
        4,
    ));
    let targets = [
        ParamRef::unit(chorus, UnitParam::Rate),
        ParamRef::unit(phaser, UnitParam::Wet),
    ];
    let mut m = s.mods();
    let offset = m.macro_(0.5);
    for at in targets {
        m.range(at, 0.0, 1.0);
        m.route(offset, at, Depth(0.5));
    }
    settle(&mut s);
    for at in targets {
        assert!(
            s.mod_state().lane(at.key()).is_some(),
            "{at:?} has its lane"
        );
    }
}

/// **A synth added as a graph node is a modulation target.** A `PolySynth`
/// goes in through `Graph::add` like any node; a route into its volume
/// resolves (nothing refused, nothing unresolved), and the volume's lane has
/// the synth's own volume cell as its knob.
///
/// Mutations (run in `bevy-tutti`), each failed: the synth registered with no
/// params (`impl SessionNode for PolySynth {}` in place of
/// `param_session_node!`): the lane has no knob; the synth's `ParamSet`
/// handing `Volume` a fresh cell rather than the synth's own: the knob does
/// not follow a store into the synth's volume.
#[cfg(feature = "synth")]
#[test]
fn an_added_synth_is_a_modulation_target() {
    use std::sync::atomic::Ordering;
    use tutti_polysynth::{PolySynth, SynthConfig};

    let (mut s, _side) = rig(2);
    let node = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let cell = node.volume_atomic();
    let (synth, _) = s.graph().add(node);
    master(&mut s, &[synth, synth]);
    let volume = ParamRef::unit(synth, UnitParam::Volume);
    let mut m = s.mods();
    m.range(volume, 0.0, 1.0);
    let offset = m.macro_(1.0);
    m.route(offset, volume, Depth(0.25));
    settle(&mut s);
    let (_, lane) = s
        .mod_state()
        .lane(volume.key())
        .expect("the volume has a lane");
    // The knob is read from its cell: a store into the synth's own volume
    // cell is what the lane reads, value for value.
    for v in [0.37, 0.81] {
        cell.store(v, Ordering::Release);
        assert_eq!(lane.knob(), Some(v), "its knob is the synth's volume cell");
    }
}

// ----------------------------------------------- what an edit sends --

/// The plan the graph sent last.
fn sent(s: &Session) -> Arc<tutti_graph::Plan> {
    s.graph_ref().sent_plan().expect("a plan was sent")
}

/// Nothing is staged: the last update's commit took every edit.
fn idle(s: &Session) -> bool {
    !s.graph_ref().is_edited()
}

/// One update, then a block rendered, as a device would between updates.
fn frame(s: &mut Session, side: &mut AudioSide) {
    s.update();
    render(side, 128, 128);
}

/// A cutoff probe (knob 500, range `0..=1000`) on output 0, a sine at `rate`
/// Hz routed into it at `depth`, settled. Returns the probe's param, the
/// route's key.
fn lfo_scene(s: &mut Session, side: &mut AudioSide, rate: f32, depth: f32) -> (ParamRef, RouteKey) {
    let (filter, _) = s.graph().add(ProbeUnit::new(500.0));
    master(s, &[filter]);
    let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let lfo = m.lfo(Lfo::sine().hz(Hz(rate)));
    let route = m.route(lfo, cutoff, Depth(depth));
    for _ in 0..4 {
        frame(s, side);
    }
    assert_eq!(s.mod_state().refusal(), None);
    assert!(
        s.mod_state().lane(cutoff.key()).is_some(),
        "precondition: a lane"
    );
    (cutoff, route)
}

/// **A range or depth edit sends no plan; a follower route sends one.** Doc
/// 014 §8.2: a program edit is a store into the lane, not a rebuilt node; a
/// follower is a node, so routing one is one commit, and the updates after
/// it send nothing more.
///
/// Each edit is checked to have reached the render, so "no plan" is not an
/// edit that did nothing. (The reconciler's half is `tutti-nodes`'
/// `range_and_depth_edits_need_no_commit_and_a_follower_route_one`; this is
/// the session's: its updates stage and commit nothing for a program edit.)
///
/// Mutations (run in `bevy-tutti`), each failed: a lane rebuilt when its
/// program changes (`Modulation::apply`'s gone-lanes filter also taking a
/// changed program: "a depth edit sent a plan"); a follower rebuilt on every
/// apply (`p.decl != n.decl` as `==`: more than one plan); the range store
/// skipped (`if lane.range != range` as `false`: "the range clamps" reads
/// 500). Run here: the same lane rebuild.
#[test]
fn range_and_depth_edits_send_no_plan_and_a_follower_route_one_commit() {
    let (mut s, mut side) = rig(1);
    // A rate low enough that one block barely moves it: the render after an
    // edit is read against the edit, not the wave.
    let (cutoff, route) = lfo_scene(&mut s, &mut side, 0.5, 0.3);
    let settled = sent(&s);

    // The depth: 0.3 → 0.0, the knob alone.
    s.mods().route_mut(route).unwrap().depth = Depth(0.0);
    for _ in 0..3 {
        frame(&mut s, &mut side);
        assert!(Arc::ptr_eq(&sent(&s), &settled), "a depth edit sent a plan");
        assert!(idle(&s), "a depth edit staged an edit");
    }
    let v = render(&mut side, 128, 128);
    assert!(
        v[0].iter().all(|&x| x == 500.0),
        "depth 0 is the knob: {v:?}"
    );

    // The range: clamped to 600..=700, the knob (500) reads 600.
    s.mods().range(cutoff, 600.0, 700.0);
    for _ in 0..3 {
        frame(&mut s, &mut side);
        assert!(Arc::ptr_eq(&sent(&s), &settled), "a range edit sent a plan");
        assert!(idle(&s), "a range edit staged an edit");
    }
    let v = render(&mut side, 128, 128);
    assert!(v[0].iter().all(|&x| x == 600.0), "the range clamps: {v:?}");
    // And back to 0..=1000 (a route's depth is a fraction of the span).
    s.mods().range(cutoff, 0.0, 1_000.0);
    frame(&mut s, &mut side);
    assert!(Arc::ptr_eq(&sent(&s), &settled), "a range edit sent a plan");

    // A follower on a pulse, routed in: the pulse node first, on its own.
    let (pulse, _) = s.graph().add(ForkByClone(Pulse {
        from: 0,
        to: u64::MAX,
        level: 0.8,
    }));
    for _ in 0..3 {
        frame(&mut s, &mut side);
    }
    let mut plans = vec![sent(&s)];
    let mut m = s.mods();
    let kick = m.follower(Follower::default(), pulse);
    m.route(kick, cutoff, Depth(0.15));
    for _ in 0..6 {
        frame(&mut s, &mut side);
        let now = sent(&s);
        if !Arc::ptr_eq(plans.last().unwrap(), &now) {
            plans.push(now);
        }
    }
    assert_eq!(plans.len(), 2, "a follower route sends exactly one plan");
    assert!(idle(&s));
    // And it modulates: the knob 500 plus the follower's 0.8 · 0.15 · 1000,
    // 620, inside 600..=700.
    let v = render(&mut side, 4_800, 128);
    let last = *v[0].last().unwrap();
    assert!((last - 620.0).abs() < 0.5, "the follower raises it: {last}");
}

/// **A depth slider keeps the LFO's phase.** Dragging a route's depth is a
/// store into the lane's program: the wave goes on from where it was, at the
/// new depth, on the frame the next block starts; a rebuilt source node would
/// restart the wave.
///
/// The edit lands at a block boundary; every frame after it reads
/// `500 + 1000 · 0.1 · sin(2π · 4 · t)` with `t` counted from frame 0, within
/// the control grid's bound.
///
/// Mutation (run in `bevy-tutti`): a lane rebuilt when its program changes
/// fails at "a depth edit sent a plan", and with that assertion taken out, at
/// the render too (frame 1056 reads 640.1, the new lane's connect crossfade
/// from the old one, against 552.5). Run here: the same.
#[test]
fn a_depth_slider_keeps_phase() {
    let (mut s, mut side) = rig(1);
    let rate = 4.0;
    let (_, route) = lfo_scene(&mut s, &mut side, rate, 0.3);
    let settled = sent(&s);
    // `lfo_scene` rendered its updates' blocks: the clock is past them.
    let start = 4 * 128;
    let before = render(&mut side, 2 * DECLICK, 128).swap_remove(0);
    let value = |depth: f64, f: usize| {
        500.0 + 1000.0 * depth * (std::f64::consts::TAU * f64::from(rate) * f as f64 / RATE).sin()
    };
    let h = std::f64::consts::TAU * f64::from(rate) * STEP as f64 / RATE;
    let tol = |depth: f64| h * h / 8.0 * 1000.0 * depth + 1e-3;
    for (i, &v) in before.iter().enumerate().skip(DECLICK) {
        let f = start + i;
        let want = value(0.3, f);
        assert!(
            (f64::from(v) - want).abs() < tol(0.3),
            "before, frame {f}: {v} vs {want}"
        );
    }

    s.mods().route_mut(route).unwrap().depth = Depth(0.1);
    s.update();
    assert!(Arc::ptr_eq(&sent(&s), &settled), "a depth edit sent a plan");
    let after = render(&mut side, 12_000, 128).swap_remove(0);
    let start = start + before.len();
    for (i, &v) in after.iter().enumerate().skip(STEP) {
        let f = start + i;
        let want = value(0.1, f);
        assert!(
            (f64::from(v) - want).abs() < tol(0.1),
            "after, frame {f}: {v} vs {want}"
        );
    }
}

// ------------------------------------------------------------- fork --

/// **A fork renders a modulated param as live does**, a knob written under
/// its modulation and a knob left as it was built alike. The constraint
/// export by `Editor::fork` rests on: a modulated param's cell is its lane's
/// knob (one writer, doc 014 §8.2), and its lane is a graph node, so the fork
/// carries both. A macro at 1 by a fifth of `0..10` adds 2: the knob written
/// to 6 reads 8, the one built at 1 reads 3, live and forked.
///
/// The session half of `bevy-tutti`'s `every_param_write_path_reaches_a_fork`
/// (its unmodulated paths are `graph.rs`' test of the same name).
///
/// Mutation (run): `OwnKeys::node` answering `None` (no target resolves, so
/// no lane is built): `settle` reports the unresolved targets and fails. The
/// fork's own half: `ControlLaneNode::fork_fresh` giving the fork's lane a
/// knob cell of 0 instead of the one it holds → the fork reads 2 where live
/// reads 8 and 3 (fails).
#[test]
fn a_fork_renders_a_modulated_param_as_live_does() {
    let mut s = Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate(RATE))
        .expect("builds");
    let (written, _) = s.graph().add(ProbeUnit::of(UnitParam::Drive, 1.0));
    let (built, _) = s.graph().add(ProbeUnit::of(UnitParam::Drive, 1.0));
    master(&mut s, &[written, built]);
    let mut m = s.mods();
    let offset = m.macro_(1.0);
    for node in [written, built] {
        let drive = ParamRef::unit(node, UnitParam::Drive);
        m.range(drive, 0.0, 10.0);
        m.route(offset, drive, Depth(0.2));
    }
    settle(&mut s);
    s.graph().set_param(written, UnitParam::Drive, 6.0);
    s.update();

    // Past the lanes' connect declick (256 frames).
    let mut live = [0.0f32; 2];
    for _ in 0..512 {
        s.render_frame(&mut live);
    }
    let mut fork = s.graph_ref().fork_live().expect("every knob is forkable");
    let mut forked = [0.0f32; 2];
    fork.tick(&[], &mut forked);
    assert_eq!(live[0], 8.0, "the knob written under its modulation, live");
    assert_eq!(forked[0], 8.0, "and in the fork");
    assert_eq!(live[1], 3.0, "the knob built under its modulation, live");
    assert_eq!(forked[1], 3.0, "and in the fork");
}

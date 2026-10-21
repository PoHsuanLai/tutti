//! Parameter automation reaches a hosted plugin through the graph: a
//! foreign control lane on the plugin node's event input, a curve segment
//! scheduled into it, and a lane sending an envelope's segments, against
//! the reference CLAP plugin's gain parameter.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv};

use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_core::{Beat, SampleRate, Samples};
use tutti_graph::{
    Editor, EventEdge, EventIn, EventKind, EventOut, ForkMode, ForkTarget, ParamRamp, ParamSegment,
    Prepare, SegmentShape,
};
use tutti_mod::ControlProgram;
use tutti_nodes::automation::{
    AutomationEnvelope, AutomationPoint, ControlLaneNode, Curve, CurveType,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{At, Frame, NodeKey};

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 256;

/// The probe's gain parameter (`GAIN_PARAM_ID`), and its plain range in dB.
const GAIN: u32 = 77;
const GAIN_DB: (f64, f64) = (-60.0, 12.0);

/// A curve holding one value at every beat.
struct Constant(f32);

impl Curve for Constant {
    fn value_at(&self, _beat: Beat) -> Option<f32> {
        Some(self.0)
    }
}

/// The probe's output (its tag, 1.0 on channel 0) scaled by the gain a
/// normalized `v` sets.
fn gained(v: f64) -> f32 {
    let db = GAIN_DB.0 + v * (GAIN_DB.1 - GAIN_DB.0);
    10f64.powf(db / 20.0) as f32
}

/// **Automation reaches the plugin.** The probe renders its tag (1.0 on
/// channel 0) scaled by its gain parameter; a lane of automation alone
/// holding the gain at normalized 0.75 (-6 dB) feeds the plugin's event
/// input. Rendered in an export (an offline fork of both, rolling), so the
/// pipeline answers every chunk: once the first chunk has passed, the
/// output is 10^(-6/20).
///
/// Ported from the deleted `PluginAutomation` node.
///
/// Mutations (run), each fails with the gain left at 0 dB: the
/// plugin node ignoring segment events (the lane's) in its event walk;
/// decoding a segment's number in the other address model
/// (`address(id, !indexed)`), so the loader finds no parameter `Index(77)`.
/// The ramp path (the deleted node's) is pinned by
/// `an_interrupting_ramp_stops_a_segment_on_a_plugin`: ignoring ramp events
/// there fails (run); here it survives, as a lane sends segments.
#[test]
fn automation_reaches_the_plugin_through_its_event_input() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_ONLY)
        .gain_enabled(true);
    let probe = load_probe(SAMPLE_RATE);
    let automation = ControlLaneNode::for_foreign(
        GAIN,
        ControlProgram::from_automation(Arc::new(Constant(0.75))),
        None,
        (0.0, 1.0),
    );
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut live, _exec) = Editor::new(prepare);
    let (auto_key, key) = (NodeKey(1), NodeKey(9));
    let _automation = live.insert(auto_key, "automation", automation);
    let _controls = live.insert(key, "plugin", probe.client.bind());
    live.spec_mut().connect_events(
        EventIn { node: key, port: 0 },
        EventEdge::Direct(EventOut {
            node: auto_key,
            port: 0,
        }),
    );
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let (_fork_ed, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline, prepare)
        .expect("the lane and the plugin fork");

    let mut block = vec![0.0f32; BLOCK];
    for _ in 0..8 {
        timeline.render_graph(&mut fork_exec, BLOCK, &[], &mut [&mut block[..]]);
    }
    let last = *block.last().expect("rendered");
    assert!(
        (last - gained(0.75)).abs() < 1e-4,
        "{last} ≠ {} (-6 dB)",
        gained(0.75)
    );
}

// ---- curve segments ---------------------------------------------------------

/// The probe's output (channel 0, its tag scaled by its gain), `blocks`
/// blocks of an export of a graph with the probe at [`PLUGIN`] fed by
/// `feeder` (inserted at key 1, its event output on the plugin's event
/// input) when there is one, the `schedule` commands sent to the fork
/// before it renders. Also the delay through the plugin: output frame
/// `f + delay` plays input frame `f`. That is the pipeline's chunk, which
/// an offline fork sets to its `MaxBlock` ([`BLOCK`]); the probe's own
/// declared latency is not rendered in `TAG_ONLY` mode, so the figure is
/// the chunk, not `declared_latency`.
fn render_gain(
    feeder: Option<ControlLaneNode>,
    schedule: &[(u64, EventKind)],
    blocks: usize,
) -> (Vec<f32>, usize) {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_ONLY)
        .gain_enabled(true);
    let probe = load_probe(SAMPLE_RATE);
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut live, _exec) = Editor::new(prepare);
    let _controls = live.insert(PLUGIN, "plugin", probe.client.bind());
    if let Some(lane) = feeder {
        live.insert(NodeKey(1), "lane", lane);
        live.spec_mut().connect_events(
            EventIn {
                node: PLUGIN,
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
    }
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: PLUGIN,
        port: 0,
    })];
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let (mut fork_ed, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline, prepare)
        .expect("the graph forks");
    for &(frame, kind) in schedule {
        fork_ed
            .schedule(
                At::Frame(Frame(frame)),
                EventIn {
                    node: PLUGIN,
                    port: 0,
                },
                kind,
            )
            .expect("scheduled");
    }
    let mut out = Vec::new();
    let mut block = vec![0.0f32; BLOCK];
    for _ in 0..blocks {
        timeline.render_graph(&mut fork_exec, BLOCK, &[], &mut [&mut block[..]]);
        fork_ed.collect();
        out.extend_from_slice(&block);
    }
    (out, BLOCK)
}

const PLUGIN: NodeKey = NodeKey(9);

/// Whether `got` is the gain normalized `v` sets, to an `f32`'s precision.
fn is_gain(got: f32, v: f32) -> bool {
    (got - gained(f64::from(v))).abs() <= 1e-5 * gained(f64::from(v)).max(1e-3)
}

/// The output frames `L + start .. L + end` (input frames `start..end`)
/// follow `curve` — each frame's gain lies between the curve's value one
/// point spacing earlier and its value at the end of the `piece`-frame
/// span holding the frame, rising — and from input frame `end` on it is
/// exactly `to`'s gain, which it is not one frame earlier.
///
/// `piece` is 1 where every point is the curve's own value at its frame
/// (a scheduled segment), so a frame's gain is at most the curve's there.
/// A control lane sends the curve as `Linear` pieces between its 32-frame
/// grid frames (doc 014 §5.3), and a chunk's last frame carries the piece's
/// value there: a point on the chord from `curve(g)` to `curve(g + 32)`,
/// which on a convex rising curve lies above the curve but, the curve
/// being monotone, never above `curve(g + 32)`. So for a lane `piece` is
/// 32 and the upper bound is the curve at the next grid frame.
///
/// Changed in doc 014 §12.2: the upper bound was `curve(f)` for both
/// tests; the lane's chunk-edge point at frame 255 (on its 224..256 piece)
/// is the chord's value, 2.9e-5 relative above `curve(255)` and below
/// `curve(256)`.
fn follows(
    out: &[f32],
    latency: usize,
    start: usize,
    end: usize,
    piece: usize,
    curve: impl Fn(usize) -> f32,
    to: f32,
) {
    // The play clock's grid (doc 014 §5.3): a point at least every 32
    // frames, plus the chunk's last frame.
    const SPACING: usize = 32;
    let mut distinct = 0;
    for f in start..end {
        let got = out[f + latency];
        let hi = gained(f64::from(curve(f.next_multiple_of(piece).min(end))));
        let lo = gained(f64::from(curve(f.saturating_sub(SPACING).max(start))));
        assert!(
            got <= hi * (1.0 + 1e-5) && got >= lo * (1.0 - 1e-5),
            "input frame {f}: {got} not between {lo} and {hi}"
        );
        if f > start && got != out[f + latency - 1] {
            assert!(got > out[f + latency - 1], "input frame {f}: falls");
            distinct += 1;
        }
    }
    assert!(distinct >= 20, "{distinct} steps: not a curve");
    assert!(
        !is_gain(out[end - 1 + latency], to),
        "not there a frame early"
    );
    for f in end..out.len() - latency {
        assert!(
            is_gain(out[f + latency], to),
            "input frame {f}: {}",
            out[f + latency]
        );
    }
}

/// **A segment scheduled into a plugin reaches its end value.** A plugin
/// takes parameter points, so the node sends points along the segment in
/// every chunk (four 256-frame chunks here) and the end value on the
/// segment's last frame: the gain rises through the curve's values and
/// lands exactly on the end gain on its frame, and stays.
///
/// Mutations (run), each fails: the plugin node sending only the segment's
/// first point (no `SegmentPoints` tracking) → the gain stays near the
/// start; no point on the arrival frame → the end gain lands late, on the
/// next stride point. Mutation (run): the play-grid points dropped
/// (`on_grid` false in `SegmentPoints::points`) → the gain holds the start
/// until the chunk's last frame, "input frame 334" below the curve 32
/// frames earlier → fails.
#[test]
fn a_scheduled_segment_drives_a_plugin_parameter_to_its_end() {
    const AT: u64 = 300;
    let seg = ParamSegment::foreign(GAIN, 0.5, 0.75, Samples(1000), SegmentShape::PowerIn(2.0));
    let (out, latency) = render_gain(None, &[(AT, EventKind::Segment(seg))], 10);
    let end = AT as usize + seg.arrives_after().expect("arrives").get();
    follows(
        &out,
        latency,
        AT as usize,
        end,
        1,
        |f| seg.raw_value_after(Samples(f - AT as usize)),
        0.75,
    );
}

/// **An interrupting event wins.** A ramp to 0.25 landing mid-segment holds
/// the gain at 0.25 from its frame on: the segment sends no more points.
///
/// Mutation (run): the plugin node not interrupting a segment on a ramp
/// (`SegmentPoints::interrupt` not called) → the next stride point pulls
/// the gain back onto the segment → fails. Mutation (run):
/// the plugin ignoring ramp events (`params.point` not called) → the
/// segment's value is left → fails.
#[test]
fn an_interrupting_ramp_stops_a_segment_on_a_plugin() {
    const AT: u64 = 300;
    const CUT: u64 = 800;
    let seg = ParamSegment::foreign(GAIN, 0.5, 0.75, Samples(1000), SegmentShape::PowerIn(2.0));
    let ramp = ParamRamp::foreign(GAIN, 0.25, Samples(0));
    let (out, latency) = render_gain(
        None,
        &[(AT, EventKind::Segment(seg)), (CUT, EventKind::Ramp(ramp))],
        10,
    );
    assert!(!is_gain(out[CUT as usize - 1 + latency], 0.25));
    for f in CUT as usize..out.len() - latency {
        assert!(
            is_gain(out[f + latency], 0.25),
            "input frame {f}: {}",
            out[f + latency]
        );
    }
}

/// **An automation lane drives a plugin parameter along its curve.** A lane
/// sending an exponential envelope as segments (`for_foreign`) gives what a
/// scheduled segment gives: the gain follows the curve within one point
/// spacing and lands exactly on the end gain on the envelope's last frame.
///
/// Mutation (run): the plugin node sending only each segment's first point
/// → the gain moves only at the lane's block-edge re-sends, more than one
/// spacing behind the curve → fails. Mutation (run): each piece's end
/// value read one grid step late (`d + 32.0` in the lane's piece end) →
/// the chunk edge at frame 255 lies above `curve(256)` → fails.
#[test]
fn a_lane_drives_a_plugin_parameter_along_its_curve() {
    let mut env = AutomationEnvelope::new(());
    env.add_point(AutomationPoint::with_curve(
        0.0,
        0.5,
        CurveType::Exponential,
    ));
    // 120 BPM at 48 kHz: 24 000 frames a beat, so frame 1500.
    env.add_point(AutomationPoint::new(0.0625, 0.75));
    let lane = ControlLaneNode::for_foreign(
        GAIN,
        ControlProgram::from_automation(Arc::new(env.clone())),
        None,
        (0.0, 1.0),
    );
    let (out, latency) = render_gain(Some(lane), &[], 10);
    let beat = |f: usize| Beat(f as f64 / 24_000.0);
    follows(
        &out,
        latency,
        0,
        1500,
        32,
        |f| env.value_at(beat(f)).expect("a value"),
        0.75,
    );
}

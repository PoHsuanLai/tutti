//! [`PluginAutomation`]: a hosted plugin's parameter automation as an event
//! source node (doc 013, rewrite item 5).
//!
//! One [`Curve`] per plugin parameter ([`TimedParam`]), sampled each block at
//! the beat `Env` puts each frame at (`Env::transport_at`: the host's own
//! clock, loop wraps and mid-block starts included) and sent to the plugin
//! node's event input as [`ParamRamp::foreign`] events, one per point. The
//! plugin node turns them into its chunk's `ParameterChanges`; the edge
//! between the two is a graph edge, so the compiler's delay compensation
//! covers it like any other (defect D9).
//!
//! **Points.** At frame 0, every `stride` frames and the block's last frame,
//! the stride widening with the block so a parameter gets at most
//! `MAX_POINTS` per block. A stopped transport sends nothing: the plugin
//! keeps its last value, as a paused playhead freezes it. A curve with no
//! value there, or a value that is not finite, sends nothing at that point.
//!
//! **Addresses.** A plugin's parameters are either opaque handles (VST3,
//! CLAP, AU) or VST2 positional indices ([`ParamAddress`]); a ramp carries a
//! bare `u32`. The node is made for one plugin
//! ([`PluginControls::automation`](super::PluginControls::automation)), knows
//! which model it speaks, and refuses an address of the other model when it
//! is set; the plugin node reads the number back in its own model.
//!
//! **Fork.** A fork samples a frozen copy of every curve
//! ([`Curve::frozen`]): an immutable curve is shared, one that reads live
//! state (a [`PluginParamTarget`](super::PluginParamTarget) the mod router
//! writes) is copied as it stands, authored layers only.

use std::sync::Arc;

use tutti_core::RtPublish;
use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, ParamRamp,
    Prepare, Shape, Status,
};
use tutti_nodes::automation::Curve;
use tutti_types::{ChannelLayout, Samples};

use super::param_automation_source::{stride_for, MAX_POINTS};
use super::TimedParam;
use crate::protocol::{ParamAddress, ParamId};

/// Ramps one automation node sends per block, at most: past it the rest are
/// refused and counted (`Executor::dropped_events`). Ten points per
/// parameter for 400 parameters.
pub const AUTOMATION_EVENT_CAPACITY: u32 = 4_000;

/// One parameter, as the node samples it: the number its ramps carry and the
/// curve.
#[derive(Clone)]
struct Lane {
    id: u32,
    curve: Arc<dyn Curve>,
}

/// What the node and its controls share: the published lanes, and the
/// address model the plugin speaks.
struct Shared {
    lanes: RtPublish<Box<[Lane]>>,
    indexed: bool,
}

impl Shared {
    /// `params` as lanes in this plugin's address model; one of the other
    /// model is refused (logged), as a loader would refuse it.
    fn lanes(&self, params: impl IntoIterator<Item = TimedParam>) -> Box<[Lane]> {
        params
            .into_iter()
            .filter_map(|p| {
                let id = match (p.param_id, self.indexed) {
                    (ParamAddress::Opaque(id), false) => id.get(),
                    (ParamAddress::Index(i), true) => u32::try_from(i).ok()?,
                    (address, indexed) => {
                        tracing::warn!(
                            ?address,
                            indexed,
                            "plugin automation: an address of the other model is refused"
                        );
                        return None;
                    }
                };
                Some(Lane { id, curve: p.curve })
            })
            .collect()
    }
}

/// A plugin's parameter automation, as a graph node with one event output:
/// wire it to the plugin node's event input. See the module docs.
pub struct PluginAutomation {
    shared: Arc<Shared>,
}

impl PluginAutomation {
    /// Automation for a plugin that addresses its parameters by VST2 index
    /// (`indexed`) or by opaque handle.
    pub(super) fn new(params: impl IntoIterator<Item = TimedParam>, indexed: bool) -> Self {
        let mut shared = Shared {
            lanes: RtPublish::new(Box::default()),
            indexed,
        };
        let lanes = shared.lanes(params);
        shared.lanes = RtPublish::new(lanes);
        Self {
            shared: Arc::new(shared),
        }
    }
}

/// The host's handle on a [`PluginAutomation`] in a graph: replace its
/// curves. Cheap to clone; every clone reaches the same node.
#[derive(Clone)]
pub struct AutomationControls {
    shared: Arc<Shared>,
}

impl AutomationControls {
    /// Sample `params` from the next block on. Control thread.
    pub fn set_params(&self, params: impl IntoIterator<Item = TimedParam>) {
        self.shared
            .lanes
            .publish(Arc::new(self.shared.lanes(params)));
    }

    /// Send nothing from the next block on: the plugin keeps its values.
    pub fn clear(&self) {
        self.shared.lanes.publish(Arc::new(Box::default()));
    }

    /// How many parameters the node samples.
    pub fn len(&self) -> usize {
        self.shared.lanes.read().len()
    }

    /// Whether the node samples no parameter.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Node for PluginAutomation {
    /// No audio; one event output carrying parameter ramps.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(AUTOMATION_EVENT_CAPACITY)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let frames = io.frames();
        let lanes = self.shared.lanes.read();
        if frames == 0 || lanes.is_empty() {
            return Status::Modified;
        }
        let out = io.event_out(0);
        let (last, stride) = (frames - 1, stride_for(frames));
        let mut offset = 0;
        loop {
            if let Some(at) = cx.env.offset(offset) {
                let t = cx.env.transport_at(at);
                if t.playing {
                    let beat = t.beat();
                    for lane in lanes.iter() {
                        let Some(v) = lane.curve.value_at(beat).filter(|v| v.is_finite()) else {
                            continue;
                        };
                        // Refused past the capacity: counted by the executor.
                        let _ =
                            out.push(Event::ramp(at, ParamRamp::foreign(lane.id, v, Samples(0))));
                    }
                }
            }
            if offset == last {
                break;
            }
            offset = (offset + stride).min(last);
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A fork of a [`PluginAutomation`]: its lanes frozen as they stand.
struct AutomationFork {
    shared: Arc<Shared>,
}

impl ForkSource for AutomationFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let lanes: Box<[Lane]> = self
            .shared
            .lanes
            .read()
            .iter()
            .map(|l| Lane {
                id: l.id,
                curve: l.curve.frozen().unwrap_or_else(|| Arc::clone(&l.curve)),
            })
            .collect();
        let node = PluginAutomation {
            shared: Arc::new(Shared {
                lanes: RtPublish::new(lanes),
                indexed: self.shared.indexed,
            }),
        };
        Ok(Forked::new(Box::new(node)))
    }
}

impl IntoNode for PluginAutomation {
    type Controls = AutomationControls;

    fn into_parts(self) -> NodeParts<AutomationControls> {
        let controls = AutomationControls {
            shared: Arc::clone(&self.shared),
        };
        let fork = AutomationFork {
            shared: Arc::clone(&self.shared),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

/// The address a ramp's number names on a plugin that addresses its
/// parameters by VST2 index (`indexed`) or by opaque handle: the inverse of
/// [`PluginAutomation`]'s encoding.
pub(super) fn address(id: u32, indexed: bool) -> Option<ParamAddress> {
    if indexed {
        i32::try_from(id).ok().map(ParamAddress::Index)
    } else {
        Some(ParamAddress::Opaque(ParamId::new(id)))
    }
}

/// Add a point to `changes`' queue for `address`, making the queue if there
/// is room. A queue already holding `MAX_POINTS` has its last point replaced
/// (the latest value wins: a chunk spanning several blocks gets each block's
/// points, and the end of the chunk must stay exact). Nothing allocates:
/// past the inline queue count a new parameter is dropped.
pub(super) fn add_point(
    changes: &mut crate::protocol::ParameterChanges,
    address: ParamAddress,
    offset: i32,
    value: f32,
) {
    let queues = &mut changes.queues;
    let i = match queues.iter().position(|q| q.param_id == address) {
        Some(i) => i,
        None if queues.len() < queues.inline_size() => {
            queues.push(crate::protocol::ParameterQueue::new(address));
            queues.len() - 1
        }
        None => return,
    };
    let q = &mut queues[i];
    if q.points.len() >= MAX_POINTS {
        q.points.pop();
    }
    q.add_point(offset, f64::from(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ParameterChanges;
    use audio_automation::{AutomationEnvelope, AutomationPoint};
    use std::sync::Mutex;
    use tutti_core::{Beat, Bpm, NodeKey, SampleRate};
    use tutti_graph::{
        Editor, EventEdge, EventIn, EventKind, EventOut, Executor, ForkTarget, Transport,
    };

    const RATE: f64 = 48_000.0;

    /// Collects every ramp reaching its event input as `(frame, id, value)`.
    /// A fork's clone records into the same list.
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<(u64, u32, f32)>>>);

    impl Node for Sink {
        /// A silent mono output, so a fork targeting it has one.
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(1, 0)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(0.0);
            let mut seen = self.0.lock().unwrap();
            for e in io.events(0) {
                if let EventKind::Ramp(r) = e.kind {
                    let tutti_types::ParamAddr::Id(id) = r.addr() else {
                        panic!("automation ramps are foreign");
                    };
                    let v = r.foreign_target(id).unwrap();
                    seen.push((cx.env.frame.get() + u64::from(e.offset.get()), id, v));
                }
            }
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    type Seen = Arc<Mutex<Vec<(u64, u32, f32)>>>;

    fn rig(node: PluginAutomation, max: usize) -> (Editor, Executor, AutomationControls, Seen) {
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(max)));
        let controls = ed.insert(NodeKey(1), "automation", node);
        let seen = Seen::default();
        ed.insert(
            NodeKey(2),
            "sink",
            tutti_graph::ForkByClone(Sink(Arc::clone(&seen))),
        );
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(2),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
        ed.commit().expect("commits");
        (ed, exec, controls, seen)
    }

    /// 120 BPM at 48 kHz: 24 000 frames a beat.
    fn at(frame: u64, playing: bool) -> Transport {
        Transport::new(playing, Bpm(120.0), Beat(frame as f64 / 24_000.0), None)
    }

    /// A 0→1 ramp over 4 beats on parameter `id`.
    fn ramp(id: u32) -> TimedParam {
        let mut env: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0f32);
        env.add_point(AutomationPoint::new(0.0, 0.0));
        env.add_point(AutomationPoint::new(4.0, 1.0));
        TimedParam {
            param_id: ParamAddress::Opaque(ParamId::new(id)),
            curve: Arc::new(env),
        }
    }

    /// **Points at 0, every 8 frames and the last frame, valued at each
    /// frame's beat.** A 64-frame block with the transport at beat 1: 9
    /// points, the first 0.25, rising.
    ///
    /// Mutation: sample every point at the block's first beat → the values
    /// do not rise → fails. Mutation: skip the block's last frame → 8 points
    /// → fails.
    #[test]
    fn points_follow_the_beat_across_the_block() {
        let (_ed, mut exec, _c, seen) = rig(PluginAutomation::new([ramp(7)], false), 64);
        exec.process(64, &at(24_000, true), &[], &mut []);
        let seen = seen.lock().unwrap();
        let frames: Vec<u64> = seen.iter().map(|p| p.0).collect();
        // The executor's own frames (its first block); the values follow the
        // transport's beat.
        let mut want: Vec<u64> = (0..64).step_by(8).collect();
        want.push(63);
        assert_eq!(frames, want);
        assert!(seen.iter().all(|p| p.1 == 7));
        assert!((seen[0].2 - 0.25).abs() < 1e-6);
        let (a, b) = (seen[0].2, seen.last().unwrap().2);
        assert!((b - a - 63.0 / 24_000.0 / 4.0).abs() < 1e-6, "{a} → {b}");
    }

    /// **A stopped transport sends nothing**, so the plugin keeps its value.
    ///
    /// Mutation: ignore `playing` → points while stopped → fails.
    #[test]
    fn a_stopped_transport_sends_nothing() {
        let (_ed, mut exec, _c, seen) = rig(PluginAutomation::new([ramp(7)], false), 64);
        exec.process(64, &at(24_000, false), &[], &mut []);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// **A long block stays within `MAX_POINTS` a parameter**: the stride
    /// widens. A 4 096-frame block of two parameters sends 2 × 10 ramps, the
    /// last on the block's last frame.
    ///
    /// Mutation: a fixed stride of 8 → 513 points a parameter → fails.
    #[test]
    fn a_long_block_stays_within_the_point_budget() {
        let (_ed, mut exec, _c, seen) =
            rig(PluginAutomation::new([ramp(1), ramp(2)], false), 4_096);
        exec.process(4_096, &at(0, true), &[], &mut []);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2 * MAX_POINTS);
        assert_eq!(seen.last().unwrap().0, 4_095);
    }

    /// **A NaN never leaves the node**: a curve answering NaN sends nothing.
    ///
    /// Mutation: drop the finiteness filter → NaN ramps → fails.
    #[test]
    fn a_nan_curve_sends_nothing() {
        struct NanCurve;
        impl Curve for NanCurve {
            fn value_at(&self, _beat: Beat) -> Option<f32> {
                Some(f32::NAN)
            }
        }
        let nan = TimedParam {
            param_id: ParamAddress::Opaque(ParamId::new(7)),
            curve: Arc::new(NanCurve),
        };
        let (_ed, mut exec, _c, seen) = rig(PluginAutomation::new([nan], false), 64);
        exec.process(64, &at(0, true), &[], &mut []);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// **An address of the other model is refused; the node's own model
    /// round-trips.** On an indexed (VST2) plugin, `Index(3)` is sent as 3 and
    /// read back as `Index(3)`; `Opaque(9)` is refused. On an opaque plugin,
    /// the reverse.
    ///
    /// Mutation: accept any address → the refused one is sent → fails.
    #[test]
    fn an_address_of_the_other_model_is_refused() {
        let curve = ramp(0).curve;
        let p = |a: ParamAddress| TimedParam {
            param_id: a,
            curve: Arc::clone(&curve),
        };
        for indexed in [true, false] {
            let (own, other) = if indexed {
                (
                    ParamAddress::Index(3),
                    ParamAddress::Opaque(ParamId::new(9)),
                )
            } else {
                (
                    ParamAddress::Opaque(ParamId::new(3)),
                    ParamAddress::Index(9),
                )
            };
            let (_ed, mut exec, _c, seen) =
                rig(PluginAutomation::new([p(own), p(other)], indexed), 8);
            exec.process(8, &at(0, true), &[], &mut []);
            let seen = seen.lock().unwrap();
            assert!(seen.iter().all(|s| s.1 == 3), "indexed {indexed}: {seen:?}");
            assert_eq!(address(3, indexed), Some(own));
        }
    }

    /// **`set_params` and `clear` reach the running node** from the next
    /// block.
    ///
    /// Mutation: `set_params` not publishing → the old parameter keeps
    /// sending → fails.
    #[test]
    fn the_controls_replace_the_curves() {
        let (_ed, mut exec, controls, seen) = rig(PluginAutomation::new([ramp(1)], false), 8);
        controls.set_params([ramp(2)]);
        exec.process(8, &at(0, true), &[], &mut []);
        assert!(seen.lock().unwrap().iter().all(|s| s.1 == 2));
        controls.clear();
        seen.lock().unwrap().clear();
        exec.process(8, &at(8, true), &[], &mut []);
        assert!(seen.lock().unwrap().is_empty());
        assert!(controls.is_empty());
    }

    /// **Live modulation does not reach a fork.** A `PluginParamTarget` is
    /// live state the mod router writes every frame; a fork samples a frozen
    /// copy of its authored part (base and `AUTOMATION` layer as they stood,
    /// modulation layers dropped, doc 013 gap 7), and nothing written to the
    /// live target afterwards reaches it. The fork (the sink and what feeds
    /// it) sends 0.6 before and after the live target moves.
    ///
    /// Mutation: share the curve in the fork (`Arc::clone(&l.curve)`) → the
    /// fork reads the live modulation and the later writes → fails.
    #[test]
    fn a_fork_freezes_live_modulation_targets() {
        use tutti_nodes::{LayerKey, ModTarget};
        let target = Arc::new(super::super::PluginParamTarget::new(0.5, 0.0, 1.0));
        target.accumulate(LayerKey::AUTOMATION, 0.1);
        target.accumulate(LayerKey(3), 0.2);
        let param = TimedParam {
            param_id: ParamAddress::Opaque(ParamId::new(1)),
            curve: target.clone(),
        };
        let (mut live, _exec, _c, seen) = rig(PluginAutomation::new([param], false), 8);
        live.spec_mut().topology.outputs = vec![tutti_types::graph::Source::Node(
            tutti_types::graph::OutPort {
                node: NodeKey(2),
                port: 0,
            },
        )];
        live.commit().expect("commits");
        let prepare = *live.prepare();
        let (_fork, mut fork_exec) = live
            .fork(ForkTarget::Node(NodeKey(2)), ForkMode::Live, prepare)
            .expect("the automation node and the sink fork");
        let mut out = [0.0f32; 8];
        fork_exec.process(8, &at(0, true), &[], &mut [&mut out[..]]);
        target.accumulate(LayerKey::AUTOMATION, -0.4);
        target.accumulate(LayerKey(3), 0.3);
        fork_exec.process(8, &at(8, true), &[], &mut [&mut out[..]]);
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|s| (s.2 - 0.6).abs() < 1e-6),
            "base + authored automation, no modulation: {seen:?}"
        );
    }

    /// **The plugin side keeps a chunk's points within budget**: a queue at
    /// `MAX_POINTS` has its last point replaced, so the chunk's end value is
    /// the latest.
    ///
    /// Mutation: push past the budget → the queue spills → fails.
    #[test]
    fn add_point_replaces_the_last_point_at_the_budget() {
        let mut changes = ParameterChanges::new();
        let a = ParamAddress::Opaque(ParamId::new(1));
        for i in 0..(MAX_POINTS as i32 + 5) {
            add_point(&mut changes, a, i, i as f32 / 100.0);
        }
        let q = &changes.queues[0];
        assert_eq!(q.points.len(), MAX_POINTS);
        assert!(!q.points.spilled());
        assert_eq!(
            q.points.last().unwrap().sample_offset,
            MAX_POINTS as i32 + 4
        );
    }
}

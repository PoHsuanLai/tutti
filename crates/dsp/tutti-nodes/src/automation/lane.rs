//! [`AutomationLaneNode`]: a curve evaluated at the transport's beat, as a
//! graph node.

use std::sync::Arc;

use tutti_core::ChannelLayout;
use tutti_graph::{
    Cx, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, Shape,
    Status,
};
use tutti_types::{Beat, Latency, Tail};

use super::Curve;

/// An automation lane that evaluates a [`Curve`] against musical time.
///
/// A generator: no inputs, one output, the curve's value at the transport
/// beat of every frame, read from the block's `Env`
/// ([`tutti_graph::Env::for_each_beat`]). The lane holds no transport and
/// needs no edge: it is a pure function of the beat its block hands it, which
/// makes it per-sample accurate and lets an offline render drive it from its
/// own transport without any special casing. The beat is already
/// loop-wrapped by the transport, so the lane does not consult a loop range;
/// while the transport is stopped the lane holds the value at its beat.
///
/// **Arrival.** It has no inputs, so its compiled arrival is zero by
/// construction and the beat it reads is its block's own; a consumer behind
/// a latent path is aligned by the compiler, which delays this node's edge
/// into it like any other source's.
///
/// The curve is held behind `Arc<dyn Curve>` so the node stays cheap to clone
/// and agnostic to how the curve is stored — an [`AutomationEnvelope`], a
/// constant, an LFO shape.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]) with no controls: a live curve swap is a
/// respawn ([`set_curve`](Self::set_curve) takes `&mut self`). A fork reads
/// the curve's [`frozen`](Curve::frozen) copy when it has one (a curve that
/// reads state written after it was built), and shares the `Arc` otherwise
/// (an envelope, read-only once shared). It is a generator
/// ([`Tail::Unbounded`]), so the executor never skips it.
///
/// [`AutomationEnvelope`]: audio_automation::AutomationEnvelope
///
/// # Example
///
/// A four-beat ramp from silence to unity, read at beat 2 (the transport
/// stopped there), which is halfway along it.
///
/// ```
/// use tutti_core::{Beat, Bpm, SampleRate, Samples};
/// use tutti_graph::{Prepare, Solo, Transport};
/// use tutti_nodes::automation::{AutomationEnvelope, AutomationLaneNode, AutomationPoint};
///
/// let mut envelope: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0);
/// envelope.add_point(AutomationPoint::new(0.0, 0.0));
/// envelope.add_point(AutomationPoint::new(4.0, 1.0));
///
/// // No inputs, one output (the curve's value).
/// let lane = AutomationLaneNode::new(envelope);
/// let mut solo = Solo::new(lane, Prepare::new(SampleRate(48_000.0), Samples(64)));
/// solo.renderer_mut()
///     .set_transport(Transport::new(false, Bpm(120.0), Beat(2.0), None));
/// let out = solo.render(1);
/// assert!((out[0][0] - 0.5).abs() < 1e-3, "midpoint of a 0..1 ramp, got {}", out[0][0]);
/// ```
pub struct AutomationLaneNode {
    curve: Arc<dyn Curve>,
    last_value: f32,
}

/// Another name for [`AutomationLaneNode`], kept for consumers that spell
/// it this way. The lane is not generic over the envelope's label.
pub type LiveAutomationLane = AutomationLaneNode;

impl AutomationLaneNode {
    /// Builds a lane that evaluates `curve` at the transport beat.
    ///
    /// The curve is held behind an `Arc` and read on the audio thread, so it
    /// must be cheap to evaluate and must not allocate in `value_at`.
    pub fn new(curve: impl Curve + 'static) -> Self {
        Self {
            curve: Arc::new(curve),
            last_value: 0.0,
        }
    }

    /// Replaces the curve, allocating a new `Arc`.
    ///
    /// `&mut self`, so it cannot reach a node already live in the graph — a
    /// live curve swap goes through a respawn. Does not clear
    /// [`last_value`](Self::last_value).
    pub fn set_curve(&mut self, curve: impl Curve + 'static) {
        self.curve = Arc::new(curve);
    }

    /// Most recent value the lane emitted: after a block, the value at its
    /// last frame — the end-of-block value, not the average. A node reset
    /// clears it to zero. (A lane inside a graph belongs to the executor;
    /// this is for a lane driven by hand.)
    pub fn last_value(&self) -> f32 {
        self.last_value
    }

    /// Evaluates the curve at `beat` **without** recording it as the last
    /// value.
    ///
    /// Returns `0.0` where the curve has no value — before its first point, or
    /// on an empty envelope. Use [`update_to`](Self::update_to) to evaluate and
    /// record in one step.
    pub fn get_value_at(&self, beat: Beat) -> f32 {
        self.curve.value_at(beat).unwrap_or(0.0)
    }

    /// Evaluate at `beat` and record it as the last value.
    pub fn update_to(&mut self, beat: Beat) -> f32 {
        self.last_value = self.get_value_at(beat);
        self.last_value
    }

    /// A copy of this lane for a fork: the curve's
    /// [`frozen`](Curve::frozen) copy when it has one, the shared `Arc`
    /// otherwise (read-only once shared), and no last value.
    pub fn fork_fresh(&self) -> Self {
        Self {
            curve: self
                .curve
                .frozen()
                .unwrap_or_else(|| Arc::clone(&self.curve)),
            last_value: 0.0,
        }
    }
}

impl Node for AutomationLaneNode {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }

    /// Nothing is derived from the rate: the beat arrives in each block's
    /// `Env`.
    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let out = io.output(0);
        let curve = &*self.curve;
        cx.env
            .for_each_beat(|i, beat| out[i] = curve.value_at(beat).unwrap_or(0.0));
        if let Some(&last) = out.last() {
            self.last_value = last;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.last_value = 0.0;
    }
}

/// The fork source of a lane: [`AutomationLaneNode::fork_fresh`] of the lane
/// as inserted.
struct LaneFork(AutomationLaneNode);

impl ForkSource for LaneFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.0.fork_fresh())))
    }
}

/// Inserted with no controls and a fork that freezes a live curve.
impl IntoNode for AutomationLaneNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        let fork = LaneFork(self.clone());
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}

impl Clone for AutomationLaneNode {
    fn clone(&self) -> Self {
        Self {
            curve: Arc::clone(&self.curve),
            last_value: self.last_value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_automation::{AutomationEnvelope, AutomationPoint};
    use std::sync::atomic::Ordering;
    use tutti_core::{AtomicF32, Bpm, SampleRate, Samples};
    use tutti_graph::{Solo, Transport};

    fn ramp_envelope() -> AutomationEnvelope<&'static str> {
        let mut env: AutomationEnvelope<&str> = AutomationEnvelope::new("volume");
        env.add_point(AutomationPoint::new(0.0, 0.0));
        env.add_point(AutomationPoint::new(4.0, 1.0));
        env.add_point(AutomationPoint::new(8.0, 0.5));
        env
    }

    /// `frames` of `lane` through a graph at 48 kHz in one block, the
    /// transport `transport` at the block's start.
    fn render(lane: AutomationLaneNode, transport: Transport, frames: usize) -> Vec<f32> {
        let mut solo = Solo::new(lane, Prepare::new(SampleRate(48_000.0), Samples(frames)));
        solo.renderer_mut().set_transport(transport);
        solo.render(frames).remove(0)
    }

    fn stopped_at(beat: f64) -> Transport {
        Transport::new(false, Bpm(120.0), Beat(beat), None)
    }

    #[test]
    fn declares_no_inputs_and_one_output() {
        let shape = AutomationLaneNode::new(ramp_envelope()).shape();
        assert_eq!(shape.audio_in.count(), 0);
        assert_eq!(shape.audio_out.count(), 1);
        assert_eq!(shape.tail, Tail::Unbounded, "a generator is never skipped");
    }

    #[test]
    fn test_update_tracks_beat_position() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());

        assert!((lane.update_to(Beat(0.0)) - 0.0).abs() < 0.01);
        assert!((lane.update_to(Beat(2.0)) - 0.5).abs() < 0.01);
        assert!((lane.update_to(Beat(4.0)) - 1.0).abs() < 0.01);
        assert!((lane.update_to(Beat(6.0)) - 0.75).abs() < 0.01);
    }

    /// The transport hands an already-wrapped beat, so a lane at beat 6
    /// behaves identically whether or not a loop produced it.
    #[test]
    fn wrapped_beat_needs_no_loop_handling_in_the_lane() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        // A 4..8 loop wraps beat 10 to beat 6 in the transport.
        let wrapped = lane.update_to(Beat(6.0));
        assert!(
            (wrapped - 0.75).abs() < 0.01,
            "expected ~0.75, got {wrapped}"
        );
    }

    /// The node renders the curve at its block's beat.
    #[test]
    fn a_block_reads_the_curve_at_the_transport_beat() {
        let at = |beat| {
            render(
                AutomationLaneNode::new(ramp_envelope()),
                stopped_at(beat),
                1,
            )[0]
        };
        assert!((at(4.0) - 1.0).abs() < 0.01);
        assert!((at(0.0) - 0.0).abs() < 0.01);
        assert!((at(2.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_set_curve_changes_output() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        assert!((lane.update_to(Beat(2.0)) - 0.5).abs() < 0.01);

        let mut flat: AutomationEnvelope<&str> = AutomationEnvelope::new("flat");
        flat.add_point(AutomationPoint::new(0.0, 0.9));
        flat.add_point(AutomationPoint::new(8.0, 0.9));
        lane.set_curve(flat);

        assert!((lane.update_to(Beat(2.0)) - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_reset_clears_last_value() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        lane.update_to(Beat(4.0));
        assert!((lane.last_value() - 1.0).abs() < 0.01);

        Node::reset(&mut lane);
        assert_eq!(lane.last_value(), 0.0);
    }

    #[test]
    fn test_empty_envelope_returns_zero() {
        let empty: AutomationEnvelope<&str> = AutomationEnvelope::new("empty");
        let mut lane = AutomationLaneNode::new(empty);
        assert_eq!(lane.update_to(Beat(5.0)), 0.0);
        let empty: AutomationEnvelope<&str> = AutomationEnvelope::new("empty");
        let out = render(AutomationLaneNode::new(empty), stopped_at(5.0), 1);
        assert_eq!(out[0], 0.0);
    }

    /// A held beat holds the value for the whole block.
    #[test]
    fn test_process_fills_block_from_beat_input() {
        let out = render(
            AutomationLaneNode::new(ramp_envelope()),
            stopped_at(4.0),
            32,
        );
        for (i, val) in out.iter().enumerate() {
            assert!(
                (val - 1.0).abs() < 0.01,
                "Sample {i} expected ~1.0, got {val}"
            );
        }
    }

    /// After a block, `last_value` is the block's last frame.
    ///
    /// Mutation (run): drop the `self.last_value = last` in `process` →
    /// fails.
    #[test]
    fn test_process_updates_last_value() {
        let mut flat: AutomationEnvelope<&str> = AutomationEnvelope::new("flat");
        flat.add_point(AutomationPoint::new(0.0, 0.5));
        flat.add_point(AutomationPoint::new(8.0, 0.5));
        let mut d = tutti_graph::contract::Direct::new(
            AutomationLaneNode::new(flat),
            SampleRate(48_000.0),
            64,
        );
        assert_eq!(d.node.last_value(), 0.0);
        d.block();
        assert!(
            (d.node.last_value() - 0.5).abs() < 0.01,
            "Expected ~0.5, got {}",
            d.node.last_value()
        );
    }

    /// The whole point of the per-frame beat: the value moves WITHIN a
    /// block, following the transport, rather than being held
    /// block-constant.
    ///
    /// Mutation (run): evaluate every frame at `cx.env.transport.beat()`
    /// (the block's first beat) → the block holds 0.0 → fails.
    #[test]
    fn test_process_per_sample_varies() {
        // 120 BPM at 48 kHz, rolling from beat 0.
        let rolling = Transport::new(true, Bpm(120.0), Beat(0.0), None);
        let out = render(AutomationLaneNode::new(ramp_envelope()), rolling, 32);

        assert!((out[0] - 0.0).abs() < 0.001);
        let last = out[31];
        assert!(last > 0.0, "Expected > 0.0, got {last}");
        assert!(last < 0.01, "Expected small value, got {last}");
        for i in 1..32 {
            assert!(
                out[i] >= out[i - 1],
                "Sample {i} should be >= sample {}",
                i - 1
            );
        }
    }

    /// A curve that reads state written after it was built, and freezes it.
    struct Live(Arc<AtomicF32>);

    struct Fixed(f32);

    impl Curve for Fixed {
        fn value_at(&self, _: Beat) -> Option<f32> {
            Some(self.0)
        }
    }

    impl Curve for Live {
        fn value_at(&self, _: Beat) -> Option<f32> {
            Some(self.0.load(Ordering::Acquire))
        }

        fn frozen(&self) -> Option<Arc<dyn Curve>> {
            Some(Arc::new(Fixed(self.0.load(Ordering::Acquire))))
        }
    }

    /// A fork reads the curve's frozen copy: a write the live curve sees
    /// after the fork does not reach it.
    ///
    /// Mutation (run): `fork_fresh` sharing the `Arc` (ignoring `frozen`) →
    /// the fork renders the later 0.75 → fails.
    #[test]
    fn a_fork_freezes_a_live_curve() {
        let cell = Arc::new(AtomicF32::new(0.25));
        let lane = AutomationLaneNode::new(Live(Arc::clone(&cell)));
        let fork = lane.fork_fresh();
        cell.store(0.75, Ordering::Release);
        assert_eq!(render(fork, stopped_at(1.0), 1)[0], 0.25);
        assert_eq!(render(lane, stopped_at(1.0), 1)[0], 0.75);
    }
}

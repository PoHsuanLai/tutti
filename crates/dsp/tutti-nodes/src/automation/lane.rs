//! [`AutomationLaneNode`]: a curve evaluated at the transport's beat, as a
//! graph node.

use std::sync::Arc;

use audio_automation::CurveType;
use tutti_core::ChannelLayout;
use tutti_graph::{
    Cx, Env, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Offset,
    ParamSegment, Prepare, SegmentShape, Shape, Status,
};
use tutti_mod::CurveSegment;
use tutti_types::{Beat, Latency, ParamKey, SamplePosition, Samples, Tail, Unit};

use super::Curve;

/// Segments one event-output lane sends per block, at most: one where each
/// transport piece starts, one per breakpoint and per loop wrap. Past it the
/// rest are refused and counted (`Executor::dropped_events`).
pub const LANE_EVENT_CAPACITY: u32 = 128;

/// The [`SegmentShape`] an `audio-automation` [`CurveType`] is, or `None`
/// for the shapes the closed set does not carry.
///
/// | `CurveType` | `SegmentShape` |
/// |---|---|
/// | `Linear` | `Linear` |
/// | `Stepped` | `Hold` |
/// | `Exponential`, `QuadIn` (both `t²`) | `PowerIn(2)` |
/// | `Logarithmic` (`√t`) | `PowerIn(0.5)` |
/// | `CubicIn`, `QuartIn`, `QuintIn` | `PowerIn(3, 4, 5)` |
/// | `QuadOut` … `QuintOut` | `PowerOut(2 … 5)` |
/// | `SCurve`, `QuadInOut` (both quadratic) | `PowerInOut(2)` |
/// | `CubicInOut` … `QuintInOut` | `PowerInOut(3 … 5)` |
/// | `Circular`, `Back`, `Elastic`, `Bounce`, `Bezier` | none |
///
/// `Back`, `Elastic` and `Bounce` overshoot their endpoints and `Bezier`'s
/// two control values would not fit an event; `Circular` is a quarter
/// circle no power matches. A lane sends those pieces as a piecewise-linear
/// approximation, sampled at the edges of each block (see
/// [`AutomationLaneNode::for_param`]).
pub fn segment_shape(curve: CurveType) -> Option<SegmentShape> {
    use CurveType as C;
    use SegmentShape as S;
    Some(match curve {
        C::Linear => S::Linear,
        C::Stepped => S::Hold,
        C::Exponential | C::QuadIn => S::PowerIn(2.0),
        C::Logarithmic => S::PowerIn(0.5),
        C::CubicIn => S::PowerIn(3.0),
        C::QuartIn => S::PowerIn(4.0),
        C::QuintIn => S::PowerIn(5.0),
        C::QuadOut => S::PowerOut(2.0),
        C::CubicOut => S::PowerOut(3.0),
        C::QuartOut => S::PowerOut(4.0),
        C::QuintOut => S::PowerOut(5.0),
        C::SCurve | C::QuadInOut => S::PowerInOut(2.0),
        C::CubicInOut => S::PowerInOut(3.0),
        C::QuartInOut => S::PowerInOut(4.0),
        C::QuintInOut => S::PowerInOut(5.0),
        C::Circular | C::Back | C::Elastic | C::Bounce | C::Bezier(..) => return None,
    })
}

/// How an event-output lane builds a segment for its parameter: the typed
/// key (or foreign id) bound in, so the lane itself is not generic.
type MakeSegment = dyn Fn(f32, f32, SegmentShape) -> ParamSegment + Send + Sync;

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
/// **Two outputs to choose from.** [`new`](Self::new) renders the curve as
/// audio, one value per frame, for anything that reads a signal (an audio
/// param source, a meter). [`for_param`](Self::for_param) and
/// [`for_foreign`](Self::for_foreign) send it as **curve-segment events**
/// ([`ParamSegment`]) for a param port instead: one per breakpoint, which the
/// port evaluates per frame — see [`for_param`](Self::for_param).
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
    /// `None`: one audio output. `Some`: one event output of segments,
    /// built by this.
    events: Option<Arc<MakeSegment>>,
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
            events: None,
        }
    }

    /// A lane that sends `curve` to built-in parameter `key` as
    /// curve-segment events on one event output (no audio): wire it to a
    /// param port with `ParamFrom::Events`. The curve's values are `key`'s
    /// raw values.
    ///
    /// **What it sends.** It reads the transport per frame from its block's
    /// `Env`, as the audio lane does, and sends a [`ParamSegment`] wherever
    /// the curve's piece ([`Curve::segment_at`]) changes: at each block's
    /// first frame and each transport change (a start, stop, seek or tempo
    /// change), at each breakpoint, and at each loop wrap. A piece is sent
    /// with its endpoints, its shape ([`segment_shape`]) and the fractional
    /// position the frame is at along it (beats to frames at the piece's
    /// tempo), so the port renders the curve's own value at every frame,
    /// breakpoints that fall between frames included. Because a segment is
    /// absolute, the re-send at every block edge changes nothing on a
    /// continuing piece; it is what makes the lane a pure function of the
    /// transport, as the audio lane is.
    ///
    /// **What it cannot send exactly.** A piece whose shape has no
    /// [`segment_shape`], a curve that cannot say its pieces (`segment_at`
    /// is `None`: an LFO, a sum of layers, an envelope with a `step_size` or
    /// a clamp cutting into a piece) goes as linear segments between the
    /// curve's values at the edges of each block (and at a breakpoint or loop
    /// wrap inside it): the piecewise-linear fallback, exact at those edges.
    /// While stopped it sends the value at its beat, held. Where the curve
    /// has no value, or one that is not finite, it sends nothing and the
    /// port keeps the value it has, as a plugin keeps its last parameter
    /// value.
    pub fn for_param<U: Unit<Raw = f32> + 'static>(
        curve: impl Curve + 'static,
        key: ParamKey<U>,
    ) -> Self {
        Self::with_events(
            curve,
            Arc::new(move |from, to, shape| {
                ParamSegment::new(key, U::from_raw(from), U::from_raw(to), Samples(0), shape)
            }),
        )
    }

    /// As [`for_param`](Self::for_param), for a foreign unit's parameter
    /// `id` (in its own normalisation).
    pub fn for_foreign(curve: impl Curve + 'static, id: u32) -> Self {
        Self::with_events(
            curve,
            Arc::new(move |from, to, shape| ParamSegment::foreign(id, from, to, Samples(0), shape)),
        )
    }

    fn with_events(curve: impl Curve + 'static, make: Arc<MakeSegment>) -> Self {
        Self {
            curve: Arc::new(curve),
            last_value: 0.0,
            events: Some(make),
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
    /// last frame — the end-of-block value, not the average (for an event
    /// lane, the value its last segment gives that frame; unchanged by a
    /// block it sent nothing in). A node reset
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

    /// Evaluates at `beat` and records it as the last value.
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
            events: self.events.clone(),
        }
    }
}

/// Frames per beat at `tempo` and `rate`, when both are usable.
fn frames_per_beat(rate: f64, tempo: f64) -> Option<f64> {
    let usable = tempo.is_finite() && tempo > 0.0 && rate.is_finite() && rate > 0.0;
    usable.then(|| rate * 60.0 / tempo)
}

/// What an event lane sent last, to tell when to send again.
struct Sent {
    /// Send again at this frame of the block whatever the beat does (the
    /// end of a fallback piece).
    until_frame: usize,
    /// Send again once the beat reaches this (the piece's end).
    until_beat: f64,
}

impl AutomationLaneNode {
    /// The event lane's block: see [`for_param`](Self::for_param).
    fn send_segments(&mut self, env: &Env, io: &mut Io<'_>, make: &MakeSegment) {
        let frames = io.frames();
        let out = io.event_out(0);
        let curve = &*self.curve;
        let rate = env.sample_rate.get();
        // The segment sent last and its frame, for `last_value`.
        let mut last: Option<(usize, ParamSegment)> = None;
        let mut send = |i: usize, seg: ParamSegment| {
            if let Some(at) = env.offset(i) {
                // Refused past the capacity: counted by the executor.
                let _ = out.push(Event::segment(at, seg));
                last = Some((i, seg));
            }
        };
        for (start, piece) in env.segments() {
            let t = piece.transport;
            let first = start.index();
            let Some(fpb) = frames_per_beat(rate, t.tempo.get()).filter(|_| t.playing) else {
                // Stopped (or no usable tempo): the value at its beat, held.
                if let Some(v) = curve.value_at(t.beat()).filter(|v| v.is_finite()) {
                    send(first, make(v, v, SegmentShape::Linear));
                }
                continue;
            };
            let piece_last = first + piece.block_len.get() - 1;
            let loop_end = t.looping.map(|l| l.end.get());
            let mut sent: Option<Sent> = None;
            let mut prev = f64::NEG_INFINITY;
            piece.for_each_piece_beat(start, |i, beat| {
                let b = beat.get();
                let wrapped = b < prev;
                prev = b;
                if let Some(s) = &sent {
                    if !wrapped && i < s.until_frame && b < s.until_beat {
                        return;
                    }
                }
                // The last frame before beat `to` is reached from here.
                let before = |to: f64| {
                    let k = ((to - b) * fpb).ceil();
                    if k >= 1.0 {
                        i.saturating_add(k.min(usize::MAX as f64) as usize - 1)
                    } else {
                        i
                    }
                };
                let here = curve.segment_at(beat);
                let exact = match here {
                    Some(CurveSegment::Hold { value, until }) if value.is_finite() => {
                        send(i, make(value, value, SegmentShape::Linear));
                        Some(until.map_or(f64::INFINITY, Beat::get))
                    }
                    Some(CurveSegment::Move {
                        start,
                        end,
                        from,
                        to,
                        shape,
                    }) if from.is_finite() && to.is_finite() => match segment_shape(shape) {
                        Some(shape) => {
                            let (s, e) = (start.get(), end.get());
                            let seg = make(from, to, shape).resumed(
                                SamplePosition((b - s) * fpb),
                                SamplePosition((e - s) * fpb),
                            );
                            send(i, seg);
                            Some(e)
                        }
                        None => None,
                    },
                    _ => None,
                };
                sent = Some(match exact {
                    Some(until_beat) => Sent {
                        until_frame: usize::MAX,
                        until_beat,
                    },
                    None => {
                        // The piecewise-linear fallback: a line to the
                        // curve's value at the last frame before the next
                        // edge — the piece's end, a loop wrap or the end of
                        // this transport piece.
                        let piece_end = match here {
                            Some(CurveSegment::Move { end, .. }) => Some(end.get()),
                            Some(CurveSegment::Hold { until, .. }) => until.map(Beat::get),
                            None => None,
                        };
                        let mut e = piece_last;
                        for to in [piece_end, loop_end].into_iter().flatten() {
                            if to > b {
                                e = e.min(before(to));
                            }
                        }
                        let e = e.max(i);
                        let v0 = curve.value_at(beat);
                        let v1 = if e == i {
                            v0
                        } else {
                            Offset::new(e - first, piece.block_len)
                                .and_then(|o| curve.value_at(piece.transport_at(o).beat()))
                        };
                        let finite = |v: Option<f32>| v.filter(|v| v.is_finite());
                        if let (Some(v0), Some(v1)) = (finite(v0), finite(v1)) {
                            // From `v0` on this frame (position 0) to `v1`
                            // on frame `e`.
                            send(
                                i,
                                if e == i {
                                    make(v0, v0, SegmentShape::Linear)
                                } else {
                                    make(v0, v1, SegmentShape::Linear).resumed(
                                        SamplePosition(0.0),
                                        SamplePosition((e - i) as f64),
                                    )
                                },
                            );
                        }
                        Sent {
                            until_frame: e + 1,
                            until_beat: piece_end.unwrap_or(f64::INFINITY),
                        }
                    }
                });
            });
        }
        if let Some((i, seg)) = last {
            let j = u32::try_from(frames - 1 - i).unwrap_or(u32::MAX);
            self.last_value = seg.raw_value_after(j);
        }
    }
}

impl Node for AutomationLaneNode {
    /// One audio output ([`new`](Self::new)), or one event output
    /// ([`for_param`](Self::for_param)).
    fn shape(&self) -> Shape {
        if self.events.is_some() {
            return Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
                .with_events(0, 1)
                .with_event_capacity(LANE_EVENT_CAPACITY)
                .with_tail(Tail::Unbounded);
        }
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
        if let Some(make) = self.events.clone() {
            self.send_segments(cx.env, &mut io, &*make);
            return Status::Modified;
        }
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
            events: self.events.clone(),
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

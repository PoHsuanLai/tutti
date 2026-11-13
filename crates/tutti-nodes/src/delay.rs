//! The feedback delay, [`DelayNode`].
//!
//! [`DelayLine`](tutti_dsp::DelayLine) is the bare fractional-read ring;
//! [`DelayFamily`] is the delay as a family (the loop, the routing, the glides), and
//! this file holds the cells and how the graph reaches them. The read is fractional: a
//! delay whose time is modulated must interpolate between taps or it steps audibly.

use tutti_dsp::{
    DelayCapacity, DelayControls, DelayFamily, Family, Grows, InterpolationMode, MaxDelay, Reach,
    TAIL_FLOOR,
};
use tutti_types::{BusTopology, Feedback, SampleRate, Seconds};

use crate::reverb::{FamilyNode, NodeFamily, Rings};

/// A feedback delay of any width: `N` audio inputs, `N` outputs, one delay line per
/// channel.
///
/// A [`FamilyNode`] over [`DelayFamily`], the loop being the family's recurrence; this
/// file holds the cells and how the graph reaches them.
///
/// - [`mono`](Self::mono) and [`stereo`](Self::stereo): one and two channels, read with
///   [`Linear`](InterpolationMode::Linear) interpolation.
/// - [`new`](Self::new): any width up to one bus (16 channels), on a checked
///   [`BusTopology`], with the [`MaxDelay`] and the [`InterpolationMode`] of your own.
///   A cross-feedback [`Routing`](tutti_dsp::Routing) or times of the channels past the
///   first ([`ChannelTimes`](tutti_dsp::ChannelTimes)) are the family's:
///   [`DelayNode::live`] over a [`DelayFamily::with_routing`] and
///   [`with_times`](DelayFamily::with_times).
///
/// # The loop
///
/// Each channel recirculates its own delayed tap, scaled by the **feedback**, and, when
/// the family has a routing, the taps of the other channels, scaled by the
/// **cross-feedback**. Width 2 defaults to the swap routing `[[0, 1], [1, 0]]` (a stereo
/// ping-pong) and every other width to none, where the cross-feedback is inert. The
/// feedback and cross-feedback are bounded as a **pair**
/// ([`Feedback::stable_pair`]), so the loop gain stays under [`Feedback::MAX_STABLE`]
/// whatever they, or the routing, say.
///
/// The output is the delayed signal alone (the wet signal). The blend with the dry input
/// is the graph's: `Editor::set_mix` mixes the output with the input, which this node
/// does not delay (it declares no latency: the echo is the effect, and PDC must not delay
/// the rest of the mix to line up with it).
///
/// # The controls
///
/// The ports are in this order: **time**, **feedback**, **cross-feedback**.
///
/// - The **time** ([`DelayTime`](tutti_types::UnitParam::DelayTime), in seconds) is channel
///   0's, and clamped into `0..=max_delay`. The other channels follow it, or hold a time of
///   their own that is a construction constant ([`ChannelTimes`](tutti_dsp::ChannelTimes)).
///   A time the graph feeds drives **every** channel through one shared value, per frame
///   (the natural flanger control: the lines interpolate); a frame that is not finite falls
///   back to the time of the cell. A cell that moves glides, at no more than
///   [`DELAY_SLEW`](tutti_dsp::DELAY_SLEW) frames of delay per frame: a moving delay is a
///   pitch bend, so it is its speed that is bounded, whatever the distance.
/// - The **feedback** is per frame too when driven, clamped into the stable range; a cell
///   that moves glides over [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE).
/// - The **cross-feedback** is a cell the stream's 16-frame grid reads, and glides like the
///   feedback.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are a
/// [`ParamSet`](tutti_graph::ParamSet) over those three, and a fork of it starts from the
/// values last set through it. The graph prepares it at the device rate, which sizes the
/// lines (`ceil(max_delay * rate)` frames each), before its first block; a re-prepare
/// empties them. Its tail is [`Tail::Unknown`](tutti_types::Tail::Unknown): a recirculating
/// line rings for as long as its feedback says, so the executor never skips it.
pub type DelayNode = FamilyNode<DelayFamily>;

impl DelayNode {
    /// A delay on the bus `input`, as wide as it, holding up to `max_delay`, reading with
    /// `interpolation` and running at `controls`.
    ///
    /// The channels are linked in time and routed by the default for the width
    /// ([`DelayFamily::new`]). The lines are sized at the rate
    /// [`Node::prepare`](tutti_graph::Node::prepare) hands it, which is what makes
    /// `max_delay` hold at whatever rate the graph runs at. `new(BusTopology::STEREO, ..)`
    /// is [`stereo`](Self::stereo); an `n`-channel one on the engine's order for that width
    /// is `new(BusTopology::assumed(n)?, ..)`, which refuses a width past one bus. Allocates.
    #[must_use]
    pub fn new(
        input: BusTopology,
        max_delay: MaxDelay,
        interpolation: InterpolationMode,
        controls: DelayControls,
    ) -> Self {
        Self::live(DelayFamily::new(input, max_delay, interpolation), controls)
    }

    /// A mono delay (1 in, 1 out) with linear interpolation.
    #[must_use]
    pub fn mono(max_delay: MaxDelay, controls: DelayControls) -> Self {
        Self::new(
            BusTopology::MONO,
            max_delay,
            InterpolationMode::Linear,
            controls,
        )
    }

    /// A stereo delay (2 in, 2 out) with linear interpolation and the L-R swap routing
    /// (a ping-pong, once the cross-feedback is above zero). Channel 1 follows the time;
    /// a ping-pong spread is [`DelayNode::live`] over a [`DelayFamily`]
    /// [`with_times`](DelayFamily::with_times).
    #[must_use]
    pub fn stereo(max_delay: MaxDelay, controls: DelayControls) -> Self {
        Self::new(
            BusTopology::STEREO,
            max_delay,
            InterpolationMode::Linear,
            controls,
        )
    }
}

/// The delay's controls: the time and the feedback, which are read per frame when the
/// graph drives them, and the cross-feedback.
///
/// The ports are in this order: time, feedback, cross-feedback.
#[derive(Clone, Copy, Debug)]
pub struct DelaySet;

control_set! {
    DelaySet(_s): DelayControls = DelayControls::DEFAULT;
    live: (all);
    DelayTime => Seconds = time { fast },
    Feedback => Feedback = feedback { fast },
    CrossFeedback => Feedback = cross_feedback { whole };

    fn defaults(&self) -> DelayControls {
        DelayControls::DEFAULT
    }
}

/// A recirculating line rings for as long as its feedback says, which is a ceiling the
/// graph's bounce would have to render: it is declared [`Tail::Unknown`](tutti_types::Tail::Unknown)
/// and never narrowed, so the executor never skips it.
impl NodeFamily for DelayFamily {
    type Set = DelaySet;

    fn rings(&self) -> Rings {
        Rings::Unknown
    }
    const SAMPLING: tutti_dsp::Cadence = <Self as Family>::CADENCE;

    fn set(&self, _: &DelayControls) -> DelaySet {
        DelaySet
    }

    /// The lines of the family's own longest delay ([`DelayFamily::max_delay`], the
    /// [`Domain`](tutti_dsp::Domain) it is built to) at `rate`.
    fn capacity(&self, rate: SampleRate) -> DelayCapacity {
        let reach =
            Reach::new(rate, self.max_delay(), TAIL_FLOOR).expect("TAIL_FLOOR is a valid floor");
        self.reach(&reach)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_dsp::{ChannelTimes, Routing};
    use tutti_graph::contract::{assert_param_fork, drive, prepared};
    use tutti_graph::{Node, ParamNode};
    use tutti_types::{ChannelLayout, Latency, Samples, Tail, UnitParam};

    /// A rate at which a time of `n / 1024` seconds is exactly `n` frames.
    const RATE: SampleRate = SampleRate::of(1_024.0);
    const MAX: MaxDelay = MaxDelay::of(Seconds(1.0));

    fn controls(time: f64, feedback: f32, cross: f32) -> DelayControls {
        DelayControls {
            time: Seconds(time),
            feedback: Feedback(feedback),
            cross_feedback: Feedback(cross),
        }
    }

    /// `n` frames, as the time a delay of that many frames is asked for by.
    fn frames(n: f64) -> f64 {
        n / RATE.get()
    }

    fn bus(width: u16) -> BusTopology {
        BusTopology::assumed(ChannelLayout::from(width)).expect("a test width is one bus")
    }

    /// `node` prepared at [`RATE`], over `inputs` (one `Vec` per channel) frame by frame,
    /// `params[k]` on param port `k` when `Some` (so a change lands whole on its frame).
    fn frames_of(
        mut node: DelayNode,
        inputs: &[Vec<f32>],
        params: &[Option<&[f32]>],
    ) -> Vec<Vec<f32>> {
        node = prepared(node, RATE, 1024);
        let n = inputs[0].len();
        let mut out = vec![Vec::with_capacity(n); inputs.len()];
        for i in 0..n {
            let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[i..=i]).collect();
            let ps: Vec<Option<&[f32]>> = params.iter().map(|p| p.map(|v| &v[i..=i])).collect();
            for (o, v) in out.iter_mut().zip(drive(&mut node, RATE, &ins, &ps)) {
                o.push(v[0]);
            }
        }
        out
    }

    /// An impulse of `len` frames on each of `width` channels where `hot`.
    fn impulses(width: usize, len: usize, hot: &[usize]) -> Vec<Vec<f32>> {
        (0..width)
            .map(|c| {
                let mut v = vec![0.0; len];
                if hot.contains(&c) {
                    v[0] = 1.0;
                }
                v
            })
            .collect()
    }

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it, and the set has the three controls in the family's order.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = DelayNode::stereo(MAX, controls(0.05, 0.4, 0.2));
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [
                UnitParam::DelayTime,
                UnitParam::Feedback,
                UnitParam::CrossFeedback
            ]
        );
        assert_param_fork(node);
    }

    /// The node declares time, feedback and cross-feedback as its params in that order,
    /// each with its control as the base, and never changes the arity: a delay is as wide as
    /// it was built, in and out. It declares no latency (musical delay is not processing
    /// delay) and a tail that is unknown, since a feedback loop rings as long as it says.
    #[test]
    fn the_shape_declares_the_three_controls_and_no_latency() {
        let d = DelayNode::stereo(MAX, controls(0.01, 0.5, 0.25));
        let shape = d.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[
                UnitParam::DelayTime,
                UnitParam::Feedback,
                UnitParam::CrossFeedback
            ][..]
        );
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.latency, Latency::new(Samples(0)));
        assert_eq!(shape.tail, Tail::Unknown);
        assert_eq!(
            d.param_base(0),
            Some(0.01),
            "the time's base is its control"
        );
        assert_eq!(d.param_base(1), Some(0.5));
        assert_eq!(d.param_base(2), Some(0.25));
        assert_eq!(d.param_base(3), None, "no fourth param");
        let wide = DelayNode::new(
            bus(6),
            MAX,
            InterpolationMode::Linear,
            controls(0.01, 0.3, 0.0),
        );
        assert_eq!(
            (
                wide.shape().audio_in().count(),
                wide.shape().audio_out().count()
            ),
            (6, 6)
        );
        let mono = DelayNode::mono(MAX, controls(0.01, 0.3, 0.0));
        assert_eq!(mono.shape().audio_in().count(), 1);
    }

    /// A zero delay without feedback passes the input through.
    #[test]
    fn a_zero_delay_passes_the_input_through() {
        let node = DelayNode::mono(MAX, controls(0.0, 0.0, 0.0));
        let out = frames_of(node, &[vec![0.5]], &[]);
        assert_eq!(out[0][0], 0.5);
    }

    /// The echo is heard at the delay, and the stereo channels have times of their own when
    /// the family gives the right one (here 5 and 10 frames).
    #[test]
    fn the_stereo_channels_echo_at_their_own_times() {
        let family = DelayFamily::new(BusTopology::STEREO, MAX, InterpolationMode::Linear)
            .with_times(ChannelTimes::each(&[Seconds(frames(10.0))]).unwrap())
            .unwrap();
        let node = DelayNode::live(family, controls(frames(5.0), 0.0, 0.0));
        let out = frames_of(node, &impulses(2, 12, &[0, 1]), &[]);
        let heard = |c: &[f32]| c.iter().position(|&s| s != 0.0);
        assert_eq!((heard(&out[0]), heard(&out[1])), (Some(5), Some(10)));
    }

    /// An impulse on the left only is picked up by the right through the default swap
    /// routing, and by neither when the cross-feedback is zero.
    #[test]
    fn the_stereo_cross_feedback_reaches_the_other_channel() {
        let x = impulses(2, 41, &[0]);
        let out = frames_of(
            DelayNode::stereo(MAX, controls(frames(10.0), 0.0, 0.5)),
            &x,
            &[],
        );
        assert_eq!(
            out[1][20], 0.5,
            "the right hears the left's echo a period on, scaled"
        );
        let out = frames_of(
            DelayNode::stereo(MAX, controls(frames(10.0), 0.0, 0.0)),
            &x,
            &[],
        );
        assert!(out[1].iter().all(|&s| s == 0.0));
    }

    /// A 6-channel delay: an impulse on channel 3 echoes only on channel 3, and the
    /// cross-feedback (which has nowhere to go with no routing) is inert.
    #[test]
    fn wide_delay_channels_are_independent_without_a_routing() {
        let node = DelayNode::new(
            bus(6),
            MAX,
            InterpolationMode::Linear,
            controls(frames(10.0), 0.0, 0.9),
        );
        let out = frames_of(node, &impulses(6, 13, &[3]), &[]);
        for (c, ch) in out.iter().enumerate() {
            assert_eq!(ch.iter().any(|&s| s != 0.0), c == 3, "channel {c}");
        }
    }

    /// The time and feedback lanes are the time and feedback of each frame, **over the
    /// cells**: with the cells at zero, a lane held at `D` frames and `g` is the echo train
    /// of the definition, `g^k` at `(k + 1) D`, and nothing between.
    #[test]
    fn fed_lanes_are_the_time_and_feedback_over_the_cells() {
        let (d, g, len) = (16usize, 0.5f32, 16 * 8 + 3);
        let time = vec![frames(d as f64) as f32; len];
        let feedback = vec![g; len];
        let node = DelayNode::mono(MAX, controls(0.0, 0.0, 0.0));
        let out = frames_of(
            node,
            &impulses(1, len, &[0]),
            &[Some(&time), Some(&feedback)],
        );
        for (i, &y) in out[0].iter().enumerate() {
            let want = if i >= d && i % d == 0 {
                g.powi((i / d - 1) as i32)
            } else {
                0.0
            };
            assert!((y - want).abs() < 1e-6, "frame {i}: {y}, want {want}");
        }
    }

    /// A higher fed feedback is a longer and louder tail: the port drives the feedback.
    #[test]
    fn a_fed_feedback_drives_the_tail() {
        let run = |fb: f32| -> f32 {
            let node = DelayNode::stereo(MAX, controls(frames(10.0), 0.0, 0.0));
            let lane = vec![fb; 200];
            let out = frames_of(node, &impulses(2, 200, &[0, 1]), &[None, Some(&lane)]);
            out[0][50..].iter().map(|s| s * s).sum()
        };
        let (low, high) = (run(0.1), run(0.9));
        assert!(high > low * 2.0, "low {low}, high {high}");
    }

    /// A ring routing spins an echo around a six-channel field: channel 0's impulse
    /// recirculates into channel 1's line and nowhere else on the first lap.
    #[test]
    fn a_ring_routing_feeds_each_channel_into_the_next() {
        let b = bus(6);
        let family = DelayFamily::new(b, MAX, InterpolationMode::Linear)
            .with_routing(Routing::ring(b))
            .unwrap();
        let node = DelayNode::live(family, controls(frames(10.0), 0.0, 0.8));
        let out = frames_of(node, &impulses(6, 26, &[0]), &[]);
        let energy: Vec<f32> = out
            .iter()
            .map(|c| c[1..].iter().map(|s| s * s).sum())
            .collect();
        assert!(energy[0] > 0.5 && energy[1] > 0.1, "{energy:?}");
        assert!(energy[2..].iter().all(|&e| e == 0.0), "{energy:?}");
    }

    /// Whatever the matrix says, a loop cannot run away: a dense routing is normalized to
    /// rows summing to 1, and the feedbacks are bounded as a pair.
    #[test]
    fn a_dense_routing_at_full_feedback_stays_bounded() {
        let b = bus(4);
        let family = DelayFamily::new(b, MaxDelay::of(Seconds(0.1)), InterpolationMode::Linear)
            .with_routing(Routing::normalized(b, &[1.0; 16]).unwrap())
            .unwrap();
        let node = DelayNode::live(family, controls(0.002, 0.99, 0.99));
        let out = frames_of(node, &impulses(4, 8_001, &[0, 1, 2, 3]), &[]);
        let peak = out
            .iter()
            .flat_map(|c| &c[1..])
            .fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(
            peak.is_finite() && peak < 4.0,
            "the loop ran away: peak {peak}"
        );
    }

    /// A time past the longest the line holds reads as the longest, exactly: a cell at 5 s
    /// on a line of 100 ms at 44.1 kHz is heard at the line's 4410 frames.
    #[test]
    fn a_time_past_the_line_is_the_longest() {
        let rate = SampleRate::SR_44K1;
        let node = DelayNode::mono(MaxDelay::of(Seconds(0.1)), controls(5.0, 0.0, 0.0));
        let mut node = prepared(node, rate, 4_800);
        let x = impulses(1, 4_800, &[0]);
        let out = drive(&mut node, rate, &[&x[0]], &[]);
        assert_eq!(out[0].iter().position(|&s| s != 0.0), Some(4_410));
    }

    /// A re-prepare empties the lines: the delays in frames were the old rate's, so the
    /// echoes still in flight are gone.
    #[test]
    fn a_re_prepare_empties_the_lines() {
        let mut node = prepared(
            DelayNode::mono(MAX, controls(frames(50.0), 0.9, 0.0)),
            RATE,
            64,
        );
        let x = impulses(1, 64, &[0]);
        drive(&mut node, RATE, &[&x[0]], &[]);
        node = prepared(node, RATE, 64);
        let silence = [0.0f32; 64];
        let out = drive(&mut node, RATE, &[&silence], &[]);
        assert!(out[0].iter().all(|&s| s == 0.0));
    }

    /// A control the graph drives and the recurrence does not read per frame (the
    /// cross-feedback) is sampled on the stream's 16-frame grid, not at the block's last
    /// frame, so what is heard does not depend on how the stream is cut into blocks; and
    /// the lane is heard, since the cell it overrides is zero.
    #[test]
    fn a_driven_slow_control_is_sampled_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let lane: Vec<f32> = (0..len).map(|n| 0.5 * n as f32 / len as f32).collect();
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, driven: bool| -> Vec<f32> {
            let mut node = prepared(
                DelayNode::stereo(MAX, controls(frames(100.0), 0.3, 0.0)),
                RATE,
                1_024,
            );
            let mut out = Vec::new();
            for (n, chunk) in x.chunks(block).enumerate() {
                let at = n * block;
                let l = &lane[at..at + chunk.len()];
                let params = [None, None, driven.then_some(l)];
                out.extend(drive(&mut node, RATE, &[chunk, chunk], &params).remove(0));
            }
            out
        };
        let whole = render(1_024, true);
        assert_ne!(whole, render(1_024, false), "the lane is heard");
        for block in [1, 16, 64, 128] {
            assert_eq!(render(block, true), whole, "blocks of {block}");
        }
    }
}

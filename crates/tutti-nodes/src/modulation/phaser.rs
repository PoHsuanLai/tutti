//! Phaser: a swept all-pass chain, of any width.
//!
//! [`PhaserFamily`] is the algorithm (the chain, the sweep, the LFO); this file holds the
//! cells and how the graph reaches them. The one modulation effect here with no delay line:
//! it notches by phase cancellation, which is why its notches are fewer and unevenly spaced
//! compared with a flanger's.

use tutti_dsp::{Family, PhaserControls, PhaserFamily, Stages, Stagger, StaggerError, SweepRange};
use tutti_types::{BusTopology, Depth, Feedback, Hz, SampleRate};

use crate::reverb::{FamilyNode, NodeFamily, Rings};

/// Phaser of any width: `N` audio inputs, `N` outputs.
///
/// A [`FamilyNode`] over [`PhaserFamily`]: a chain of all-pass stages whose centre
/// frequency an internal LFO sweeps across a [`SweepRange`]. Each stage passes every
/// frequency at unity and shifts only phase; blending that against the dry signal (the
/// graph's mix, `Editor::set_mix`: the node outputs the wet signal alone) turns the shift
/// into cancellation notches, and sweeping the centre moves them. An all-pass chain alone
/// barely changes the magnitude, so a phaser wants a mix below one, deepest at one half.
///
/// **This is not a delay effect.** Unlike chorus and flanger it holds no delay line, so
/// its notches are unevenly spaced and fewer, the reason a phaser sounds hollower and less
/// metallic than a flanger. The [`Stages`] set how many notches there are (a pair each);
/// feedback deepens them.
///
/// One LFO serves every channel; each channel keeps its own all-pass state, so the channels
/// phase identically without bleeding into each other. Every channel sweeps in step by
/// default, which widens nothing by itself: a [`Stagger`] (`[0.0, 0.25]` cycles on a stereo
/// phaser sweeps the sides a quarter cycle apart) is what widens it.
///
/// - [`mono`](Self::mono) and [`stereo`](Self::stereo): `stages` stages sweeping 200 to
///   4000 Hz, no stagger, at your controls ([`PhaserControls::DEFAULT`]).
/// - [`new`](Self::new): any width on a checked [`BusTopology`] (an `n`-channel one on the
///   engine's order for that width is `BusTopology::assumed(n)?`), your own [`SweepRange`] and
///   [`Stagger`].
///
/// # The controls
///
/// The ports are in this order: **rate** ([`Rate`](tutti_types::UnitParam::Rate), [`Hz`]),
/// **depth** ([`Depth`](tutti_types::UnitParam::Depth), unitless) and [`Feedback`]. All
/// three are read on the stream's 16-frame grid, so what is heard does not depend on how the
/// stream is cut into blocks; a driven port is read at the grid's solve frame.
///
/// - The **rate** is how fast the notches sweep: phaser rates are slow, typically 0.1 to
///   2 Hz. **Floored at 0.01 Hz, a lane's value and a cell's alike.** It lands whole, and the
///   LFO's phase is continuous across it.
/// - The **depth** is a fraction of the sweep range, `0..=1` (a chorus's is seconds of
///   delay): 1 sweeps the whole range, 0 parks the notches at its bottom. A cell that moves
///   glides over [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE).
/// - The **feedback** is how much of the chain's output recirculates, clamped into
///   `0..=`[`Feedback::MAX_STABLE`]. A cell that moves glides over
///   [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE).
///
/// The sweep's top is held at 0.90 of Nyquist at the graph's rate, and the authored top is
/// kept, so a node prepared at a higher rate restores it.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)), `N` in and `N` out, with no latency.
/// Inserted, its controls are a [`ParamSet`](tutti_graph::ParamSet) over those three; a fork
/// starts from the values last set through it, with the LFO back at the start. The graph
/// prepares it at the device rate before its first block, and **a re-prepare restarts the
/// sweep**. Its tail is [`Tail::Unknown`](tutti_types::Tail::Unknown) (the feedback
/// recirculates), so it is never skipped.
pub type PhaserNode = FamilyNode<PhaserFamily>;

impl PhaserNode {
    /// A phaser on the bus `input`, as wide as it, with `stages` all-passes per channel
    /// sweeping `range`, its channels' sweeps offset by `stagger` and running at
    /// `controls`.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when `stagger` names an offset for each channel and not for
    /// `input`'s.
    pub fn new(
        input: BusTopology,
        stages: Stages,
        range: SweepRange,
        stagger: Stagger,
        controls: PhaserControls,
    ) -> Result<Self, StaggerError> {
        Ok(Self::live(
            PhaserFamily::new(input, stages, range, stagger)?,
            controls,
        ))
    }

    /// A mono phaser of `stages` stages sweeping [`SweepRange::DEFAULT`] at `controls`.
    #[must_use]
    pub fn mono(stages: Stages, controls: PhaserControls) -> Self {
        Self::unstaggered(BusTopology::MONO, stages, controls)
    }

    /// A stereo phaser of `stages` stages sweeping [`SweepRange::DEFAULT`] at `controls`, its
    /// channels in step.
    #[must_use]
    pub fn stereo(stages: Stages, controls: PhaserControls) -> Self {
        Self::unstaggered(BusTopology::STEREO, stages, controls)
    }

    fn unstaggered(input: BusTopology, stages: Stages, controls: PhaserControls) -> Self {
        Self::live(
            PhaserFamily::new(input, stages, SweepRange::DEFAULT, Stagger::NONE)
                .expect("a step fits any width"),
            controls,
        )
    }
}

/// The phaser's controls, in port order: rate, depth, feedback. Each lands whole on the
/// stream grid (the recurrence's own glides are the depth's and the feedback's: the reader
/// must not ramp them across the block as well).
#[derive(Clone, Copy, Debug)]
pub struct PhaserSet;

control_set! {
    PhaserSet(_s): PhaserControls = PhaserControls::DEFAULT;
    live: (all);
    // How fast the notches sweep: floored at 0.01 Hz, a lane's value and a cell's alike.
    Rate => Hz = rate { whole },
    // A fraction of the sweep range.
    Depth => Depth = depth { whole },
    Feedback => Feedback = feedback { whole };

    fn defaults(&self) -> PhaserControls {
        PhaserControls::DEFAULT
    }
}

/// The closed loop's ring-out has no bound a tail could be cut at, so it is declared
/// [`Tail::Unknown`](tutti_types::Tail::Unknown) and never narrowed or parked.
impl NodeFamily for PhaserFamily {
    type Set = PhaserSet;

    const SAMPLING: tutti_dsp::Cadence = <Self as Family>::CADENCE;

    fn rings(&self) -> Rings {
        Rings::Unknown
    }

    fn set(&self, _: &PhaserControls) -> PhaserSet {
        PhaserSet
    }

    fn capacity(&self, _: SampleRate) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::{assert_param_fork, drive, prepared};
    use tutti_graph::{Node, ParamNode};
    use tutti_types::{ChannelLayout, Latency, PhaseIncrement, Samples, Tail, UnitParam};

    const RATE: SampleRate = SampleRate(48_000.0);

    fn bus(width: u16) -> BusTopology {
        BusTopology::assumed(ChannelLayout::from(width)).expect("a test width is one bus")
    }

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it, and the set has the three controls in the family's order.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = PhaserNode::stereo(Stages::of(4), PhaserControls::DEFAULT);
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Rate, UnitParam::Depth, UnitParam::Feedback]
        );
        assert_param_fork(node);
    }

    /// The node declares rate, depth and feedback as its params in that order, each with its
    /// control as the base, as wide as it was built, in and out, with no latency (the sweep
    /// is the sound) and an unknown tail (the feedback recirculates).
    #[test]
    fn the_shape_declares_the_three_controls_no_latency_and_an_unknown_tail() {
        let node = prepared(
            PhaserNode::new(
                bus(3),
                Stages::of(5),
                SweepRange::DEFAULT,
                Stagger::NONE,
                PhaserControls::DEFAULT,
            )
            .unwrap(),
            RATE,
            64,
        );
        let shape = node.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Rate, UnitParam::Depth, UnitParam::Feedback][..]
        );
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.latency, Latency::new(Samples(0)));
        assert_eq!(shape.tail, Tail::Unknown);
        assert_eq!(node.param_base(0), Some(0.3), "the rate's base is its cell");
        assert_eq!(node.param_base(1), Some(0.5));
        assert_eq!(node.param_base(2), Some(0.5));
        assert_eq!(node.param_base(3), None, "no fourth param");
    }

    /// A stagger that names an offset per channel must name the bus's.
    #[test]
    fn a_stagger_of_the_wrong_length_is_refused_at_construction() {
        let two = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(0.5)]).unwrap();
        let new = |w| {
            PhaserNode::new(
                bus(w),
                Stages::of(4),
                SweepRange::DEFAULT,
                two,
                PhaserControls::DEFAULT,
            )
        };
        assert!(new(2).is_ok());
        assert_eq!(
            new(3).unwrap_err(),
            StaggerError::Length { width: 3, got: 2 }
        );
    }

    /// The textbook phase of one stage tuned to `hz`, from `H(z) = (c + z^-1) / (1 + c z^-1)`
    /// with `c = (tan(pi hz / sr) - 1) / (tan(pi hz / sr) + 1)`, at `at` Hz.
    fn stage_phase(hz: f64, at: f64) -> f64 {
        let t = (std::f64::consts::PI * hz / RATE.get()).tan();
        let c = (t - 1.0) / (t + 1.0);
        let w = std::f64::consts::TAU * at / RATE.get();
        let (zr, zi) = (w.cos(), -w.sin());
        let (nr, ni) = (c + zr, zi);
        let (dr, di) = (1.0 + c * zr, c * zi);
        (ni * dr - nr * di).atan2(nr * dr + ni * di)
    }

    /// The rate is **floored at 0.01 Hz, a lane's value and a cell's alike**: a rate of -3 Hz,
    /// written to the cell or fed on the port, is the 0.01 Hz sweep, which after 0.1 s has
    /// moved the LFO a thousandth of a cycle, so the centre of a 4-stage phaser at full depth
    /// over 200 to 4000 Hz is `200 + 3800 (1 + sin(2 pi 0.001)) / 2`. A 1 kHz sine comes out
    /// with the phase of four stages tuned there (a quasi-static prediction from the textbook
    /// stage, since the coefficient has barely moved). Running backwards at 3 Hz would have
    /// the centre near its trough, 0.3 of a cycle behind, and a different phase.
    #[test]
    fn a_rate_below_the_floor_is_the_floor_in_a_cell_and_on_a_lane() {
        let n = 4_800;
        let centre = 200.0 + 3_800.0 * 0.5 * (1.0 + (std::f64::consts::TAU * 0.001).sin());
        let wanted = 4.0 * stage_phase(centre, 1_000.0);
        let backwards = 200.0 + 3_800.0 * 0.5 * (1.0 + (-std::f64::consts::TAU * 0.3).sin());
        assert!(
            (4.0 * stage_phase(backwards, 1_000.0) - wanted).abs() > 0.1,
            "a backwards sweep would be heard"
        );
        let controls = PhaserControls {
            depth: Depth::of(1.0),
            feedback: Feedback::NONE,
            ..PhaserControls::DEFAULT
        };
        let x: Vec<f32> = (0..n + 64)
            .map(|i| (std::f64::consts::TAU * 1_000.0 * i as f64 / RATE.get()).sin() as f32)
            .collect();
        let render = |cell: Option<f32>, lane: Option<f32>| {
            let node = PhaserNode::mono(Stages::of(4), controls);
            if let Some(v) = cell {
                assert!(node.param_set().set(UnitParam::Rate, v));
            }
            let mut node = prepared(node, RATE, 64);
            let mut y = Vec::new();
            for chunk in x.chunks(64) {
                let rate = lane.map(|v| vec![v; chunk.len()]);
                let params = [rate.as_deref(), None, None];
                y.extend(drive(&mut node, RATE, &[chunk], &params).remove(0));
            }
            y
        };
        for (what, y) in [
            ("cell", render(Some(-3.0), None)),
            ("lane", render(None, Some(-3.0))),
        ] {
            let w = std::f64::consts::TAU * 1_000.0 / RATE.get();
            let worst = (n - 256..n)
                .map(|i| (f64::from(y[i]) - (w * i as f64 + wanted).sin()).abs())
                .fold(0.0, f64::max);
            assert!(worst < 5e-3, "a negative rate on a {what}: {worst}");
        }
    }

    /// A depth lane past 1 sweeps the whole range and no further: at 0.01 Hz after 0.1 s the
    /// LFO is a thousandth of a cycle on, so a 1 kHz sine through four stages comes out with
    /// the phase of four stages tuned to `200 + 3800 (1 + sin(2 pi 0.001)) / 2` Hz, the
    /// sweep at depth exactly 1 (a lane at 8 unclamped would put the centre at 15 kHz and more).
    #[test]
    fn a_depth_lane_past_one_sweeps_the_whole_range_and_no_further() {
        let n = 4_800;
        let centre = 200.0 + 3_800.0 * 0.5 * (1.0 + (std::f64::consts::TAU * 0.001).sin());
        let wanted = 4.0 * stage_phase(centre, 1_000.0);
        let unclamped = 200.0 + 3_800.0 * 0.5 * 8.0;
        assert!((4.0 * stage_phase(unclamped, 1_000.0) - wanted).abs() > 0.1);
        let controls = PhaserControls {
            rate: Hz(0.01),
            feedback: Feedback::NONE,
            ..PhaserControls::DEFAULT
        };
        let mut node = prepared(PhaserNode::mono(Stages::of(4), controls), RATE, 64);
        let x: Vec<f32> = (0..n + 64)
            .map(|i| (std::f64::consts::TAU * 1_000.0 * i as f64 / RATE.get()).sin() as f32)
            .collect();
        let mut y = Vec::new();
        for chunk in x.chunks(64) {
            let lane = vec![8.0f32; chunk.len()];
            let params = [None, Some(&lane[..]), None];
            y.extend(drive(&mut node, RATE, &[chunk], &params).remove(0));
        }
        let w = std::f64::consts::TAU * 1_000.0 / RATE.get();
        let worst = (n - 256..n)
            .map(|i| (f64::from(y[i]) - (w * i as f64 + wanted).sin()).abs())
            .fold(0.0, f64::max);
        assert!(worst < 5e-3, "depth driven to 8: {worst}");
    }

    /// A control the graph drives is sampled on the stream's 16-frame grid, not at the block's
    /// last frame, so what is heard does not depend on how the stream is cut into blocks
    /// (the node against itself under cuts, not a reference render); and the lane is heard,
    /// since the cell it overrides is a different depth.
    #[test]
    fn a_driven_slow_control_is_sampled_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let lane: Vec<f32> = (0..len)
            .map(|n| 0.1 + 0.9 * n as f32 / len as f32)
            .collect();
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, driven: bool| -> Vec<f32> {
            let mut node = prepared(
                PhaserNode::mono(Stages::of(6), PhaserControls::DEFAULT),
                RATE,
                1_024,
            );
            let mut out = Vec::new();
            for (n, chunk) in x.chunks(block).enumerate() {
                let at = n * block;
                let l = &lane[at..at + chunk.len()];
                let params = [None, driven.then_some(l), None];
                out.extend(drive(&mut node, RATE, &[chunk], &params).remove(0));
            }
            out
        };
        let whole = render(1_024, true);
        assert_ne!(whole, render(1_024, false), "the lane is heard");
        for block in [1, 16, 64, 128] {
            assert_eq!(render(block, true), whole, "blocks of {block}");
        }
    }

    /// A cell that moves lands on the stream's grid, whatever the blocks: the rate, the depth
    /// and the feedback written between two blocks at frame 1024 (a multiple of every block
    /// below) give the same render in blocks of 1, 16, 64, 128 and 1024 (the node against
    /// itself under cuts), because the reader takes each whole at the grid and the
    /// recurrence's glides are on the stream's clock, not a block's. And the move is heard.
    #[test]
    fn a_moved_cell_lands_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, moved: bool| -> Vec<f32> {
            let node = PhaserNode::mono(Stages::of(6), PhaserControls::DEFAULT);
            let set = node.param_set();
            let mut node = prepared(node, RATE, 1_024);
            let mut out = Vec::new();
            for (n, chunk) in x.chunks(block).enumerate() {
                if moved && n * block == 1_024 {
                    assert!(set.set(UnitParam::Rate, 8.0));
                    assert!(set.set(UnitParam::Depth, 0.2));
                    assert!(set.set(UnitParam::Feedback, 0.1));
                }
                out.extend(drive(&mut node, RATE, &[chunk], &[]).remove(0));
            }
            out
        };
        let whole = render(1_024, true);
        assert_ne!(whole, render(1_024, false), "the move is heard");
        for block in [1, 16, 64, 128] {
            assert_eq!(render(block, true), whole, "blocks of {block}");
        }
    }

    /// Feedback recirculates: after an impulse, a 6-stage phaser at 0.9 feedback is still
    /// ringing 500 frames on, where with none the all-pass chain's response has decayed away
    /// (an all-pass section's pole is its coefficient, `|c| < 1`, and the chain's impulse
    /// response is under 1e-3 well before that at these centres).
    #[test]
    fn feedback_keeps_the_chain_ringing() {
        let ring = |fb: f32| {
            let node = PhaserNode::mono(
                Stages::of(6),
                PhaserControls {
                    feedback: Feedback(fb),
                    ..PhaserControls::DEFAULT
                },
            );
            let mut node = prepared(node, SampleRate(44_100.0), 1_024);
            let mut x = [0.0f32; 1_024];
            x[0] = 1.0;
            let y = drive(&mut node, SampleRate(44_100.0), &[&x], &[]).remove(0);
            y[500..].iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert!(ring(0.9) > 1e-3, "feedback rings: {}", ring(0.9));
        assert!(ring(0.0) < 1e-3, "no feedback does not: {}", ring(0.0));
    }
}

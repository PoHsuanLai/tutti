//! Chorus and flanger: one width-generic swept delay, two presets.
//!
//! [`ModDelayFamily`] is the algorithm (the sweep, the LFO, the loop); this file holds the
//! cells and how the graph reaches them. A chorus sits at a long base delay (10 ms) so the
//! copy reads as a second voice thickening the sound, a flanger at a short one (1 ms) so
//! the copy combs against the original and the sweep moves the notches: the same node at
//! different numbers ([`Sweep`]), at any width.

use tutti_dsp::{
    Family, Grows, ModDelayCapacity, ModDelayControls, ModDelayFamily, Reach, Stagger,
    StaggerError, Sweep, TAIL_FLOOR,
};
use tutti_types::{BusTopology, Feedback, Hz, SampleRate, Seconds};

use crate::reverb::{FamilyNode, NodeFamily, Rings, TailCeiling};

/// A swept delay of any width, a chorus or a flanger by its [`Sweep`].
///
/// A [`FamilyNode`] over [`ModDelayFamily`]: each channel has its own delay line swept by
/// one LFO, staggered per channel by a [`Stagger`] (channel `c` runs `c` times a step ahead
/// of channel 0, or an offset of its own). At width 2 with the [`chorus`](Self::chorus) or
/// [`flanger`](Self::flanger) preset it is a stereo chorus or flanger.
///
/// - [`chorus`](Self::chorus), [`flanger`](Self::flanger) and
///   [`general_midi_chorus`](Self::general_midi_chorus): the presets, on a checked
///   [`BusTopology`] (an `n`-channel one on the engine's order for that width is
///   `BusTopology::assumed(n)?`, which refuses a width past one bus), starting at their
///   controls ([`ModDelayControls::CHORUS`] and kin).
/// - [`new`](Self::new): a [`Sweep`] and a [`Stagger`] of your own.
///
/// # The controls
///
/// The ports are in this order: **rate** ([`Rate`](tutti_types::UnitParam::Rate), [`Hz`]),
/// **depth** ([`Depth`](tutti_types::UnitParam::Depth), [`Seconds`] of delay sweep) and
/// [`Feedback`]. All three are read on the stream's 16-frame grid, so what is heard does not
/// depend on how the stream is cut into blocks; a driven port is read at the grid's solve
/// frame.
///
/// - The **rate** is how fast the delay sweeps: chorus and flanger rates are slow, typically
///   0.1 to 2 Hz, and faster reads as vibrato. **Floored at 0.01 Hz, a lane's value and a
///   cell's alike.** It lands whole, and the LFO's phase is continuous across it.
/// - The **depth** is a duration added to the base delay, unlike the phaser's unitless depth;
///   it is clamped into the sweep's range. A cell that moves glides, at no more than
///   [`DELAY_SLEW`](tutti_dsp::DELAY_SLEW) frames of delay per frame: a moving delay is a
///   pitch bend, so it is its speed that is bounded.
/// - The **feedback** is how much of the delayed signal recirculates, clamped into the
///   sweep's range; on a flanger it is the most characterful control. A cell that moves
///   glides over [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE).
///
/// The output is the wet signal alone; the blend with the dry input is the graph's
/// (`Editor::set_mix`; the node has no latency, so its aligned input is its input). At fully
/// wet a chorus's detuned copy stands alone and a flanger's notches (wet and dry
/// interfering) vanish, so both effects want a mix below one.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)), `N` in and `N` out, with zero latency:
/// the modulated delay is the effect's sound, not processing latency, and PDC would otherwise
/// delay every other path by the base delay. Inserted, its controls are a
/// [`ParamSet`](tutti_graph::ParamSet) over those three; a fork starts from the values last
/// set through it, with the LFO back at the start. The graph prepares it at the device rate,
/// which sizes the lines, before its first block, and a re-prepare restarts the sweep.
///
/// # Tail
///
/// [`Tail::Unknown`](tutti_types::Tail::Unknown) (a recirculating line), so it is never
/// skipped, unless the sweep's [`feedback_max`](Sweep::feedback_max) is [`Feedback::NONE`]
/// (the General MIDI chorus): then nothing recirculates, an output frame is made of input at
/// most [`max_delay`](Sweep::max_delay) old, and it declares that as its
/// [ceiling](Rings::Ceiling).
pub type ModDelayNode = FamilyNode<ModDelayFamily>;

impl ModDelayNode {
    /// A swept delay on the bus `input`, as wide as it, built to `sweep`, its channels'
    /// sweeps offset by `stagger` and running at `controls`.
    ///
    /// The lines are sized at the rate [`Node::prepare`](tutti_graph::Node::prepare) hands
    /// it, which is what makes the longest delay hold at whatever rate the graph runs at.
    /// Allocates.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when `stagger` names an offset for each channel and not for
    /// `input`'s.
    pub fn new(
        input: BusTopology,
        sweep: Sweep,
        stagger: Stagger,
        controls: ModDelayControls,
    ) -> Result<Self, StaggerError> {
        Ok(Self::live(
            ModDelayFamily::new(input, sweep, stagger)?,
            controls,
        ))
    }

    /// A chorus on the bus `input`: [`ModDelayFamily::chorus`] at
    /// [`ModDelayControls::CHORUS`].
    #[must_use]
    pub fn chorus(input: BusTopology) -> Self {
        Self::live(ModDelayFamily::chorus(input), ModDelayControls::CHORUS)
    }

    /// A flanger on the bus `input`: [`ModDelayFamily::flanger`] at
    /// [`ModDelayControls::FLANGER`].
    #[must_use]
    pub fn flanger(input: BusTopology) -> Self {
        Self::live(ModDelayFamily::flanger(input), ModDelayControls::FLANGER)
    }

    /// The General MIDI chorus on the bus `input`: [`ModDelayFamily::general_midi_chorus`] at
    /// [`ModDelayControls::GENERAL_MIDI_CHORUS`]. It has no feedback, so it declares a finite
    /// tail.
    #[must_use]
    pub fn general_midi_chorus(input: BusTopology) -> Self {
        Self::live(
            ModDelayFamily::general_midi_chorus(input),
            ModDelayControls::GENERAL_MIDI_CHORUS,
        )
    }
}

/// The swept delay's controls, in port order: rate, depth, feedback. Each lands whole on the
/// stream grid (the recurrence's own glides are the depth's, at a pitch bend's speed, and
/// the feedback's: the reader must not ramp them across the block as well).
#[derive(Clone, Copy, Debug)]
pub struct ModDelaySet;

control_set! {
    ModDelaySet(_s): ModDelayControls = ModDelayControls::DEFAULT;
    live: (all);
    // How fast the delay sweeps: floored at 0.01 Hz, a lane's value and a cell's alike.
    Rate => Hz = rate { whole },
    // The swing, in seconds of delay.
    Depth => Seconds = depth { whole },
    Feedback => Feedback = feedback { whole };

    fn defaults(&self) -> ModDelayControls {
        ModDelayControls::DEFAULT
    }
}

/// A recirculating line rings for as long as its feedback says, which is a ceiling the
/// graph's bounce would have to render: it is declared [`Tail::Unknown`](tutti_types::Tail::Unknown)
/// and never narrowed. A sweep with no feedback cannot recirculate: its ceiling is its
/// longest delay.
impl NodeFamily for ModDelayFamily {
    type Set = ModDelaySet;

    const SAMPLING: tutti_dsp::Cadence = <Self as Family>::CADENCE;

    fn rings(&self) -> Rings {
        if self.sweep().feedback_max() == Feedback::NONE {
            Rings::Ceiling(TailCeiling::from(self.max_delay()))
        } else {
            Rings::Unknown
        }
    }

    fn set(&self, _: &ModDelayControls) -> ModDelaySet {
        ModDelaySet
    }

    /// The lines of the sweep's own longest delay ([`ModDelayFamily::max_delay`], the
    /// [`Domain`](tutti_dsp::Domain) it is built to) at `rate`.
    fn capacity(&self, rate: SampleRate) -> ModDelayCapacity {
        let reach =
            Reach::new(rate, self.max_delay(), TAIL_FLOOR).expect("TAIL_FLOOR is a valid floor");
        self.reach(&reach)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::{assert_param_fork, drive, prepared};
    use tutti_graph::{Node, ParamNode};
    use tutti_types::{ChannelLayout, Latency, PhaseIncrement, Samples, Tail, UnitParam};

    const RATE: SampleRate = SampleRate::SR_48K;

    fn bus(width: u16) -> BusTopology {
        BusTopology::assumed(ChannelLayout::from(width)).expect("a test width is one bus")
    }

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it, and the set has the three controls in the family's order.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = ModDelayNode::flanger(BusTopology::STEREO);
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Rate, UnitParam::Depth, UnitParam::Feedback]
        );
        assert_param_fork(node);
    }

    /// The node declares rate, depth and feedback as its params in that order, each with its
    /// control as the base, as wide as it was built, in and out, with no latency (the sweep
    /// is the sound, not processing delay). A recirculating line's tail is unknown; the
    /// General MIDI chorus, which cannot recirculate, declares its longest delay, 5 ms, as
    /// a finite ceiling.
    #[test]
    fn the_shape_declares_the_three_controls_no_latency_and_its_tail() {
        let chorus = ModDelayNode::chorus(bus(3));
        let shape = chorus.shape();
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
        assert_eq!(
            chorus.param_base(0),
            Some(1.0),
            "the rate's base is its cell"
        );
        assert_eq!(chorus.param_base(1), Some(0.005));
        assert_eq!(chorus.param_base(2), Some(0.3));
        assert_eq!(chorus.param_base(3), None, "no fourth param");
        let flanger = ModDelayNode::flanger(BusTopology::MONO);
        assert_eq!(flanger.shape().tail, Tail::Unknown);
        let gm = prepared(
            ModDelayNode::general_midi_chorus(BusTopology::STEREO),
            RATE,
            64,
        );
        assert_eq!(gm.shape().tail, Tail::Finite(Samples(240)));
        assert_eq!(gm.shape().latency, Latency::new(Samples(0)));
    }

    /// A stagger that names an offset per channel must name the bus's.
    #[test]
    fn a_stagger_of_the_wrong_length_is_refused_at_construction() {
        let two = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(0.5)]).unwrap();
        assert!(
            ModDelayNode::new(
                BusTopology::STEREO,
                Sweep::CHORUS,
                two,
                ModDelayControls::CHORUS
            )
            .is_ok()
        );
        assert_eq!(
            ModDelayNode::new(bus(3), Sweep::CHORUS, two, ModDelayControls::CHORUS).unwrap_err(),
            StaggerError::Length { width: 3, got: 2 }
        );
    }

    /// The delay the node used at frame `n`, by feeding the ramp `x[n] = n` through a mono
    /// chorus with no feedback and reading `n - y[n]` (a linear read of a ramp is exact).
    fn delays(node: ModDelayNode, lane: Option<f32>, len: usize) -> Vec<f64> {
        let mut node = prepared(node, RATE, 64);
        let x: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let mut y = Vec::new();
        for chunk in x.chunks(64) {
            let rate = lane.map(|v| vec![v; chunk.len()]);
            let params = [rate.as_deref(), None, None];
            y.extend(drive(&mut node, RATE, &[chunk], &params).remove(0));
        }
        y.iter()
            .enumerate()
            .map(|(n, &v)| n as f64 - f64::from(v))
            .collect()
    }

    fn mono_chorus_without_feedback() -> ModDelayNode {
        let node = ModDelayNode::chorus(BusTopology::MONO);
        assert!(node.param_set().set(UnitParam::Feedback, 0.0));
        node
    }

    /// The rate is **floored at 0.01 Hz, a lane's value and a cell's alike**: a rate of
    /// -3 Hz, written to the cell or fed on the port, is the 0.01 Hz sweep, which has raised
    /// the 10 ms base by `5 ms * sin(2 pi 0.01 * 0.1 s)`, 1.508 frames, after 0.1 s. A
    /// negative cell used to run the LFO backwards (the delay would be 1.508 frames
    /// shorter).
    #[test]
    fn a_rate_below_the_floor_is_the_floor_in_a_cell_and_on_a_lane() {
        let figure = 0.005 * 48_000.0 * (std::f64::consts::TAU * 0.01 * 0.1).sin();
        let at = 4_800;
        let on_cell = {
            let node = mono_chorus_without_feedback();
            assert!(node.param_set().set(UnitParam::Rate, -3.0));
            delays(node, None, at + 64)
        };
        let on_lane = delays(mono_chorus_without_feedback(), Some(-3.0), at + 64);
        for (what, d) in [("cell", on_cell), ("lane", on_lane)] {
            assert!(
                (d[at] - (480.0 + figure)).abs() < 0.05,
                "a negative rate on a {what}: delay {}, the 0.01 Hz sweep {}",
                d[at],
                480.0 + figure
            );
        }
    }

    /// Channel 3 of a 6-wide chorus (quarter-cycle stagger) sweeps a full cycle from channel
    /// 0, and a silent channel stays silent: no channel reads another's line.
    #[test]
    fn six_channels_are_six_swept_lines_and_a_silent_one_stays_silent() {
        let mut node = prepared(ModDelayNode::chorus(bus(6)), RATE, 128);
        assert!(node.param_set().set(UnitParam::Feedback, 0.0));
        let len = 4_096usize;
        let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let silent = vec![0.0f32; len];
        let ins: [&[f32]; 6] = [&ramp, &ramp, &ramp, &silent, &ramp, &ramp];
        let mut out = vec![Vec::new(); 6];
        for at in (0..len).step_by(128) {
            let chunk: Vec<&[f32]> = ins.iter().map(|c| &c[at..at + 128]).collect();
            for (o, b) in out.iter_mut().zip(drive(&mut node, RATE, &chunk, &[])) {
                o.extend(b);
            }
        }
        assert!(
            out[3].iter().all(|&s| s == 0.0),
            "no bleed into a silent channel"
        );
        // Channel 4's stagger is 4 quarter cycles: a whole one.
        assert_eq!(out[4], out[0], "a full-cycle stagger is no stagger");
        assert_ne!(out[2], out[0], "a half-cycle stagger differs");
        assert_ne!(out[1], out[0], "a quarter-cycle stagger differs");
    }

    /// A control the graph drives is sampled on the stream's 16-frame grid, not at the
    /// block's last frame, so what is heard does not depend on how the stream is cut into
    /// blocks (the node against itself under cuts, not a reference render); and the lane
    /// is heard, since the cell it overrides is a different depth.
    #[test]
    fn a_driven_slow_control_is_sampled_on_the_grid_whatever_the_blocks() {
        let len = 4_096;
        let lane: Vec<f32> = (0..len)
            .map(|n| 0.001 + 0.009 * n as f32 / len as f32)
            .collect();
        let x: Vec<f32> = (0..len)
            .map(|n| ((n * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let render = |block: usize, driven: bool| -> Vec<f32> {
            let mut node = prepared(ModDelayNode::chorus(BusTopology::MONO), RATE, 1_024);
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
}

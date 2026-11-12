//! The noise gate [`GateNode`]: closes below a threshold, keyed from a side-chain or from
//! its own signal.

use tutti_dsp::{GateControls, GateFamily, KeySource};
use tutti_types::{BusTopology, Db, SampleRate, Seconds};

use crate::reverb::{FamilyNode, NodeFamily, Rings};

/// A noise gate: `channels = N` means N audio inputs, then (for a side-chain key) N key
/// inputs, and N outputs, with one **linked** open/closed decision from the loudest key
/// channel applied to every output.
///
/// A [`FamilyNode`] over [`GateFamily`], the detector and the gain being the family's
/// recurrence; this file holds the cells and how the graph reaches them.
///
/// # The key
///
/// The detector listens to a [`KeySource`], chosen when the node is built:
///
/// - [`mono`](Self::mono) and [`stereo`](Self::stereo) (and [`new`](Self::new) with
///   [`KeySource::Sidechain`]) have a **side-chain** key: a second bus, as wide as the
///   signal, after the audio inputs.
///   **It must be wired**: the graph feeds an unconnected input silence, and a gate keyed
///   by silence stays closed (at its range). Wire it with a bus edge, for example
///   `graph.connect(kick.bus_out(0), gate.bus_in(1))` in `tutti-session`, or each key
///   channel into the inputs after the audio.
/// - [`new`](Self::new) with [`KeySource::Own`] has no key bus: the gate listens to its own
///   signal, as an ordinary gate does, and needs nothing wired.
///
/// The node declares no tail: the release decays after the key goes silent, but it only
/// scales, so a silent input is a silent output.
///
/// # Controls
///
/// Threshold, attack, hold, release and range are live [`Params`](crate::Params) shared
/// across clones, set through the node's [`ParamSet`](tutti_graph::ParamSet) or the
/// graph's `set_param` (a time under 0 reads as 0 and a range over 0 dB as 0 dB).
///
/// **The threshold is read per frame** when the graph drives it: a lane on its port is the
/// recurrence's own ([`Feed`](tutti_dsp::Feed) lane 0), and the cell is the base a frame
/// nobody drives (or a non-finite one) is compared against. **Every other control is on
/// the stream grid** (16 frames from the last reset,
/// [`Family::CADENCE`](tutti_dsp::Family::CADENCE)): a cell write or a driven port is
/// sampled at the grid's solve frame, so it reaches the render within 16 frames and the
/// render does not depend on how the stream is cut into blocks. The times land whole (the
/// follower smooths them), and the range glides over
/// [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE), 5 ms.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from
/// the values last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares
/// it at the device rate before its first block.
pub type GateNode = FamilyNode<GateFamily>;

impl GateNode {
    /// A gate on the bus `input`, its detector on `key`, running at `controls`.
    ///
    /// The main bus keeps its speaker placement; a [`KeySource::Sidechain`] is a second bus
    /// as wide, which is a level to read, not a placement, so it has none. **A side-chain key
    /// must be wired** (see the [type](GateNode)); a [`KeySource::Own`] gate listens to its
    /// own signal and has no key bus. `new(BusTopology::STEREO, ..)` is
    /// [`stereo`](Self::stereo) for a side-chain; an `n`-channel one on the engine's order for
    /// that width is `new(BusTopology::assumed(n)?, ..)`, which refuses a width past one bus.
    ///
    /// The controls are as [`GateControls`] says, and are the cells' values: the threshold
    /// (the key level in [`Db`] at or above which the gate opens), the times (`attack` how
    /// fast it opens, `hold` how long it stays open after the key falls back below the
    /// threshold, which is what stops a gate chattering on a key hovering at it, and
    /// `release` how fast it then closes) and the `range`, how far a closed gate attenuates.
    ///
    /// **The range is not defaulted for you.** [`GateControls::DEFAULT`] has a range of
    /// 0 dB, a gate that passes everything (what a non-finite control falls back to), where
    /// the gate this node has always been built as closes to **-80 dB**, effectively
    /// silent. A caller that wants a gate writes the range:
    /// `GateControls { threshold, range: Db(-80.0), ..GateControls::DEFAULT }`. A gentler
    /// `-12.0` ducks the signal without removing it, which sounds more natural on drums and
    /// room mics.
    ///
    /// The attack and release coefficients and the hold count are derived from the times at
    /// the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    #[must_use]
    pub fn new(input: BusTopology, key: KeySource, controls: GateControls) -> Self {
        Self::live(GateFamily::new(input, key), controls)
    }

    /// Mono + mono side-chain: 2 inputs (audio, key), 1 output.
    ///
    /// **The key is a side-chain that must be wired** (see the [type](GateNode)); for a gate
    /// that listens to its own signal use [`new`](Self::new) with [`KeySource::Own`].
    #[must_use]
    pub fn mono(controls: GateControls) -> Self {
        Self::new(BusTopology::MONO, KeySource::Sidechain, controls)
    }

    /// Stereo + stereo side-chain: 4 inputs (L, R, key L, key R), 2 outputs.
    ///
    /// The gate is **linked**: one open/closed decision from the loudest key channel,
    /// applied to both, so the two channels always gate together. The key is as
    /// [`mono`](Self::mono)'s: a side-chain that must be wired.
    #[must_use]
    pub fn stereo(controls: GateControls) -> Self {
        Self::new(BusTopology::STEREO, KeySource::Sidechain, controls)
    }
}

/// The gate's controls: threshold, attack, hold, release and range.
#[derive(Clone, Copy, Debug)]
pub struct GateSet;

control_set! {
    GateSet(_s): GateControls = GateControls::DEFAULT;
    live: (all);
    Threshold => Db = threshold { fast },
    Attack => Seconds = attack { whole },
    Hold => Seconds = hold { whole },
    Release => Seconds = release { whole },
    // The range glides in the recurrence, over `GAIN_GLIDE`: the reader must not ramp it
    // across the block as well.
    Range => Db = range { whole };

    fn defaults(&self) -> GateControls {
        GateControls::DEFAULT
    }
}

/// A gate rings on only as a gain (it scales its input), so it declares no tail and never
/// parks: its follower must keep tracking the key, which the main input's silence says
/// nothing of.
impl NodeFamily for GateFamily {
    type Set = GateSet;

    fn rings(&self) -> Rings {
        Rings::Silent
    }
    const SAMPLING: tutti_dsp::Cadence = <Self as tutti_dsp::Family>::CADENCE;

    fn set(&self, _: &GateControls) -> GateSet {
        GateSet
    }

    fn capacity(&self, _: SampleRate) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE_44K, prepared_at, tick, tick_fed};
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode};
    use tutti_types::UnitParam;

    /// The controls of a gate the tests name four of: the closed floor is -80 dB (the
    /// node's range before it was a control), not `GateControls::DEFAULT`'s 0 dB.
    fn four(threshold: f32, attack: f64, hold: f64, release: f64) -> GateControls {
        GateControls {
            threshold: Db::new(threshold),
            attack: Seconds::new(attack),
            hold: Seconds::new(hold),
            release: Seconds::new(release),
            range: Db(-80.0),
        }
    }

    fn with_range(c: GateControls, range: f32) -> GateControls {
        GateControls {
            range: Db(range),
            ..c
        }
    }

    fn gate(threshold: f32, attack: f64, hold: f64, release: f64) -> GateNode {
        GateNode::mono(four(threshold, attack, hold, release))
    }

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(GateNode::stereo(with_range(
            four(-30.0, 0.001, 0.01, 0.1),
            -20.0,
        )));
    }

    /// A block longer than 64 frames renders whole: the gain lane is sized from the
    /// prepared `MaxBlock`, not a constant.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(&mut node, RATE_44K, &[&audio, &sc], &[]).remove(0);
        assert!(out[1023] > 0.3, "the loud sidechain opened the gate");
    }

    /// A new gate is closed: the first frames are the signal at the range, -80 dB.
    #[test]
    fn a_new_gate_is_closed() {
        let mut node = prepared_at(gate(-30.0, 0.001, 0.01, 0.1), RATE_44K);
        let mut out = [0.0f32];
        tick(&mut node, &[0.5, 0.0], &mut out);
        assert!((out[0] / (0.5 * 1.0e-4) - 1.0).abs() < 1e-4, "{}", out[0]);
    }

    #[test]
    fn a_loud_side_chain_opens_the_gate() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..500 {
            tick(&mut node, &[0.5, 0.9], &mut out);
        }
        assert!((out[0] - 0.5).abs() < 1e-3, "open: {}", out[0]);
    }

    #[test]
    fn a_quiet_side_chain_closes_the_gate() {
        let mut node = prepared_at(
            GateNode::mono(with_range(four(-20.0, 0.001, 0.001, 0.001), -60.0)),
            RATE_44K,
        );
        let mut out = [0.0f32];
        for _ in 0..500 {
            tick(&mut node, &[0.5, 0.9], &mut out);
        }
        assert!(out[0] > 0.45);
        for _ in 0..2000 {
            tick(&mut node, &[0.5, 0.01], &mut out);
        }
        assert!(out[0] < 0.01, "closed to -60 dB: {}", out[0]);
    }

    /// A range written over 0 dB reads as 0 dB: a closed gate that gains nothing and
    /// loses nothing.
    #[test]
    fn a_range_over_zero_reads_as_zero() {
        let mut node = prepared_at(
            GateNode::mono(with_range(four(-20.0, 0.001, 0.01, 0.1), 10.0)),
            RATE_44K,
        );
        let mut out = [0.0f32];
        for _ in 0..100 {
            tick(&mut node, &[0.5, 0.0], &mut out);
        }
        assert_eq!(out[0], 0.5);
        assert!(node.param_set().set(UnitParam::Range, 6.0));
        for _ in 0..100 {
            tick(&mut node, &[0.5, 0.0], &mut out);
        }
        assert_eq!(out[0], 0.5);
    }

    /// With a -12 dB range a closed gate attenuates the signal by that figure and does not
    /// mute it.
    #[test]
    fn the_range_attenuates_rather_than_mutes() {
        let mut node = prepared_at(
            GateNode::mono(with_range(four(-20.0, 0.001, 0.001, 0.001), -12.0)),
            RATE_44K,
        );
        let mut out = [0.0f32];
        for _ in 0..2000 {
            tick(&mut node, &[0.5, 0.01], &mut out);
        }
        let want = 0.5 * 10f32.powf(-12.0 / 20.0);
        assert!((out[0] - want).abs() < 1e-5, "{} vs {want}", out[0]);
    }

    /// `reset` clears the follower and the hold: after it a gate that was open and holding
    /// is closed.
    #[test]
    fn a_reset_closes_the_gate() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.5, 0.1), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..500 {
            tick(&mut node, &[0.5, 0.9], &mut out);
        }
        tick(&mut node, &[0.5, 0.0], &mut out);
        assert!(out[0] > 0.49, "the hold keeps it open");
        Node::reset(&mut node);
        tick(&mut node, &[0.5, 0.0], &mut out);
        assert!(out[0] < 1e-3, "a reset gate is closed: {}", out[0]);
    }

    #[test]
    fn a_stereo_gate_is_linked() {
        let mut node = prepared_at(GateNode::stereo(four(-20.0, 0.0001, 0.01, 0.1)), RATE_44K);
        let mut out = [0.0f32; 2];
        for _ in 0..500 {
            tick(&mut node, &[0.8, 0.3, 0.9, 0.001], &mut out);
        }
        // Both channels open on the loudest key; the signal ratio is kept.
        assert!(
            (out[0] - 0.8).abs() < 1e-3 && (out[1] - 0.3).abs() < 1e-3,
            "{out:?}"
        );
    }

    /// `2 * width` inputs (audio, then key) and `width` outputs for a side-chain gate, and
    /// `width` inputs for an own-key one, at every width, from `new` and from the
    /// shorthands: the shorthands are a side-chain key at their width.
    #[test]
    fn the_constructors_and_the_key_choice_are_the_port_layout() {
        let c = four(-20.0, 0.001, 0.01, 0.1);
        let wide = BusTopology::assumed(tutti_types::ChannelLayout::from(4u16))
            .expect("at most one bus wide");
        for (node, inputs, outputs) in [
            (GateNode::mono(c), 2, 1),
            (GateNode::stereo(c), 4, 2),
            (GateNode::new(wide, KeySource::Sidechain, c), 8, 4),
            (GateNode::new(BusTopology::MONO, KeySource::Own, c), 1, 1),
            (GateNode::new(BusTopology::STEREO, KeySource::Own, c), 2, 2),
            (GateNode::new(wide, KeySource::Own, c), 4, 4),
        ] {
            let shape = node.shape();
            assert_eq!(
                (shape.audio_in().count(), shape.audio_out().count()),
                (inputs, outputs)
            );
        }
    }

    /// The controls given to a constructor are the cells' values, the range too (which the
    /// old positional constructors fixed at -80 dB and left to a builder).
    #[test]
    fn the_constructor_controls_reach_the_cells() {
        let c = GateControls {
            threshold: Db(-35.0),
            attack: Seconds(0.002),
            hold: Seconds(0.03),
            release: Seconds(0.25),
            range: Db(-15.0),
        };
        for node in [
            GateNode::stereo(c),
            GateNode::new(BusTopology::MONO, KeySource::Own, c),
        ] {
            let bases: Vec<_> = (0..5).map(|k| node.param_base(k)).collect();
            assert_eq!(
                bases,
                [-35.0, 0.002, 0.03, 0.25, -15.0].map(Some),
                "threshold, attack, hold, release, range"
            );
        }
    }

    /// An own-key gate listens to its signal: a loud one passes, one under the threshold is
    /// closed to the range, and nothing is wired.
    #[test]
    fn an_own_key_gate_listens_to_its_signal() {
        let mk = || {
            GateNode::new(
                BusTopology::MONO,
                KeySource::Own,
                with_range(four(-20.0, 0.0001, 0.001, 0.001), -40.0),
            )
        };
        let mut out = [0.0f32];
        let mut node = prepared_at(mk(), RATE_44K);
        for _ in 0..500 {
            tick(&mut node, &[0.5], &mut out);
        }
        assert!((out[0] - 0.5).abs() < 1e-3, "a loud signal opens itself");
        let mut node = prepared_at(mk(), RATE_44K);
        for _ in 0..500 {
            tick(&mut node, &[0.01], &mut out);
        }
        assert!((out[0] / (0.01 * 0.01) - 1.0).abs() < 1e-3, "{}", out[0]);
    }

    /// The shape declares every control in the order of the set (threshold, attack, hold,
    /// release, range), with the cells as their bases, no latency and no tail.
    #[test]
    fn gate_declares_its_params() {
        let m = GateNode::mono(with_range(four(-20.0, 0.0001, 0.01, 0.1), -30.0));
        let shape = m.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[
                UnitParam::Threshold,
                UnitParam::Attack,
                UnitParam::Hold,
                UnitParam::Release,
                UnitParam::Range
            ][..]
        );
        assert_eq!(shape.audio_in().count(), 2);
        assert_eq!(shape.latency, tutti_types::Latency::ZERO);
        assert_eq!(shape.tail, tutti_types::Tail::None);
        for (k, base) in [-20.0, 0.0001, 0.01, 0.1, -30.0].into_iter().enumerate() {
            assert_eq!(m.param_base(k), Some(base), "control {k}'s base");
        }
    }

    /// A threshold written between blocks is read again: a key that was under it is over the
    /// lowered one, and raising it closes the gate again.
    #[test]
    fn a_changed_threshold_applies_within_a_grid_step() {
        let mut node = prepared_at(gate(-20.0, 0.0, 0.0, 0.0), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.05], &mut out); // -26 dB
        }
        assert!(out[0] < 0.01, "closed");
        assert!(node.param_set().set(UnitParam::Threshold, -30.0));
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.05], &mut out);
        }
        assert_eq!(out[0], 1.0, "open");
        assert!(node.param_set().set(UnitParam::Threshold, -20.0));
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.05], &mut out);
        }
        assert!(out[0] < 0.01, "raising it closes the gate again");
    }

    /// A fed threshold is read per frame: a frame fed a threshold over the key is closed
    /// where the cell's would open it, and one fed under it is open where the cell's would
    /// not.
    #[test]
    fn a_fed_threshold_is_the_one_read_per_frame() {
        let (mut a, mut b) = ([0.0f32], [0.0f32]);
        let mut open_base = prepared_at(gate(-30.0, 0.0, 0.0, 0.0), RATE_44K);
        tick(&mut open_base, &[1.0, 0.1], &mut a); // -20 dB key over -30
        assert_eq!(a[0], 1.0);
        let mut fed = prepared_at(gate(-30.0, 0.0, 0.0, 0.0), RATE_44K);
        tick_fed(&mut fed, &[1.0, 0.1], &[Some(-10.0)], &mut b);
        assert!(b[0] < 0.01, "a fed -10 dB closes it: {}", b[0]);
        let mut closed_base = prepared_at(gate(-10.0, 0.0, 0.0, 0.0), RATE_44K);
        tick_fed(&mut closed_base, &[1.0, 0.1], &[Some(-30.0)], &mut b);
        assert_eq!(b[0], 1.0, "a fed -30 dB opens it");
    }

    /// NaN on the key is ignored by the max-abs detector (reads as silent): the gate stays
    /// closed and finite, and opens on the next finite level. Audio NaN passes through, as
    /// a gain does.
    #[test]
    fn a_nan_key_does_not_poison_the_gate() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let audio = vec![1.0f32; 64];
        let nan = vec![f32::NAN; 64];
        let out = drive(&mut node, RATE_44K, &[&audio, &nan], &[]).remove(0);
        assert!(out.iter().all(|y| y.is_finite()));
        assert!(out[63] < 1e-3, "closed");
        let (audio, loud) = (vec![1.0f32; 1024], vec![0.5f32; 1024]);
        let out = drive(&mut node, RATE_44K, &[&audio, &loud], &[]).remove(0);
        assert!(out[1023] > 0.99, "recovers on a finite key");
    }

    /// A control written NaN is read as the last value it held (`LastGood`): the gate
    /// keeps its threshold and its output stays finite.
    #[test]
    fn a_nan_write_keeps_the_last_threshold() {
        let mut node = prepared_at(gate(-20.0, 0.0, 0.0, 0.0), RATE_44K);
        assert!(node.param_set().set(UnitParam::Threshold, f32::NAN));
        let mut out = [0.0f32];
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.5], &mut out);
        }
        assert_eq!(
            out[0], 1.0,
            "a -6 dB key is still over the -20 dB threshold"
        );
    }
}

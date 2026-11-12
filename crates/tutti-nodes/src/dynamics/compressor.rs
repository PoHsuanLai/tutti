//! The feed-forward [`CompressorNode`] with an external side-chain, soft knee and
//! makeup gain.

use tutti_dsp::{CompressorControls, CompressorFamily, KeySource};
use tutti_types::{BusTopology, CompressionRatio, Db, SampleRate, Seconds};

use crate::reverb::{FamilyNode, NodeFamily, Rings};

/// A compressor: `channels = N` means N audio inputs, then (for a side-chain key) N key
/// inputs, and N outputs, with one **linked** gain computed from the loudest key channel and
/// applied to every output.
///
/// A [`FamilyNode`] over [`CompressorFamily`], the detector and the gain being the
/// family's recurrence; this file holds the cells and how the graph reaches them.
///
/// - [`new`](Self::new): any width up to one bus (16 channels), on a checked
///   [`BusTopology`], with the [`KeySource`] and the [`CompressorControls`] you give it.
/// - [`mono`](Self::mono): 2 inputs (audio, key), 1 output, a side-chain key.
/// - [`stereo`](Self::stereo): 4 inputs (L, R, key L, key R), 2 outputs, linked gain, a
///   side-chain key.
///
/// # The key
///
/// The detector listens to a [`KeySource`], chosen when the node is built. `mono` and
/// `stereo` have a **side-chain** key (as does `new` with [`KeySource::Sidechain`]), a
/// second bus after the audio inputs, **and it must be wired**: the graph feeds an
/// unconnected input silence, which compresses nothing, so a compressor whose key nobody
/// wired does nothing. Wire it with a bus edge, for example
/// `graph.connect(kick.bus_out(0), comp.bus_in(1))` in `tutti-session`, or each key channel
/// into the inputs after the audio. A compressor that should listen to its own signal, as
/// an ordinary compressor does, is `new(.., KeySource::Own, ..)`, which has no key bus and
/// needs nothing wired.
///
/// The node declares no tail: the release decays after the key goes silent, but it only
/// scales, so a silent input is a silent output.
///
/// # Controls
///
/// Threshold, knee, ratio, attack, release and makeup (as [`UnitParam::GainDb`](tutti_types::UnitParam::GainDb)) are live
/// [`Params`](crate::Params) shared across clones, set through the node's
/// [`ParamSet`](tutti_graph::ParamSet) or the graph's `set_param` (a knee under 0 reads as
/// 0, a ratio under 1 as 1 and a time under 0 as 0: a compressor does not expand).
///
/// **The threshold is read per frame** when the graph drives it: a lane on its port is the
/// recurrence's own ([`Feed`](tutti_dsp::Feed) lane 0), and the cell is the base a frame
/// nobody drives (or a non-finite one) is compared against. **Every other control is on
/// the stream grid** (16 frames from the last reset, [`Family::CADENCE`](tutti_dsp::Family::CADENCE)):
/// a cell write or a driven port is sampled at the grid's solve frame, so it reaches the
/// render within 16 frames and the render does not depend on how the stream is cut into
/// blocks. The detector's controls land whole (the follower smooths them), and the makeup
/// glides over [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE), 5 ms.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from
/// the values last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares
/// it at the device rate before its first block.
pub type CompressorNode = FamilyNode<CompressorFamily>;

impl CompressorNode {
    /// A compressor on the bus `input`, its detector on `key`, running at `controls`.
    ///
    /// The main bus keeps its speaker placement; a [`KeySource::Sidechain`] is a second bus
    /// as wide, which is a level to read, not a placement, so it has none. **A side-chain key
    /// must be wired** (see the [type](CompressorNode)); a [`KeySource::Own`] compressor
    /// listens to its own signal and has no key bus. `new(BusTopology::STEREO, ..)` is
    /// [`stereo`](Self::stereo) for a side-chain; an `n`-channel one on the engine's order
    /// for that width is `new(BusTopology::assumed(n)?, ..)`, which refuses a width past
    /// one bus.
    ///
    /// The controls are as [`CompressorControls`] says, and are the cells' values: the
    /// threshold (`Db`, typically negative, since 0 dB is full scale), the knee width, the
    /// ratio (`4.0` is 4 dB in for 1 dB out above the threshold; at least 1, no expansion),
    /// the envelope times (short attacks catch transients, long releases sound smoother)
    /// and the makeup gain (applied after the reduction, unconditionally). The envelope's
    /// one-pole coefficients are derived from the times at the rate
    /// [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    /// [`CompressorControls::DEFAULT`] does nothing (ratio 1, threshold 0 dB), so a caller
    /// names what it wants: `CompressorControls { threshold, ratio, ..CompressorControls::DEFAULT }`.
    #[must_use]
    pub fn new(input: BusTopology, key: KeySource, controls: CompressorControls) -> Self {
        Self::live(CompressorFamily::new(input, key), controls)
    }

    /// Mono + mono side-chain: 2 inputs (audio, key), 1 output.
    ///
    /// **The key is a side-chain that must be wired** (see the [type](CompressorNode)); for
    /// a compressor that listens to its own signal use [`new`](Self::new) with
    /// [`KeySource::Own`].
    #[must_use]
    pub fn mono(controls: CompressorControls) -> Self {
        Self::new(BusTopology::MONO, KeySource::Sidechain, controls)
    }

    /// Stereo + stereo side-chain: 4 inputs (L, R, key L, key R), 2 outputs.
    ///
    /// The gain is **linked**: one reduction computed from the loudest key channel and
    /// applied to both, so the stereo image does not shift when one side is louder. The key
    /// is as [`mono`](Self::mono)'s: a side-chain that must be wired.
    #[must_use]
    pub fn stereo(controls: CompressorControls) -> Self {
        Self::new(BusTopology::STEREO, KeySource::Sidechain, controls)
    }
}

/// The compressor's controls: threshold, knee, ratio, attack, release and makeup.
#[derive(Clone, Copy, Debug)]
pub struct CompressorSet;

control_set! {
    CompressorSet(_s): CompressorControls = CompressorControls::DEFAULT;
    live: (all);
    Threshold => Db = threshold { fast },
    Knee => Db = knee { whole },
    Ratio => CompressionRatio = ratio { whole },
    Attack => Seconds = attack { whole },
    Release => Seconds = release { whole },
    // The makeup glides in the recurrence, over `GAIN_GLIDE`: the reader must not ramp it
    // across the block as well. It is addressed as `GainDb`.
    GainDb => Db = makeup { whole };

    fn defaults(&self) -> CompressorControls {
        CompressorControls::DEFAULT
    }
}

/// A compressor rings on only as a gain (it scales its input), so it declares no tail and
/// never parks: its follower must keep tracking the key, which the main input's silence
/// says nothing of.
impl NodeFamily for CompressorFamily {
    type Set = CompressorSet;

    fn rings(&self) -> Rings {
        Rings::Silent
    }
    const SAMPLING: tutti_dsp::Cadence = <Self as tutti_dsp::Family>::CADENCE;

    fn set(&self, _: &CompressorControls) -> CompressorSet {
        CompressorSet
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

    /// The controls of a compressor the tests name four of, the others at the default's.
    fn four(threshold: f32, ratio: f32, attack: f64, release: f64) -> CompressorControls {
        CompressorControls {
            threshold: Db::new(threshold),
            ratio: CompressionRatio::new(ratio),
            attack: Seconds::new(attack),
            release: Seconds::new(release),
            ..CompressorControls::DEFAULT
        }
    }

    fn comp(threshold: f32, ratio: f32, attack: f64, release: f64) -> CompressorNode {
        CompressorNode::mono(four(threshold, ratio, attack, release))
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(CompressorNode::stereo(CompressorControls {
            knee: Db::new(6.0),
            makeup: Db::new(3.0),
            ..four(-20.0, 4.0, 0.001, 0.1)
        }));
    }

    /// A block longer than 64 frames renders whole: the gain lane is sized from the
    /// prepared `MaxBlock`, not a constant.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(&mut node, RATE_44K, &[&audio, &sc], &[]).remove(0);
        assert!(out[1023] < 0.5, "the loud sidechain reduced the tail");
    }

    /// A NaN ratio at construction is read as no compression, not as 0, which is below
    /// the type's floor: the output stays finite, `reset` is not needed to recover, and a
    /// later finite ratio takes effect.
    #[test]
    fn a_nan_ratio_at_construction_is_finite_and_recovers() {
        let mut node = prepared_at(comp(-20.0, f32::NAN, 0.0001, 0.1), RATE_44K);
        let audio = vec![0.5f32; 256];
        let sc = vec![0.9f32; 256];
        for _ in 0..16 {
            let out = drive(&mut node, RATE_44K, &[&audio, &sc], &[]).remove(0);
            assert!(out.iter().all(|s| s.is_finite()), "NaN ratio");
            assert!(
                out.iter().all(|&s| s == 0.5),
                "NaN ratio compresses nothing"
            );
        }
        assert!(node.param_set().set(UnitParam::Ratio, 4.0));
        let mut last = 0.5;
        for _ in 0..16 {
            let out = drive(&mut node, RATE_44K, &[&audio, &sc], &[]).remove(0);
            assert!(out.iter().all(|s| s.is_finite()), "after the write");
            last = out[255];
        }
        assert!(last < 0.5, "the finite ratio compresses: {last}");
        Node::reset(&mut node);
        let out = drive(&mut node, RATE_44K, &[&audio, &sc], &[]).remove(0);
        assert!(out.iter().all(|s| s.is_finite()), "after reset");
    }

    /// An own-key compressor listens to its signal: no key bus (one input a channel), a
    /// loud signal is reduced and a quiet one under the threshold is not.
    #[test]
    fn an_own_key_compressor_listens_to_its_signal() {
        let mk = || {
            CompressorNode::new(
                BusTopology::MONO,
                KeySource::Own,
                four(-20.0, 4.0, 0.0001, 0.1),
            )
        };
        let shape = mk().shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (1, 1)
        );
        let mut output = [0.0f32];
        let mut loud = prepared_at(mk(), RATE_44K);
        for _ in 0..1000 {
            tick(&mut loud, &[0.5], &mut output);
        }
        assert!(output[0] < 0.4, "a loud signal ducks itself: {}", output[0]);
        let mut quiet = prepared_at(mk(), RATE_44K);
        for _ in 0..1000 {
            tick(&mut quiet, &[0.05], &mut output);
        }
        assert_eq!(output[0], 0.05, "under the threshold nothing is reduced");
    }

    #[test]
    fn test_compressor_mono_reduces_gain_on_loud_sidechain() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.9], &mut output);
        }
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_compressor_mono_no_reduction_below_threshold() {
        let mut node = prepared_at(comp(-10.0, 4.0, 0.001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.1], &mut output);
        }
        // A key at -20 dB under a -10 dB threshold: the gain is 1.
        assert_eq!(output[0], 0.5);
    }

    #[test]
    fn test_compressor_soft_knee_differs_from_hard_knee() {
        let mut hard = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut soft = prepared_at(
            CompressorNode::mono(CompressorControls {
                knee: Db::new(12.0),
                ..four(-20.0, 4.0, 0.0001, 0.1)
            }),
            RATE_44K,
        );
        let mut hard_out = [0.0f32];
        let mut soft_out = [0.0f32];
        for _ in 0..1000 {
            tick(&mut hard, &[0.5, 0.15], &mut hard_out);
            tick(&mut soft, &[0.5, 0.15], &mut soft_out);
        }
        assert!(
            (hard_out[0] - soft_out[0]).abs() > 0.001,
            "Soft knee should produce different output near threshold: hard={}, soft={}",
            hard_out[0],
            soft_out[0]
        );
    }

    #[test]
    fn test_compressor_makeup_gain() {
        let mut with = prepared_at(
            CompressorNode::mono(CompressorControls {
                makeup: Db::new(6.0),
                ..four(-20.0, 4.0, 0.0001, 0.1)
            }),
            RATE_44K,
        );
        let mut without = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        let mut output_no_makeup = [0.0f32];
        for _ in 0..1000 {
            tick(&mut with, &[0.5, 0.9], &mut output);
            tick(&mut without, &[0.5, 0.9], &mut output_no_makeup);
        }
        // Six dB is a factor of 10^(6/20), the same gain on the same reduction.
        let ratio = f64::from(output[0]) / f64::from(output_no_makeup[0]);
        assert!((ratio - 10f64.powf(6.0 / 20.0)).abs() < 1e-4, "{ratio}");
    }

    /// `reset` clears the follower: a quiet key after it reads no reduction, where without
    /// it the slow release (100 ms) would still hold the last one.
    #[test]
    fn a_reset_clears_the_follower() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.9], &mut output);
        }
        tick(&mut node, &[0.5, 0.0], &mut output);
        assert!(output[0] < 0.5, "the release still holds the reduction");

        Node::reset(&mut node);
        tick(&mut node, &[0.5, 0.0], &mut output);
        assert_eq!(output[0], 0.5, "a reset follower is at no reduction");
    }

    /// A ratio written below 1 reads as 1: a compressor does not expand, and at ratio 1
    /// nothing is reduced.
    #[test]
    fn a_ratio_below_one_reads_as_one() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        assert!(node.param_set().set(UnitParam::Ratio, 0.5));
        let mut output = [0.0f32];
        for _ in 0..100 {
            tick(&mut node, &[0.5, 0.9], &mut output);
        }
        assert_eq!(output[0], 0.5);
    }

    #[test]
    fn test_compressor_stereo_reduces_on_loud_sidechain() {
        let mut node = prepared_at(
            CompressorNode::stereo(four(-20.0, 4.0, 0.0001, 0.1)),
            RATE_44K,
        );
        let mut output = [0.0f32; 2];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.5, 0.9, 0.9], &mut output);
        }
        assert!((output[0] - output[1]).abs() < 0.001);
        assert!(output[0] < 0.5);
    }

    /// `2 * width` inputs (audio, then key) and `width` outputs for a side-chain compressor
    /// and `width` inputs for an own-key one, at every width, from `new` and from the
    /// shorthands: the shorthands are a side-chain key at their width.
    #[test]
    fn the_constructors_and_the_key_choice_are_the_port_layout() {
        let c = four(-20.0, 4.0, 0.001, 0.1);
        let wide = BusTopology::assumed(tutti_types::ChannelLayout::from(4u16))
            .expect("at most one bus wide");
        for (node, inputs, outputs) in [
            (CompressorNode::mono(c), 2, 1),
            (CompressorNode::stereo(c), 4, 2),
            (CompressorNode::new(wide, KeySource::Sidechain, c), 8, 4),
            (
                CompressorNode::new(BusTopology::MONO, KeySource::Own, c),
                1,
                1,
            ),
            (
                CompressorNode::new(BusTopology::STEREO, KeySource::Own, c),
                2,
                2,
            ),
            (CompressorNode::new(wide, KeySource::Own, c), 4, 4),
        ] {
            let shape = node.shape();
            assert_eq!(
                (shape.audio_in().count(), shape.audio_out().count()),
                (inputs, outputs)
            );
        }
    }

    /// The controls given to a constructor are the cells' values, all six of them (the
    /// knee and makeup too, which the old positional constructors left to builders).
    #[test]
    fn the_constructor_controls_reach_the_cells() {
        let c = CompressorControls {
            threshold: Db(-18.0),
            knee: Db(6.0),
            ratio: CompressionRatio(3.0),
            attack: Seconds(0.002),
            release: Seconds(0.25),
            makeup: Db(2.5),
        };
        for node in [
            CompressorNode::stereo(c),
            CompressorNode::new(BusTopology::MONO, KeySource::Own, c),
        ] {
            let bases: Vec<_> = (0..6).map(|k| node.param_base(k)).collect();
            assert_eq!(
                bases,
                [-18.0, 6.0, 3.0, 0.002, 0.25, 2.5].map(Some),
                "threshold, knee, ratio, attack, release, makeup"
            );
        }
    }

    /// The shape declares every control in the order of the set (threshold, knee, ratio,
    /// attack, release, makeup as `GainDb`), with the cells as their bases, no latency and
    /// no tail, at every width.
    #[test]
    fn compressor_declares_its_params() {
        let m = comp(-20.0, 4.0, 0.0001, 0.1);
        let shape = m.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[
                UnitParam::Threshold,
                UnitParam::Knee,
                UnitParam::Ratio,
                UnitParam::Attack,
                UnitParam::Release,
                UnitParam::GainDb
            ][..]
        );
        assert_eq!(shape.audio_in().count(), 2);
        assert_eq!(shape.latency, tutti_types::Latency::ZERO);
        assert_eq!(shape.tail, tutti_types::Tail::None);
        for (k, base) in [-20.0, 0.0, 4.0, 0.0001, 0.1, 0.0].into_iter().enumerate() {
            assert_eq!(m.param_base(k), Some(base), "control {k}'s base");
        }
    }

    /// A fed threshold is read per frame: a frame fed a lower threshold than the cell's is
    /// reduced more than one at the cell's, and the cell is the base a frame nobody
    /// drives sits at.
    #[test]
    fn a_fed_threshold_is_the_one_read_per_frame() {
        let mut held = prepared_at(comp(-10.0, 4.0, 0.0, 0.0), RATE_44K);
        let mut fed = prepared_at(comp(-10.0, 4.0, 0.0, 0.0), RATE_44K);
        let (mut a, mut b) = ([0.0f32], [0.0f32]);
        tick(&mut held, &[0.5, 0.5], &mut a);
        tick_fed(&mut fed, &[0.5, 0.5], &[Some(-40.0)], &mut b);
        assert!(
            b[0] < a[0],
            "a lower threshold reduces more: {} vs {}",
            b[0],
            a[0]
        );
        // A key at -6 dB, ratio 4, threshold -40: 25.5 dB of reduction, instant follower.
        let want = 0.5 * 10f32.powf(-0.75 * (-6.0206 + 40.0) / 20.0);
        assert!((b[0] - want).abs() < 1e-4, "{} vs {want}", b[0]);
    }

    /// A threshold that moves inside a block is read at the frame it moves on, not at the
    /// stream grid's next solve (16 frames on): frame 5 of a block whose lane steps from
    /// 20 dB to -40 dB there is reduced, the frame before it is not.
    #[test]
    fn a_threshold_that_steps_inside_a_block_acts_on_that_frame() {
        let mut node = prepared_at(comp(-10.0, 4.0, 0.0, 0.0), RATE_44K);
        let n = 64;
        let (audio, key) = (vec![0.5f32; n], vec![0.5f32; n]);
        let lane: Vec<f32> = (0..n).map(|i| if i < 5 { 20.0 } else { -40.0 }).collect();
        let out = drive(&mut node, RATE_44K, &[&audio, &key], &[Some(&lane)]).remove(0);
        assert_eq!(out[4], 0.5, "over the 20 dB threshold: no reduction");
        assert!(
            out[5] < 0.2,
            "the -40 dB threshold reduces at once: {}",
            out[5]
        );
    }

    /// A control other than the threshold is sampled on the stream's 16-frame grid, so a
    /// write reaches the render within 16 frames wherever the blocks fall, and (the
    /// follower being instant here) lands whole.
    #[test]
    fn a_cell_write_reaches_the_render_within_a_grid_step() {
        let mut node = prepared_at(comp(-20.0, 2.0, 0.0, 0.0), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..40 {
            tick(&mut node, &[0.5, 0.5], &mut out);
        }
        let before = out[0];
        assert!(node.param_set().set(UnitParam::Ratio, 8.0));
        for _ in 0..16 {
            tick(&mut node, &[0.5, 0.5], &mut out);
        }
        assert!(
            out[0] < before,
            "ratio 8 reduces more than 2: {} vs {before}",
            out[0]
        );
        // A key at 0.5 (-6.02 dB) over -20 dB at 8:1: 7/8 of the 13.98 dB over it.
        let want = 0.5 * 10f32.powf(-0.875 * (20.0 * 0.5f32.log10() + 20.0) / 20.0);
        assert!((out[0] - want).abs() < 1e-4, "{} vs {want}", out[0]);
    }

    /// A makeup written between blocks glides over 5 ms (240 frames at 48 kHz) wherever the
    /// blocks fall: monotone, starting near the old gain and landing on the new one, which
    /// is the old times 10^(6/20).
    #[test]
    fn a_makeup_write_glides_over_five_milliseconds() {
        let rate = SampleRate(48_000.0);
        let mut node = tutti_graph::contract::prepared(comp(0.0, 1.0, 0.0, 0.0), rate, 64);
        let (audio, key) = (vec![0.5f32; 64], vec![0.0f32; 64]);
        for _ in 0..4 {
            drive(&mut node, rate, &[&audio, &key], &[]);
        }
        assert!(node.param_set().set(UnitParam::GainDb, 6.0));
        let mut out = Vec::new();
        for _ in 0..8 {
            out.extend(drive(&mut node, rate, &[&audio, &key], &[]).remove(0));
        }
        // The write is sampled on the grid (within 16 frames), then 240 frames of glide.
        assert!(
            out[0] < 0.5 * 1.01,
            "the first frame is near the old gain: {}",
            out[0]
        );
        for w in out.windows(2) {
            assert!(w[1] >= w[0], "the glide is monotone: {w:?}");
            // 6 dB over 240 frames is 0.025 dB a frame, a factor of 1.0029.
            assert!(w[1] / w[0] < 1.0035, "no step in the glide: {w:?}");
        }
        let want = 0.5 * 10f32.powf(6.0 / 20.0);
        assert!(
            (out[out.len() - 1] - want).abs() < 1e-5,
            "{}",
            out[out.len() - 1]
        );
        assert!(
            out[16 + 240 + 16] > want * 0.9999,
            "arrived by 5 ms after the grid point"
        );
    }
}

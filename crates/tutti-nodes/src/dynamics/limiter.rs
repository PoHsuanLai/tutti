//! The lookahead limiter, [`LimiterNode`].
//!
//! It delays its audio so the gain decision runs ahead of a peak, which costs
//! latency and sounds transparent. For a hard clip that costs nothing and distorts,
//! a safety catch, see [`BrickwallLimiterNode`](super::BrickwallLimiterNode).

use tutti_dsp::{LimiterControls, LimiterFamily, Lookahead};
use tutti_types::{BusTopology, Db, SampleRate, Seconds};

use crate::reverb::{FamilyNode, NodeFamily, Rings};

/// A lookahead limiter: `N` audio inputs and `N` outputs, with gain reduction **linked**
/// across all channels.
///
/// A [`FamilyNode`] over [`LimiterFamily`], the delay and the gain being the family's
/// recurrence; this file holds the cells and how the graph reaches them.
///
/// - [`mono`](Self::mono) and [`stereo`](Self::stereo): one and two channels, at the
///   default lookahead of 5 ms ([`Lookahead::DEFAULT`]).
/// - [`new`](Self::new): any width up to one bus (16 channels), on a checked
///   [`BusTopology`], at a lookahead of your own.
///
/// A limiter is a compressor at an effectively infinite ratio: nothing is allowed past
/// the ceiling: the output is bounded by it (a gain divide and a product can in principle
/// round one ulp past).
///
/// # The curve
///
/// The threshold drives the input. With `drive = ceiling - threshold` in dB, the gain at a
/// linked peak is `min(drive, ceiling / peak)`, so the output is
/// `min(peak * drive, ceiling)`: under the threshold the signal is moved by the drive
/// (boosted when the ceiling is above the threshold, attenuated when below), and a louder
/// one settles at the ceiling. The curve is continuous and monotonic, with its knee at the
/// threshold. With [`LimiterControls::DEFAULT`] (-6 dB threshold, -0.3 dB ceiling) a
/// quiet signal is raised by 5.7 dB and the output reaches -0.3 dB when the input reaches
/// -6 dB. The gain is then smoothed (instant attack, `release`) and its minimum over the
/// lookahead window applied. The **lookahead** is what makes that possible without
/// distortion: the audio is delayed by the lookahead window while the gain decision runs
/// ahead of it, so the reduction is already in place when a peak arrives rather than
/// chasing it.
///
/// That delay is real latency: the node declares it in its [`Shape`](tutti_graph::Shape),
/// and the graph's PDC compensates a dry path mixed against it. It declares no tail: a tail
/// excludes latency, and what the delay holds when the input stops comes out within it.
/// The release shapes gain, and gain applied to silence is silence.
///
/// The first audio after a prepare or a reset is limited from its first sample: the
/// envelope has no history then, and the first frame takes its target at once.
///
/// # Controls
///
/// Threshold, ceiling and release are live [`Params`](crate::Params) shared across clones,
/// set through the node's [`ParamSet`](tutti_graph::ParamSet) or the graph's `set_param`
/// (a release under 1 ms reads as 1 ms, and a NaN threshold or ceiling is ignored).
///
/// **The threshold and the ceiling are read per frame** when the graph drives them: a lane
/// on their ports is the recurrence's own ([`Feed`](tutti_dsp::Feed) lanes 0 and 1), and
/// the cell is the base a frame nobody drives (or a non-finite one) is read at. **The
/// release is on the stream grid** (16 frames from the last reset,
/// [`Family::CADENCE`](tutti_dsp::Family::CADENCE)): a cell write or a driven port is
/// sampled at the grid's solve frame, so it reaches the render within 16 frames and the
/// render does not depend on how the stream is cut into blocks. Every coefficient lands
/// whole: a glided ceiling would let a sample past the new one.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from
/// the values last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares
/// it at the device rate before its first block, which sizes the delay.
pub type LimiterNode = FamilyNode<LimiterFamily>;

impl LimiterNode {
    /// A limiter on the bus `input`, as wide as it, delaying by `lookahead` (at least one
    /// frame) and running at `controls`.
    ///
    /// **The lookahead is the node's latency**: the audio is delayed by the window so the
    /// gain decision can run ahead of it. Longer catches peaks more transparently and
    /// delays more; [`Lookahead::DEFAULT`], 5 ms, is a typical compromise.
    /// It is fixed here, because it sizes the delay: the frame count is derived from these
    /// seconds at the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it, and
    /// again at every other rate.
    ///
    /// The gain is linked: the peak is the loudest channel of the frame, and every channel
    /// is scaled by the one gain, so a loud peak in one ducks them all together and the
    /// image does not shift. `new(BusTopology::STEREO, ..)` is [`stereo`](Self::stereo); an
    /// `n`-channel one on the engine's order for that width is
    /// `new(BusTopology::assumed(n)?, ..)`, which refuses a width past one bus.
    #[must_use]
    pub fn new(input: BusTopology, lookahead: Lookahead, controls: LimiterControls) -> Self {
        Self::live(LimiterFamily::new(input, lookahead), controls)
    }

    /// A mono limiter at the default lookahead: 1 input, 1 output.
    #[must_use]
    pub fn mono(controls: LimiterControls) -> Self {
        Self::new(BusTopology::MONO, Lookahead::DEFAULT, controls)
    }

    /// A stereo limiter at the default lookahead: 2 inputs, 2 outputs, linked.
    #[must_use]
    pub fn stereo(controls: LimiterControls) -> Self {
        Self::new(BusTopology::STEREO, Lookahead::DEFAULT, controls)
    }
}

/// The limiter's controls: the threshold and the ceiling, which are read per frame, and the
/// release. A threshold and a ceiling of `+-inf` are levels, and a NaN is not.
///
/// The ports are in this order: threshold, ceiling, release.
#[derive(Clone, Copy, Debug)]
pub struct LimiterSet;

control_set! {
    LimiterSet(_s): LimiterControls = LimiterControls::DEFAULT;
    live: (all);
    Threshold => Db = threshold { fast, not_nan },
    Ceiling => Db = ceiling { fast, not_nan },
    Release => Seconds = release { whole };

    fn defaults(&self) -> LimiterControls {
        LimiterControls::DEFAULT
    }
}

/// A limiter rings on only as a gain (it scales its delayed input), and a tail excludes
/// the latency the delay is, so it declares no tail and never parks.
impl NodeFamily for LimiterFamily {
    type Set = LimiterSet;

    fn rings(&self) -> Rings {
        Rings::Silent
    }
    const SAMPLING: tutti_dsp::Cadence = <Self as tutti_dsp::Family>::CADENCE;

    fn set(&self, _: &LimiterControls) -> LimiterSet {
        LimiterSet
    }

    fn capacity(&self, _: SampleRate) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE_44K, prepared_at, tick, tick_fed};
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode, Prepare};
    use tutti_types::{Amplitude, ChannelLayout, Latency, MaxBlock, Samples, Tail, UnitParam};

    /// The usual mastering limiter at the default lookahead, as the node's own tests make
    /// it: stereo, at `threshold` and `ceiling`, with the default release.
    fn limiter(threshold: Db, ceiling: Db) -> LimiterNode {
        LimiterNode::stereo(LimiterControls {
            threshold,
            ceiling,
            ..LimiterControls::DEFAULT
        })
    }

    /// `limiter` with a release of `release` seconds.
    fn limiter_releasing(threshold: Db, ceiling: Db, release: f64) -> LimiterNode {
        LimiterNode::stereo(LimiterControls {
            threshold,
            ceiling,
            release: Seconds(release),
        })
    }

    fn amplitude_to_db(a: f32) -> f32 {
        tutti_dsp::amplitude_to_db(Amplitude::new(a)).get()
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    #[test]
    fn a_limiter_fork_shares_no_cell() {
        assert_param_fork(limiter(Db::new(-6.0), Db::new(-0.3)));
    }

    /// Each constructor's ports: `mono` is one channel in and out, `stereo` two, and
    /// `new` any width on its bus, with no key bus (a limiter limits what it delays).
    #[test]
    fn each_constructor_declares_its_ports() {
        let c = LimiterControls::DEFAULT;
        let ports = |n: LimiterNode| {
            let shape = n.shape();
            (shape.audio_in().count(), shape.audio_out().count())
        };
        assert_eq!(ports(LimiterNode::mono(c)), (1, 1));
        assert_eq!(ports(LimiterNode::stereo(c)), (2, 2));
        let wide =
            BusTopology::assumed(ChannelLayout::from(6u16)).expect("six channels are one bus");
        assert_eq!(
            ports(LimiterNode::new(wide, Lookahead::of(Seconds(0.002)), c)),
            (6, 6)
        );
    }

    /// The controls given to a constructor are the cells' values: the shape declares the
    /// ports in the set's order (threshold, ceiling, release), no tail, the latency of the
    /// lookahead the constructor was given, and a base for each that is the control's.
    #[test]
    fn the_constructor_controls_are_the_cells_and_the_lookahead_is_the_latency() {
        let c = LimiterControls {
            threshold: Db(-9.0),
            ceiling: Db(-2.0),
            release: Seconds(0.25),
        };
        let lim = prepared_at(
            LimiterNode::new(BusTopology::STEREO, Lookahead::of(Seconds(0.002)), c),
            RATE_44K,
        );
        let shape = lim.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Threshold, UnitParam::Ceiling, UnitParam::Release][..]
        );
        assert_eq!(shape.tail, Tail::None, "a tail excludes latency");
        // 2 ms at 44.1 kHz is 88.2 frames, rounded up.
        assert_eq!(shape.latency, Latency::new(Samples(89)));
        assert_eq!(lim.param_base(0), Some(-9.0), "the threshold's base");
        assert_eq!(lim.param_base(1), Some(-2.0), "the ceiling's base");
        assert_eq!(lim.param_base(2), Some(0.25), "the release's base");
        // A shorthand's lookahead is the default's: 5 ms is 220.5 frames, rounded up.
        let mono = prepared_at(LimiterNode::mono(c), RATE_44K);
        assert_eq!(mono.shape().latency, Latency::new(Samples(221)));
    }

    /// A limiter is **born at the placeholder rate**, and `prepare` is what makes its
    /// lookahead a wall-clock duration rather than a frame count: the window is `ceil`ed
    /// from the *exact* product (0.1 s at 48 kHz is 4800 frames, where the `f32` nearest
    /// 0.1 would make 4801).
    #[test]
    fn lookahead_frames_are_the_ceil_of_the_exact_product() {
        let device = SampleRate(48_000.0);
        let mut lim = LimiterNode::new(
            BusTopology::STEREO,
            Lookahead::of(Seconds(0.1)),
            LimiterControls::DEFAULT,
        );
        lim.prepare(&Prepare::new(device, MaxBlock::of(Samples(64))));
        assert_eq!(
            lim.shape().latency,
            Latency::new(Samples((0.1_f64 * 48_000.0).ceil() as usize))
        );
        assert_eq!(lim.shape().latency.samples().get(), 4800);
    }

    /// The lookahead is a duration, and `prepare` is what preserves it: the count is
    /// right for the placeholder rate and short at the device's until the node is prepared,
    /// then it is the same wall-clock window; and the graph is told the corrected figure,
    /// the one PDC compensates against. Repeated rate changes land on the same count each
    /// time (the count is `ceil`ed, so re-deriving it from the count rather than from the
    /// request would ratchet it up).
    #[test]
    fn lookahead_is_a_duration_and_prepare_is_what_preserves_it() {
        const LOOKAHEAD: Seconds = Seconds(0.005);
        let device = SampleRate(48_000.0);
        let mut lim = limiter(Db::new(-6.0), Db::new(-0.3));

        let frames = |lim: &LimiterNode| lim.shape().latency.samples().get();
        let born = frames(&lim);
        assert_eq!(
            born,
            LOOKAHEAD.to_samples_ceil(SampleRate::DEFAULT).get(),
            "construction sizes the delay at the placeholder rate"
        );
        let born_secs = born as f64 / device.get();
        assert!(
            born_secs < LOOKAHEAD.get() * 0.95,
            "an uncorrected delay is audibly short at {device:?}: {born_secs:.5}s of the {:.5}s",
            LOOKAHEAD.get()
        );

        lim.prepare(&Prepare::new(device, MaxBlock::of(Samples(64))));
        let fixed = frames(&lim);
        assert_eq!(fixed, LOOKAHEAD.to_samples_ceil(device).get());
        assert!(
            fixed > born,
            "a higher rate needs more frames ({born} -> {fixed})"
        );
        assert!(((fixed as f64 / device.get()) - LOOKAHEAD.get()).abs() < 1e-4);

        for _ in 0..4 {
            lim.prepare(&Prepare::new(
                SampleRate::DEFAULT,
                MaxBlock::of(Samples(64)),
            ));
            assert_eq!(
                frames(&lim),
                born,
                "back to the original rate, the original count"
            );
            lim.prepare(&Prepare::new(device, MaxBlock::of(Samples(64))));
            assert_eq!(
                frames(&lim),
                fixed,
                "a rate change does not accumulate rounding"
            );
        }
    }

    /// The limiter's output ends exactly its latency after its input: nothing rings once
    /// the lookahead is compensated, so it declares no tail. A tone over the threshold (so
    /// the release envelope is mid-swing when the input stops) leaves its last non-zero
    /// frame `lookahead` frames after the last input frame, and silence after that.
    #[test]
    fn the_limiter_stops_at_its_latency_and_declares_no_tail() {
        let mut lim = prepared_at(limiter(Db::new(-6.0), Db::new(-0.3)), RATE_44K);
        let shape = lim.shape();
        let n = shape.latency.samples().get();
        assert!(n > 1, "a lookahead to compensate");
        assert_eq!(shape.tail, Tail::None, "a tail excludes latency");

        const SOUNDING: usize = 1_000;
        let mut out = [0.0f32; 2];
        let mut last = None;
        for i in 0..SOUNDING + 4 * n {
            let x = if i < SOUNDING { 0.9 } else { 0.0 };
            tick(&mut lim, &[x, x], &mut out);
            if out.iter().any(|&y| y != 0.0) {
                last = Some(i);
            }
        }
        assert_eq!(
            last,
            Some(SOUNDING - 1 + n),
            "the last input frame leaves exactly the latency later, and nothing after"
        );
    }

    /// A control written through the node's `ParamSet` reaches the render: the ceiling set
    /// to -20 dB bounds what comes out of the delay from the grid's next solve (16 frames)
    /// on, at the new ceiling and not the old.
    #[test]
    fn a_ceiling_set_by_address_reaches_the_limiter() {
        // A release of 5 ms, which the warm-up outlasts.
        let mut lim = prepared_at(
            limiter_releasing(Db::new(-6.0), Db::new(-0.3), 0.005),
            RATE_44K,
        );
        let latency = lim.shape().latency.samples().get();
        let mut out = [0.0f32; 2];
        for _ in 0..2_000 {
            tick(&mut lim, &[1.0, 1.0], &mut out);
        }
        let old = Db(-0.3).to_amplitude().get();
        assert!(out[0] > 0.9 * old, "limited at the old ceiling: {}", out[0]);

        assert!(lim.param_set().set(UnitParam::Ceiling, -20.0));
        let new = Db(-20.0).to_amplitude().get();
        // What is fed from now on, after the solve, leaves a lookahead later.
        for i in 0..16 + latency + 64 {
            tick(&mut lim, &[1.0, 1.0], &mut out);
            if i >= 16 + latency {
                assert!(out[0].abs() <= new * (1.0 + 1e-6), "frame {i}: {}", out[0]);
            }
        }
        assert!(out[0] > 0.9 * new, "and it is the new ceiling: {}", out[0]);
    }

    /// A driven ceiling lane is the ceiling of each frame, **not the one the stream's
    /// 16-frame grid samples**: a lane that is 0 dB on even frames and -20 dB on odd ones
    /// (the grid's frames are all even) bounds what comes out of the delay for frame `i` by
    /// frame `i`'s own ceiling, which a ceiling read at the grid's frames, always 0 dB, would
    /// not.
    #[test]
    fn a_fed_ceiling_is_the_one_read_per_frame() {
        let mut lim = prepared_at(
            limiter_releasing(Db::new(-6.0), Db::new(-0.3), 0.005),
            RATE_44K,
        );
        let latency = lim.shape().latency.samples().get();
        let lane = |i: usize| if i.is_multiple_of(2) { 0.0f32 } else { -20.0 };
        let low = Db(-20.0).to_amplitude().get();
        let mut out = [0.0f32; 2];
        let mut outputs = Vec::new();
        for i in 0..4_000 {
            tick_fed(&mut lim, &[1.0, 1.0], &[None, Some(lane(i))], &mut out);
            outputs.push(out[0]);
        }
        let mut reached = false;
        for (i, &y) in outputs.iter().enumerate().skip(latency) {
            let bound = Db(lane(i - latency)).to_amplitude().get() * (1.0 + 1e-6);
            assert!(y.abs() <= bound, "frame {i}: {y} over {bound}");
            reached |= i > 2_000 && (i - latency) % 2 == 1 && y.abs() > 0.9 * low;
        }
        assert!(
            reached,
            "the -20 dB frames were limited to their own ceiling"
        );
    }

    /// A threshold lane is the threshold of its frame: a quiet signal is moved by the drive
    /// (the ceiling over the threshold in dB), so a fed -30 dB against the cell's -10 dB
    /// raises it by 24 dB and not 4.
    #[test]
    fn a_fed_threshold_is_the_one_read_per_frame() {
        let mut lim = prepared_at(
            limiter_releasing(Db::new(-10.0), Db::new(-6.0), 0.001),
            RATE_44K,
        );
        let mut out = [0.0f32; 2];
        for _ in 0..4_000 {
            tick_fed(&mut lim, &[0.01, 0.01], &[Some(-30.0), None], &mut out);
        }
        let gain_db = amplitude_to_db(out[0] / 0.01);
        assert!((gain_db - 24.0).abs() < 1e-2, "{gain_db} dB");
    }

    /// A stereo limiter fed on one channel only limits that channel and
    /// leaves the other silent: the graph hands an unconnected input
    /// silence, which the linked detector reads as such (a graph node is
    /// always handed every input it declares).
    #[test]
    fn limiter_leaves_an_unfed_channel_silent() {
        let mut lim = prepared_at(limiter(Db::new(-6.0), Db::new(-0.3)), RATE_44K);
        let mut out = [0.0f32; 2];
        // Run past the 5 ms lookahead so the delayed signal emerges.
        for _ in 0..500 {
            tick(&mut lim, &[0.9, 0.0], &mut out);
        }
        assert!(out[0].abs() > 1e-4, "ch0 silent: {}", out[0]);
        assert_eq!(out[1], 0.0, "ch1 carries a signal it was never fed");
    }

    /// A loud transient on one channel of a six-channel limiter reduces ALL channels by the
    /// same linked gain (max-abs across the frame), preserving the inter-channel ratios.
    #[test]
    fn a_wide_limiter_links_the_gain_across_all_channels() {
        let wide =
            BusTopology::assumed(ChannelLayout::from(6u16)).expect("six channels are one bus");
        let mut lim = prepared_at(
            LimiterNode::new(wide, Lookahead::DEFAULT, LimiterControls::DEFAULT),
            RATE_44K,
        );
        let mut out = [0.0f32; 6];
        // ch0 loud, the others at half: the whole frame is limited together.
        let inp = [1.0f32, 0.5, 0.5, 0.5, 0.5, 0.5];
        for _ in 0..500 {
            tick(&mut lim, &inp, &mut out);
        }
        assert!(out[0].abs() > 1e-3, "{}", out[0]);
        for (c, &y) in out.iter().enumerate().skip(1) {
            let ratio = y.abs() / out[0].abs();
            assert!(
                (ratio - 0.5).abs() < 1e-5,
                "ch{c} not linked to ch0: ratio {ratio}"
            );
        }
    }

    /// How close the one-pole envelope gets to a steady target: it stops when
    /// a step rounds away, about `1 / coeff` ulps short (44 ulps at 1 ms).
    const SETTLED: f32 = 2e-5;

    /// A block of `n` frames of deterministic noise in `[-amp, amp]`.
    fn noise(seed: &mut u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Whatever loud noise goes in, the output never exceeds the ceiling:
    /// the gain window includes the delayed sample's own frame, so its gain is
    /// at most `ceiling / |x|`. The bound is exact, with no tolerance: this
    /// noise never rounds a product past the ceiling. Threshold at the
    /// ceiling, so the quiet gain is unity.
    #[test]
    fn loud_noise_stays_at_the_ceiling() {
        let ceiling = Db(-6.0);
        let limit = ceiling.to_amplitude().get();
        let mut lim = prepared_at(limiter(ceiling, ceiling), RATE_44K);
        let latency = lim.shape().latency.samples().get();
        let mut seed = 7;
        let mut output = Vec::new();
        for _ in 0..40 {
            let l = noise(&mut seed, 1024, 4.0);
            let r = noise(&mut seed, 1024, 4.0);
            output.extend(drive(&mut lim, RATE_44K, &[&l, &r], &[]).remove(0));
        }
        let mut limited = 0;
        for (i, &y) in output.iter().enumerate().skip(latency) {
            assert!(y.abs() <= limit, "frame {i}: {y} over {limit}");
            limited += usize::from(y.abs() > 0.9 * limit);
        }
        assert!(limited > 1000, "the limiter was exercised: {limited}");
    }

    /// A steady signal under the threshold passes at unity once the lookahead
    /// has filled; one over it settles at `ceiling / peak`.
    #[test]
    fn a_steady_signal_passes_under_and_settles_at_ceiling_over_peak() {
        let ceiling = Db(-6.0);
        let limit = ceiling.to_amplitude().get();
        for (level, want) in [(0.25f32, 1.0f32), (1.0, limit), (3.0, limit / 3.0)] {
            let mut lim = prepared_at(limiter_releasing(ceiling, ceiling, 0.001), RATE_44K);
            let x = vec![level; 1024];
            let mut out = Vec::new();
            for _ in 0..16 {
                out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
            }
            let want = level * want;
            for &y in &out[512..] {
                assert!(
                    (y - want).abs() <= SETTLED * want,
                    "level {level}: {y} against {want}"
                );
            }
        }
    }

    /// With the ceiling under the threshold the reduction is still
    /// `ceiling / peak` over the threshold, and the constant
    /// `ceiling / threshold` under it.
    #[test]
    fn a_ceiling_under_the_threshold_scales_by_the_gap_under_it() {
        let (threshold, ceiling) = (Db(-3.0), Db(-9.0));
        let (t, c) = (threshold.to_amplitude().get(), ceiling.to_amplitude().get());
        for (level, gain) in [(0.1f32, c / t), (2.0, c / 2.0)] {
            let mut lim = prepared_at(limiter_releasing(threshold, ceiling, 0.001), RATE_44K);
            let x = vec![level; 1024];
            let mut out = Vec::new();
            for _ in 0..16 {
                out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
            }
            let want = level * gain;
            assert!(
                (out[1023] - want).abs() <= SETTLED * want,
                "level {level}: {} against {want}",
                out[1023]
            );
        }
    }

    /// The steady-state output level in dB of a constant input at `in_db`.
    fn steady_out_db(threshold: Db, ceiling: Db, in_db: f32) -> f32 {
        let mut lim = prepared_at(limiter_releasing(threshold, ceiling, 0.001), RATE_44K);
        let level = Db(in_db).to_amplitude().get();
        let x = vec![level; 1024];
        let mut out = Vec::new();
        for _ in 0..16 {
            out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
        }
        let ceiling_lin = ceiling.to_amplitude().get();
        // The output never exceeds the ceiling (to a rounding step), in
        // every frame, including the release that rises from zero.
        assert!(
            out.iter().all(|y| y.abs() <= ceiling_lin * (1.0 + 1e-6)),
            "threshold {threshold:?}, ceiling {ceiling:?}, in {in_db}: past the ceiling"
        );
        amplitude_to_db(out[1023])
    }

    /// Threshold, ceiling pairs covering a boost, a cut, and no drive.
    const CURVES: [(f32, f32); 5] = [
        (-6.0, -0.3),
        (-12.0, -1.0),
        (-3.0, -9.0),
        (-6.0, -6.0),
        (-20.0, 0.0),
    ];

    /// The threshold drives the input: under the knee the output is the
    /// input plus `ceiling - threshold` dB, and over it the output sits at
    /// the ceiling. This is the curve for a ceiling above the threshold as
    /// well as below; it replaces a curve that boosted quiet signals by the
    /// drive but held loud ones at the threshold (a jump of the drive at the
    /// knee, and a louder input giving a quieter output).
    #[test]
    fn the_threshold_drives_the_input_and_loud_signals_settle_at_the_ceiling() {
        for (t, c) in CURVES {
            let (threshold, ceiling) = (Db(t), Db(c));
            let mut in_db = -80.0f32;
            while in_db <= 12.0 {
                let out_db = steady_out_db(threshold, ceiling, in_db);
                let want = if in_db <= t { in_db + (c - t) } else { c };
                assert!(
                    (out_db - want).abs() <= 2e-3,
                    "({t}, {c}) in {in_db}: {out_db} dB against {want}"
                );
                in_db += 1.0;
            }
        }
    }

    /// The steady-state transfer curve never decreases and never jumps: a
    /// 1 dB step of input moves the output by between 0 and 1 dB (the
    /// slope is the drive's unity under the knee and zero over it).
    #[test]
    fn the_transfer_curve_is_monotonic_and_continuous() {
        for (t, c) in CURVES {
            let mut prev: Option<f32> = None;
            let mut in_db = -80.0f32;
            while in_db <= 12.0 {
                let out_db = steady_out_db(Db(t), Db(c), in_db);
                if let Some(prev) = prev {
                    let step = out_db - prev;
                    assert!(step >= -2e-3, "({t}, {c}) at {in_db}: fell by {}", -step);
                    assert!(step <= 1.0 + 2e-3, "({t}, {c}) at {in_db}: jumped {step}");
                }
                prev = Some(out_db);
                in_db += 1.0;
            }
        }
    }

    /// A NaN sample passes through as a NaN and does not poison the state:
    /// the peak detector ignores it (`max`), so the gain is untouched and the
    /// frames after it limit as before.
    ///
    /// Two layers keep a NaN out: the peak fold's `max` ignores it, and
    /// `LimiterCurve::gain` sends one to the quiet gain. Either alone is
    /// absorbed (the envelope attacks straight back), so only losing
    /// both.
    #[test]
    fn a_nan_input_does_not_poison_the_gain() {
        let ceiling = Db(-6.0);
        let limit = ceiling.to_amplitude().get();
        let mut lim = prepared_at(limiter_releasing(ceiling, ceiling, 0.001), RATE_44K);
        let latency = lim.shape().latency.samples().get();
        let mut x = vec![1.0f32; 1024];
        x[10] = f32::NAN;
        let out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
        assert!(
            out[10 + latency].is_nan(),
            "the NaN is delayed, not dropped"
        );
        let x = vec![1.0f32; 1024];
        let out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
        for (i, &y) in out.iter().enumerate().skip(latency + 1) {
            assert!(
                (y - limit).abs() <= SETTLED * limit,
                "frame {i}: {y} against {limit}"
            );
        }
    }

    /// A control written NaN is ignored (read as the last value it held): the limiter keeps
    /// its ceiling and its output stays finite and bounded, where a NaN ceiling would make
    /// the gain a NaN and keep it so.
    #[test]
    fn a_nan_write_keeps_the_last_ceiling_and_threshold() {
        let mut lim = prepared_at(
            limiter_releasing(Db::new(-6.0), Db::new(-6.0), 0.005),
            RATE_44K,
        );
        let set = lim.param_set();
        assert!(set.set(UnitParam::Ceiling, f32::NAN));
        assert!(set.set(UnitParam::Threshold, f32::NAN));
        let limit = Db(-6.0).to_amplitude().get();
        let mut out = [0.0f32; 2];
        // 5 ms is 220 frames: 6 000 is 27 time constants, the envelope has settled.
        for _ in 0..6_000 {
            tick(&mut lim, &[1.0, 1.0], &mut out);
        }
        assert!((out[0] - limit).abs() <= 1e-4, "{}", out[0]);
    }

    /// Silence, a burst, silence, at thresholds so low that `10^((ceiling -
    /// threshold)/20)` overflows `f32` without the clamp in
    /// `LimiterCurve::new`: `0 * inf` would put a NaN on every silent sample.
    #[test]
    fn a_very_low_threshold_never_produces_a_nan() {
        for threshold in [f32::NEG_INFINITY, -800.0, -150.0] {
            let mut lim = prepared_at(limiter_releasing(Db(threshold), Db(-6.0), 0.001), RATE_44K);
            let mut x = vec![0.0f32; 1024];
            x[300..400].fill(2.0);
            x[400..500].fill(-0.001);
            let mut output = Vec::new();
            for _ in 0..4 {
                output.extend(drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0));
            }
            for (i, &y) in output.iter().enumerate() {
                assert!(y.is_finite(), "threshold {threshold}: frame {i} is {y}");
            }
        }
    }
}

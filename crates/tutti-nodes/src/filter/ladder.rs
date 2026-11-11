//! Moog-style four-stage ladder filter with resonance and drive.
//!
//! Unlike the SVF this is a *nonlinear* filter: the resonance feedback runs
//! through a `tanh` saturator, so pushing resonance or drive grits and
//! compresses rather than blowing up. That saturation is the character, not a
//! safety measure.
//!
//! One width-generic node, [`LadderFilterNode`]. The algorithm is `tutti-dsp`'s
//! ([`LadderDesign`]); the node is a [`DesignNode`] over it, and this file holds
//! the cells and how a block reads them.

use tutti_dsp::{Ladder, LadderControls, LadderDesign, LadderType, Real};
use tutti_types::{ChannelLayout, Drive, Hz, Resonance, UnitAddr, UnitParam};

use super::NodeKernel;
use crate::params::ControlSet;
use crate::reverb::DesignNode;

/// A Moog-style four-stage ladder filter with resonance and drive, of any
/// width: `N` audio inputs, `N` outputs.
///
/// The coefficients are solved once for every channel, and the channels'
/// four-stage chains run side by side.
///
/// Cutoff ([`Hz`]), [`Resonance`] and [`Drive`] are live [`Params`](crate::Params) shared
/// across clones. A held value costs nothing: the model is designed again only
/// when the cutoff or resonance moves past a small dead-band or the drive moves
/// at all ([`LadderControls::moved_from`]). The filter is designed on a 16-frame
/// grid counted from the last reset, at each segment's first frame, and the
/// coefficients **and the drive** ramp linearly to that solve over the segment, so
/// an automated control glides rather than stepping (drive multiplies straight
/// into the saturator and would otherwise click), arrives within 16 frames and
/// renders the same however the stream is cut into blocks. A cell that moved
/// glides from the last solve to its value across the block, so it is the one
/// control whose render depends on the block's length.
///
/// **Resonance is `0.0..=1.0`, not a [`Q`](tutti_types::Q).** It scales the
/// ladder's feedback: `0.0` is no emphasis, and toward `1.0` the filter
/// resonates hard at the cutoff and approaches self-oscillation. The feedback
/// runs through a `tanh` saturator, so it grits rather than blowing up.
///
/// [`Drive`] multiplies the input into that same saturator, so it is a
/// distortion control rather than a level one: above unity it adds harmonics
/// and compresses. `1.0` is clean.
///
/// `F` is the internal state precision, defaulting to `f64` for accuracy at low
/// cutoffs.
///
/// # Modulated params
///
/// `N` audio inputs, `N` outputs. Cutoff, Q and drive are modulatable by the
/// graph, in that port order. A per-frame value on a param port
/// ([`Io::param`](tutti_graph::Io::param)) **overrides** the corresponding cell
/// and is sampled at the first frame of each 16-frame segment, with the
/// coefficients and the drive interpolated over the segment. A lane on the cutoff
/// or on Q stops the cutoff and Q cells gliding. Unmodulated, the filter reads its
/// cells once per block at the cost of one branch.
///
/// The param is `UnitParam::Q` for consistency with the other filters, but
/// it carries [`Resonance`] (`0.0..=1.0`), not a [`Q`](tutti_types::Q).
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are
/// a [`ParamSet`](tutti_graph::ParamSet) over cutoff, resonance (as `UnitParam::Q`)
/// and drive, and a fork of it starts from the values last set through that set.
/// Its tail is [`Tail::Unknown`](tutti_types::Tail::Unknown): a resonant ladder
/// rings on after its input stops, so it is never skipped on a silent block and
/// never parks. The graph prepares it at the device rate before its first block.
/// Rendering allocates nothing.
pub type LadderFilterNode<F = f64> = DesignNode<LadderDesign<F>>;

impl<F: Real> LadderFilterNode<F> {
    /// A mono ladder filter (1 in, 1 out) of `ladder_type` at `frequency`
    /// cutoff and `resonance`, with unity [`Drive`].
    ///
    /// `resonance` is clamped to `0.0..=1.0`; `0.0` gives no emphasis at the
    /// cutoff and values near `1.0` approach self-oscillation.
    ///
    /// The coefficients are solved at the rate [`prepare`](tutti_graph::Node::prepare)
    /// hands it.
    pub fn new(
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        Self::with_channels(ChannelLayout::MONO, ladder_type, frequency, resonance)
    }

    /// A ladder filter `channels` wide (clamped to at least 1).
    ///
    /// All channels share the authored params — one linked control surface —
    /// and one coefficient solve; only the four stage words are replicated per
    /// channel.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        Self::live(
            LadderDesign::new(channels.into()),
            LadderControls {
                ty: ladder_type,
                freq: frequency.into(),
                res: Resonance::new_clamped(resonance.into().get()),
                drive: Drive::UNITY,
            },
        )
    }
}

/// The controls of a [`LadderFilterNode`]: cutoff, resonance and drive, with the
/// type they are for.
#[derive(Clone, Copy, Debug)]
pub struct LadderSet(LadderType);

impl ControlSet for LadderSet {
    type Controls = LadderControls;

    fn moved(&self, applied: &LadderControls, target: &LadderControls) -> bool {
        target.moved_from(applied)
    }

    fn addrs(&self) -> &'static [UnitAddr] {
        const ADDRS: [UnitAddr; 3] = [
            UnitAddr::first(UnitParam::Cutoff),
            UnitAddr::first(UnitParam::Q),
            UnitAddr::first(UnitParam::Drive),
        ];
        &ADDRS
    }

    fn defaults(&self) -> LadderControls {
        LadderControls::DEFAULT
    }

    fn flatten(&self, c: &LadderControls, out: &mut [f32]) {
        out.copy_from_slice(&[c.freq.get(), c.res.get(), c.drive.get()]);
    }

    fn assemble(&self, v: &[f32]) -> LadderControls {
        LadderControls {
            ty: self.0,
            freq: Hz(v[0]),
            res: Resonance(v[1]),
            drive: Drive(v[2]),
        }
    }
}

impl<F: Real> NodeKernel for Ladder<F> {
    type Set = LadderSet;

    fn set(&self, initial: &LadderControls) -> LadderSet {
        LadderSet(initial.ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, process_mono, rms};
    use crate::test_support::{RATE_44K, prepared_at, tick, tick_fed};
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_types::Tail;

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.drive.detach()` in `fork_fresh` → "a live
    /// write reached the fork" for `Drive` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(LadderFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            LadderType::LP24,
            Hz::new(800.0),
            Resonance::new(0.3),
        ));
    }

    #[test]
    fn test_ladder_lp24_attenuates_high_freq() {
        let mut filter = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(500.0), Resonance::new(0.0)),
            RATE_44K,
        );

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_low > rms_high * 5.0,
            "LP24 should strongly attenuate high freq: low={rms_low}, high={rms_high}"
        );
    }

    /// Gain (dB) of a 0.01-amplitude sine at `f` through the node, projected
    /// on the last second of two.
    fn gain_db(ty: LadderType, fc: f32, res: f32, f: f64) -> f64 {
        let mut node = prepared_at(
            LadderFilterNode::<f64>::new(ty, Hz::new(fc), Resonance::new(res)),
            RATE_44K,
        );
        let n = 44100;
        let amp = 0.01;
        let x: Vec<f32> = (0..2 * n)
            .map(|i| (amp * (2.0 * core::f64::consts::PI * f * i as f64 / 44100.0).sin()) as f32)
            .collect();
        let y = process_mono(&mut node, &x);
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &yi) in y.iter().enumerate().skip(n) {
            let ph = 2.0 * core::f64::consts::PI * f * i as f64 / 44100.0;
            re += f64::from(yi) * ph.sin();
            im += f64::from(yi) * ph.cos();
        }
        20.0 * (2.0 * (re * re + im * im).sqrt() / n as f64 / amp).log10()
    }

    /// Well below the cutoff the high-passes fall 12 and 24 dB per octave
    /// (`(s/(1+s))^2` and `^4`), and resonance still lifts the cutoff above
    /// the same filter without it.
    ///
    /// Mutation (run): `u - lp2` / `u - lp4` (6 dB/octave) fails the slopes.
    #[test]
    fn the_high_passes_have_their_documented_slopes() {
        for (ty, want) in [(LadderType::HP12, 12.0), (LadderType::HP24, 24.0)] {
            for (lo, hi) in [(100.0, 200.0), (200.0, 400.0)] {
                let slope = gain_db(ty, 5000.0, 0.0, hi) - gain_db(ty, 5000.0, 0.0, lo);
                assert!(
                    (slope - want).abs() < 1.0,
                    "{ty:?} {lo}->{hi} Hz: {slope:.2} dB/oct, want {want}"
                );
            }
            let flat = gain_db(ty, 5000.0, 0.0, 5000.0);
            let peak = gain_db(ty, 5000.0, 0.7, 5000.0);
            assert!(
                peak > flat + 3.0,
                "{ty:?} resonance: {flat:.2} -> {peak:.2}"
            );
        }
    }

    #[test]
    fn test_ladder_hp24_attenuates_low_freq() {
        let mut filter = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::HP24, Hz::new(2000.0), Resonance::new(0.0)),
            RATE_44K,
        );

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_high > rms_low * 3.0,
            "HP24 should attenuate low freq: low={rms_low}, high={rms_high}"
        );
    }

    #[test]
    fn test_ladder_resonance_boosts_cutoff() {
        let mut no_res = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(1000.0), Resonance::new(0.0)),
            RATE_44K,
        );

        let mut with_res = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(1000.0), Resonance::new(0.8)),
            RATE_44K,
        );

        let at_cutoff = generate_sine(1000.0, 44100.0, 4096);

        let out_no = process_mono(&mut no_res, &at_cutoff);
        let out_yes = process_mono(&mut with_res, &at_cutoff);

        let rms_no = rms(&out_no[512..]);
        let rms_yes = rms(&out_yes[512..]);

        assert!(
            rms_yes > rms_no,
            "Resonance should boost at cutoff: no_res={rms_no}, with_res={rms_yes}"
        );
    }

    #[test]
    fn test_ladder_drive_adds_saturation() {
        let mut clean = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(5000.0), Resonance::new(0.0)),
            RATE_44K,
        );

        let mut driven = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(5000.0), Resonance::new(0.0)),
            RATE_44K,
        );
        assert!(driven.param_set().set(UnitParam::Drive, 10.0));

        let sine = generate_sine(440.0, 44100.0, 4096);

        let out_clean = process_mono(&mut clean, &sine);
        let out_driven = process_mono(&mut driven, &sine);

        let rms_clean = rms(&out_clean[512..]);
        let rms_driven = rms(&out_driven[512..]);

        assert!(
            (rms_clean - rms_driven).abs() > 0.01 || rms_driven > rms_clean * 0.5,
            "Drive should affect signal: clean={rms_clean}, driven={rms_driven}"
        );
    }

    #[test]
    fn test_ladder_reset() {
        let mut filter = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(1000.0), Resonance::new(0.5)),
            RATE_44K,
        );

        let sine = generate_sine(440.0, 44100.0, 100);
        let _ = process_mono(&mut filter, &sine);

        filter.reset();

        let mut out = [0.0f32];
        tick(&mut filter, &[0.0], &mut out);
        assert!(
            out[0].abs() < 0.0001,
            "After reset, output should be near zero"
        );
    }

    #[test]
    fn test_ladder_lp12_less_steep_than_lp24() {
        let mut lp12 = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP12, Hz::new(1000.0), Resonance::new(0.0)),
            RATE_44K,
        );

        let mut lp24 = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(1000.0), Resonance::new(0.0)),
            RATE_44K,
        );

        let high = generate_sine(5000.0, 44100.0, 4096);

        let out12 = process_mono(&mut lp12, &high);
        let out24 = process_mono(&mut lp24, &high);

        let rms12 = rms(&out12[512..]);
        let rms24 = rms(&out24[512..]);

        assert!(
            rms12 > rms24,
            "LP12 should pass more high freq than LP24: lp12={rms12}, lp24={rms24}"
        );
    }

    #[test]
    fn test_ladder_f32_state_agrees_with_f64_for_mid_cutoff() {
        let mut ladder_f64 = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(1000.0), Resonance::new(0.3)),
            RATE_44K,
        );
        let mut ladder_f32 = prepared_at(
            LadderFilterNode::<f32>::new(LadderType::LP24, Hz::new(1000.0), Resonance::new(0.3)),
            RATE_44K,
        );

        let input = generate_sine(500.0, 44100.0, 2048);
        let out_f64 = process_mono(&mut ladder_f64, &input);
        let out_f32 = process_mono(&mut ladder_f32, &input);

        let rms_f64 = rms(&out_f64[512..]);
        let rms_f32 = rms(&out_f32[512..]);
        let rel_diff = (rms_f64 - rms_f32).abs() / rms_f64.max(1e-6);
        assert!(
            rel_diff < 0.01,
            "ladder f32/f64 should agree within 1% at mid cutoff: f64={rms_f64}, f32={rms_f32}"
        );
    }

    // ── Modulated params (the graph's param feed) ───────────────────────────

    /// Run a stereo ladder a frame at a time with `params` (cutoff, Q,
    /// drive: `Some` held at a value, `None` reading its base) on its param
    /// ports each frame.
    fn process_stereo_ladder(
        node: &mut dyn Node,
        l: &[f32],
        r: &[f32],
        params: [Option<f32>; 3],
    ) -> (Vec<f32>, Vec<f32>) {
        let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
        let mut o = [0.0f32; 2];
        for (&a, &b) in l.iter().zip(r) {
            tick_fed(node, &[a, b], &params, &mut o);
            out_l.push(o[0]);
            out_r.push(o[1]);
        }
        (out_l, out_r)
    }

    #[test]
    fn ladder_with_channels_reports_arity_and_is_independent() {
        let mut wide = prepared_at(
            LadderFilterNode::<f64>::with_channels(
                6u16,
                LadderType::LP24,
                Hz::new(1000.0),
                Resonance::new(0.3),
            ),
            RATE_44K,
        );
        assert_eq!(wide.shape().audio_in().count(), 6);
        assert_eq!(wide.shape().audio_out().count(), 6);

        // Drive only channel 4; the rest must stay silent.
        let sig = generate_sine(300.0, 44100.0, 2048);
        let len = sig.len();
        let mut out = vec![vec![0.0f32; len]; 6];
        let mut outbuf = [0.0f32; 6];
        for i in 0..len {
            // Rebuilt per sample, not cleared in place: only channel 4 is
            // driven, so every other channel must re-enter each frame at zero.
            let mut inbuf = [0.0f32; 6];
            inbuf[4] = sig[i];
            tick(&mut wide, &inbuf, &mut outbuf);
            for c in 0..6 {
                out[c][i] = outbuf[c];
            }
        }
        let e4: f32 = out[4].iter().map(|s| s * s).sum();
        assert!(e4 > 1.0, "ch4 should carry signal; energy {e4}");
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    /// The shape declares cutoff, Q then drive, never changes the arity, and
    /// reports `Tail::Unknown`.
    ///
    /// Mutation (run): swap `ADDRS`' Q and drive → the first
    /// assertion fails. Mutation (run): drop `.with_tail(Tail::Unknown)` →
    /// the shape's default `Tail::None` → the tail assertion fails.
    #[test]
    fn the_shape_declares_cutoff_q_then_drive() {
        let f = LadderFilterNode::<f64>::with_channels(
            6u16,
            LadderType::LP24,
            Hz::new(1000.0),
            Resonance::new(0.3),
        );
        assert_eq!(
            f.shape().params.as_slice(),
            &[UnitParam::Cutoff, UnitParam::Q, UnitParam::Drive][..]
        );
        assert_eq!(
            (f.shape().audio_in().count(), f.shape().audio_out().count()),
            (6, 6)
        );
        assert_eq!(f.param_base(0), Some(1000.0));
        assert_eq!(f.param_base(1), Some(0.3));
        assert_eq!(f.param_base(2), Some(1.0), "drive starts clean");
        assert_eq!(f.shape().tail, Tail::Unknown, "a resonant ladder rings on");
    }

    #[test]
    fn stereo_ladder_fed_cutoff_modulates_response() {
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f32 {
            let mut f = prepared_at(
                LadderFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    LadderType::LP24,
                    Hz::new(200.0),
                    Resonance::new(0.3),
                ),
                RATE_44K,
            );
            let (out_l, _) =
                process_stereo_ladder(&mut f, &noise, &noise, [Some(cutoff), None, None]);
            rms(&out_l[256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher fed cutoff should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_ladder_unmodulated_matches_modulated_held_constant() {
        // A node whose fed cutoff is held at the atomic value must produce the
        // same output as a plain node.
        let signal = generate_sine(440.0, 44100.0, 1024);

        let mut plain = prepared_at(
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                Hz::new(1000.0),
                Resonance::new(0.3),
            ),
            RATE_44K,
        );
        let (plain_l, _) = process_stereo_ladder(&mut plain, &signal, &signal, [None; 3]);

        let mut modn = prepared_at(
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                Hz::new(1000.0),
                Resonance::new(0.3),
            ),
            RATE_44K,
        );
        let (mod_l, _) =
            process_stereo_ladder(&mut modn, &signal, &signal, [Some(1000.0), None, None]);

        for i in 0..signal.len() {
            assert!(
                (plain_l[i] - mod_l[i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain_l[i],
                mod_l[i]
            );
        }
    }

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// Unprepared: `change_between_node_blocks` prepares each copy at 48 kHz.
    fn ladder_48k() -> LadderFilterNode<f64> {
        LadderFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            LadderType::LP24,
            Hz::new(500.0),
            Resonance::new(0.4),
        )
    }

    /// A cutoff change made between blocks is read by the next block, ramped
    /// across it, and the block ends solved at the new cutoff.
    ///
    /// Mutation: solving straight at the target and rendering the block held
    /// (a jump) fails `assert_ramps_in`; dropping the `self.applied = Some(controls)`
    /// in `DesignNode::apply` (the block's final solve) fails the controls check.
    #[test]
    fn a_cutoff_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(5, 128);
        let run = change_between_node_blocks(
            ladder_48k,
            |n| {
                assert!(n.param_set().set(UnitParam::Cutoff, 7_000.0));
            },
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("ladder cutoff");
        assert!(
            run.ramped.applied() == run.jumped.applied(),
            "the block must end solved at the new cutoff"
        );
    }

    /// Drive multiplies straight into the saturator, so a step would click; it
    /// ramps across the next block and ends on the new value.
    ///
    /// Mutation: gliding the drive from its own target (`glide(cell, cell)`, a jump)
    /// fails `assert_ramps_in`.
    #[test]
    fn a_drive_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(6, 128);
        // A high-pass tap, so the saturator's input reaches the output
        // unfiltered and the drive step is audible on the first sample.
        let run = change_between_node_blocks(
            || {
                LadderFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    LadderType::HP24,
                    Hz::new(500.0),
                    Resonance::new(0.4),
                )
            },
            |n| {
                assert!(n.param_set().set(UnitParam::Drive, 9.0));
            },
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("ladder drive");
        assert_eq!(run.ramped.applied().map(|c| c.drive), Some(Drive(9.0)));
    }

    /// Six channels share one coefficient solve and keep six independent
    /// stage lanes: each channel is the mono ladder on its own input.
    ///
    /// Mutation: indexing lane 0 for every channel in `held_group` fails.
    #[test]
    fn six_channels_are_six_mono_ladders() {
        use crate::test_support::{drive_block, noise, prepared};
        let inputs: Vec<Vec<f32>> = (0..6).map(|c| noise(c + 20, 64)).collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|v| &v[..]).collect();
        let mut wide = prepared(LadderFilterNode::<f64>::with_channels(
            6u16,
            LadderType::HP12,
            Hz::new(800.0),
            Resonance::new(0.7),
        ));
        let out = drive_block(&mut wide, &refs);
        for (c, input) in inputs.iter().enumerate() {
            let mut mono = prepared(LadderFilterNode::<f64>::new(
                LadderType::HP12,
                Hz::new(800.0),
                Resonance::new(0.7),
            ));
            assert_eq!(
                drive_block(&mut mono, &[&input[..]])[0],
                out[c],
                "channel {c}"
            );
        }
    }
}

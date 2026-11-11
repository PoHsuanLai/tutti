//! State-variable filter — the crate's general-purpose filter.
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change. One width-generic node,
//! [`SvfFilterNode`]: the coefficients are shared across channels and only the
//! integrator state is per channel. The algorithm is `tutti-dsp`'s
//! ([`SvfDesign`]); the node is a [`DesignNode`] over it, and this file holds the
//! cells and how a block reads them.

use tutti_dsp::{Real, Svf, SvfControls, SvfDesign, SvfType};
use tutti_types::{ChannelLayout, Db, Hz, Q, UnitAddr, UnitParam};

use super::NodeKernel;
use crate::params::ControlSet;
use crate::reverb::DesignNode;

/// A state-variable filter of any width: `N` audio inputs, `N` outputs, one
/// coefficient set shared across the channels and one integrator pair per
/// channel.
///
/// Eight responses ([`SvfType`]) share one coefficient solve (Cytomic /
/// Zavalishin TPT form), so switching type is as cheap as any parameter
/// change. The response is fixed at construction: a node with another one is
/// another node.
///
/// # Examples
///
/// ```
/// use tutti_types::{Hz, Q, UnitParam};
/// use tutti_graph::{Prepare, Solo};
/// use tutti_nodes::{SvfFilterNode, SvfType};
/// use tutti_types::{SampleRate, Samples};
///
/// let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
/// let mut solo = Solo::new(filter, Prepare::new(SampleRate(48_000.0), Samples(64)));
/// let out = solo.render_input(&[&[1.0; 64]]);
/// assert_eq!(out.len(), 1);
///
/// // Move the cutoff on the running node; it glides over the next block.
/// assert!(solo.controls().set(UnitParam::Cutoff, 2_000.0));
/// ```
///
/// Cutoff ([`Hz`]), [`Q`] and gain ([`Db`]) are live [`Params`](crate::Params) shared across
/// clones. A held value costs nothing: the model is designed again only when one
/// moves past a small dead-band ([`SvfControls::moved_from`]). The filter is
/// designed on a 16-frame grid counted from the last reset, at each segment's
/// first frame, and the coefficients ramp linearly to that solve over the segment,
/// so an automated cutoff glides rather than stepping, arrives within 16 frames
/// and renders the same however the stream is cut into blocks. A cell that moved
/// glides from the last solve to its value across the block, so it is the one
/// control whose render depends on the block's length.
///
/// Gain applies only to [`Bell`](SvfType::Bell),
/// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf); the
/// other types ignore it.
///
/// `F` is the internal state precision. It defaults to `f64` for accuracy at
/// low cutoffs, where an `f32` integrator loses resolution in the feedback
/// path; `SvfFilterNode::<f32>::new(…)` trades that for CPU.
///
/// # Modulated params
///
/// Cutoff (in [`Hz`]), [`Q`] and the gain (in [`Db`]) are modulatable by the
/// graph, in that port order. A per-frame value on a param port
/// ([`Io::param`](tutti_graph::Io::param)) **overrides** the corresponding cell
/// (and a lane on any of the three stops the cells gliding): it is sampled at the
/// first frame of each 16-frame segment, with the coefficients interpolated over
/// the segment, instead of a `tan` per sample. Unmodulated, the filter reads its
/// cells once per block at the cost of one branch, which is the common case.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are
/// a [`ParamSet`](tutti_graph::ParamSet) over cutoff, Q and gain by [`UnitParam`],
/// and a fork of it starts from the values last set through that set. Its tail is
/// [`Tail::Unknown`](tutti_types::Tail::Unknown): a resonant filter rings on after
/// its input stops, for as long as its Q says, so the graph's tail fold must not
/// read it as `None`; it never parks. The graph prepares it at the device rate
/// before its first block. Rendering allocates nothing.
pub type SvfFilterNode<F = f64> = DesignNode<SvfDesign<F>>;

impl<F: Real> SvfFilterNode<F> {
    /// A mono filter (1 in, 1 out) of `filter_type` at `frequency` cutoff and
    /// resonance `q`, with 0 dB gain.
    ///
    /// `q` around `0.707` is the flattest (Butterworth) response; higher values
    /// resonate at the cutoff, and a band-pass or notch narrows as it rises.
    ///
    /// The coefficients are solved at the rate [`prepare`](tutti_graph::Node::prepare)
    /// hands it.
    pub fn new(filter_type: SvfType, frequency: impl Into<Hz>, q: impl Into<Q>) -> Self {
        Self::with_channels(ChannelLayout::MONO, filter_type, frequency, q)
    }

    /// A filter `channels` wide (clamped to at least 1).
    ///
    /// The coefficients are channel-shared — one linked control surface across
    /// all channels — and only the per-channel integrator state is replicated,
    /// so widening costs state but not parameter work.
    ///
    /// Speaker placement is the upstream panner's job: this is a per-channel
    /// filter, not a spatial process.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
    ) -> Self {
        Self::with_controls(
            channels,
            SvfControls {
                ty: filter_type,
                freq: frequency.into(),
                q: q.into(),
                gain: Db(0.0),
            },
        )
    }

    /// A filter `channels` wide with every control of `controls` (the response,
    /// cutoff, Q and the gain of a bell or shelf, read only by those three).
    ///
    /// [`new`](Self::new) and [`with_channels`](Self::with_channels) start at
    /// 0 dB; this is the constructor of a gain-bearing filter. The response is
    /// fixed here: a node with another one is another node.
    #[must_use]
    pub fn with_controls(channels: impl Into<ChannelLayout>, controls: SvfControls) -> Self {
        Self::live(SvfDesign::new(channels.into()), controls)
    }
}

/// The controls of an [`SvfFilterNode`]: cutoff, Q and gain, with the response
/// they are for.
#[derive(Clone, Copy, Debug)]
pub struct SvfSet(SvfType);

impl ControlSet for SvfSet {
    type Controls = SvfControls;

    fn moved(&self, applied: &SvfControls, target: &SvfControls) -> bool {
        target.moved_from(applied)
    }

    fn addrs(&self) -> &'static [UnitAddr] {
        const ADDRS: [UnitAddr; 3] = [
            UnitAddr::first(UnitParam::Cutoff),
            UnitAddr::first(UnitParam::Q),
            UnitAddr::first(UnitParam::GainDb),
        ];
        &ADDRS
    }

    fn defaults(&self) -> SvfControls {
        SvfControls::DEFAULT
    }

    fn flatten(&self, c: &SvfControls, out: &mut [f32]) {
        out.copy_from_slice(&[c.freq.get(), c.q.get(), c.gain.get()]);
    }

    fn assemble(&self, v: &[f32]) -> SvfControls {
        SvfControls {
            ty: self.0,
            freq: Hz(v[0]),
            q: Q(v[1]),
            gain: Db(v[2]),
        }
    }
}

impl<F: Real> NodeKernel for Svf<F> {
    type Set = SvfSet;

    fn set(&self, initial: &SvfControls) -> SvfSet {
        SvfSet(initial.ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, impulse, rms};
    use crate::test_support::{RATE, drive_block, prepared};
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode};
    use tutti_types::{SampleRate, Tail};

    /// `node`, prepared at 44.1 kHz, over a mono `input` in one block.
    fn render(node: SvfFilterNode, input: &[f32]) -> Vec<f32> {
        let rate = SampleRate(44_100.0);
        let mut node = tutti_graph::contract::prepared(node, rate, input.len());
        drive(&mut node, rate, &[input], &[]).remove(0)
    }

    /// `node`, prepared at 44.1 kHz, over `inputs` (one slice per channel)
    /// in one block.
    fn render_wide(node: SvfFilterNode, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
        let rate = SampleRate(44_100.0);
        let mut node = tutti_graph::contract::prepared(node, rate, inputs[0].len());
        drive(&mut node, rate, inputs, &[])
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it. See
    /// `tutti_graph::contract::assert_param_fork` for what it pins.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = SvfFilterNode::<f64>::with_controls(
            ChannelLayout::MONO,
            SvfControls {
                ty: SvfType::Bell,
                freq: Hz::new(500.0),
                q: Q::new(0.707),
                gain: Db::new(-3.0),
            },
        );
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb]
        );
        assert_param_fork(node);
    }

    #[test]
    fn test_svf_lowpass_attenuates_high_freq() {
        let make = || SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(500.0), Q::new(0.707));
        let out_low = render(make(), &generate_sine(100.0, 44100.0, 4096));
        let out_high = render(make(), &generate_sine(5000.0, 44100.0, 4096));
        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);
        assert!(
            rms_low > rms_high * 3.0,
            "LP should pass low freq ({rms_low}) much more than high ({rms_high})"
        );
    }

    #[test]
    fn test_svf_highpass_attenuates_low_freq() {
        let make = || SvfFilterNode::<f64>::new(SvfType::HighPass, Hz::new(2000.0), Q::new(0.707));
        let out_low = render(make(), &generate_sine(100.0, 44100.0, 4096));
        let out_high = render(make(), &generate_sine(5000.0, 44100.0, 4096));
        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);
        assert!(
            rms_high > rms_low * 3.0,
            "HP should pass high freq ({rms_high}) much more than low ({rms_low})"
        );
    }

    #[test]
    fn test_svf_bandpass() {
        let make = || SvfFilterNode::<f64>::new(SvfType::BandPass, Hz::new(1000.0), Q::new(5.0));
        let out_on = render(make(), &generate_sine(1000.0, 44100.0, 4096));
        let out_off = render(make(), &generate_sine(100.0, 44100.0, 4096));
        let rms_on = rms(&out_on[512..]);
        let rms_off = rms(&out_off[512..]);
        assert!(
            rms_on > rms_off * 2.0,
            "BP should favor center freq ({rms_on}) over off-freq ({rms_off})"
        );
    }

    #[test]
    fn test_svf_notch() {
        let make = || SvfFilterNode::<f64>::new(SvfType::Notch, Hz::new(1000.0), Q::new(5.0));
        let out_on = render(make(), &generate_sine(1000.0, 44100.0, 4096));
        let out_off = render(make(), &generate_sine(5000.0, 44100.0, 4096));
        let rms_on = rms(&out_on[512..]);
        let rms_off = rms(&out_off[512..]);
        assert!(
            rms_off > rms_on * 2.0,
            "Notch should reject center ({rms_on}) vs off-freq ({rms_off})"
        );
    }

    #[test]
    fn test_svf_allpass_flat_magnitude() {
        // An allpass passes all frequencies at ~unity magnitude (it only shifts
        // phase). RMS out should track RMS in at both a low and a high tone,
        // unlike a notch which would attenuate near its centre.
        for hz in [200.0, 1000.0, 5000.0] {
            let sig = generate_sine(hz, 44100.0, 8192);
            let out = render(
                SvfFilterNode::<f64>::new(SvfType::Allpass, Hz::new(1000.0), Q::new(0.707)),
                &sig,
            );
            let (rin, rout) = (rms(&sig[1024..]), rms(&out[1024..]));
            let rel = (rout - rin).abs() / rin.max(1e-6);
            assert!(
                rel < 0.05,
                "allpass should preserve magnitude at {hz} Hz: in={rin}, out={rout} (rel {rel})"
            );
        }
    }

    #[test]
    fn test_svf_impulse_response_finite() {
        let ir = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(1000.0), Q::new(0.707)),
            &impulse(1024),
        );
        let first_half_energy: f32 = ir[..512].iter().map(|s| s * s).sum();
        let second_half_energy: f32 = ir[512..].iter().map(|s| s * s).sum();
        assert!(
            first_half_energy > second_half_energy,
            "IR should decay: first_half={first_half_energy}, second_half={second_half_energy}"
        );
    }

    /// `reset` clears the integrators: silence in is silence out.
    #[test]
    fn test_svf_reset() {
        let mut filter = prepared(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz::new(1000.0),
            Q::new(0.707),
        ));
        drive_block(&mut filter, &[&generate_sine(100.0, 48_000.0, 100)]);
        Node::reset(&mut filter);
        let out = drive_block(&mut filter, &[&[0.0; 4]]);
        assert!(
            out[0].iter().all(|s| s.abs() < 1e-4),
            "After reset, output should be near zero: {:?}",
            out[0]
        );
    }

    /// The by-address path: a `UnitParam` set through the node's
    /// `ParamSet` lands in the same cell the bespoke setter writes, and an
    /// address the node does not own is refused.
    #[test]
    fn test_svf_set_via_unit_param() {
        let node = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            Hz::new(1000.0),
            Q::new(0.707),
        );
        let params = node.param_set();
        assert!(params.set(UnitParam::Cutoff, 5000.0));
        assert!(params.set(UnitParam::Q, 2.5));
        assert_eq!(node.param_base(0), Some(5000.0));
        assert_eq!(node.param_base(1), Some(2.5));
        assert!(!params.set(UnitParam::Wet, 0.5));
    }

    #[test]
    fn test_svf_parameter_modulation() {
        let noise: Vec<f32> = (0..1000)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let out1 = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(500.0), Q::new(0.707)),
            &noise,
        );
        let high = SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(500.0), Q::new(0.707));
        assert!(high.param_set().set(UnitParam::Cutoff, 5000.0));
        let out2 = render(high, &noise);
        let rms1 = rms(&out1[100..]);
        let rms2 = rms(&out2[100..]);
        assert!(
            rms2 > rms1,
            "Higher cutoff should pass more signal: low_cutoff={rms1}, high_cutoff={rms2}"
        );
    }

    #[test]
    fn test_svf_f32_state_matches_f64_for_mid_cutoff() {
        let input = generate_sine(500.0, 44100.0, 2048);
        let rate = SampleRate(44_100.0);
        let mut f64_node = tutti_graph::contract::prepared(
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(1000.0), Q::new(0.707)),
            rate,
            2048,
        );
        let mut f32_node = tutti_graph::contract::prepared(
            SvfFilterNode::<f32>::new(SvfType::LowPass, Hz::new(1000.0), Q::new(0.707)),
            rate,
            2048,
        );
        let out_f64 = drive(&mut f64_node, rate, &[&input], &[]).remove(0);
        let out_f32 = drive(&mut f32_node, rate, &[&input], &[]).remove(0);
        let rms_f64 = rms(&out_f64[512..]);
        let rms_f32 = rms(&out_f32[512..]);
        let rel_diff = (rms_f64 - rms_f32).abs() / rms_f64.max(1e-6);
        assert!(
            rel_diff < 0.01,
            "f32 and f64 SVF should agree within 1% at mid cutoff: f64={rms_f64}, f32={rms_f32}"
        );
    }

    #[test]
    fn test_stereo_svf_matches_mono_per_channel() {
        // Identical signal on L+R must match what a mono SVF would produce.
        let signal = generate_sine(440.0, 44100.0, 2048);
        let mono_out = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(1000.0), Q::new(0.707)),
            &signal,
        );
        let out = render_wide(
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                Hz::new(1000.0),
                Q::new(0.707),
            ),
            &[&signal, &signal],
        );
        for i in 0..signal.len() {
            assert!(
                (mono_out[i] - out[0][i]).abs() < 1e-5,
                "L channel diverges from mono at sample {i}"
            );
            assert!(
                (mono_out[i] - out[1][i]).abs() < 1e-5,
                "R channel diverges from mono at sample {i}"
            );
        }
    }

    #[test]
    fn test_stereo_svf_per_channel_independence() {
        // Different signals on L vs R should not bleed across channels.
        let l_in = generate_sine(200.0, 44100.0, 2048);
        let r_in = vec![0.0f32; 2048];
        let out = render_wide(
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                Hz::new(1000.0),
                Q::new(0.707),
            ),
            &[&l_in, &r_in],
        );
        let l_energy: f32 = out[0].iter().map(|s| s * s).sum();
        let r_energy: f32 = out[1].iter().map(|s| s * s).sum();
        assert!(
            r_energy < 1e-10,
            "R should stay silent when only L has input; got energy {r_energy}"
        );
        assert!(l_energy > 1.0, "L should carry meaningful signal");
    }

    #[test]
    fn test_stereo_svf_reset_clears_both_channels() {
        let mut stereo = prepared(SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            Hz::new(1000.0),
            Q::new(0.707),
        ));
        let signal = generate_sine(100.0, 48_000.0, 200);
        drive_block(&mut stereo, &[&signal, &signal]);
        Node::reset(&mut stereo);
        let out = drive_block(&mut stereo, &[&[0.0], &[0.0]]);
        assert!(out[0][0].abs() < 1e-4 && out[1][0].abs() < 1e-4);
    }

    /// The shape is as wide as the filter was built, in and out, declares
    /// cutoff, Q and gain as its modulatable params, and reports
    /// `Tail::Unknown`. (Doc 014 §12.6 added the gain port: a route onto an
    /// SVF's gain was refused once the frame-rate driver went.)
    #[test]
    fn the_shape_is_the_width_and_declares_cutoff_q_and_gain() {
        let f = SvfFilterNode::<f64>::with_channels(
            6u16,
            SvfType::LowPass,
            Hz::new(1000.0),
            Q::new(0.7),
        );
        let shape = f.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (6, 6)
        );
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb][..]
        );
        assert_eq!(
            f.param_base(0),
            Some(1000.0),
            "cutoff's base is its control"
        );
        assert_eq!(f.param_base(1), Some(0.7), "Q's base is its control");
        assert_eq!(f.param_base(2), Some(0.0), "gain's base is its control");
        assert_eq!(shape.tail, Tail::Unknown, "a resonant filter rings on");
    }

    #[test]
    fn wide_channel_matches_mono_and_is_independent() {
        // Each of 6 channels must filter exactly like a mono SVF with the same
        // coeffs, and carry only its own input (no cross-channel bleed).
        let sig = generate_sine(300.0, 44100.0, 2048);
        let silent = vec![0.0f32; sig.len()];
        let mut frames: Vec<&[f32]> = vec![&silent; 6];
        frames[4] = &sig;
        let out = render_wide(
            SvfFilterNode::<f64>::with_channels(
                6u16,
                SvfType::LowPass,
                Hz::new(1000.0),
                Q::new(0.707),
            ),
            &frames,
        );
        let mono_out = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(1000.0), Q::new(0.707)),
            &sig,
        );
        for i in 0..sig.len() {
            assert!(
                (mono_out[i] - out[4][i]).abs() < 1e-5,
                "ch4 diverges from mono at {i}"
            );
        }
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    // ── Modulated params (the graph's param ports) ──────────────────────────

    #[test]
    fn stereo_svf_fed_cutoff_modulates_response() {
        // Same noise through a low-pass: a cutoff fed high should pass more
        // energy than the same node fed low. Proves the param port actually
        // drives the coefficients.
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f32 {
            let mut f = prepared(SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                Hz::new(200.0),
                Q::new(0.707),
            ));
            let held = vec![cutoff; noise.len()];
            let out = drive(&mut f, RATE, &[&noise, &noise], &[Some(&held), None]);
            rms(&out[0][256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher fed cutoff should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_svf_unmodulated_process_matches_modulated_held_constant() {
        // A node whose fed cutoff is held at the same value as an unmodulated
        // node's cell must produce the same output — the modulated path is a
        // faithful superset.
        let signal = generate_sine(440.0, 48_000.0, 1024);
        let make = || {
            prepared(SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                Hz::new(1000.0),
                Q::new(0.707),
            ))
        };
        let plain = drive_block(&mut make(), &[&signal, &signal]);
        let held = vec![1000.0f32; signal.len()];
        let modn = drive(&mut make(), RATE, &[&signal, &signal], &[Some(&held), None]);
        for i in 0..signal.len() {
            assert!(
                (plain[0][i] - modn[0][i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain[0][i],
                modn[0][i]
            );
        }
    }

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// A cutoff change made between blocks is read by the next block and
    /// ramped across it, and that block ends solved exactly at the new cutoff.
    ///
    /// (A write *during* a block cannot be staged in a single-threaded test —
    /// the control thread writes between callbacks — so "between blocks" is the
    /// observable form of "not before the next block".)
    #[test]
    fn a_cutoff_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(3, 128);
        let run = change_between_node_blocks(
            || {
                SvfFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    SvfType::LowPass,
                    Hz::new(400.0),
                    Q::new(0.707),
                )
            },
            |n| {
                assert!(n.param_set().set(UnitParam::Cutoff, 6_000.0));
            },
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("svf cutoff");
        assert!(
            run.ramped.applied() == run.jumped.applied(),
            "the block must end solved at the new cutoff"
        );
    }

    /// A NaN Q at construction is read as the Butterworth Q, not as 0, which
    /// is below the type's floor. A low-pass at 800 Hz on an 800 Hz sine has a
    /// steady-state gain of Q at the cutoff, so RMS is `0.7071 · 0.7071 = 0.5` (0.457 measured: the bilinear warp costs a few percent);
    /// a later finite Q takes effect, and `reset` keeps the output finite.
    #[test]
    fn a_nan_q_at_construction_is_butterworth_and_recovers() {
        let x = generate_sine(800.0, 44_100.0, 8 * 1024);
        let mut f = prepared(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz::new(800.0),
            Q(f32::NAN),
        ));
        let mut last = Vec::new();
        for block in x.chunks(1024) {
            last = drive_block(&mut f, &[block]).remove(0);
            assert!(last.iter().all(|s| s.is_finite()), "NaN Q");
        }
        let level = rms(&last);
        assert!((level - 0.5).abs() < 0.05, "RMS {level}");
        assert!(f.param_set().set(UnitParam::Q, 2.0));
        for block in x.chunks(1024) {
            last = drive_block(&mut f, &[block]).remove(0);
            assert!(last.iter().all(|s| s.is_finite()), "after a Q write");
        }
        let raised = rms(&last);
        assert!(raised > 0.9, "Q 2 takes effect: {raised}");
        Node::reset(&mut f);
        let out = drive_block(&mut f, &[&x[..1024]]);
        assert!(out[0].iter().all(|s| s.is_finite()), "after reset");
    }

    /// A NaN in a raw cell never reaches the filter state — not even in the
    /// block where another control moves and forces a re-solve, which is the
    /// case a threshold comparison alone lets through. The filter keeps
    /// running at the last good cutoff, and picks up the next finite write.
    #[test]
    fn a_nan_in_a_raw_cell_never_reaches_the_state() {
        use crate::test_support::noise;
        let x = noise(31, 64);
        let mut f = prepared(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz::new(800.0),
            Q::new(0.7),
        ));
        drive_block(&mut f, &[&x]);
        f.param_set()
            .cell(UnitParam::Cutoff)
            .unwrap()
            .store(f32::NAN, std::sync::atomic::Ordering::Release);
        let held = drive_block(&mut f, &[&x]);
        assert!(held[0].iter().all(|s| s.is_finite()), "a NaN alone");
        assert!(f.param_set().set(UnitParam::Q, 3.0));
        let moved = drive_block(&mut f, &[&x]);
        assert!(
            moved[0].iter().all(|s| s.is_finite()),
            "a NaN with another control moving in the same block"
        );
        assert_eq!(
            f.applied().unwrap().freq,
            Hz(800.0),
            "held at the last good cutoff"
        );
        assert!(f.param_set().set(UnitParam::Cutoff, 2_000.0));
        drive_block(&mut f, &[&x]);
        assert_eq!(
            f.applied().unwrap().freq,
            Hz(2_000.0),
            "and back on a finite write"
        );
    }
}

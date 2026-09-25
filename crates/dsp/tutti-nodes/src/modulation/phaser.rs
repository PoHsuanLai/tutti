//! Phaser — a swept all-pass chain, of any width.
//!
//! The one modulation effect here with no delay line: it notches by phase
//! cancellation, which is why its notches are fewer and unevenly spaced
//! compared with a flanger's.

use tutti_core::{Arc, AtomicF32, MAX_BUFFER_SIZE};
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};

use super::shared::{LfoDrive, LinearModMix};
use crate::ramp::{self, Ramp};
use tutti_core::{ChannelLayout, Depth, Feedback, Hz, Mix, Phase, PhaseIncrement, SampleRate};

const MAX_STAGES: usize = 12;

/// Inclusive frequency range that the phaser LFO sweeps across.
#[derive(Debug, Clone, Copy)]
pub struct FrequencyRange {
    /// Bottom of the sweep in [`Hz`] — where the all-pass centre sits at the
    /// LFO's trough.
    pub min_hz: Hz,
    /// Top of the sweep in [`Hz`] — where the all-pass centre sits at the LFO's
    /// peak.
    ///
    /// Bounded at 0.90 of Nyquist when applied to a node: several all-pass
    /// stages compound their phase error near the limit.
    pub max_hz: Hz,
}

impl FrequencyRange {
    /// Pairs a sweep floor and ceiling in [`Hz`].
    ///
    /// Neither bound is validated here — the clamping happens where the range
    /// meets a node's sample rate, in
    /// [`PhaserNode::set_frequency_range`].
    pub fn new(min_hz: impl Into<Hz>, max_hz: impl Into<Hz>) -> Self {
        Self {
            min_hz: min_hz.into(),
            max_hz: max_hz.into(),
        }
    }
}

/// The block's depth / feedback / mix — where the next block's ramps start.
#[derive(Clone, Copy)]
struct PhaserControls {
    depth: f32,
    fb: f32,
    mix: f32,
}

/// One first-order all-pass coefficient for a sweep position.
///
/// `tan` of the normalised centre, folded into the all-pass form. This is the
/// per-sample `tan` the old node paid (twice, per channel); it is now solved
/// only at the control points — every 16 samples and a block's last sample —
/// and interpolated between.
#[inline]
fn allpass_coeff(sweep_hz: f32, sr: f32) -> f32 {
    let w = core::f32::consts::PI * sweep_hz / sr;
    (w.tan() - 1.0) / (w.tan() + 1.0)
}

/// Phaser of any width: `N` audio inputs, `N` outputs.
///
/// A chain of all-pass stages whose centre frequency an internal LFO sweeps
/// across a configured frequency range. Each stage passes every frequency at
/// unity and shifts only phase; blending that against the dry signal turns the
/// shift into cancellation notches, and sweeping the centre moves them.
///
/// **This is not a delay effect.** Unlike chorus and flanger it holds no delay
/// line, so its notches are unevenly spaced and fewer — the reason a phaser
/// sounds hollower and less metallic than a flanger. Stage count sets how many
/// notches there are; feedback deepens them.
///
/// This used to be a mono `PhaserNode` and a `StereoPhaserNode` built from two
/// of them, each running its own LFO and solving its own coefficient (with two
/// `tan`s) every sample. Now one LFO serves every channel, the coefficient is
/// solved at control points and interpolated, and the all-pass state is
/// stored stage-major across channels, so a stage runs over every channel in
/// one inner loop.
///
/// Every channel sweeps in step by default — the old stereo phaser's
/// behaviour, which widens nothing by itself.
/// [`with_phase_offsets`](Self::with_phase_offsets) staggers them.
///
/// Rate, [`Depth`], [`Feedback`] and [`Mix`] are live params read **once per
/// block**; depth, feedback and mix ramp across the block when they moved.
/// `tick` is a block of one.
pub struct PhaserNode {
    /// All-pass stages per channel, `2..=12`.
    stages: usize,
    /// Audio width.
    width: usize,
    /// Stage input history, stage-major: `x1[s * width + c]`.
    x1: Vec<f32>,
    /// Stage output history, stage-major: `y1[s * width + c]`.
    y1: Vec<f32>,
    /// Each channel's last chain output, fed back into its input.
    feedback_sample: Vec<f32>,
    /// Per-channel LFO phase offset.
    phase_offsets: Vec<PhaseIncrement>,
    /// The coefficient each channel's last rendered sample ran at — where the
    /// next block's interpolation starts.
    last_coeff: Vec<f32>,
    /// Per-sample, per-channel coefficients for the block, sample-major
    /// (`coeffs[i * width + c]`). Scratch sized at construction for
    /// [`MAX_BUFFER_SIZE`] samples.
    coeffs: Vec<f32>,
    /// The running signal through the chain, one lane per channel. Scratch.
    lane: Vec<f32>,
    lfo: LfoDrive,
    mix: LinearModMix,
    sample_rate: SampleRate,
    /// The sweep range in effect: `authored_max_hz` clamped to this rate's
    /// ceiling.
    range: FrequencyRange,
    /// The sweep top as it was asked for, before the rate's ceiling clamped
    /// it. Kept so a rate that rises again restores it — clamping `range`
    /// in place would ratchet the top down for good after one low rate.
    authored_max_hz: Hz,
    /// `None` until the first block (and after `reset`).
    last: Option<PhaserControls>,
}

impl PhaserNode {
    /// A mono phaser (1 in, 1 out) with `stages` all-pass sections, clamped to
    /// `2..=12`.
    ///
    /// Stages come in pairs — each pair produces one notch — so 4 gives the
    /// classic two-notch phaser and higher counts thicken the effect. Defaults:
    /// 0.3 Hz rate, half depth, 0.5 feedback, 50/50 [`Mix`], sweeping
    /// 200–4000 Hz.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**; call
    /// [`AudioUnit::set_sample_rate`] before the first `process`. Two things
    /// skew together if it is missed at 48 kHz: the all-pass corner frequencies
    /// (so the notches sit 8.8% high) and the LFO's per-sample phase increment
    /// (so a 0.3 Hz sweep actually runs at 0.276 Hz). Both stay musical-sounding,
    /// which is why nothing catches it. See the crate-level "born at a
    /// placeholder rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new(stages: usize) -> Self {
        Self::with_channels(ChannelLayout::MONO, stages)
    }

    /// A phaser `channels` wide (clamped to at least 1) with `stages` all-pass
    /// sections per channel. Every control is shared; each channel keeps its
    /// own all-pass state, so the channels phase identically without bleeding
    /// into each other. Otherwise as [`new`](Self::new).
    pub fn with_channels(channels: impl Into<ChannelLayout>, stages: usize) -> Self {
        let width = usize::from(channels.into().count()).max(1);
        let stages = stages.clamp(2, MAX_STAGES);
        Self {
            stages,
            width,
            x1: vec![0.0; stages * width],
            y1: vec![0.0; stages * width],
            feedback_sample: vec![0.0; width],
            phase_offsets: vec![PhaseIncrement(0.0); width],
            last_coeff: vec![0.0; width],
            coeffs: vec![0.0; MAX_BUFFER_SIZE * width],
            lane: vec![0.0; width],
            lfo: LfoDrive::new(0.3),
            mix: LinearModMix::new(0.5, 0.5, 0.5),
            sample_rate: SampleRate::DEFAULT,
            range: FrequencyRange::new(200.0, 4000.0),
            authored_max_hz: Hz(4000.0),
            last: None,
        }
    }

    /// Staggers the channels' sweeps: channel `c` runs `offsets[c]` cycles
    /// ahead of the shared LFO (wrapped). All-zero is the default; `[0.0,
    /// 0.25]` on a stereo phaser sweeps the sides a quarter-cycle apart, which
    /// is what widens it.
    ///
    /// # Panics
    ///
    /// If `offsets.len()` is not the node's width — a build-time shape error.
    pub fn with_phase_offsets(mut self, offsets: &[PhaseIncrement]) -> Self {
        assert_eq!(offsets.len(), self.width, "one phase offset per channel");
        for (o, &new) in self.phase_offsets.iter_mut().zip(offsets) {
            *o = PhaseIncrement(Phase::START.advance(new).get());
        }
        self
    }

    /// The shared LFO rate cell in [`Hz`] — how fast the notches sweep.
    ///
    /// Phaser rates are slow, typically 0.1–2 Hz. Shared across clones.
    pub fn rate(&self) -> Arc<AtomicF32> {
        self.lfo.rate.as_atomic()
    }

    /// The shared [`Depth`] cell — how much of the configured frequency range
    /// the sweep actually covers.
    ///
    /// **Unitless, `0.0..=1.0`**, unlike chorus and flanger whose depth is in
    /// seconds of delay. `1.0` sweeps the full configured range; `0.0` parks the
    /// notches at the range floor.
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.mix.depth.as_atomic()
    }

    /// The shared [`Feedback`] cell — how much of the all-pass output
    /// recirculates.
    ///
    /// Deepens and sharpens the notches. Writing the raw cell bypasses
    /// [`set_feedback`](Self::set_feedback)'s stability clamp.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.mix.feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell: `0.0` dry, `1.0` fully wet.
    ///
    /// A phaser needs both halves — the notches come from the phase-shifted and
    /// dry signals cancelling, so 50/50 is deepest and fully wet is nearly
    /// inaudible, since an all-pass chain alone barely changes the magnitude.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.mix.as_atomic()
    }

    /// Sets the LFO rate in [`Hz`], floored at 0.01 Hz.
    pub fn set_rate(&self, hz: impl Into<Hz>) {
        self.lfo.rate.store(Hz(hz.into().get().max(0.01)));
    }

    /// Sets the sweep [`Depth`], clamped to the unit range.
    pub fn set_depth(&self, d: impl Into<Depth>) {
        self.mix.depth.store(Depth::new_clamped(d.into().get()));
    }

    /// Sets the [`Feedback`], clamped to the stable range.
    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.mix
            .feedback
            .store(Feedback::new_clamped(fb.into().get()));
    }

    /// Sets the wet/dry [`Mix`], clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.mix.mix.store(Mix::new_clamped(mix.into().get()));
    }

    /// Sets the band the notches sweep across, in [`Hz`].
    ///
    /// `min_hz` is floored at 20 Hz and `max_hz` capped at 0.90 of Nyquist —
    /// a wider margin than the filters take, because several all-pass stages
    /// compound their phase error near the limit. A narrow range gives a
    /// focused sweep; a wide one sounds more dramatic.
    ///
    /// `&mut self`, so it cannot reach a node already live in the graph.
    pub fn set_frequency_range(&mut self, min_hz: impl Into<Hz>, max_hz: impl Into<Hz>) {
        self.range.min_hz = Hz(min_hz.into().get().max(20.0));
        self.authored_max_hz = max_hz.into();
        self.range.max_hz = self.authored_max_hz.min(self.range_ceiling());
    }

    /// The highest all-pass centre this rate allows.
    ///
    /// 0.90 of Nyquist — a wider margin than the SVF's 0.998, because several
    /// all-pass stages compound their phase error near the limit.
    #[inline]
    fn range_ceiling(&self) -> Hz {
        self.sample_rate.nyquist_scaled(0.90)
    }

    /// The one render kernel behind `tick` and `process`. `size` is at most
    /// [`MAX_BUFFER_SIZE`], which `AudioUnit::process` guarantees.
    fn render(
        &mut self,
        size: usize,
        x: impl Fn(usize, usize) -> f32,
        mut y: impl FnMut(usize, usize, f32),
    ) {
        debug_assert!(size <= MAX_BUFFER_SIZE);
        let w = self.width;
        // Every control is read here, once, for the whole block.
        let (depth, fb, mix) = self.mix.load();
        let target = PhaserControls {
            depth,
            fb,
            mix: mix.get(),
        };
        let primed = self.last.is_some();
        let from = self.last.unwrap_or(target);
        let depth_r = Ramp::new(from.depth, target.depth, size);
        let fb_r = Ramp::new(from.fb, target.fb, size);
        let mix_r = Ramp::new(from.mix, target.mix, size);

        let mut phases = [Phase::START; MAX_BUFFER_SIZE];
        self.lfo.fill_block(self.sample_rate, &mut phases[..size]);

        // Narrowed once for the all-pass coefficients below.
        let sr = self.sample_rate.get() as f32;
        let (min_hz, max_hz) = (self.range.min_hz.get(), self.range.max_hz.get());
        let coeff_at = |i: usize, offset: PhaseIncrement| -> f32 {
            let lfo_raw = phases[i].offset_by(offset).to_radians().get().sin();
            let lfo = lfo_raw * 0.5 + 0.5;
            let sweep = min_hz + (max_hz - min_hz) * lfo * depth_r.at(i);
            allpass_coeff(sweep, sr)
        };

        // The coefficient table, solved at the control points and interpolated
        // between. A channel whose offset equals the previous one's copies it.
        for c in 0..w {
            let offset = self.phase_offsets[c];
            if c > 0 && offset == self.phase_offsets[c - 1] {
                for i in 0..size {
                    self.coeffs[i * w + c] = self.coeffs[i * w + c - 1];
                }
                self.last_coeff[c] = self.last_coeff[c - 1];
                continue;
            }
            let mut prev = if primed {
                self.last_coeff[c]
            } else {
                coeff_at(0, offset)
            };
            for (start, end) in ramp::segments(size, !primed) {
                let next = coeff_at(end - 1, offset);
                let seg = Ramp::new(prev, next, end - start);
                for i in start..end {
                    self.coeffs[i * w + c] = seg.at(i - start);
                }
                prev = next;
            }
            self.last_coeff[c] = prev;
        }

        // Sample-outer; each stage runs across every channel in one inner loop.
        for i in 0..size {
            let fb = fb_r.at(i);
            let mix = Mix(mix_r.at(i));
            let coeffs = &self.coeffs[i * w..(i + 1) * w];
            for c in 0..w {
                self.lane[c] = x(c, i) + self.feedback_sample[c] * fb;
            }
            for s in 0..self.stages {
                let x1 = &mut self.x1[s * w..(s + 1) * w];
                let y1 = &mut self.y1[s * w..(s + 1) * w];
                for c in 0..w {
                    let input = self.lane[c];
                    let out = coeffs[c] * (input - y1[c]) + x1[c];
                    x1[c] = input;
                    y1[c] = out;
                    self.lane[c] = out;
                }
            }
            for c in 0..w {
                self.feedback_sample[c] = self.lane[c];
                y(c, i, mix.blend(x(c, i), self.lane[c]));
            }
        }
        self.last = Some(target);
    }
}

impl AudioUnit for PhaserNode {
    fn inputs(&self) -> usize {
        self.width
    }
    fn outputs(&self) -> usize {
        self.width
    }

    /// Detach every control cell this node reads (see `Param::detach`), so
    /// a fork renders the controls as they were when it was taken, not the
    /// live knob moves made while it runs. Values are kept.
    fn isolate(&mut self) {
        self.lfo.detach();
        self.mix.detach();
    }

    fn reset(&mut self) {
        self.x1.fill(0.0);
        self.y1.fill(0.0);
        self.feedback_sample.fill(0.0);
        self.lfo.reset_phase();
        self.last = None;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        // Re-clamped from what was asked for, not from the last clamp, so a
        // rate that rises again restores the authored top.
        self.range.max_hz = self.authored_max_hz.min(self.range_ceiling());
        // The interpolation start was solved at the old rate.
        self.last = None;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.render(1, |c, _| input[c], |c, _, v| output[c] = v);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if size == 0 {
            return;
        }
        self.render(
            size,
            |c, i| input.at_f32(c, i),
            |c, i, v| output.set_f32(c, i, v),
        );
    }

    fn set(&mut self, setting: tutti_core::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Rate => self.set_rate(value),
                tutti_core::UnitParam::Depth => self.set_depth(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        if self.width == 1 {
            crate::node_id::PHASER_ID
        } else {
            crate::node_id::PHASER_ID ^ 0xDA02
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(self.width);
        for c in 0..self.width {
            out.set(c, input.at(c));
        }
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + (self.x1.len() + self.y1.len() + self.coeffs.len()) * core::mem::size_of::<f32>()
    }
}

impl Clone for PhaserNode {
    fn clone(&self) -> Self {
        Self {
            stages: self.stages,
            width: self.width,
            x1: self.x1.clone(),
            y1: self.y1.clone(),
            feedback_sample: self.feedback_sample.clone(),
            phase_offsets: self.phase_offsets.clone(),
            last_coeff: self.last_coeff.clone(),
            coeffs: self.coeffs.clone(),
            lane: self.lane.clone(),
            lfo: self.lfo.clone(),
            mix: self.mix.clone(),
            sample_rate: self.sample_rate,
            range: self.range,
            authored_max_hz: self.authored_max_hz,
            last: self.last,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phaser_passthrough_dry() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));
        phaser.set_mix(0.0);

        let mut out = [0.0f32];
        phaser.tick(&[0.5], &mut out);
        assert!((out[0] - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_phaser_stages_affect_sound() {
        let sr = 44100.0;
        let mut phaser_4 = PhaserNode::new(4);
        phaser_4.set_sample_rate(tutti_core::SampleRate(sr));
        phaser_4.set_mix(1.0);

        let mut phaser_12 = PhaserNode::new(12);
        phaser_12.set_sample_rate(tutti_core::SampleRate(sr));
        phaser_12.set_mix(1.0);

        let mut sum_4 = 0.0f64;
        let mut sum_12 = 0.0f64;
        let mut out = [0.0f32];

        for i in 0..4410 {
            let input = (core::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin();
            phaser_4.tick(&[input], &mut out);
            sum_4 += out[0] as f64;
            phaser_12.tick(&[input], &mut out);
            sum_12 += out[0] as f64;
        }

        assert!(
            (sum_4 - sum_12).abs() > 0.01,
            "Different stage counts should sound different"
        );
    }

    #[test]
    fn test_phaser_reset() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32];
        for _ in 0..100 {
            phaser.tick(&[1.0], &mut out);
        }
        phaser.reset();
        phaser.tick(&[0.0], &mut out);
        assert!(
            out[0].abs() < 0.01,
            "After reset, output should be near zero"
        );
    }

    #[test]
    fn test_phaser_feedback_resonance() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));
        phaser.set_feedback(0.9);
        phaser.set_mix(1.0);

        let mut out = [0.0f32];
        phaser.tick(&[1.0], &mut out);

        let mut max_output = 0.0f32;
        for _ in 0..500 {
            phaser.tick(&[0.0], &mut out);
            max_output = max_output.max(out[0].abs());
        }
        assert!(
            max_output > 0.001,
            "High feedback should produce resonance: {max_output}"
        );
    }

    #[test]
    fn test_phaser_clamp_stages() {
        let phaser = PhaserNode::new(1);
        assert_eq!(phaser.stages, 2);

        let phaser = PhaserNode::new(20);
        assert_eq!(phaser.stages, MAX_STAGES);
    }

    // ── Per-block reads and width ────────────────────────────────────────────

    fn phaser_48k(channels: usize) -> PhaserNode {
        let mut n = PhaserNode::with_channels(channels, 6);
        n.set_sample_rate(tutti_core::SampleRate(48_000.0));
        n
    }

    /// A mix change made between blocks fades across the next block.
    ///
    /// Mutation: `Ramp::new(target.mix, target.mix, size)` (a jump) fails.
    #[test]
    fn a_mix_change_fades_across_the_next_block() {
        use crate::test_support::{change_between_blocks, noise};
        let x = noise(13, 128);
        let run = change_between_blocks(
            || phaser_48k(2),
            |n| n.set_mix(1.0),
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("phaser mix");
    }

    /// Six channels with the default (zero) offsets: each is the mono phaser on
    /// its own input, bit for bit — one LFO and one coefficient table serve all
    /// six, and the stage-major state keeps them apart.
    ///
    /// Mutation: reading lane 0's input history (`x1[0]`) for every channel
    /// fails.
    #[test]
    fn six_channels_are_six_mono_phasers() {
        use crate::test_support::{noise, process_block};
        let inputs: Vec<Vec<f32>> = (0..6).map(|c| noise(c + 30, 64)).collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|v| &v[..]).collect();
        let mut wide = phaser_48k(6);
        let mut out = Vec::new();
        for _ in 0..4 {
            out = process_block(&mut wide, &refs);
        }
        for (c, input) in inputs.iter().enumerate() {
            let mut mono = phaser_48k(1);
            let mut want = Vec::new();
            for _ in 0..4 {
                want = process_block(&mut mono, &[&input[..]]);
            }
            assert_eq!(want[0], out[c], "channel {c}");
        }
    }

    /// Per-channel phase offsets stagger the sweep: a quarter-cycle offset
    /// changes the channel, a full-cycle one is no offset at all.
    ///
    /// Mutation: ignoring the offset in `coeff_at` fails the first assertion.
    #[test]
    fn phase_offsets_stagger_the_channels() {
        use crate::test_support::{noise, process_block};
        let x = noise(14, 64);
        let mut node = PhaserNode::with_channels(3usize, 4).with_phase_offsets(&[
            PhaseIncrement(0.0),
            PhaseIncrement(0.25),
            PhaseIncrement(1.0),
        ]);
        node.set_sample_rate(tutti_core::SampleRate(48_000.0));
        node.set_rate(5.0);
        let mut out = Vec::new();
        for _ in 0..8 {
            out = process_block(&mut node, &[&x, &x, &x]);
        }
        assert_ne!(out[0], out[1], "a quarter-cycle offset sweeps elsewhere");
        assert_eq!(out[0], out[2], "a full-cycle offset wraps to none");
    }

    /// A low rate clamps the sweep top to its ceiling; a higher rate after it
    /// restores what was asked for.
    ///
    /// Mutation: re-clamping from `self.range.max_hz` (the old in-place clamp)
    /// leaves the top at 3600 Hz after the rate rises and fails.
    #[test]
    fn a_rising_rate_restores_the_authored_sweep_top() {
        let mut phaser = PhaserNode::new(4);
        phaser.set_sample_rate(tutti_core::SampleRate(8_000.0));
        assert_eq!(phaser.range.max_hz, Hz(3_600.0), "0.90 of a 4 kHz Nyquist");
        phaser.set_sample_rate(tutti_core::SampleRate(48_000.0));
        assert_eq!(phaser.range.max_hz, Hz(4_000.0));
        phaser.set_frequency_range(300.0, 30_000.0);
        assert_eq!(phaser.range.max_hz, Hz(21_600.0));
        phaser.set_sample_rate(tutti_core::SampleRate(96_000.0));
        assert_eq!(phaser.range.max_hz, Hz(30_000.0));
    }
}

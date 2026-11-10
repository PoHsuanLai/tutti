// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the reverb is
// rustysynth's reverb.rs (a Freeverb), moved here as a pure algorithm over slices.

//! A Freeverb: eight parallel damped comb filters then four all-pass filters
//! per channel, fed by one mono input and giving a stereo wet signal.
//!
//! [`Freeverb`] starts with the figures of the General MIDI reverb the SoundFont
//! unit used to carry inside itself (room size 0.5, damping 0.5, width 1, wet 1).
//! It is also the [`Realization`] of [`FreeverbDesign`](crate::FreeverbDesign): the
//! two coefficients a network carries (the comb feedback and damping) are the
//! only figures [`retarget`](Realization::retarget) moves, and the lines and the
//! processing loops are untouched by it. It allocates in [`new`](Freeverb::new)
//! and in [`prepare`](Realization::prepare) at another rate, and nowhere after.

use std::cmp;

use tutti_types::{Db, SampleRate, Seconds};

use crate::network::{Absorption, Bounds, Effort, Network, Realization, Setup, Stage};

/// A Freeverb at one sample rate: mono in, stereo out, with the comb feedback and
/// damping the only figures that move ([`Realization::retarget`]).
///
/// Its comb and all-pass lengths are Freeverb's, tuned at 44.1 kHz and scaled
/// to the rate. The input is scaled by a fixed gain inside
/// [`process`](Self::process), so a caller feeds it at the level it wants wet.
#[derive(Debug, Clone)]
pub struct Freeverb {
    cfs_l: Vec<CombFilter>,
    cfs_r: Vec<CombFilter>,
    apfs_l: Vec<AllPassFilter>,
    apfs_r: Vec<AllPassFilter>,

    /// The rate the lines were tuned for, in whole hertz.
    rate: SampleRate,
}

impl Freeverb {
    /// The gain the input is scaled by.
    pub const FIXED_GAIN: f32 = 0.015;
    /// The comb feedback: Freeverb's room size 0.5 scaled (`0.5 * 0.28 + 0.7`).
    /// A bare `f32` because the comb loop multiplies it per sample and this
    /// change leaves that code alone; the design carries it as an `Amplitude`
    /// (`Feedback` would also hold it, but caps at 0.99 and is for a control).
    pub(crate) const ROOM_FEEDBACK: f32 = 0.5 * 0.28 + 0.7;
    /// The comb damping: Freeverb's damp 0.5 scaled (`0.5 * 0.4`).
    pub(crate) const DAMPING: f32 = 0.5 * 0.4;
    /// The all-pass feedback.
    pub(crate) const ALLPASS_FEEDBACK: f32 = 0.5;
    pub(crate) const STEREO_SPREAD: usize = 23;

    pub(crate) const CF_TUNING_L1: usize = 1116;
    pub(crate) const CF_TUNING_R1: usize = 1116 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L2: usize = 1188;
    pub(crate) const CF_TUNING_R2: usize = 1188 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L3: usize = 1277;
    pub(crate) const CF_TUNING_R3: usize = 1277 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L4: usize = 1356;
    pub(crate) const CF_TUNING_R4: usize = 1356 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L5: usize = 1422;
    pub(crate) const CF_TUNING_R5: usize = 1422 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L6: usize = 1491;
    pub(crate) const CF_TUNING_R6: usize = 1491 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L7: usize = 1557;
    pub(crate) const CF_TUNING_R7: usize = 1557 + Freeverb::STEREO_SPREAD;
    pub(crate) const CF_TUNING_L8: usize = 1617;
    pub(crate) const CF_TUNING_R8: usize = 1617 + Freeverb::STEREO_SPREAD;
    pub(crate) const APF_TUNING_L1: usize = 556;
    pub(crate) const APF_TUNING_R1: usize = 556 + Freeverb::STEREO_SPREAD;
    pub(crate) const APF_TUNING_L2: usize = 441;
    pub(crate) const APF_TUNING_R2: usize = 441 + Freeverb::STEREO_SPREAD;
    pub(crate) const APF_TUNING_L3: usize = 341;
    pub(crate) const APF_TUNING_R3: usize = 341 + Freeverb::STEREO_SPREAD;
    pub(crate) const APF_TUNING_L4: usize = 225;
    pub(crate) const APF_TUNING_R4: usize = 225 + Freeverb::STEREO_SPREAD;

    /// A reverb at `rate`, rounded to whole hertz. Allocates the filter lines.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let sample_rate = SampleRate(rate.get().round());
        let mut cfs_l: Vec<CombFilter> = vec![
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L1)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L2)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L3)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L4)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L5)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L6)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L7)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L8)),
        ];

        let mut cfs_r: Vec<CombFilter> = vec![
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R1)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R2)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R3)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R4)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R5)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R6)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R7)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_R8)),
        ];

        let mut apfs_l: Vec<AllPassFilter> = vec![
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_L1)),
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_L2)),
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_L3)),
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_L4)),
        ];

        let mut apfs_r: Vec<AllPassFilter> = vec![
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_R1)),
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_R2)),
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_R3)),
            AllPassFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::APF_TUNING_R4)),
        ];

        for apf in apfs_l.iter_mut().chain(&mut apfs_r) {
            apf.feedback = Self::ALLPASS_FEEDBACK;
        }
        for cf in cfs_l.iter_mut().chain(&mut cfs_r) {
            cf.feedback = Self::ROOM_FEEDBACK;
            cf.damp1 = Self::DAMPING;
            cf.damp2 = 1_f32 - Self::DAMPING;
        }

        Freeverb {
            cfs_l,
            cfs_r,
            apfs_l,
            apfs_r,
            rate: sample_rate,
        }
    }

    /// Empties every filter line: the next block starts from silence.
    pub fn mute(&mut self) {
        for cf in self.cfs_l.iter_mut() {
            cf.mute();
        }

        for cf in self.cfs_r.iter_mut() {
            cf.mute();
        }

        for apf in self.apfs_l.iter_mut() {
            apf.mute();
        }

        for apf in self.apfs_r.iter_mut() {
            apf.mute();
        }
    }

    pub(crate) fn scale_tuning(rate: SampleRate, tuning: usize) -> usize {
        (rate.get() / 44100_f64 * (tuning as f64)).round() as usize
    }

    /// Reverberates `input` (scaled by [`FIXED_GAIN`](Self::FIXED_GAIN)) into
    /// the two outputs, which are overwritten. The three slices are the same
    /// length.
    pub fn process(&mut self, input: &[f32], output_left: &mut [f32], output_right: &mut [f32]) {
        debug_assert_eq!(input.len(), output_left.len());
        debug_assert_eq!(input.len(), output_right.len());
        let output_left_length = output_left.len();
        let output_right_length = output_right.len();

        for lsample in output_left.iter_mut().take(output_left_length) {
            *lsample = 0_f32;
        }
        for rsample in output_right.iter_mut().take(output_right_length) {
            *rsample = 0_f32;
        }

        for cf in self.cfs_l.iter_mut() {
            cf.process(input, Self::FIXED_GAIN, output_left);
        }

        for apf in self.apfs_l.iter_mut() {
            apf.process(output_left);
        }

        for cf in self.cfs_r.iter_mut() {
            cf.process(input, Self::FIXED_GAIN, output_right);
        }

        for apf in self.apfs_r.iter_mut() {
            apf.process(output_right);
        }
    }

    /// Makes the last right comb `len` frames long (empty), for the realization
    /// contract's defective realization.
    #[cfg(test)]
    pub(crate) fn lengthen_last_comb(&mut self, len: usize) {
        if let Some(c) = self.cfs_r.last_mut() {
            c.buffer = vec![0_f32; len];
            c.buffer_index = 0;
        }
    }

    /// The comb and all-pass lengths and the first comb's coefficients, for the
    /// design's tests: `(left combs, right combs, left all-passes, right
    /// all-passes, (feedback, damp1, damp2))`.
    #[cfg(test)]
    pub(crate) fn figures(&self) -> Figures {
        let len = |v: &[CombFilter]| v.iter().map(|c| c.buffer.len()).collect();
        let alen = |v: &[AllPassFilter]| v.iter().map(|a| a.buffer.len()).collect();
        let c = &self.cfs_l[0];
        Figures {
            combs_left: len(&self.cfs_l),
            combs_right: len(&self.cfs_r),
            all_passes_left: alen(&self.apfs_l),
            all_passes_right: alen(&self.apfs_r),
            comb: (c.feedback, c.damp1, c.damp2),
            all_pass_feedback: self.apfs_l[0].feedback,
        }
    }

    /// How long the wet signal takes to fall to `floor` of what it carried
    /// when the input stops. Each trip round the longest comb loop keeps the
    /// room size's fraction of what it carried (at the lowest frequency the
    /// damping leaves it, the figure of the loop's DC gain), and the all-pass
    /// chain adds its delay once. A bound, not a measure.
    #[must_use]
    pub fn ring_out(&self, floor: Db) -> Seconds {
        let longest = self
            .cfs_l
            .iter()
            .chain(&self.cfs_r)
            .map(|c| c.buffer.len())
            .max()
            .unwrap_or(0);
        let chain: usize = self.apfs_l.iter().map(|a| a.buffer.len()).sum();
        // The first echo comes after one trip round the loop at full strength, so
        // the n-th has the room's fraction to the power n - 1: falling to `floor`
        // takes one trip more than the power does.
        let loops = (floor.to_amplitude().get().ln() / Self::ROOM_FEEDBACK.ln()).ceil() + 1_f32;
        Seconds((f64::from(loops) * longest as f64 + chain as f64) / self.rate.get())
    }
}

impl Freeverb {
    /// Sets every comb's loop gain and damping. The damping's complement is
    /// computed here as `1 - damp`, the operation the constructor uses for the
    /// General MIDI figure, so the same figures give the same coefficients to the
    /// bit.
    fn set_combs(&mut self, feedback: f32, damp: f32) {
        for cf in self.cfs_l.iter_mut().chain(&mut self.cfs_r) {
            cf.feedback = feedback;
            cf.damp1 = damp;
            cf.damp2 = 1_f32 - damp;
        }
    }
}

/// The realization of [`FreeverbDesign`](crate::FreeverbDesign): `process` is the
/// comb and all-pass loops of [`Freeverb::process`], untouched.
///
/// [`retarget`](Realization::retarget) copies the comb loop gain and the damping off
/// the network's first recirculating stage (a one-pole absorption on its first
/// line) into every comb, whole and at once: a step in the loop gain is a step
/// in what the next trip carries, not in the output, which is read from the
/// past, so it needs no ramp to be free of a click (the realization contract holds
/// it to that). A network without a one-pole absorption, or with figures that
/// are not finite or not a stable loop, is ignored. The network's pre-delay,
/// width and wet gain are not read: [`FreeverbDesign`](crate::FreeverbDesign)
/// never sets them.
impl Realization for Freeverb {
    /// Re-tunes the lines when the graph runs at another rate than they were
    /// built for, to the rounded hertz [`Freeverb::new`] uses. The coefficients
    /// are kept.
    fn prepare(&mut self, _bounds: &Bounds, setup: &Setup) {
        if setup.rate.get().round() != self.rate.get() {
            let (feedback, damp) = self
                .cfs_l
                .first()
                .map_or((Self::ROOM_FEEDBACK, Self::DAMPING), |c| {
                    (c.feedback, c.damp1)
                });
            *self = Freeverb::new(setup.rate);
            self.set_combs(feedback, damp);
        }
    }

    fn retarget(&mut self, network: &Network) {
        let one_pole = network.stages.iter().find_map(|stage| match stage {
            Stage::Fdn(f) => match f.lines.first().map(|l| l.absorption) {
                Some(Absorption::OnePole { gain, damp }) => Some((gain.get(), damp)),
                _ => None,
            },
            _ => None,
        });
        if let Some((feedback, damp)) = one_pole
            && feedback.is_finite()
            && (0.0..1.0).contains(&feedback)
            && damp.is_finite()
            && damp.abs() < 1.0
        {
            self.set_combs(feedback, damp);
        }
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        if let ([mono], [left, right, ..]) = (input, output) {
            Freeverb::process(self, mono, left, right);
        }
    }

    fn reset(&mut self) {
        self.mute();
    }

    fn effort(&self) -> Effort {
        // Measured by `tutti-nodes`'s `benches/node_kernels.rs` (`reverb`): 18 ns
        // a frame and output channel, the class for 2.8 to 45 (doc 013, "The cost
        // model and coarsening").
        Effort::Moderate
    }

    fn fresh(&self) -> Self {
        let mut fresh = self.clone();
        fresh.mute();
        fresh
    }
}

/// What [`Freeverb::figures`] reads.
#[cfg(test)]
pub(crate) struct Figures {
    pub(crate) combs_left: Vec<usize>,
    pub(crate) combs_right: Vec<usize>,
    pub(crate) all_passes_left: Vec<usize>,
    pub(crate) all_passes_right: Vec<usize>,
    pub(crate) comb: (f32, f32, f32),
    pub(crate) all_pass_feedback: f32,
}

#[derive(Debug, Clone)]
struct CombFilter {
    buffer: Vec<f32>,

    buffer_index: usize,
    filter_store: f32,

    feedback: f32,
    damp1: f32,
    damp2: f32,
}

impl CombFilter {
    fn new(buffer_size: usize) -> Self {
        Self {
            buffer: vec![0_f32; buffer_size],
            buffer_index: 0,
            filter_store: 0_f32,
            feedback: 0_f32,
            damp1: 0_f32,
            damp2: 0_f32,
        }
    }

    fn mute(&mut self) {
        let buffer_length = self.buffer.len();
        for i in 0..buffer_length {
            self.buffer[i] = 0_f32;
        }

        self.filter_store = 0_f32;
    }

    fn process(&mut self, input_block: &[f32], input_gain: f32, output_block: &mut [f32]) {
        let buffer_length = self.buffer.len();
        let output_block_length = output_block.len();

        let mut block_index: usize = 0;
        while block_index < output_block_length {
            if self.buffer_index == buffer_length {
                self.buffer_index = 0;
            }

            let src_rem = buffer_length - self.buffer_index;
            let dst_rem = output_block_length - block_index;
            let rem = cmp::min(src_rem, dst_rem);

            for t in 0..rem {
                let block_pos = block_index + t;
                let buffer_pos = self.buffer_index + t;

                let input = input_block[block_pos] * input_gain;

                // The following ifs are to avoid performance problem due to denormalized number.
                // The original implementation uses unsafe cast to detect denormalized number.
                // I tried to reproduce the original implementation using Unsafe.As,
                // but the simple Math.Abs version was faster according to some benchmarks.

                let mut output = self.buffer[buffer_pos];
                if output.abs() < 1.0E-6_f32 {
                    output = 0_f32;
                }

                self.filter_store = (output * self.damp2) + (self.filter_store * self.damp1);
                if self.filter_store.abs() < 1.0E-6_f32 {
                    self.filter_store = 0_f32;
                }

                self.buffer[buffer_pos] = input + (self.filter_store * self.feedback);
                output_block[block_pos] += output;
            }

            self.buffer_index += rem;
            block_index += rem;
        }
    }
}

#[derive(Debug, Clone)]
struct AllPassFilter {
    buffer: Vec<f32>,

    buffer_index: usize,

    feedback: f32,
}

impl AllPassFilter {
    fn new(buffer_size: usize) -> Self {
        Self {
            buffer: vec![0_f32; buffer_size],
            buffer_index: 0,
            feedback: 0_f32,
        }
    }

    fn mute(&mut self) {
        let buffer_length = self.buffer.len();
        for i in 0..buffer_length {
            self.buffer[i] = 0_f32;
        }
    }

    fn process(&mut self, block: &mut [f32]) {
        let buffer_length = self.buffer.len();
        let block_length = block.len();

        let mut block_index: usize = 0;
        while block_index < block_length {
            if self.buffer_index == buffer_length {
                self.buffer_index = 0;
            }

            let src_rem = buffer_length - self.buffer_index;
            let dst_rem = block_length - block_index;
            let rem = cmp::min(src_rem, dst_rem);

            for t in 0..rem {
                let block_pos = block_index + t;
                let buffer_pos = self.buffer_index + t;

                let input = block[block_pos];

                let mut bufout = self.buffer[buffer_pos];
                if bufout.abs() < 1.0E-6_f32 {
                    bufout = 0_f32;
                }

                block[block_pos] = bufout - input;
                self.buffer[buffer_pos] = input + (bufout * self.feedback);
            }

            self.buffer_index += rem;
            block_index += rem;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Freeverb's all-pass is `H(z) = ((1 + g) z^-D - 1) / (1 - g z^-D)`, the
    /// network's `AllPassForm::Freeverb`, not the unit-magnitude lattice. Its
    /// impulse response, from the transfer function, is -1 at frame 0 and
    /// `g^(k-1)` at frame `k D` for `k >= 1`, and zero elsewhere; its gain is 1
    /// at DC for `g = 0.5` and `(2 + g) / (1 + g)` where `D w = pi`.
    ///
    /// Mutations (run): `bufout - input` -> `input - bufout` in
    /// `AllPassFilter::process` flips the sign and fails the first assertion;
    /// `input + bufout * feedback` -> `input` removes the recirculation and
    /// fails the `g^(k-1)` train.
    #[test]
    fn the_all_pass_is_the_freeverb_form_not_the_lattice() {
        let (d, g) = (8usize, 0.5f32);
        let mut ap = AllPassFilter::new(d);
        ap.feedback = g;
        let mut h = vec![0.0f32; 18 * d + 1];
        h[0] = 1.0;
        ap.process(&mut h);
        assert!((h[0] + 1.0).abs() < 1e-6, "h[0] = {}", h[0]);
        for k in 1..=17usize {
            assert!(
                (h[k * d] - g.powi(k as i32 - 1)).abs() < 1e-5,
                "h[{}] = {}",
                k * d,
                h[k * d]
            );
        }
        assert!((0..h.len()).filter(|n| n % d != 0).all(|n| h[n] == 0.0));
        let gain_at = |omega: f64| {
            let (re, im) = h
                .iter()
                .enumerate()
                .fold((0.0f64, 0.0f64), |(re, im), (n, &x)| {
                    let a = omega * n as f64;
                    (
                        re + f64::from(x) * libm::cos(a),
                        im - f64::from(x) * libm::sin(a),
                    )
                });
            re.hypot(im)
        };
        assert!((gain_at(0.0) - 1.0).abs() < 1e-3, "DC {}", gain_at(0.0));
        let peak = gain_at(std::f64::consts::PI / d as f64);
        assert!(
            (peak - (2.0 + 0.5) / (1.0 + 0.5)).abs() < 1e-3,
            "peak {peak}"
        );
    }
}

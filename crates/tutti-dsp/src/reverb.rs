// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the reverb is
// rustysynth's reverb.rs (a Freeverb), moved here as a pure algorithm over slices.

//! A Freeverb: eight parallel damped comb filters then four all-pass filters
//! per channel, fed by one mono input and giving a stereo wet signal.
//!
//! [`Freeverb`] holds the figures of the General MIDI reverb the SoundFont unit
//! used to carry inside itself (room size 0.5, damping 0.5, width 1, wet 1), and
//! no controls: every figure is a constant of the type. It allocates in
//! [`new`](Freeverb::new) and nowhere after.

use std::cmp;

use tutti_types::{Db, SampleRate, Seconds};

/// A Freeverb at one sample rate: mono in, stereo out, nothing to set.
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

    gain: f32,
    room_size: f32,
    room_size1: f32,
    damp: f32,
    damp1: f32,
    wet: f32,
    wet1: f32,
    wet2: f32,
    width: f32,
    hz: i32,
}

impl Freeverb {
    /// The gain the input is scaled by.
    pub const FIXED_GAIN: f32 = 0.015;
    const SCALE_WET: f32 = 3.0;
    const SCALE_DAMP: f32 = 0.4;
    const SCALE_ROOM: f32 = 0.28;
    const OFFSET_ROOM: f32 = 0.7;
    const INITIAL_ROOM: f32 = 0.5;
    const INITIAL_DAMP: f32 = 0.5;
    const INITIAL_WET: f32 = 1.0 / Freeverb::SCALE_WET;
    const INITIAL_WIDTH: f32 = 1.0;
    const STEREO_SPREAD: usize = 23;

    const CF_TUNING_L1: usize = 1116;
    const CF_TUNING_R1: usize = 1116 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L2: usize = 1188;
    const CF_TUNING_R2: usize = 1188 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L3: usize = 1277;
    const CF_TUNING_R3: usize = 1277 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L4: usize = 1356;
    const CF_TUNING_R4: usize = 1356 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L5: usize = 1422;
    const CF_TUNING_R5: usize = 1422 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L6: usize = 1491;
    const CF_TUNING_R6: usize = 1491 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L7: usize = 1557;
    const CF_TUNING_R7: usize = 1557 + Freeverb::STEREO_SPREAD;
    const CF_TUNING_L8: usize = 1617;
    const CF_TUNING_R8: usize = 1617 + Freeverb::STEREO_SPREAD;
    const APF_TUNING_L1: usize = 556;
    const APF_TUNING_R1: usize = 556 + Freeverb::STEREO_SPREAD;
    const APF_TUNING_L2: usize = 441;
    const APF_TUNING_R2: usize = 441 + Freeverb::STEREO_SPREAD;
    const APF_TUNING_L3: usize = 341;
    const APF_TUNING_R3: usize = 341 + Freeverb::STEREO_SPREAD;
    const APF_TUNING_L4: usize = 225;
    const APF_TUNING_R4: usize = 225 + Freeverb::STEREO_SPREAD;

    /// A reverb at `rate`, rounded to whole hertz. Allocates the filter lines.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let sample_rate = rate.get().round() as i32;
        let cfs_l: Vec<CombFilter> = vec![
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L1)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L2)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L3)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L4)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L5)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L6)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L7)),
            CombFilter::new(Freeverb::scale_tuning(sample_rate, Freeverb::CF_TUNING_L8)),
        ];

        let cfs_r: Vec<CombFilter> = vec![
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

        for apf in apfs_l.iter_mut() {
            apf.set_feedback(0.5_f32);
        }

        for apf in apfs_r.iter_mut() {
            apf.set_feedback(0.5_f32);
        }

        let mut reverb = Freeverb {
            cfs_l,
            cfs_r,
            apfs_l,
            apfs_r,
            gain: 0_f32,
            room_size: 0_f32,
            room_size1: 0_f32,
            damp: 0_f32,
            damp1: 0_f32,
            wet: 0_f32,
            wet1: 0_f32,
            wet2: 0_f32,
            width: 0_f32,
            hz: sample_rate,
        };

        reverb.set_wet(Freeverb::INITIAL_WET);
        reverb.set_room_size(Freeverb::INITIAL_ROOM);
        reverb.set_damp(Freeverb::INITIAL_DAMP);
        reverb.set_width(Freeverb::INITIAL_WIDTH);

        reverb
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

    fn scale_tuning(sample_rate: i32, tuning: usize) -> usize {
        ((sample_rate as f64) / 44100_f64 * (tuning as f64)).round() as usize
    }

    /// Reverberates `input` (scaled by [`FIXED_GAIN`](Self::FIXED_GAIN)) into
    /// the two outputs, which are overwritten. The three slices are the same
    /// length.
    pub fn process(&mut self, input: &[f32], output_left: &mut [f32], output_right: &mut [f32]) {
        debug_assert_eq!(input.len(), output_left.len());
        debug_assert_eq!(input.len(), output_right.len());
        let input_length = input.len();
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

        // With the default settings, we can skip this part.
        if 1_f32 - self.wet1 > 1.0E-3_f32 || self.wet2 > 1.0E-3_f32 {
            for t in 0..input_length {
                let left = output_left[t];
                let right = output_right[t];
                output_left[t] = left * self.wet1 + right * self.wet2;
                output_right[t] = right * self.wet1 + left * self.wet2;
            }
        }
    }

    fn update(&mut self) {
        self.wet1 = self.wet * (self.width / 2_f32 + 0.5_f32);
        self.wet2 = self.wet * ((1_f32 - self.width) / 2_f32);

        self.room_size1 = self.room_size;
        self.damp1 = self.damp;
        self.gain = Freeverb::FIXED_GAIN;

        for cf in self.cfs_l.iter_mut() {
            cf.set_feedback(self.room_size1);
            cf.set_damp(self.damp1);
        }

        for cf in self.cfs_r.iter_mut() {
            cf.set_feedback(self.room_size1);
            cf.set_damp(self.damp1);
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
        let loops = (floor.to_amplitude().get().ln() / self.room_size1.ln()).ceil();
        Seconds((loops * longest as f32 + chain as f32) / self.hz as f32)
    }

    fn set_room_size(&mut self, value: f32) {
        self.room_size = (value * Freeverb::SCALE_ROOM) + Freeverb::OFFSET_ROOM;
        self.update();
    }

    fn set_damp(&mut self, value: f32) {
        self.damp = value * Freeverb::SCALE_DAMP;
        self.update();
    }

    fn set_wet(&mut self, value: f32) {
        self.wet = value * Freeverb::SCALE_WET;
        self.update();
    }

    fn set_width(&mut self, value: f32) {
        self.width = value;
        self.update();
    }
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

    fn set_feedback(&mut self, value: f32) {
        self.feedback = value;
    }

    fn set_damp(&mut self, value: f32) {
        self.damp1 = value;
        self.damp2 = 1_f32 - value;
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

    fn set_feedback(&mut self, value: f32) {
        self.feedback = value;
    }
}

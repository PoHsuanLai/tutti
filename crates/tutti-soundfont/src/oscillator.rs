// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use crate::render::Render;
use tutti_sf2::{LoopMode, Pcm24, SamplePool};

// In this class, fixed-point numbers are used for speed-up.
// A fixed-point number is expressed by Int64, whose lower 24 bits represent the fraction part,
// and the rest represent the integer part.
// For clarity, fixed-point number variables have a suffix "_fp".

/// A sample pool read as integers of its own width, with the scale that takes a
/// point at the fixed-point fraction to a full-scale-1 `f32`.
trait Points: Copy {
    /// One over the point's full scale and the fixed-point unit.
    const FP_TO_SAMPLE: f32;

    fn at(self, index: usize) -> i64;
}

impl Points for &[i16] {
    const FP_TO_SAMPLE: f32 = 1_f32 / (32768 * Oscillator::FRAC_UNIT) as f32;

    fn at(self, index: usize) -> i64 {
        i64::from(self[index])
    }
}

impl Points for Pcm24<'_> {
    const FP_TO_SAMPLE: f32 = 1_f32 / (8_388_608 * Oscillator::FRAC_UNIT) as f32;

    fn at(self, index: usize) -> i64 {
        i64::from(self.high()[index]) << 8 | i64::from(self.low()[index])
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Oscillator {
    synthesizer_sample_rate: i32,

    loop_mode: LoopMode,
    sample_sample_rate: i32,
    start: i32,
    end: i32,
    start_loop: i32,
    end_loop: i32,
    root_key: i32,

    tune: f32,
    pitch_change_scale: f32,
    sample_rate_ratio: f32,

    looping: bool,

    position_fp: i64,
}

impl Oscillator {
    const FRAC_BITS: i32 = 24;
    const FRAC_UNIT: i64 = 1_i64 << Oscillator::FRAC_BITS;

    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            synthesizer_sample_rate: settings.hz(),
            loop_mode: LoopMode::NoLoop,
            sample_sample_rate: 0,
            start: 0,
            end: 0,
            start_loop: 0,
            end_loop: 0,
            root_key: 0,
            tune: 0_f32,
            pitch_change_scale: 0_f32,
            sample_rate_ratio: 0_f32,
            looping: false,
            position_fp: 0,
        }
    }

    pub(crate) fn start(
        &mut self,
        loop_mode: LoopMode,
        sample_rate: i32,
        start: i32,
        end: i32,
        start_loop: i32,
        end_loop: i32,
        root_key: i32,
        coarse_tune: i32,
        fine_tune: i32,
        scale_tuning: i32,
    ) {
        self.loop_mode = loop_mode;
        self.sample_sample_rate = sample_rate;
        self.start = start;
        self.end = end;
        self.start_loop = start_loop;
        self.end_loop = end_loop;
        self.root_key = root_key;

        self.tune = coarse_tune as f32 + 0.01_f32 * fine_tune as f32;
        self.pitch_change_scale = 0.01_f32 * scale_tuning as f32;
        self.sample_rate_ratio = sample_rate as f32 / self.synthesizer_sample_rate as f32;
        self.looping = self.loop_mode != LoopMode::NoLoop;
        self.position_fp = (start as i64) << Oscillator::FRAC_BITS;
    }

    pub(crate) fn release(&mut self) {
        if self.loop_mode == LoopMode::LoopUntilNoteOff {
            self.looping = false;
        }
    }

    pub(crate) fn process(&mut self, data: SamplePool<'_>, block: &mut [f32], pitch: f32) -> bool {
        let pitch_change = self.pitch_change_scale * (pitch - self.root_key as f32) + self.tune;
        let pitch_ratio = self.sample_rate_ratio * 2_f32.powf(pitch_change / 12_f32);
        // The pool's width is matched once per block, not per point.
        match data {
            SamplePool::Pcm16(points) => self.fill_block(points, block, pitch_ratio as f64),
            SamplePool::Pcm24(points) => self.fill_block(points, block, pitch_ratio as f64),
        }
    }

    fn fill_block<P: Points>(&mut self, data: P, block: &mut [f32], pitch_ratio: f64) -> bool {
        let pitch_ratio_fp = (Oscillator::FRAC_UNIT as f64 * pitch_ratio) as i64;

        if self.looping {
            self.fill_block_continuous(data, block, pitch_ratio_fp)
        } else {
            self.fill_block_no_loop(data, block, pitch_ratio_fp)
        }
    }

    fn fill_block_no_loop<P: Points>(
        &mut self,
        data: P,
        block: &mut [f32],
        pitch_ratio_fp: i64,
    ) -> bool {
        for t in 0..block.len() {
            let index = (self.position_fp >> Oscillator::FRAC_BITS) as usize;
            if index >= self.end as usize {
                if t > 0 {
                    let len = block.len();
                    block[t..len].fill(0_f32);
                    return true;
                } else {
                    return false;
                }
            }

            let x1 = data.at(index);
            let x2 = data.at(index + 1);
            let a_fp = self.position_fp & (Oscillator::FRAC_UNIT - 1);
            block[t] = P::FP_TO_SAMPLE * ((x1 << Oscillator::FRAC_BITS) + a_fp * (x2 - x1)) as f32;

            self.position_fp += pitch_ratio_fp;
        }

        true
    }

    fn fill_block_continuous<P: Points>(
        &mut self,
        data: P,
        block: &mut [f32],
        pitch_ratio_fp: i64,
    ) -> bool {
        let end_loop_fp = (self.end_loop as i64) << Oscillator::FRAC_BITS;
        let loop_length = (self.end_loop - self.start_loop) as i64;
        let loop_length_fp = loop_length << Oscillator::FRAC_BITS;

        for sample in block.iter_mut() {
            if self.position_fp >= end_loop_fp {
                self.position_fp -= loop_length_fp;
            }

            let index1 = (self.position_fp >> Oscillator::FRAC_BITS) as usize;
            let mut index2 = index1 + 1;
            if index2 >= self.end_loop as usize {
                index2 -= loop_length as usize;
            }

            let x1 = data.at(index1);
            let x2 = data.at(index2);
            let a_fp = self.position_fp & (Oscillator::FRAC_UNIT - 1);
            *sample = P::FP_TO_SAMPLE * ((x1 << Oscillator::FRAC_BITS) + a_fp * (x2 - x1)) as f32;

            self.position_fp += pitch_ratio_fp;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tutti_fixtures::sf2::{Inst, Preset, bytes, with_pool};
    use tutti_sf2::SoundFont;
    use tutti_types::SampleRate;

    /// A font of 2100 points: `smpl` words that walk the 16-bit range and
    /// `sm24` bytes that walk the byte range.
    fn font(low: bool) -> (SoundFont, Vec<i32>) {
        let high: Vec<i16> = (0..2100).map(|i| (i * 31 - 32_000) as i16).collect();
        let low_bytes: Vec<u8> = (0..2100).map(|i| (i * 7 % 256) as u8).collect();
        let smpl: Vec<u8> = high.iter().flat_map(|p| p.to_le_bytes()).collect();
        let file = bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![vec![]],
            }],
        );
        let file = with_pool(&file, (2, 4), Some(&smpl), low.then_some(&low_bytes[..]));
        let font = SoundFont::from_reader(&mut Cursor::new(file)).unwrap();
        let points = high
            .iter()
            .zip(&low_bytes)
            .map(|(&h, &l)| {
                if low {
                    i32::from(h) * 256 + i32::from(l)
                } else {
                    i32::from(h)
                }
            })
            .collect();
        (font, points)
    }

    /// The first `count` frames of a no-loop read at a pitch ratio of 1: the
    /// position steps by exactly one point from point 0, so every read sits on
    /// a point and the interpolation has no fraction.
    fn read(pool: SamplePool<'_>, count: usize) -> Vec<f32> {
        let rate = SampleRate::SR_44K1;
        let mut oscillator = Oscillator::new(&Render::new(rate));
        oscillator.start(
            LoopMode::NoLoop,
            rate.get() as i32,
            0,
            2000,
            0,
            0,
            60,
            0,
            0,
            100,
        );
        let mut block = vec![0_f32; count];
        assert!(oscillator.process(pool, &mut block, 60.0));
        block
    }

    /// A 24-bit point is `(smpl << 8) | sm24` over 2^23: on a point of the pool
    /// the read is that figure exactly (a 24-bit integer shifted by the 24
    /// fraction bits has 24 significant bits, which an `f32` holds, and 2^-47
    /// is a power of two).
    /// Spec: sf2.sdta.sm24.low-byte
    #[test]
    fn a_24_bit_point_reads_as_its_value_over_2_to_the_23() {
        let (font, points) = font(true);
        assert_eq!(font.bits_per_sample(), 24);
        let out = read(font.sample_pool(), 1500);
        for (t, &sample) in out.iter().enumerate() {
            assert_eq!(sample, points[t] as f32 / 8_388_608.0, "frame {t}");
        }
    }

    /// The 16-bit pool reads a point as its value over 2^15, as it always has.
    #[test]
    fn a_16_bit_point_reads_as_its_value_over_2_to_the_15() {
        let (font, points) = font(false);
        assert_eq!(font.bits_per_sample(), 16);
        let out = read(font.sample_pool(), 1500);
        for (t, &sample) in out.iter().enumerate() {
            assert_eq!(sample, points[t] as f32 / 32_768.0, "frame {t}");
        }
    }
}

// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use std::f32::consts;

#[non_exhaustive]
pub(crate) struct SoundFontMath {}

impl SoundFontMath {
    pub(crate) const HALF_PI: f32 = consts::PI / 2_f32;
    /// The level (-60 dB) below which a voice's envelope is dropped: tutti's
    /// threshold, not the spec's. A release time is the time to -100 dB, so a
    /// released voice is dropped at 0.6 of it. `LOG_NON_AUDIBLE` is its natural
    /// log.
    pub(crate) const NON_AUDIBLE: f32 = 1.0e-3_f32;
    /// The natural-log fall of a 100 dB amplitude change: `100 / 20 * ln 10`.
    /// The volume envelope's decay and release times are the time to fall this
    /// far (SF2 2.04 section 8.1.2, generators 36 and 38: "a 100% change", the
    /// time "until 100dB attenuation were reached"), at a constant dB per second.
    pub(crate) const HUNDRED_DB_NEPERS: f64 = 11.512_925_464_970_229;
    pub(crate) const LOG_NON_AUDIBLE: f32 = -6.907_755_4_f32;

    pub(crate) fn max(x: f32, y: f32) -> f32 {
        if x > y { x } else { y }
    }

    pub(crate) fn clamp(value: f32, min: f32, max: f32) -> f32 {
        if value < min {
            min
        } else if value > max {
            max
        } else {
            value
        }
    }

    pub(crate) fn timecents_to_seconds(x: f32) -> f32 {
        2_f32.powf((1_f32 / 1200_f32) * x)
    }

    pub(crate) fn cents_to_hertz(x: f32) -> f32 {
        8.176_f32 * 2_f32.powf((1_f32 / 1200_f32) * x)
    }

    pub(crate) fn cents_to_multiplying_factor(x: f32) -> f32 {
        2_f32.powf((1_f32 / 1200_f32) * x)
    }

    pub(crate) fn decibels_to_linear(x: f32) -> f32 {
        10_f32.powf(0.05_f32 * x)
    }

    pub(crate) fn linear_to_decibels(x: f32) -> f32 {
        20_f32 * x.log10()
    }

    pub(crate) fn key_number_to_multiplying_factor(cents: i32, key: i32) -> f32 {
        SoundFontMath::timecents_to_seconds((cents * (60 - key)) as f32)
    }

    pub(crate) fn exp_cutoff(x: f64) -> f64 {
        if x < SoundFontMath::LOG_NON_AUDIBLE as f64 {
            0_f64
        } else {
            x.exp()
        }
    }
}

/// SF2 2.04 section 9.3 defines the units generators are written in. The
/// figures below are the definitions' own, not another render's.
#[cfg(test)]
mod tests {
    use super::SoundFontMath;

    fn close(a: f32, b: f32, relative: f32) -> bool {
        ((a - b) / b).abs() < relative
    }

    /// 9.3, absolute timecents: zero is one second, and a timecent is the 1200th
    /// root of two in time, so 1200 doubles it and -1200 halves it.
    ///
    /// Mutation (run): `1200` -> `1000` in `timecents_to_seconds` fails the
    /// doubling.
    #[test]
    fn timecents_are_one_second_at_zero_and_double_every_1200() {
        assert_eq!(SoundFontMath::timecents_to_seconds(0.0), 1.0);
        assert!(close(
            SoundFontMath::timecents_to_seconds(1200.0),
            2.0,
            1e-6
        ));
        assert!(close(
            SoundFontMath::timecents_to_seconds(-1200.0),
            0.5,
            1e-6
        ));
        // 9.3's own example, in the generator table: -7973 timecents is 10 ms.
        assert!(close(
            SoundFontMath::timecents_to_seconds(-7973.0),
            0.01,
            1e-3
        ));
    }

    /// 9.3, absolute cents: 6900 is 440 Hz (A-440), and 1200 cents is an octave.
    ///
    /// Mutation (run): the 8.176 reference nudged to 8.0 fails the 440 Hz.
    #[test]
    fn absolute_cents_6900_is_440_hz_and_an_octave_is_1200() {
        assert!(close(SoundFontMath::cents_to_hertz(6900.0), 440.0, 1e-3));
        assert!(close(
            SoundFontMath::cents_to_hertz(8100.0) / SoundFontMath::cents_to_hertz(6900.0),
            2.0,
            1e-5,
        ));
    }

    /// 9.3, relative cents: a ratio of the 1200th root of two each.
    #[test]
    fn relative_cents_multiply_by_a_1200th_root_of_two() {
        assert_eq!(SoundFontMath::cents_to_multiplying_factor(0.0), 1.0);
        assert!(close(
            SoundFontMath::cents_to_multiplying_factor(1200.0),
            2.0,
            1e-6
        ));
    }

    /// 9.3, centibels: a tenth of a decibel, a ratio in amplitude of the 200th
    /// root of ten; so 200 centibels (20 dB) is a tenth in amplitude, and 60
    /// dB a thousandth. (The generators convert centibels to decibels with a
    /// factor of 0.1, tested at `RegionPair`.)
    ///
    /// Mutation (run): `0.05` -> `0.1` in `decibels_to_linear` fails both.
    #[test]
    fn decibels_are_twenty_log_amplitude() {
        assert!(close(SoundFontMath::decibels_to_linear(-20.0), 0.1, 1e-5));
        assert!(close(SoundFontMath::decibels_to_linear(-60.0), 1e-3, 1e-4));
        assert!(close(SoundFontMath::linear_to_decibels(0.1), -20.0, 1e-5));
    }

    /// 8.1.2, generators 31, 32, 39 and 40 (key number to envelope hold and
    /// decay): the time at key 60 is unchanged, and a value of 100 makes it
    /// track the keyboard (an octave up halves it). The spec's own worked
    /// example: a hold of -7973 timecents (10 ms) with a key-number-to-hold of
    /// 50, played at key 36, is 20 ms.
    ///
    /// Mutation (run): `60 - key` -> `key - 60` fails it.
    #[test]
    fn key_number_scaling_follows_the_spec_example() {
        let base = SoundFontMath::timecents_to_seconds(-7973.0);
        let factor = SoundFontMath::key_number_to_multiplying_factor(50, 36);
        assert!(close(base * factor, 0.02, 1e-3), "{}", base * factor);
    }
}

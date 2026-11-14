//! Every range-checked setting keeps the contract in `CLAUDE.md`: `new` refuses
//! and never clamps, `of` panics, `TryFrom` agrees and carries the refused raw.
//!
//! The crate has no serde, so the check runs on the `try_from` shape written
//! here for the tests alone.

use serde::{Deserialize, Serialize};
use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_types::{Cents, ChannelLayout, Db, SampleRate, Seconds};

use crate::{
    BandCount, BandsPerOctave, HarmonicDecay, Harmonics, NoteSpan, PeakDecay, PeakFloor,
    PitchSplit, PriorSpread, SelfTransition, Sharpness, SpectrumFloor, SpectrumTime, Tightness,
    TuningOffset, WindowedLayout, WindowedRate,
};

macro_rules! serde_through_try_from {
    ($($ty:ty: $raw:ty),* $(,)?) => {$(
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                self.get().serialize(s)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::try_from(<$raw>::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
    )*};
}

serde_through_try_from! {
    Harmonics: usize,
    HarmonicDecay: f32,
    PeakFloor: Db,
    TuningOffset: Cents,
    PriorSpread: f32,
    Tightness: f32,
    SpectrumTime: Seconds,
    SpectrumFloor: Db,
    PeakDecay: Db,
    BandsPerOctave: usize,
    BandCount: usize,
    NoteSpan: Seconds,
    PitchSplit: Cents,
    SelfTransition: f32,
    Sharpness: f32,
    WindowedRate: SampleRate,
    WindowedLayout: ChannelLayout,
}

const NAN: f32 = f32::NAN;
const INF: f32 = f32::INFINITY;
const NEG_INF: f32 = f32::NEG_INFINITY;

fn db(raws: &[f32]) -> Vec<Db> {
    raws.iter().map(|&r| Db(r)).collect()
}

fn cents(raws: &[f32]) -> Vec<Cents> {
    raws.iter().map(|&r| Cents(r)).collect()
}

fn seconds(raws: &[f64]) -> Vec<Seconds> {
    raws.iter().map(|&r| Seconds(r)).collect()
}

/// `1..=16` partials.
#[test]
fn harmonics_keep_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: Harmonics::new,
        get: Harmonics::get,
        of: Some(Harmonics::of),
        valid: &[1, 4, 16],
        invalid: &[0, 17, usize::MAX],
    });
}

/// `(0, 1]`.
#[test]
fn harmonic_decay_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: HarmonicDecay::new,
        get: HarmonicDecay::get,
        of: Some(HarmonicDecay::of),
        valid: &[f32::MIN_POSITIVE, 0.6, 1.0],
        invalid: &[0.0, -0.1, 1.000_000_1, NAN, INF, NEG_INF],
    });
}

/// `-120..=0` dB.
#[test]
fn peak_floor_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PeakFloor::new,
        get: PeakFloor::get,
        of: Some(PeakFloor::of),
        valid: &db(&[-120.0, -40.0, 0.0]),
        invalid: &db(&[1.0, -121.0, NAN, INF, NEG_INF]),
    });
}

/// `-50..=50` cents.
#[test]
fn tuning_offset_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: TuningOffset::new,
        get: TuningOffset::get,
        of: Some(TuningOffset::of),
        valid: &cents(&[-50.0, 0.0, 12.0, 50.0]),
        invalid: &cents(&[51.0, -51.0, NAN, INF, NEG_INF]),
    });
}

/// Above zero and finite.
#[test]
fn prior_spread_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PriorSpread::new,
        get: PriorSpread::get,
        of: Some(PriorSpread::of),
        valid: &[f32::MIN_POSITIVE, 1.0, f32::MAX],
        invalid: &[0.0, -1.0, NAN, INF, NEG_INF],
    });
}

/// Zero or more, finite.
#[test]
fn tightness_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: Tightness::new,
        get: Tightness::get,
        of: Some(Tightness::of),
        valid: &[0.0, 100.0, f32::MAX],
        invalid: &[-1.0, -f32::MIN_POSITIVE, NAN, INF, NEG_INF],
    });
}

/// `0..=3600` s.
#[test]
fn spectrum_time_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: SpectrumTime::new,
        get: SpectrumTime::get,
        of: Some(SpectrumTime::of),
        valid: &seconds(&[0.0, 1.0, 3600.0]),
        invalid: &seconds(&[-1.0, 3601.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `-300..=0` dB.
#[test]
fn spectrum_floor_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: SpectrumFloor::new,
        get: SpectrumFloor::get,
        of: Some(SpectrumFloor::of),
        valid: &db(&[-300.0, -96.0, 0.0]),
        invalid: &db(&[1.0, -301.0, NAN, INF, NEG_INF]),
    });
}

/// `0..=10000` dB per second.
#[test]
fn peak_decay_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PeakDecay::new,
        get: PeakDecay::get,
        of: Some(PeakDecay::of),
        valid: &db(&[0.0, 20.0, 10_000.0]),
        invalid: &db(&[-1.0, 10_001.0, NAN, INF, NEG_INF]),
    });
}

/// `1..=48` bands.
#[test]
fn bands_per_octave_keep_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: BandsPerOctave::new,
        get: BandsPerOctave::get,
        of: Some(BandsPerOctave::of),
        valid: &[1, 3, 48],
        invalid: &[0, 49, usize::MAX],
    });
}

/// `1..=4096` bands.
#[test]
fn band_count_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: BandCount::new,
        get: BandCount::get,
        of: Some(BandCount::of),
        valid: &[1, 24, 4096],
        invalid: &[0, 4097, usize::MAX],
    });
}

/// `0..=60` s.
#[test]
fn note_span_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: NoteSpan::new,
        get: NoteSpan::get,
        of: Some(NoteSpan::of),
        valid: &seconds(&[0.0, 0.06, 60.0]),
        invalid: &seconds(&[-1.0, 61.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `(0, 1200]` cents.
#[test]
fn pitch_split_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PitchSplit::new,
        get: PitchSplit::get,
        of: Some(PitchSplit::of),
        valid: &cents(&[f32::MIN_POSITIVE, 50.0, 1200.0]),
        invalid: &cents(&[0.0, -5.0, 1201.0, NAN, INF, NEG_INF]),
    });
}

/// `(0, 1)`.
#[test]
fn self_transition_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: SelfTransition::new,
        get: SelfTransition::get,
        of: Some(SelfTransition::of),
        valid: &[f32::MIN_POSITIVE, 0.9, 0.999_999_94],
        invalid: &[0.0, 1.0, -0.5, 1.5, NAN, INF, NEG_INF],
    });
}

/// `(0, 100]`.
#[test]
fn sharpness_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: Sharpness::new,
        get: Sharpness::get,
        of: Some(Sharpness::of),
        valid: &[f32::MIN_POSITIVE, 8.0, 100.0],
        invalid: &[0.0, -1.0, 100.1, NAN, INF, NEG_INF],
    });
}

/// 8 kHz..=192 kHz.
#[test]
fn windowed_rate_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: WindowedRate::new,
        get: WindowedRate::get,
        of: Some(WindowedRate::of),
        valid: &[SampleRate::SR_8K, SampleRate::SR_48K, SampleRate::SR_192K],
        invalid: &[SampleRate::of(7_999.0), SampleRate::of(192_001.0)],
    });
}

/// 1..=16 channels.
#[test]
fn windowed_layout_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: WindowedLayout::new,
        get: WindowedLayout::get,
        of: Some(WindowedLayout::of),
        valid: &[
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::from(16u16),
        ],
        invalid: &[ChannelLayout::from(0u16), ChannelLayout::from(17u16)],
    });
}

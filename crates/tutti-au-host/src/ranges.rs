//! The declared plain-unit range of each parameter, and the conversion between
//! it and the normalized `0..=1` the shared vocabulary speaks.
//!
//! `ProcessContext::param_changes` and [`Control`](tutti_plugin_types::Control)
//! carry **normalized** values (the host's authoring convention), but
//! `AudioUnitSetParameter` takes **native plain units**: AU has no
//! normalization concept at all. Denormalizing needs the declared range, and
//! re-reading `kAudioUnitProperty_ParameterInfo` per automation point on the
//! audio thread would be a property round-trip per block, so the ranges are read
//! once, when the plugin loads, and again when it says its parameter list moved.

#![cfg(target_os = "macos")]

use tutti_au_ffi::Unit;

use crate::parameters;

/// Declared plain-unit bounds for one AU parameter, as reported by
/// `kAudioUnitProperty_ParameterInfo`.
///
/// Raw `f32` Hz/dB/percent/seconds, not `tutti_types` unit newtypes: which
/// physical unit these bounds are in is per-parameter and only known at runtime
/// from `AudioUnitParameterInfo::unit`, and the values cross the AudioToolbox C
/// ABI verbatim.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParamBounds {
    pub(crate) min: f32,
    pub(crate) max: f32,
}

impl ParamBounds {
    /// The bounds as an `f64` span, `None` for a non-finite bound or a range
    /// that is not increasing: a malformed AU, which gets no invented span.
    fn span(self) -> Option<(f64, f64)> {
        let (min, max) = (f64::from(self.min), f64::from(self.max));
        (min.is_finite() && max.is_finite() && max > min).then_some((min, max))
    }

    /// Maps a normalized `0..=1` value onto `[min, max]`, narrowed to the `f32`
    /// that `AudioUnitSetParameter` takes.
    ///
    /// **This is AU's own conversion, and it is linear.** An AU has no
    /// normalized value (`AudioUnitGetParameter` and `AudioUnitSetParameter`
    /// speak plain `Float32` only), so the position is the host's, over the
    /// declared span. The display-curve flags (`DisplayLogarithmic`, ...,
    /// `AudioUnitProperties.h`) are *display scales*: the header gives them no
    /// formula, so they stay `DisplayCurve`
    /// hints for a UI and do not bend this mapping. The one curve the platform
    /// defines is `AUParameterValueFromLinear` (`AudioUnitUtilities.h`), keyed
    /// on the unit, which this crate does not call.
    ///
    /// Guards: a non-finite or non-increasing range yields `min` when finite
    /// and `0.0` otherwise, and NaN is `0.0` normalized, so the result is always
    /// finite and inside `[min, max]` (both came from an `f32`).
    pub(crate) fn to_plain(self, normalized: f64) -> f32 {
        let n = if normalized.is_nan() {
            0.0
        } else {
            normalized.clamp(0.0, 1.0)
        };
        let Some((min, max)) = self.span() else {
            return if self.min.is_finite() { self.min } else { 0.0 };
        };
        (min + n * (max - min)) as f32
    }

    /// Inverse of [`to_plain`](Self::to_plain): the AU's plain value back onto
    /// normalized `0..=1`, the read direction of the same boundary. `0.0` for
    /// NaN and for a range with no span.
    pub(crate) fn to_normalized(self, plain: f32) -> f64 {
        let Some((min, max)) = self.span() else {
            return 0.0;
        };
        if plain.is_nan() {
            return 0.0;
        }
        (f64::from(plain).clamp(min, max) - min) / (max - min)
    }
}

/// The declared bounds of every parameter, sorted by id.
///
/// A `Vec` sorted by id rather than a `HashMap`: AU parameter counts are in the
/// tens, so a binary search beats hashing and keeps the render path
/// allocation-free.
#[derive(Debug, Clone, Default)]
pub(crate) struct RangeTable {
    ranges: Vec<(u32, ParamBounds)>,
    /// The ids flagged `kAudioUnitParameterFlag_NonRealTime`, sorted: a write
    /// "in real-time will cause a glitch or otherwise undesirable effect"
    /// (`AudioUnitProperties.h`), so the audio thread never makes one.
    non_realtime: Vec<u32>,
}

impl RangeTable {
    /// Reads every parameter's declared range off the AU.
    pub(crate) fn read(unit: &Unit) -> Self {
        let list = parameters::list(unit);
        let mut non_realtime: Vec<u32> = list
            .iter()
            .filter(|p| p.non_realtime)
            .map(|p| p.id)
            .collect();
        non_realtime.sort_unstable();
        let mut ranges: Vec<(u32, ParamBounds)> = list
            .into_iter()
            .map(|p| {
                (
                    p.id,
                    ParamBounds {
                        min: p.range.min,
                        max: p.range.max,
                    },
                )
            })
            .collect();
        ranges.sort_unstable_by_key(|&(id, _)| id);
        Self {
            ranges,
            non_realtime,
        }
    }

    /// Whether the AU flagged `id` `NonRealTime`: a write to it belongs between
    /// blocks, on the control side.
    pub(crate) fn is_non_realtime(&self, id: u32) -> bool {
        self.non_realtime.binary_search(&id).is_ok()
    }

    /// Every declared id, ascending.
    pub(crate) fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.ranges.iter().map(|&(id, _)| id)
    }

    /// The declared bounds for `id`, or `None` for a parameter the AU never
    /// declared: it has no range to denormalize against, so a caller skips or
    /// passes it through rather than guessing one.
    pub(crate) fn bounds(&self, id: u32) -> Option<ParamBounds> {
        self.ranges
            .binary_search_by_key(&id, |&(pid, _)| pid)
            .ok()
            .map(|i| self.ranges[i].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_plain_maps_onto_the_declared_range_not_identity() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.to_plain(0.0), 10.0);
        assert_eq!(cutoff.to_plain(1.0), 22_050.0);
        assert_eq!(cutoff.to_plain(0.5), 11_030.0);
        // Not 1.0: 1 Hz, an inaudible filter.
        assert_ne!(cutoff.to_plain(1.0), 1.0);

        // Negative minima (AUDelay Feedback is [-99.9, 99.9]) must map too; the
        // old clamp to [0,1] made the entire negative half unreachable.
        let feedback = ParamBounds {
            min: -99.9,
            max: 99.9,
        };
        assert!((feedback.to_plain(0.0) - -99.9).abs() < 1e-3);
        assert!(feedback.to_plain(0.5).abs() < 1e-3);
        assert!((feedback.to_plain(1.0) - 99.9).abs() < 1e-3);

        // Out-of-range input is clamped to the declared endpoints, never past.
        assert_eq!(cutoff.to_plain(-5.0), 10.0);
        assert_eq!(cutoff.to_plain(9.0), 22_050.0);

        // A degenerate range yields `min` rather than NaN/inf.
        let degenerate = ParamBounds { min: 3.0, max: 3.0 };
        assert_eq!(degenerate.to_plain(0.5), 3.0);
    }

    /// The live-path half: this `to_plain`'s return value goes straight into
    /// `AudioUnitSetParameter` on a running unit, so a NaN escaping here is a NaN
    /// in a live filter coefficient.
    ///
    /// `normalized` comes over IPC and the bounds come from the plugin's own
    /// `kAudioUnitProperty_ParameterInfo`, so neither is trusted. Clamping does not
    /// substitute for the check: `f32::clamp` returns NaN for NaN, and `max <= min`
    /// is `false` when either bound is NaN.
    #[test]
    fn nan_never_reaches_a_live_au_parameter() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };

        assert!(
            cutoff.to_plain(f64::NAN).is_finite(),
            "a NaN automation point must not reach AudioUnitSetParameter"
        );
        assert_eq!(cutoff.to_plain(f64::INFINITY), 22_050.0);
        assert_eq!(cutoff.to_plain(f64::NEG_INFINITY), 10.0);

        // Bounds the AU itself reported as non-finite.
        for (min, max) in [
            (f32::NAN, 1.0),
            (0.0, f32::NAN),
            (f32::NEG_INFINITY, 1.0),
            (0.0, f32::INFINITY),
        ] {
            let broken = ParamBounds { min, max };
            for v in [0.0, 0.5, 1.0, f64::NAN] {
                assert!(
                    broken.to_plain(v).is_finite(),
                    "to_plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
        }
    }

    /// AU's position is the line over the declared range, and its inverse reads
    /// a plain value back to the position it came from. The display curves
    /// (`DisplayLogarithmic`, ...) are display hints and bend neither: a log
    /// cutoff's midpoint is the arithmetic one.
    #[test]
    fn the_position_is_the_line_and_inverts() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.to_normalized(10.0), 0.0);
        assert_eq!(cutoff.to_normalized(22_050.0), 1.0);
        assert_eq!(cutoff.to_normalized(11_030.0), 0.5);
        for n in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let back = cutoff.to_normalized(cutoff.to_plain(n));
            assert!((back - n).abs() < 1e-6, "{n} came back as {back}");
        }
        assert_eq!(cutoff.to_normalized(-1.0), 0.0, "below the range clamps");
        assert_eq!(cutoff.to_normalized(1e9), 1.0, "above the range clamps");
        assert_eq!(cutoff.to_normalized(f32::NAN), 0.0);
        assert_eq!(ParamBounds { min: 3.0, max: 3.0 }.to_normalized(3.0), 0.0);
    }

    #[test]
    fn a_table_finds_ids_by_binary_search() {
        let table = RangeTable {
            ranges: vec![
                (2u32, ParamBounds { min: 0.0, max: 1.0 }),
                (
                    7u32,
                    ParamBounds {
                        min: 10.0,
                        max: 22_050.0,
                    },
                ),
            ],
            non_realtime: vec![7],
        };
        assert!(table.is_non_realtime(7));
        assert!(!table.is_non_realtime(2));
        assert_eq!(table.bounds(7).map(|b| b.max), Some(22_050.0));
        assert_eq!(table.bounds(2).map(|b| b.min), Some(0.0));
        // A parameter the AU never declared has no range to denormalize
        // against, so it must be reported missing (and skipped), not guessed.
        assert!(table.bounds(3).is_none());
    }
}

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
use tutti_plugin_types::ParamRange;

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
    /// These bounds as the shared range type, which owns the conversion.
    ///
    /// `default` is unused by [`to_plain`](ParamRange::to_plain) /
    /// [`to_normalized`](ParamRange::to_normalized): only the endpoints
    /// participate, so `min` stands in rather than a value invented here.
    fn as_range(self) -> ParamRange {
        ParamRange::Plain {
            min: self.min as f64,
            max: self.max as f64,
            default: self.min as f64,
        }
    }

    /// Maps a normalized `0..=1` value onto `[min, max]`.
    ///
    /// Delegates to [`ParamRange::to_plain`], narrowing to the `f32` that
    /// `AudioUnitSetParameter` takes. **Delegation rather than an `f32`
    /// hand-copy**: the four guards this depends on (non-finite bounds,
    /// degenerate range, NaN value, clamp order) are stated once there, and two
    /// copies would drift apart silently.
    ///
    /// The narrowing is safe for the property this path needs: `to_plain` never
    /// returns a non-finite `f64`, and the result is bounded by `[min, max]`,
    /// both of which came *from* an `f32`.
    pub(crate) fn to_plain(self, normalized: f64) -> f32 {
        self.as_range().to_plain(normalized) as f32
    }

    /// Inverse of [`to_plain`](Self::to_plain): the AU's plain value back onto
    /// normalized `0..=1`, the read direction of the same boundary.
    pub(crate) fn to_normalized(self, plain: f32) -> f64 {
        self.as_range().to_normalized(plain as f64)
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
}

impl RangeTable {
    /// Reads every parameter's declared range off the AU.
    pub(crate) fn read(unit: &Unit) -> Self {
        let mut ranges: Vec<(u32, ParamBounds)> = parameters::list(unit)
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
        Self { ranges }
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
        };
        assert_eq!(table.bounds(7).map(|b| b.max), Some(22_050.0));
        assert_eq!(table.bounds(2).map(|b| b.min), Some(0.0));
        // A parameter the AU never declared has no range to denormalize
        // against, so it must be reported missing (and skipped), not guessed.
        assert!(table.bounds(3).is_none());
    }
}

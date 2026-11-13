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

use tutti_au_ffi::Unit;
use tutti_plugin_types::{Normalized, ParamMapping};

use std::sync::Arc;

use crate::marks::{Marks, Slot};
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
}

/// **This is AU's own conversion, and it is linear.** An AU has no normalized
/// value (`AudioUnitGetParameter` and `AudioUnitSetParameter` speak plain
/// `Float32` only), so the position is the host's, over the declared span. The
/// display-curve flags (`DisplayLogarithmic`, ..., `AudioUnitProperties.h`) are
/// *display scales*: the header gives them no formula, so they stay
/// `DisplayCurve` hints for a UI and do not bend this mapping. The one curve the
/// platform defines is `AUParameterValueFromLinear` (`AudioUnitUtilities.h`),
/// keyed on the unit, which this crate does not call.
impl ParamMapping for ParamBounds {
    /// Maps a position onto `[min, max]`, narrowed to the `f32` that
    /// `AudioUnitSetParameter` takes (and widened back, so the value is the one
    /// the unit is given).
    ///
    /// A non-finite or non-increasing range yields `min` when finite and `0.0`
    /// otherwise, so the result is always finite and inside `[min, max]` (both
    /// came from an `f32`).
    fn plain(&self, value: Normalized) -> f64 {
        let Some((min, max)) = self.span() else {
            return if self.min.is_finite() {
                f64::from(self.min)
            } else {
                0.0
            };
        };
        f64::from((min + value.get() * (max - min)) as f32)
    }

    /// The AU's plain value back onto `0..=1`, the read direction of the same
    /// boundary: clamped into the range, `0.0` for NaN and for a range with no
    /// span.
    fn normalized(&self, plain: f64) -> Normalized {
        let Some((min, max)) = self.span() else {
            return Normalized::of(0.0);
        };
        if plain.is_nan() {
            return Normalized::of(0.0);
        }
        Normalized::clamped((plain.clamp(min, max) - min) / (max - min))
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
    /// What the render tells the control side of each parameter here: sized for
    /// every id in `ranges`, so each declared parameter has a [`Slot`].
    marks: Arc<Marks>,
}

impl RangeTable {
    /// Reads every parameter's declared range off the AU. `previous` is the
    /// marks of the table this replaces, kept when they have room for every id.
    pub(crate) fn read(unit: &Unit, previous: Option<&Arc<Marks>>) -> Self {
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
        let ids: Vec<u32> = ranges.iter().map(|&(id, _)| id).collect();
        Self {
            marks: Marks::covering(previous, &ids),
            ranges,
            non_realtime,
        }
    }

    /// The marks the render sets and the control tick takes.
    pub(crate) fn marks(&self) -> &Arc<Marks> {
        &self.marks
    }

    /// `id`'s place in [`marks`](Self::marks); every declared id has one.
    pub(crate) fn slot(&self, id: u32) -> Option<Slot> {
        self.marks.slot(id)
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

    fn n(v: f64) -> Normalized {
        Normalized::of(v)
    }

    #[test]
    fn plain_maps_onto_the_declared_range_not_identity() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.plain(n(0.0)), 10.0);
        assert_eq!(cutoff.plain(n(1.0)), 22_050.0);
        assert_eq!(cutoff.plain(n(0.5)), 11_030.0);
        // Not 1.0: 1 Hz, an inaudible filter.
        assert_ne!(cutoff.plain(n(1.0)), 1.0);

        // Negative minima (AUDelay Feedback is [-99.9, 99.9]) must map too; the
        // old clamp to [0,1] made the entire negative half unreachable.
        let feedback = ParamBounds {
            min: -99.9,
            max: 99.9,
        };
        assert!((feedback.plain(n(0.0)) - -99.9).abs() < 1e-3);
        assert!(feedback.plain(n(0.5)).abs() < 1e-3);
        assert!((feedback.plain(n(1.0)) - 99.9).abs() < 1e-3);

        // A degenerate range yields `min` rather than NaN/inf.
        let degenerate = ParamBounds { min: 3.0, max: 3.0 };
        assert_eq!(degenerate.plain(n(0.5)), 3.0);
    }

    /// The live-path half: `plain`'s value goes straight into
    /// `AudioUnitSetParameter` on a running unit, so a NaN escaping here is a NaN
    /// in a live filter coefficient. The bounds come from the plugin's own
    /// `kAudioUnitProperty_ParameterInfo`, so they are not trusted.
    #[test]
    fn a_malformed_range_never_reaches_a_live_au_parameter_as_non_finite() {
        for (min, max) in [
            (f32::NAN, 1.0),
            (0.0, f32::NAN),
            (f32::NEG_INFINITY, 1.0),
            (0.0, f32::INFINITY),
        ] {
            let broken = ParamBounds { min, max };
            for v in [0.0, 0.5, 1.0] {
                assert!(
                    broken.plain(n(v)).is_finite(),
                    "plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
            assert_eq!(broken.normalized(0.5).get(), 0.0);
        }
    }

    /// AU's position is the line over the declared range. The display curves
    /// (`DisplayLogarithmic`, ...) are display hints and do not bend it: a log
    /// cutoff's midpoint is the arithmetic one. (The laws of the map are
    /// `mapping_contract!`'s, below.)
    #[test]
    fn the_position_is_the_line() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.normalized(10.0).get(), 0.0);
        assert_eq!(cutoff.normalized(22_050.0).get(), 1.0);
        assert_eq!(cutoff.normalized(11_030.0).get(), 0.5);
        assert_eq!(
            ParamBounds { min: 3.0, max: 3.0 }.normalized(3.0).get(),
            0.0
        );
    }

    tutti_plugin_types::mapping_contract!(
        cutoff_map,
        ParamBounds {
            min: 10.0,
            max: 22_050.0
        }
    );
    tutti_plugin_types::mapping_contract!(
        feedback_map,
        ParamBounds {
            min: -99.9,
            max: 99.9
        }
    );
    tutti_plugin_types::mapping_contract!(one_point_map, ParamBounds { min: 3.0, max: 3.0 });

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
            marks: Arc::new(Marks::new(vec![2, 7])),
        };
        assert!(table.is_non_realtime(7));
        assert!(!table.is_non_realtime(2));
        assert_eq!(table.bounds(7).map(|b| b.max), Some(22_050.0));
        assert_eq!(table.bounds(2).map(|b| b.min), Some(0.0));
        // A parameter the AU never declared has no range to denormalize
        // against, so it must be reported missing (and skipped), not guessed.
        assert!(table.bounds(3).is_none());
        assert!(table.slot(7).is_some() && table.slot(3).is_none());
    }
}

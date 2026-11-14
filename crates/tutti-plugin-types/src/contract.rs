//! The mapping contract: the laws every [`ParamMapping`] is held to.
//!
//! Behind the `contract` feature (off by default): a format host turns it on in
//! its dev-dependency on this crate and runs [`mapping_contract!`](crate::mapping_contract) on each
//! mapping type it has. The laws are the trait's docs, from the specs: finite
//! plain values, positions clamped onto `0..=1` with NaN never escaping, a
//! stable round trip from the plain side, a monotonic map.
//!
//! ```text
//! mapping_contract!(linear, ParamBounds::continuous(0, 20.0, 20_000.0));
//! mapping_contract!(stepped, ParamBounds::stepped(0, 0.0, 4.0), steps = [0.0, 1.0, 2.0, 3.0, 4.0]);
//! ```
//!
//! Each invocation makes a module of one `#[test]` per case. Each case is a
//! plain function from a mapping to a `Result` ([`cases`]), so a test can hand
//! one a deliberately broken mapping and assert that it fails; [`check`] runs
//! them all, for a mapping a test builds once (VST3's, which borrows a
//! controller and the main-thread token).
//!
//! The sweep is the positions `0`, `1`, a grid of interior points and values a
//! hair inside each end, the plain values those produce, and plain values off
//! the ends (past the map's range by a unit and by `1e300`, the infinities and
//! NaN). A stepped map names its `steps`: each is a plain value the map reaches
//! exactly, and the only values it produces.

// Every case returns `Err` with the message of the first figure that fails.
#![allow(clippy::missing_errors_doc)]

use crate::{Normalized, ParamMapping};

/// The positions the cases sweep: both ends, a hair inside each, a grid of
/// sixteenths and a few that no grid names.
#[must_use]
pub fn positions() -> Vec<f64> {
    let mut at: Vec<f64> = (0..=16).map(|i| f64::from(i) / 16.0).collect();
    at.extend([1e-9, 1.0 - 1e-9, 0.1234567, 0.3333333333, 0.7123456789]);
    at.sort_by(f64::total_cmp);
    at
}

fn position(n: f64) -> Normalized {
    Normalized::new(n).expect("the sweep's positions are on 0..=1")
}

/// The plain values a mapping produces over [`positions`], in position order.
fn produced<M: ParamMapping + ?Sized>(map: &M) -> Vec<f64> {
    positions()
        .into_iter()
        .map(|n| map.plain(position(n)))
        .collect()
}

/// A size to measure a tolerance against: the largest plain value at an end.
fn scale<M: ParamMapping + ?Sized>(map: &M) -> f64 {
    let (lo, hi) = (map.plain(position(0.0)), map.plain(position(1.0)));
    lo.abs().max(hi.abs()).max(f64::MIN_POSITIVE)
}

/// Which way a run of values goes: `Some(true)` up, `Some(false)` down, `None`
/// constant. `Err` when it turns.
fn direction(values: &[f64]) -> Result<Option<bool>, String> {
    let mut seen: Option<bool> = None;
    for pair in values.windows(2) {
        let step = if pair[1] > pair[0] {
            true
        } else if pair[1] < pair[0] {
            false
        } else {
            continue;
        };
        match seen {
            None => seen = Some(step),
            Some(was) if was != step => {
                return Err(format!(
                    "it turns: {} then {} after going {}",
                    pair[0],
                    pair[1],
                    if was { "up" } else { "down" }
                ));
            }
            Some(_) => {}
        }
    }
    Ok(seen)
}

/// The cases, one function each. A stepped map's `steps` are its plain values.
pub mod cases {
    use super::{direction, position, positions, produced, scale};
    use crate::ParamMapping;

    /// Law 1: every position on `0..=1` has a finite plain value.
    pub fn finite<M: ParamMapping + ?Sized>(map: &M) -> Result<(), String> {
        for n in positions() {
            let plain = map.plain(position(n));
            if !plain.is_finite() {
                return Err(format!("plain({n}) is {plain}"));
            }
        }
        Ok(())
    }

    /// Law 2: any plain value, finite or not, maps onto `0..=1`; NaN is `0.0`;
    /// a value past an end of the map reads as that end.
    pub fn bounded<M: ParamMapping + ?Sized>(map: &M) -> Result<(), String> {
        let (lo, hi) = (map.plain(position(0.0)), map.plain(position(1.0)));
        let (low, high) = (lo.min(hi), lo.max(hi));
        let inputs = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            low - 1.0,
            high + 1.0,
            1e300,
            -1e300,
        ];
        for plain in inputs.into_iter().chain(produced(map)) {
            let n = map.normalized(plain).get();
            if !(0.0..=1.0).contains(&n) {
                return Err(format!("normalized({plain}) is {n}, off 0..=1"));
            }
        }
        let nan = map.normalized(f64::NAN).get();
        if nan != 0.0 {
            return Err(format!("normalized(NaN) is {nan}, not 0.0"));
        }
        if low != high {
            for (past, end) in [(low - 1.0, low), (high + 1.0, high)] {
                let (got, want) = (map.normalized(past), map.normalized(end));
                if got.get() != want.get() {
                    return Err(format!(
                        "normalized({past}) is {}, but the end {end} reads {}",
                        got.get(),
                        want.get()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Law 3: `plain(normalized(p)) == p` for every `p` the map produces: bit
    /// for bit when `steps` is not empty, within `1e-6` of the map's size
    /// otherwise (a continuous map is held to its own width, and AU's is `f32`).
    pub fn round_trip<M: ParamMapping + ?Sized>(map: &M, steps: &[f64]) -> Result<(), String> {
        let tolerance = if steps.is_empty() {
            1e-6 * scale(map)
        } else {
            0.0
        };
        for plain in produced(map) {
            let back = map.plain(map.normalized(plain));
            if (back - plain).abs() > tolerance {
                return Err(format!(
                    "plain(normalized({plain})) is {back} (tolerance {tolerance})"
                ));
            }
        }
        Ok(())
    }

    /// Law 4: over increasing positions the plain value never turns, and over
    /// increasing plain values the position goes the same way.
    pub fn monotonic<M: ParamMapping + ?Sized>(map: &M) -> Result<(), String> {
        let plains = produced(map);
        let way = direction(&plains).map_err(|e| format!("plain: {e}"))?;
        let Some(up) = way else {
            return Ok(());
        };
        let mut sorted = plains;
        sorted.sort_by(f64::total_cmp);
        let back: Vec<f64> = sorted.iter().map(|&p| map.normalized(p).get()).collect();
        match direction(&back).map_err(|e| format!("normalized: {e}"))? {
            Some(same) if same != up => Err("normalized runs against plain".to_string()),
            _ => Ok(()),
        }
    }

    /// The two ends: position 0 and 1 are the ends of the plain range, and each
    /// end reads back as its own position.
    pub fn ends<M: ParamMapping + ?Sized>(map: &M) -> Result<(), String> {
        let (lo, hi) = (map.plain(position(0.0)), map.plain(position(1.0)));
        if lo == hi {
            return Ok(());
        }
        for (n, plain) in [(0.0, lo), (1.0, hi)] {
            let got = map.normalized(plain).get();
            if (got - n).abs() > 1e-6 {
                return Err(format!("normalized(plain({n}) = {plain}) is {got}"));
            }
        }
        Ok(())
    }

    /// A stepped map: each of `steps` is reached exactly (a position names it
    /// and reads back as it), and no other plain value is produced.
    pub fn steps<M: ParamMapping + ?Sized>(map: &M, steps: &[f64]) -> Result<(), String> {
        for &step in steps {
            let back = map.plain(map.normalized(step));
            if back != step {
                return Err(format!("step {step} reads back as {back}"));
            }
        }
        for plain in produced(map) {
            if !steps.contains(&plain) {
                return Err(format!("{plain} is produced and is not a step"));
            }
        }
        Ok(())
    }
}

/// Every case, in order, on one mapping (`steps` empty for a continuous one).
pub fn check<M: ParamMapping + ?Sized>(map: &M, steps: &[f64]) -> Result<(), String> {
    cases::finite(map).map_err(|e| format!("finite: {e}"))?;
    cases::bounded(map).map_err(|e| format!("bounded: {e}"))?;
    cases::ends(map).map_err(|e| format!("ends: {e}"))?;
    cases::round_trip(map, steps).map_err(|e| format!("round_trip: {e}"))?;
    cases::monotonic(map).map_err(|e| format!("monotonic: {e}"))?;
    if !steps.is_empty() {
        cases::steps(map, steps).map_err(|e| format!("steps: {e}"))?;
    }
    Ok(())
}

/// Runs the mapping contract on one mapping: `mapping_contract!(name, expr)`
/// for a continuous map, `mapping_contract!(name, expr, steps = [..])` for a
/// stepped one. It makes `mod name` of one `#[test]` per case, each building
/// `expr` afresh (a [`ParamMapping`]).
#[macro_export]
macro_rules! mapping_contract {
    ($name:ident, $map:expr) => {
        $crate::mapping_contract!($name, $map, steps = []);
    };
    ($name:ident, $map:expr, steps = [$($step:expr),* $(,)?]) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[test]
            fn contract_finite() {
                $crate::contract::cases::finite(&$map).unwrap();
            }
            #[test]
            fn contract_bounded() {
                $crate::contract::cases::bounded(&$map).unwrap();
            }
            #[test]
            fn contract_ends() {
                $crate::contract::cases::ends(&$map).unwrap();
            }
            #[test]
            fn contract_round_trip() {
                let steps: &[f64] = &[$($step),*];
                $crate::contract::cases::round_trip(&$map, steps).unwrap();
            }
            #[test]
            fn contract_monotonic() {
                $crate::contract::cases::monotonic(&$map).unwrap();
            }
            #[test]
            fn contract_steps() {
                let steps: &[f64] = &[$($step),*];
                if !steps.is_empty() {
                    $crate::contract::cases::steps(&$map, steps).unwrap();
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line over `[min, max]` (either way round), optionally on integer steps
    /// that it rounds, truncates or leaves alone, and with a defect to plant.
    #[derive(Clone, Copy)]
    struct Line {
        min: f64,
        max: f64,
        stepped: Option<fn(f64) -> f64>,
        defect: Defect,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Defect {
        None,
        /// Plain is NaN at position 0.5.
        NanAtMiddle,
        /// The position of a plain value ignores the sign of the range.
        Unflipped,
        /// A NaN plain value reads as 1.
        NanIsOne,
        /// A plain value past the top reads as 0.
        WrapsPastTop,
        /// Plain turns back after 0.75.
        Turns,
    }

    impl Line {
        const fn new(min: f64, max: f64) -> Self {
            Self {
                min,
                max,
                stepped: None,
                defect: Defect::None,
            }
        }
        const fn flawed(mut self, defect: Defect) -> Self {
            self.defect = defect;
            self
        }
        fn on(mut self, snap: fn(f64) -> f64) -> Self {
            self.stepped = Some(snap);
            self
        }
    }

    impl ParamMapping for Line {
        fn plain(&self, value: Normalized) -> f64 {
            let n = value.get();
            if self.defect == Defect::NanAtMiddle && n == 0.5 {
                return f64::NAN;
            }
            let n = if self.defect == Defect::Turns && n > 0.75 {
                1.5 - n
            } else {
                n
            };
            let plain = self.min + n * (self.max - self.min);
            self.stepped.map_or(plain, |snap| snap(plain))
        }

        fn normalized(&self, plain: f64) -> Normalized {
            if plain.is_nan() {
                return Normalized::of(if self.defect == Defect::NanIsOne {
                    1.0
                } else {
                    0.0
                });
            }
            let (low, high) = (self.min.min(self.max), self.min.max(self.max));
            if self.defect == Defect::WrapsPastTop && plain > high {
                return Normalized::of(0.0);
            }
            let span = if self.defect == Defect::Unflipped {
                (self.max - self.min).abs()
            } else {
                self.max - self.min
            };
            let from = if self.defect == Defect::Unflipped {
                low
            } else {
                self.min
            };
            Normalized::clamped((plain.clamp(low, high) - from) / span)
        }
    }

    fn steps_of(min: f64, max: f64) -> Vec<f64> {
        (0..=((max - min) as i32))
            .map(|i| min + f64::from(i))
            .collect()
    }

    // The contract on maps that obey it: continuous up, continuous down, and a
    // stepped one on its integers.
    mapping_contract!(rising, Line::new(20.0, 20_000.0));
    mapping_contract!(falling, Line::new(10.0, -10.0));
    mapping_contract!(
        stepped,
        Line::new(0.0, 4.0).on(f64::round),
        steps = [0.0, 1.0, 2.0, 3.0, 4.0]
    );

    /// A planted defect fails the case that states its law, which is the
    /// property the contract's cases must have: they can fail.
    #[test]
    fn a_nan_plain_value_fails_finite() {
        let map = Line::new(0.0, 1.0).flawed(Defect::NanAtMiddle);
        assert!(cases::finite(&map).is_err());
    }

    #[test]
    fn a_plain_value_that_wraps_past_the_top_fails_bounded() {
        let map = Line::new(0.0, 1.0).flawed(Defect::WrapsPastTop);
        assert!(cases::bounded(&map).is_err());
    }

    #[test]
    fn a_nan_that_reads_as_a_position_but_not_zero_fails_bounded() {
        let map = Line::new(0.0, 1.0).flawed(Defect::NanIsOne);
        assert!(cases::bounded(&map).is_err());
    }

    #[test]
    fn a_stepped_map_that_truncates_fails_the_round_trip() {
        // `normalized(15)` is `15 / 22`, and `15 / 22 * 22` is a hair under 15: a
        // truncating `plain` then reads 14.
        let map = Line::new(0.0, 22.0).on(f64::trunc);
        let steps = steps_of(0.0, 22.0);
        assert!(cases::round_trip(&map, &steps).is_err());
    }

    #[test]
    fn a_stepped_map_that_produces_a_value_between_steps_fails_steps() {
        let map = Line::new(0.0, 4.0);
        assert!(cases::steps(&map, &steps_of(0.0, 4.0)).is_err());
    }

    #[test]
    fn a_map_that_turns_fails_monotonic() {
        let map = Line::new(0.0, 1.0).flawed(Defect::Turns);
        assert!(cases::monotonic(&map).is_err());
    }

    #[test]
    fn a_position_that_ignores_a_decreasing_range_fails_the_ends() {
        let map = Line::new(10.0, -10.0).flawed(Defect::Unflipped);
        assert!(cases::ends(&map).is_err());
    }
}

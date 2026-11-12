use crate::error::TempoError;
use crate::map::TempoMap;
use crate::point::{TempoPoint, Transition};
use tutti_types::{Beat, Bpm};

/// A tempo map written left to right, where a trailing ramp cannot be written.
///
/// It starts with a tempo at beat 0 ([`TempoMap::builder`]). Each call after it
/// names the next point and how the tempo gets *to* it from the last one: the
/// point it adds is always a step, so the map's last point never ramps (the
/// case [`TempoError::RampWithoutEnd`] refuses in a list of points).
///
/// ```
/// use tutti_tempo::{TempoMap, Transition};
/// use tutti_types::{Beat, Bpm};
///
/// let map = TempoMap::builder(Bpm::of(120.0))
///     .ramp_time_to(Beat(4.0), Bpm::of(180.0))
///     .step_to(Beat(8.0), Bpm::of(90.0))
///     .build()?;
/// assert_eq!(map.points()[0].transition, Transition::RampTime);
/// assert_eq!(map.points().last().unwrap().transition, Transition::Step);
/// # Ok::<(), tutti_tempo::TempoError>(())
/// ```
///
/// A ramp with no point to ramp to has no spelling:
///
/// ```compile_fail,E0599
/// use tutti_tempo::TempoMap;
/// use tutti_types::{Beat, Bpm};
///
/// let _ = TempoMap::builder(Bpm::of(120.0)).ramp_time(Beat(4.0));
/// ```
#[derive(Clone, Debug)]
#[must_use]
pub struct TempoMapBuilder {
    points: Vec<TempoPoint>,
}

impl TempoMapBuilder {
    pub(crate) fn new(first: Bpm) -> Self {
        Self {
            points: vec![TempoPoint::step(Beat(0.0), first)],
        }
    }

    fn then(mut self, transition: Transition, beat: Beat, tempo: Bpm) -> Self {
        if let Some(last) = self.points.last_mut() {
            last.transition = transition;
        }
        self.points.push(TempoPoint::step(beat, tempo));
        self
    }

    /// Hold the last tempo until `beat`, then take `tempo` there.
    pub fn step_to(self, beat: Beat, tempo: Bpm) -> Self {
        self.then(Transition::Step, beat, tempo)
    }

    /// Move from the last tempo to `tempo`, reaching it at `beat`, with tempo
    /// linear in time ([`Transition::RampTime`]).
    pub fn ramp_time_to(self, beat: Beat, tempo: Bpm) -> Self {
        self.then(Transition::RampTime, beat, tempo)
    }

    /// Move from the last tempo to `tempo`, reaching it at `beat`, with tempo
    /// linear in beats ([`Transition::RampBeats`]).
    pub fn ramp_beats_to(self, beat: Beat, tempo: Bpm) -> Self {
        self.then(Transition::RampBeats, beat, tempo)
    }

    /// The map.
    ///
    /// # Errors
    ///
    /// As [`TempoMap::new`], for what a builder can still get wrong: a beat
    /// that is not finite or not after the last point's
    /// ([`TempoError::BadOrder`]), or a segment an `f64` cannot hold.
    pub fn build(self) -> Result<TempoMap, TempoError> {
        TempoMap::new(self.points)
    }
}

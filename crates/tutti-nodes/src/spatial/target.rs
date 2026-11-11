//! Where a source is aimed, and how it gets there without zippering.
//! Algorithm-neutral: [`SpatialTarget`] is the control→RT storage,
//! [`AngleSmoother`] the de-zipper both panners step per frame.

use tutti_dsp::Smoother;
use tutti_types::{Azimuth, Direction, Elevation, Param, SampleRate, Samples, Seconds};

/// The time constant of the position de-zipper, in audio time.
const SMOOTH_TIME: Seconds = Seconds(0.05);

/// A source's bearing and height as a pair of lock-free [`Param`] cells.
///
/// Both panners hold one and read it once per block on the audio thread; the
/// control thread writes it through [`store`](Self::store) (usually via a
/// panner's controls). Clones share the same cells, so a write through any
/// clone reaches the node.
///
/// The two fields are different types on purpose: a bearing wraps (190° is
/// 170° to the right), a height saturates (past straight up, you stop).
#[derive(Clone, Debug)]
pub struct SpatialTarget {
    /// Bearing in [`Azimuth`] degrees, wrapped onto the circle: 0 is front,
    /// 90 left, -90 right. Written lock-free from the control thread and read
    /// once per block by the panner.
    pub azimuth: Param<Azimuth>,
    /// Height in [`Elevation`] degrees, clamped to -90..90: 0 is ear level,
    /// positive is up. Saturates at the poles rather than wrapping.
    pub elevation: Param<Elevation>,
}

impl SpatialTarget {
    /// Creates a target aimed straight ahead at ear level
    /// ([`Azimuth::FRONT`] / [`Elevation::LEVEL`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
        }
    }

    /// Returns the current bearing and height. Lock-free; safe on the audio
    /// thread.
    #[inline]
    pub fn load(&self) -> Direction {
        Direction::new(self.azimuth.load(), self.elevation.load())
    }

    /// Stores a bearing and height, normalized on the way in: the bearing
    /// wraps onto -180..180, the height clamps to -90..90. Lock-free.
    pub fn store(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.azimuth.store(azimuth.into().wrap());
        self.elevation
            .store(Elevation::new_clamped(elevation.into().get()));
    }

    /// Stops sharing both cells with other clones, keeping the current values
    /// (see [`Param::detach`]).
    ///
    /// A panner's fork calls this so it renders the position it was taken at,
    /// unaffected by later moves of the live node.
    pub fn detach(&mut self) {
        self.azimuth.detach();
        self.elevation.detach();
    }
}

impl Default for SpatialTarget {
    fn default() -> Self {
        Self::new()
    }
}

/// One smoother per coordinate, each with its own arithmetic: the bearing takes
/// the short arc, the height is a plain ramp.
///
/// Shared because that asymmetry is easy to get wrong twice. Give the bearing
/// the linear form and a source crossing behind the listener (170° → -170°, a
/// 20° move) sweeps 340° the wrong way around the head.
///
/// The one-pole recurrence and its running value are [`Smoother`]'s; this type
/// derives the coefficient from the time constant and the stride, and wraps
/// the bearing's arc around it.
#[derive(Clone, Debug)]
pub(crate) struct AngleSmoother {
    azimuth: Smoother,
    elevation: Smoother,
    /// Samples of audio each step stands for: 1 for a smoother stepped every
    /// sample, the frame length for one stepped once per frame. Kept so
    /// [`set_sample_rate`](Self::set_sample_rate) can re-derive the coefficient.
    stride: Samples,
}

impl AngleSmoother {
    /// A smoother with a 50 ms time constant of audio at `sample_rate`, for
    /// a caller that steps once per `samples_per_step` samples: one for a
    /// panner that steps every frame (VBAP), the convolution frame length for
    /// one that steps once per frame (HRTF).
    pub(crate) fn new(sample_rate: SampleRate, samples_per_step: Samples) -> Self {
        let coeff = Self::coeff(sample_rate, samples_per_step);
        Self {
            azimuth: Smoother::new(coeff),
            elevation: Smoother::new(coeff),
            stride: samples_per_step,
        }
    }

    /// The fraction of the gap one step closes: the one-pole coefficient at
    /// the rate the smoother is stepped, `sr / stride`
    /// ([`Seconds::one_pole_coeff`]).
    fn coeff(sample_rate: SampleRate, stride: Samples) -> f32 {
        SMOOTH_TIME.one_pole_coeff(SampleRate(sample_rate.get() / stride.get() as f64))
    }

    /// Retune for a new sample rate, keeping the stride
    /// [`new`](Self::new) was given, so the 50 ms ramp holds at any rate.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        let coeff = Self::coeff(sample_rate, self.stride);
        self.azimuth.set_coeff(coeff);
        self.elevation.set_coeff(coeff);
    }

    /// Discard the in-flight ramp, seating both coordinates *on* the commanded
    /// position.
    ///
    /// This is what a panner's [`Node::reset`] clears here: the interpolation
    /// position is the smoother's only runtime state, and the commanded bearing
    /// and height are caller-set configuration a reset must leave alone. Seating
    /// on the target rather than at zero is the difference between resuming
    /// silently and sweeping the source in from front-centre over the ramp.
    ///
    /// [`Node::reset`]: tutti_graph::Node::reset
    pub(crate) fn reset_to_target(&mut self, target: Direction) {
        self.azimuth.seed(target.azimuth.wrap().get());
        self.elevation
            .seed(Elevation::new_clamped(target.elevation.get()).get());
    }

    /// Where the ramp currently is: the direction the last step returned (or
    /// the seeded one), without advancing.
    ///
    /// A block-rate consumer needs this as the *start* of a block: the VBAP
    /// panner solves once per block and ramps from the gains here to the gains
    /// at the block's last frame.
    #[inline]
    pub(crate) fn current(&self) -> Direction {
        Direction::new(
            Azimuth(self.azimuth.value()),
            Elevation(self.elevation.value()),
        )
    }

    /// Advance one step toward the target and return the smoothed direction.
    ///
    /// The bearing takes the short way around: a plain `target - value` is
    /// -340 degrees from 170 to -170, which is 20 degrees to travel.
    /// [`Azimuth::shortest_arc_to`] is the correct difference, and the running
    /// value is re-wrapped each step so it never drifts out of `-180..180`.
    #[inline]
    pub(crate) fn step(&mut self, target: Direction) -> Direction {
        let current = Azimuth(self.azimuth.value()).wrap();
        let arc = current.shortest_arc_to(target.azimuth.wrap());
        let next = current.rotate_by(arc * self.azimuth.coeff());
        self.azimuth.seed(next.get());
        Direction::new(next, Elevation(self.elevation.step(target.elevation.get())))
    }

    /// `step` `steps` times toward the same target, in one move:
    /// [`Smoother::advance`], the analytic
    /// `v + (target - v)(1 - (1 - coeff)^steps)` instead of the loop.
    ///
    /// Its error bound is [`Smoother::advance`]'s, `1.5 * 2^-24 * R *
    /// min(steps, 1/coeff)` for a range `R` (0.04 degrees of bearing at
    /// 48 kHz, where `1/coeff` is 2400 steps). The short arc to a target does
    /// not change direction as the value closes on it, so one arc computation
    /// stands for all the steps of the bearing.
    #[inline]
    pub(crate) fn advance(&mut self, target: Direction, steps: usize) -> Direction {
        let azimuth = if steps == 0 {
            Azimuth(self.azimuth.value())
        } else {
            let current = Azimuth(self.azimuth.value()).wrap();
            let arc = current.shortest_arc_to(target.azimuth.wrap());
            let next = current.rotate_by(arc * self.azimuth.gap_closed(steps));
            self.azimuth.seed(next.get());
            next
        };
        Direction::new(
            azimuth,
            Elevation(self.elevation.advance(target.elevation.get(), steps)),
        )
    }
}

#[cfg(test)]
mod tests;

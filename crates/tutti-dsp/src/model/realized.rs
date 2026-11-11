//! [`Realized`]: the one realization of every effect (design 024 section 4.3a,
//! amendment B1), over a [`Kernel`] that holds what differs between effects.
//!
//! A realization does four things: it starts from a design, aims at a new model,
//! runs frames with the coefficients it holds and clears. What differs from one
//! effect to the next is the per-frame math, the state, and how the coefficients
//! approach a new model ([`Glide`], in a number of steps). The family is its
//! [`Kernel`]; the realization is written once.

use core::ops::Range;

use tutti_types::Samples;

use super::{Effect, Effort, Realization, Setup};

impl Lerp for f32 {
    #[inline]
    fn lerp(&self, to: &f32, k: u32, n: u32) -> f32 {
        self + (to - self) * (k as f32 / n as f32)
    }
}

/// A coefficient set that can be blended toward another.
pub trait Lerp: Copy + PartialEq + core::fmt::Debug {
    /// The set `k` of `n` steps of the way from `self` to `to`, computed from the
    /// two ends and the step, never accumulated, so step `n` is `to` to the bit.
    /// Each set computes its own `k / n`, in the precision it runs in.
    #[must_use]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self;
}

/// How many steps a glide takes: a number, or from how far it goes at a rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Steps {
    /// This many (`Fixed(0)` lands at once).
    Fixed(u32),
    /// No faster than this much a step: `ceil(distance / rate)` steps, so a length
    /// whose sweep is a pitch shift has its speed bounded whatever the distance.
    Rate(f32),
}

/// A linear approach to a target in `n` steps, from where it was when it was aimed.
///
/// Step `k` is `from.lerp(to, k, n)`, so it lands exactly and a new aim starts from
/// the value in flight, never jumping over a glide cut short. `n = 0` lands at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glide<T: Lerp> {
    from: T,
    to: T,
    k: u32,
    n: u32,
}

impl<T: Lerp> Glide<T> {
    /// A glide at rest on `value`.
    #[must_use]
    pub fn landed(value: T) -> Self {
        Self {
            from: value,
            to: value,
            k: 0,
            n: 0,
        }
    }

    /// Aims at `to`, arriving in `n` steps from the value in flight.
    pub fn aim(&mut self, to: T, n: u32) {
        *self = Self {
            from: self.now(),
            to,
            k: 0,
            n,
        };
    }

    /// The value it is moving to.
    #[must_use]
    pub fn target(&self) -> T {
        self.to
    }

    /// Whether steps remain: the last one has not been taken.
    #[must_use]
    pub fn moving(&self) -> bool {
        self.k < self.n
    }

    /// The value at the current step.
    #[inline]
    #[must_use]
    pub fn now(&self) -> T {
        if self.k >= self.n {
            self.to
        } else {
            self.from.lerp(&self.to, self.k, self.n)
        }
    }
}

impl Glide<f32> {
    /// Aims at `to` in `steps`. A target already aimed at leaves the glide as it is
    /// (a model sent again every block does not stretch it).
    pub fn aim_by(&mut self, to: f32, steps: Steps) {
        if to == self.to {
            return;
        }
        let n = match steps {
            Steps::Fixed(n) => n,
            Steps::Rate(rate) => ((to - self.now()).abs() / rate).ceil() as u32,
        };
        self.aim(to, n);
    }
}

/// A coefficient set that approaches a target, a step at a time.
pub trait Approach {
    /// Moves one step toward the target. `false` once it is there (or was
    /// already), when nothing moved and [`Realized`] runs the rest of a block whole.
    fn step(&mut self) -> bool;

    /// Lands on the target now.
    fn settle(&mut self);
}

impl<T: Lerp> Approach for Glide<T> {
    #[inline]
    fn step(&mut self) -> bool {
        if self.k >= self.n {
            return false;
        }
        if self.from == self.to {
            self.k = self.n;
            return false;
        }
        self.k += 1;
        true
    }

    fn settle(&mut self) {
        self.from = self.to;
        self.k = self.n;
    }
}

/// An effect's per-frame math, whatever its channel shape and its state.
pub trait Kernel: Clone + core::fmt::Debug + Send + 'static {
    /// The model it realizes.
    type Model;
    /// The worst-case sizes it allocates from.
    type Bounds;
    /// What approaches a new model.
    type Coeffs: Approach + Clone + core::fmt::Debug + Send;
    /// What a block changes.
    type State: Clone + core::fmt::Debug + Send;

    /// Frames run with one coefficient step while the coefficients move.
    const STEP: usize;

    /// The coefficients and state of an unprepared realization, at rest on `model`
    /// (the design's blank). Control side; allocates.
    fn start(&self, model: &Self::Model) -> (Self::Coeffs, Self::State);

    /// Sizes `coeffs` and `state` for `setup` from the worst case `bounds`, and keeps
    /// what the kernel itself needs of them. Control side; allocates. A kernel that
    /// allocates nothing from [`Bounds`](Self::Bounds) leaves the default.
    fn prepare(
        &mut self,
        _bounds: &Self::Bounds,
        _setup: &Setup,
        _coeffs: &mut Self::Coeffs,
        _state: &mut Self::State,
    ) {
    }

    /// Aims `coeffs` at `model`, in place; audio thread, no allocation.
    fn aim(&self, coeffs: &mut Self::Coeffs, model: &Self::Model);

    /// Clears the state to silence.
    fn clear(&self, state: &mut Self::State);

    /// Renders `frames` of `input` into `output` with the coefficients as they
    /// stand: one step's worth while they move, the rest of a block once landed.
    /// `into` is how many frames of the current step came before `frames.start`
    /// (always 0 for a `STEP` of 1), so a kernel that works once a step knows where
    /// the steps begin on the stream.
    fn run(
        &self,
        coeffs: &Self::Coeffs,
        state: &mut Self::State,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        into: usize,
    );

    /// The processing latency, in frames.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// How costly it is to run.
    fn effort(&self) -> Effort {
        Effort::Heavy
    }
}

/// A deliberate flaw the contract's mutation records plant in a realization (design
/// 024 section 6.1): test-only.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Flaw {
    #[default]
    None,
    /// `reset` clears the state and leaves the coefficients where they were.
    ResetKeepsGlides,
    /// `process` starts its step count over at every call, so a glide's clock is the
    /// block's, not the stream's.
    RestartClock,
}

/// The one realization: the kernel's coefficients approaching each new model, run a
/// step at a time while they move and in one run once they have landed.
///
/// The steps are counted from the first frame since the last [`reset`](Realization::reset)
/// (or the first [`retarget`](Realization::retarget) after [`prepare`](Realization::prepare)),
/// not from a block's edge, so the render does not depend on how the input is cut
/// into blocks. That first retarget lands whole, with nothing to approach from;
/// `reset` lands and clears.
#[derive(Clone, Debug)]
pub struct Realized<K: Kernel> {
    kernel: K,
    coeffs: K::Coeffs,
    state: K::State,
    /// Whether a model has been retargeted since `prepare`.
    landed: bool,
    /// Frames run of the current step.
    into: usize,
    #[cfg(test)]
    flaw: Flaw,
}

impl<K: Kernel> Realized<K> {
    /// A realization of `kernel`, at rest on `model`. Control side; allocates.
    #[must_use]
    pub fn new(kernel: K, model: &K::Model) -> Self {
        let (coeffs, state) = kernel.start(model);
        Self {
            kernel,
            coeffs,
            state,
            landed: false,
            into: 0,
            #[cfg(test)]
            flaw: Flaw::None,
        }
    }

    /// Plants `flaw` (for the contract's mutation records).
    #[cfg(test)]
    pub(crate) fn flawed(&mut self, flaw: Flaw) {
        self.flaw = flaw;
    }

    /// The kernel, its coefficients and its state, for the tests.
    #[cfg(test)]
    pub(crate) fn parts_mut(&mut self) -> (&K, &mut K::Coeffs, &mut K::State) {
        (&self.kernel, &mut self.coeffs, &mut self.state)
    }
}

impl<K: Kernel> Realization<K::Model> for Realized<K> {
    type Bounds = K::Bounds;

    fn prepare(&mut self, bounds: &K::Bounds, setup: &Setup) {
        self.kernel
            .prepare(bounds, setup, &mut self.coeffs, &mut self.state);
        self.landed = false;
        self.into = 0;
    }

    fn retarget(&mut self, model: &K::Model) {
        self.kernel.aim(&mut self.coeffs, model);
        if !self.landed {
            self.coeffs.settle();
            self.landed = true;
            self.into = 0;
        }
    }

    fn reset(&mut self) {
        #[cfg(test)]
        if self.flaw != Flaw::ResetKeepsGlides {
            self.coeffs.settle();
        }
        #[cfg(not(test))]
        self.coeffs.settle();
        self.kernel.clear(&mut self.state);
        self.into = 0;
    }

    fn latency(&self) -> Samples {
        self.kernel.latency()
    }

    fn effort(&self) -> Effort {
        self.kernel.effort()
    }
}

impl<K: Kernel> Effect<K::Model> for Realized<K> {
    /// Runs a step at a time while the coefficients move, so a glide cut by the end
    /// of a call goes on in the next, and the rest of the block in one run.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        #[cfg(test)]
        if self.flaw == Flaw::RestartClock {
            self.into = 0;
        }
        let len = input[0].len();
        let mut at = 0;
        while at < len {
            if self.into == 0 && !self.coeffs.step() {
                break;
            }
            let end = (at + K::STEP - self.into).min(len);
            self.run(input, output, at..end);
            at = end;
        }
        if at < len {
            self.run(input, output, at..len);
        }
    }
}

impl<K: Kernel> Realized<K> {
    /// Runs `frames` and moves the step count on by them.
    fn run(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], frames: Range<usize>) {
        let n = frames.len();
        self.kernel.run(
            &self.coeffs,
            &mut self.state,
            input,
            output,
            frames,
            self.into,
        );
        self.into = (self.into + n) % K::STEP;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A glide by a rate takes `ceil(distance / rate)` steps from where it is, lands
    /// on the target exactly, and is not restarted by an aim at the target it already
    /// has; `Fixed(0)` lands at once.
    ///
    /// Mutation (run): `floor` for `ceil` in `aim_by` makes the 2.5-over-1 glide 2
    /// steps and fails the first count; dropping the `to == self.to` guard restarts
    /// the glide at the repeated aim and fails the second.
    #[test]
    fn a_rate_is_a_number_of_steps_from_the_distance() {
        let count = |g: &mut Glide<f32>| (0..100).take_while(|_| g.step()).count();
        let mut g = Glide::landed(0.0f32);
        g.aim_by(2.5, Steps::Rate(1.0));
        assert_eq!(count(&mut g), 3);
        assert_eq!(g.now(), 2.5);
        // The same target again, mid-flight, leaves the glide where it was: four
        // of its five steps remain.
        g.aim_by(0.0, Steps::Fixed(5));
        assert!(g.step());
        g.aim_by(0.0, Steps::Fixed(5));
        assert_eq!(count(&mut g), 4);
        g.aim_by(1.5, Steps::Fixed(0));
        assert_eq!((g.now(), g.step()), (1.5, false));
        // An exact multiple takes exactly that many steps.
        let mut h = Glide::landed(1.0f32);
        h.aim_by(5.0, Steps::Rate(2.0));
        assert_eq!(count(&mut h), 2);
    }
}

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

/// A coefficient set that can be blended toward another.
pub trait Lerp: Copy + PartialEq + core::fmt::Debug {
    /// The set `k` of `n` steps of the way from `self` to `to`, computed from the
    /// two ends and the step, never accumulated, so step `n` is `to` to the bit.
    /// Each set computes its own `k / n`, in the precision it runs in.
    #[must_use]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self;
}

/// A linear approach to a target in `n` steps, from where it was when it was aimed.
///
/// Step `k` is `from.lerp(to, k, n)`, so it lands exactly and a new aim starts from
/// the value in flight, never jumping over a glide cut short. `n = 0` lands at once.
/// (A step count from a rate, for a delay length whose sweep is a pitch shift, is
/// the delay's lane: `Steps::Rate`, design 024 step 5.)
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

    /// Aims `coeffs` at `model`, in place; audio thread, no allocation.
    fn aim(&self, coeffs: &mut Self::Coeffs, model: &Self::Model);

    /// Clears the state to silence.
    fn clear(&self, state: &mut Self::State);

    /// Renders `frames` of `input` into `output` with the coefficients as they
    /// stand: one step's worth while they move, the rest of a block once landed.
    fn run(
        &self,
        coeffs: &Self::Coeffs,
        state: &mut Self::State,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: Range<usize>,
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

/// The one realization: the kernel's coefficients approaching each new model, run a
/// step at a time while they move and in one run once they have landed.
///
/// The first [`retarget`](Realization::retarget) after [`prepare`](Realization::prepare)
/// lands whole, with nothing to approach from; [`reset`](Realization::reset) lands
/// and clears.
#[derive(Clone, Debug)]
pub struct Realized<K: Kernel> {
    kernel: K,
    coeffs: K::Coeffs,
    state: K::State,
    /// Whether a model has been retargeted since `prepare`.
    landed: bool,
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
        }
    }
}

impl<K: Kernel> Realization<K::Model> for Realized<K> {
    type Bounds = K::Bounds;

    fn prepare(&mut self, _: &K::Bounds, _: &Setup) {
        self.landed = false;
    }

    fn retarget(&mut self, model: &K::Model) {
        self.kernel.aim(&mut self.coeffs, model);
        if !self.landed {
            self.coeffs.settle();
            self.landed = true;
        }
    }

    fn reset(&mut self) {
        self.coeffs.settle();
        self.kernel.clear(&mut self.state);
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
        let len = input[0].len();
        let mut at = 0;
        while at < len && self.coeffs.step() {
            let end = (at + K::STEP).min(len);
            self.kernel
                .run(&self.coeffs, &mut self.state, input, output, at..end);
            at = end;
        }
        if at < len {
            self.kernel
                .run(&self.coeffs, &mut self.state, input, output, at..len);
        }
    }
}

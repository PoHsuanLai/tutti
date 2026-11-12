//! [`Realized`]: the one runner of every effect (design 024 section 2.7, amendments
//! C), over a [`Recurrence`] that holds what differs between effects.
//!
//! A processor is maps. The family is φ
//! ([`Family::member_into`](crate::Family::member_into)); the recurrence's
//! [`coefficients_into`](Recurrence::coefficients_into) is κ, from a member of the
//! family to the coefficient point it asks for; [`Approach`] is γ, the glide of the
//! coefficients in flight toward that point; [`Recurrence::run`] is K, the per-frame
//! math over the coefficients as they stand; and [`Realized`] is the one runner that
//! composes them, written once.

use core::ops::Range;

use tutti_types::Samples;

use super::{Capacity, Effort, Setup};

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

/// A linear approach to a target in `n` steps, from where it was when it was aimed:
/// γ for one coefficient set `T` that glides as a whole, with one rate.
///
/// Step `k` is `from.lerp(to, k, n)`, so it lands exactly and a new aim starts from
/// the value in flight, never jumping over a glide cut short. `n = 0` lands at once.
/// The number of steps a [`Approach::aim`] takes is the glide's own, set where it is
/// built ([`by`](Self::by)); [`aim_by`](Glide::aim_by) of a `Glide<f32>` takes a
/// [`Steps`] per call instead, for a coefficient with a rate of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glide<T: Lerp> {
    from: T,
    to: T,
    k: u32,
    n: u32,
    /// The steps [`Approach::aim`] takes.
    steps: u32,
}

impl<T: Lerp> Glide<T> {
    /// A glide at rest on `value`, whose [`Approach::aim`] lands at once (see
    /// [`by`](Self::by)).
    #[must_use]
    pub fn landed(value: T) -> Self {
        Self {
            from: value,
            to: value,
            k: 0,
            n: 0,
            steps: 0,
        }
    }

    /// The same glide, taking `steps` steps to each target it is [aimed](Approach::aim) at.
    #[must_use]
    pub fn by(self, steps: u32) -> Self {
        Self { steps, ..self }
    }

    /// Aims at `to`, arriving in `n` steps from the value in flight.
    pub fn aim_in(&mut self, to: T, n: u32) {
        *self = Self {
            from: self.now(),
            to,
            k: 0,
            n,
            steps: self.steps,
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
    /// (a member sent again every block does not stretch it).
    pub fn aim_by(&mut self, to: f32, steps: Steps) {
        if to == self.to {
            return;
        }
        let n = match steps {
            Steps::Fixed(n) => n,
            Steps::Rate(rate) => ((to - self.now()).abs() / rate).ceil() as u32,
        };
        self.aim_in(to, n);
    }
}

/// γ: a coefficient set that approaches the point κ asked for, a step at a time.
pub trait Approach {
    /// The coefficient point it is aimed at: what
    /// [`Recurrence::coefficients_into`] writes.
    type Target;

    /// Aims at `target`, from where the coefficients are now, with the rate policy
    /// of its own (a number of steps, or a rate for each coefficient). Audio thread:
    /// no allocation.
    fn aim(&mut self, target: &Self::Target);

    /// Moves one step toward the target. `false` once it is there (or was
    /// already), when nothing moved and [`Realized`] runs the rest of a block whole.
    fn step(&mut self) -> bool;

    /// Lands on the target now.
    fn settle(&mut self);
}

impl<T: Lerp> Approach for Glide<T> {
    type Target = T;

    fn aim(&mut self, target: &T) {
        self.aim_in(*target, self.steps);
    }

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

/// K: an effect's recurrence, the per-frame math over its coefficients and state,
/// whatever its channel shape (design 024 section 2.7).
///
/// A recurrence says four things. What coefficient point a member of the family asks
/// for ([`coefficients_into`](Self::coefficients_into), κ); how the coefficients move
/// to it ([`Coeffs`](Self::Coeffs), an [`Approach`], γ); what a frame does to the
/// state at the coefficients as they stand ([`run`](Self::run), K itself); and the
/// way to its first state, s₀ ([`start`](Self::start), [`prepare`](Self::prepare),
/// [`clear`](Self::clear)). [`Realized`] runs them.
pub trait Recurrence: Clone + core::fmt::Debug + Send + 'static {
    /// The member of the family it realizes.
    type Member;
    /// The worst-case sizes it allocates from ([`Grows::reach`](crate::Grows::reach)):
    /// `()` for a recurrence whose state does not grow.
    type Capacity: Capacity;
    /// The coefficient point a member asks for: what κ writes and γ is aimed at.
    type Target: Clone + core::fmt::Debug + Send;
    /// What approaches a target.
    type Coeffs: Approach<Target = Self::Target> + Clone + core::fmt::Debug + Send;
    /// What a block changes.
    type State: Clone + core::fmt::Debug + Send;

    /// Frames run with one coefficient step while the coefficients move.
    const STEP: usize;

    /// The coefficients, state and target of an unprepared runner, at rest on `member`
    /// (the family's blank). Control side; allocates.
    fn start(&self, member: &Self::Member) -> Start<Self>;

    /// Sizes `coeffs`, `state` and `target` for `setup` from the worst case `capacity`,
    /// and keeps what the recurrence itself needs of them. Control side; allocates.
    /// A recurrence that allocates nothing from [`Capacity`](Self::Capacity) leaves the
    /// default.
    fn prepare(
        &mut self,
        _capacity: &Self::Capacity,
        _setup: &Setup,
        _coeffs: &mut Self::Coeffs,
        _state: &mut Self::State,
        _target: &mut Self::Target,
    ) {
    }

    /// κ: writes the coefficient point `member` asks for into `target`, in place.
    /// Audio thread, no allocation. `false` for a member this recurrence cannot run,
    /// and then the target is as it was and the runner keeps the current aim.
    fn coefficients_into(&self, member: &Self::Member, target: &mut Self::Target) -> bool;

    /// Clears the state to silence.
    fn clear(&self, state: &mut Self::State);

    /// Renders `frames` of `input` into `output` with the coefficients as they
    /// stand: one step's worth while they move, the rest of a block once landed.
    /// `into` is how many frames of the current step came before `frames.start`
    /// (always 0 for a `STEP` of 1), so a recurrence that works once a step knows
    /// where the steps begin on the stream.
    fn run(
        &self,
        coeffs: &Self::Coeffs,
        state: &mut Self::State,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        into: usize,
    );

    /// The processing latency, in frames: never a pre-delay or a tap spacing
    /// (musical delay is not latency). Zero for a time-domain recurrence.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// How costly it is to run.
    fn effort(&self) -> Effort {
        Effort::Heavy
    }
}

/// A recurrence at rest, as [`Recurrence::start`] hands it to the runner: s₀ with the
/// coefficients it starts on and the target they are aimed at.
#[derive(Clone, Debug)]
pub struct Start<R: Recurrence> {
    /// γ's state, landed.
    pub coeffs: R::Coeffs,
    /// s₀.
    pub state: R::State,
    /// The coefficient point the coefficients rest on.
    pub target: R::Target,
}

/// A deliberate flaw the contract's mutation records plant in a runner (design 024
/// section 6.1): test-only.
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
    /// `fresh` keeps the state.
    FreshKeepsState,
}

/// The one runner: a [`Recurrence`]'s coefficients approaching the target κ asks of
/// each new member, run a step at a time while they move and in one run once they
/// have landed.
///
/// The steps are counted from the first frame since the last [`reset`](Self::reset)
/// (or the first [`retarget`](Self::retarget) after [`prepare`](Self::prepare)),
/// not from a block's edge, so the render does not depend on how the input is cut
/// into blocks. That first retarget lands whole, with nothing to approach from;
/// `reset` lands and clears.
///
/// Nothing is allocated after [`prepare`](Self::prepare): everything is sized there
/// from the [`Capacity`](Recurrence::Capacity), and `retarget`, `process` and `reset`
/// never allocate, lock or wait.
#[derive(Clone, Debug)]
pub struct Realized<R: Recurrence> {
    recurrence: R,
    coeffs: R::Coeffs,
    state: R::State,
    /// The coefficient point κ last wrote.
    target: R::Target,
    /// Whether a member has been retargeted since `prepare`.
    landed: bool,
    /// Frames run of the current step.
    into: usize,
    #[cfg(test)]
    flaw: Flaw,
}

impl<R: Recurrence> Realized<R> {
    /// A runner of `recurrence`, at rest on `member`. Control side; allocates.
    #[must_use]
    pub fn new(recurrence: R, member: &R::Member) -> Self {
        let Start {
            coeffs,
            state,
            target,
        } = recurrence.start(member);
        Self {
            recurrence,
            coeffs,
            state,
            target,
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

    /// The recurrence, its coefficients and its state, for the tests.
    #[cfg(test)]
    pub(crate) fn parts_mut(&mut self) -> (&R, &mut R::Coeffs, &mut R::State) {
        (&self.recurrence, &mut self.coeffs, &mut self.state)
    }

    /// Sizes the runner for `setup`, from the worst case `capacity`. Control side;
    /// allocates. A runner never inspects a member to allocate.
    pub fn prepare(&mut self, capacity: &R::Capacity, setup: &Setup) {
        self.recurrence.prepare(
            capacity,
            setup,
            &mut self.coeffs,
            &mut self.state,
            &mut self.target,
        );
        self.landed = false;
        self.into = 0;
    }

    /// Moves toward `member`, once per block whose member changed: κ writes the target
    /// it asks for and γ is aimed at it. Audio thread: never allocates or waits. A
    /// member the recurrence cannot run leaves the aim as it was.
    pub fn retarget(&mut self, member: &R::Member) {
        if self.recurrence.coefficients_into(member, &mut self.target) {
            self.coeffs.aim(&self.target);
        }
        if !self.landed {
            self.coeffs.settle();
            self.landed = true;
            self.into = 0;
        }
    }

    /// Returns the runner to silence, keeping what it was retargeted to.
    pub fn reset(&mut self) {
        #[cfg(test)]
        if self.flaw != Flaw::ResetKeepsGlides {
            self.coeffs.settle();
        }
        #[cfg(not(test))]
        self.coeffs.settle();
        self.recurrence.clear(&mut self.state);
        self.into = 0;
    }

    /// The processing latency, in frames: never a pre-delay or a tap spacing
    /// (musical delay is not latency).
    #[must_use]
    pub fn latency(&self) -> Samples {
        self.recurrence.latency()
    }

    /// How costly the runner is to run.
    #[must_use]
    pub fn effort(&self) -> Effort {
        self.recurrence.effort()
    }

    /// A runner of the same type, prepared for the same setup and retargeted to the
    /// same member, with empty state: what a fork runs. A clone, reset. Control
    /// side; allocates.
    #[must_use]
    pub fn fresh(&self) -> Self {
        #[cfg(test)]
        if self.flaw == Flaw::FreshKeepsState {
            return self.clone();
        }
        let mut fresh = self.clone();
        fresh.reset();
        fresh
    }

    /// Renders `frames` frames of `output` from `input`, planar slices at least that
    /// long, at most [`Setup::max_block`]. Audio thread. A reverb overwrites the
    /// output with the **wet** signal alone: mixing is the node's.
    ///
    /// Runs a step at a time while the coefficients move, so a glide cut by the end
    /// of a call goes on in the next, and the rest of the block in one run.
    pub fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], frames: usize) {
        #[cfg(test)]
        if self.flaw == Flaw::RestartClock {
            self.into = 0;
        }
        let step = R::STEP;
        let mut at = 0;
        while at < frames {
            if self.into == 0 && !self.coeffs.step() {
                break;
            }
            let end = (at + step - self.into).min(frames);
            self.run(input, output, at..end);
            at = end;
        }
        if at < frames {
            self.run(input, output, at..frames);
        }
    }

    /// Runs `frames` and moves the step count on by them.
    fn run(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], frames: Range<usize>) {
        let n = frames.len();
        self.recurrence.run(
            &self.coeffs,
            &mut self.state,
            input,
            output,
            frames,
            self.into,
        );
        self.into = (self.into + n) % R::STEP;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A glide by a rate takes `ceil(distance / rate)` steps from where it is, lands
    /// on the target exactly, and is not restarted by an aim at the target it already
    /// has; `Fixed(0)` lands at once.
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

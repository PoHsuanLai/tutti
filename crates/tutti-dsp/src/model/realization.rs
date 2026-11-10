//! [`Realization`]: what runs a model, [`Effect`], its block shape, and the
//! [`Setup`] it is prepared for.

use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

/// What a realization is prepared for: the rate, the longest block and the widths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setup {
    /// The rate every block runs at.
    pub rate: SampleRate,
    /// The longest block [`Effect::process`] is handed.
    pub max_block: MaxBlock,
    /// The input width.
    pub input: ChannelLayout,
    /// The output width.
    pub output: ChannelLayout,
}

/// How costly a realization is to run, in the three classes a graph schedules by.
///
/// `Cost` is `tutti-graph`'s and this crate must not depend on it, so the node
/// maps these with one `match`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effort {
    /// Cheaper than handing the block to another thread is worth.
    Light,
    /// A few tens of nanoseconds a frame and channel: worth a thread only in a
    /// wide graph.
    Moderate,
    /// Worth a thread of its own. What a realization that says nothing is.
    Heavy,
}

/// One realization of the model `M` (design 024 section 4.3): a state-space
/// structure that computes its input-output map.
///
/// The trait is generic over the model, not an associated type of the design:
/// **one model, several realizations**. The model is the equivalence class and a
/// realization is one member of it, so a second design with the same model
/// shares a realization, and the contract of a model's class runs over each.
/// [`retarget`](Self::retarget) is the one place the model and the realization meet:
/// it reads what the realization realises of the model (coefficients, lengths,
/// gains) and sets the targets it ramps or slews to.
///
/// Nothing is allocated after [`prepare`](Self::prepare): everything is sized there
/// from the [`Bounds`](Self::Bounds), and `retarget` and `process` never
/// allocate, lock or wait.
pub trait Realization<M>: Send + Sized {
    /// The worst-case sizes [`prepare`](Self::prepare) allocates from: the
    /// design's own [`Design::Bounds`](crate::Design::Bounds), which the realization
    /// names here so the trait carries no engineering type of its own.
    type Bounds;

    /// Sizes the realization for `setup`, from the worst case `bounds`. Control
    /// side; allocates. A realization never inspects a model to allocate.
    fn prepare(&mut self, bounds: &Self::Bounds, setup: &Setup);

    /// Moves toward `model`, once per block whose model changed. Audio thread:
    /// never allocates or waits. A model with nothing the realization understands
    /// leaves it as it was.
    fn retarget(&mut self, model: &M);

    /// Returns the realization to silence, keeping what it was retargeted to.
    fn reset(&mut self);

    /// The processing latency, in frames: never a pre-delay or a tap spacing
    /// (musical delay is not latency). Zero for a time-domain realization.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// How costly the realization is to run.
    fn effort(&self) -> Effort {
        Effort::Heavy
    }

    /// A realization of the same type, prepared for the same setup and retargeted to
    /// the same model, with empty state: what a fork runs. Control side;
    /// allocates.
    #[must_use]
    fn fresh(&self) -> Self;
}

/// The planar block shape a graph node drives: a realization that renders
/// `output` from `input`.
///
/// A resampler, a stretcher and an oscillator bank are [`Realization`]s with a
/// different shape trait for their block.
pub trait Effect<M>: Realization<M> {
    /// Renders `output` from `input`, planar slices of one length, at most
    /// [`Setup::max_block`]. Audio thread. A reverb overwrites the output with the
    /// **wet** signal alone: mixing is the node's.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]);
}

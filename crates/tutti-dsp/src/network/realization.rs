//! [`Realization`]: what runs a [`Network`], and the [`Setup`] it is prepared for.

use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

use super::{Bounds, Network};

/// What a realization is prepared for: the rate, the longest block and the widths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setup {
    /// The rate every block runs at.
    pub rate: SampleRate,
    /// The longest block [`Realization::process`] is handed.
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

/// What runs the sound of one [`Design`](super::Design) (design 022 section 4).
///
/// A realization is assembled from the pieces its algorithm wants (fused comb
/// loops, fixed-size matrices, SIMD across lines); no engine runs every
/// [`Network`]. The network is the **description** the realization is held to:
/// [`retarget`](Self::retarget) is the one place the two meet, and the realization
/// contract (`realization_contract!`, crate-private, run by this crate's tests)
/// measures a realization's impulse response against the network's own queries
/// (`rt60`, `tail`, `min_delay`), so analysis and runtime cannot drift apart
/// without a failure.
///
/// Nothing is allocated after [`prepare`](Self::prepare): everything is sized
/// there from the [`Bounds`], and `retarget` and `process` never allocate,
/// lock or wait.
pub trait Realization: Send + Sized {
    /// Sizes the realization for `setup`, from the worst case `bounds`. Control
    /// side; allocates. A realization never inspects a network to allocate.
    fn prepare(&mut self, bounds: &Bounds, setup: &Setup);

    /// Moves toward `network`, once per block whose network changed: reads
    /// what the realization realises of it (coefficients, lengths, gains) and sets
    /// the targets it ramps or slews to. Audio thread: never allocates or
    /// waits. A network with nothing the realization understands leaves it as it
    /// was.
    fn retarget(&mut self, network: &Network);

    /// Renders `output` from `input`, planar slices of one length, at most
    /// [`Setup::max_block`]. The output is overwritten with the **wet** signal
    /// alone: none of the input, since mixing is the node's. Audio thread.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]);

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
    /// the same network, with empty state: what a fork runs. Control side;
    /// allocates.
    #[must_use]
    fn fresh(&self) -> Self;
}

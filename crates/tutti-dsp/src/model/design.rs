//! [`Design`]: the pure function from a family's controls to its model, and the
//! engineering that sizes what runs it ([`Sizes`]).

use std::borrow::Borrow;
use std::fmt::Debug;

use tutti_types::{ChannelLayout, Db, SampleRate};

use super::{Cadence, Ceiling, Range, Realization, Ring, System};

/// The channel widths a [`Design`] is made for: what its node's ports and its
/// realization's [`Setup`](super::Setup) are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channels {
    /// The input width.
    pub input: ChannelLayout,
    /// The output width.
    pub output: ChannelLayout,
}

/// The sizes a realization allocates for the worst case of a design: lines, taps,
/// an impulse response's length, a longest delay (design 024 section 4.4).
/// Engineering, not mathematics, so it is the design's and not the model's.
pub trait Sizes: Copy + Debug {
    /// What one set of sizes is measured from: the part of a design's model
    /// that the sizes depend on (the model borrows it).
    type Source;

    /// The sizes of one `source`, its tail measured to `floor`: what
    /// [`Design::bounds`] must hold for every model the design can give.
    fn measure(source: &Self::Source, floor: Db) -> Self;

    /// The longest [`System::tail`] the design can give, at the ceiling's floor:
    /// what the node declares as its tail ceiling.
    fn tail(&self) -> Ring;

    /// Whether these sizes hold `other` in every figure.
    #[must_use]
    fn dominates(&self, other: &Self) -> bool;
}

/// The pure function from a family's controls to the parameters of a system
/// (design 024 section 4.1): its controls, its [`Model`](Self::Model), and the
/// [`Realization`] that runs it.
///
/// The pure half is [`design_into`](Self::design_into) and
/// [`bounds`](Self::bounds): no audio, no state and no graph type. The model is the
/// description the realization is held to ([`Realization::retarget`] reads it, and
/// the contract measures the realization against the model's queries); nothing
/// here forces a runtime shape on the realization.
pub trait Design: Clone + Send {
    /// Plain values in tutti units, one struct per family.
    type Controls: Copy + PartialEq + Send;

    /// The system a set of controls describes.
    type Model: System + Clone + Send + Borrow<<Self::Bounds as Sizes>::Source>;

    /// The worst-case sizes of this design, the figures its realization
    /// allocates from.
    type Bounds: Sizes;

    /// The family's limit on a control ([`Ceiling::range`]).
    type Range: Range;

    /// What runs this design's models.
    type Realization: Realization<Self::Model, Bounds = Self::Bounds>;

    /// How often a node designs and retargets.
    const CADENCE: Cadence = Cadence::Block;

    /// The widths of the signal this design takes and gives.
    fn channels(&self) -> Channels;

    /// An empty model at `rate`: what [`design_into`](Self::design_into) writes
    /// into.
    fn blank(&self, rate: SampleRate) -> Self::Model;

    /// Writes the model for `controls` at `rate` into `out`, replacing all of
    /// it.
    ///
    /// Pure and allocation-free: the same controls give the same model, with
    /// no clock, no state and no heap. It writes **in place** because a reverb
    /// `Network` is about 56 KB, too large to build as a value on an audio
    /// thread (whose stack may be 512 KB); a node designs into a model it owns.
    /// Total: a control outside its range is clamped and a non-finite one takes the
    /// family's default, so the model is always one the family can run.
    fn design_into(&self, controls: &Self::Controls, rate: SampleRate, out: &mut Self::Model);

    /// The model for `controls` at `rate`, as a value: a test, a UI and the
    /// control side call this; an audio thread calls
    /// [`design_into`](Self::design_into).
    fn design(&self, controls: &Self::Controls, rate: SampleRate) -> Self::Model {
        let mut model = self.blank(rate);
        self.design_into(controls, rate, &mut model);
        model
    }

    /// The worst case of [`design_into`](Self::design_into) over the controls'
    /// declared ranges (and the limits in `ceiling`): what a realization
    /// allocates for and what the node declares as its tail ceiling.
    fn bounds(&self, ceiling: &Ceiling<Self::Range>) -> Self::Bounds;

    /// An unprepared realization for this design. Control side; allocates.
    fn realization(&self) -> Self::Realization;
}

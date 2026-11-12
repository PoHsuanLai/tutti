//! [`Design`]: the pure function from a family's controls to its model, and the
//! engineering that sizes what runs it ([`Sizes`]).

use std::borrow::Borrow;
use std::fmt::Debug;

use tutti_types::{BusTopology, Db, SampleRate};

use super::{Cadence, Ceiling, Range, Realized, Recurrence, Ring, System};

/// The buses a [`Design`] is made for: its input, and its outputs, which are one
/// topology repeated `bands` times, band-major (a crossover's), or once (every
/// other design). Its node declares exactly these buses, so the graph's dry path
/// folds the input onto band 0 by speaker, and the runner's outputs are the
/// bands in order: band `b` is the `b`-th run of [`output`](Self::output)'s width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channels {
    input: BusTopology,
    output: BusTopology,
    bands: u8,
}

impl Channels {
    /// One output bus where the input is: a filter's.
    #[must_use]
    pub const fn same(input: BusTopology) -> Self {
        Self::new(input, input)
    }

    /// `input` in, `output` out: a reverb's stereo from a mono centre.
    #[must_use]
    pub const fn new(input: BusTopology, output: BusTopology) -> Self {
        Self {
            input,
            output,
            bands: 1,
        }
    }

    /// The input's speakers once per band, `bands` of them (a crossover's), or
    /// `None` for no band.
    #[must_use]
    pub const fn bands(input: BusTopology, bands: u8) -> Option<Self> {
        if bands == 0 {
            return None;
        }
        Some(Self {
            input,
            output: input,
            bands,
        })
    }

    /// The input bus.
    #[must_use]
    pub const fn input(&self) -> BusTopology {
        self.input
    }

    /// One band of the output.
    #[must_use]
    pub const fn output(&self) -> BusTopology {
        self.output
    }

    /// How many bands the output repeats: 1 but for a split.
    #[must_use]
    pub const fn band_count(&self) -> usize {
        self.bands as usize
    }

    /// The input's channels.
    #[must_use]
    pub const fn input_width(&self) -> usize {
        self.input.layout().count() as usize
    }

    /// Every output channel, all bands.
    #[must_use]
    pub const fn output_width(&self) -> usize {
        self.output.layout().count() as usize * self.bands as usize
    }
}

/// The sizes a recurrence allocates for the worst case of a design: lines, taps,
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
/// [`Recurrence`] that runs it.
///
/// This is φ, the family: controls to a member. The pure half is
/// [`design_into`](Self::design_into) and [`bounds`](Self::bounds): no audio, no
/// state and no graph type. The model is the description the recurrence is held to
/// ([`Recurrence::coefficients_into`], κ, reads it, and the contract measures the
/// render against the model's queries); nothing here forces a runtime shape on the
/// recurrence.
pub trait Design: Clone + Send {
    /// Plain values in tutti units, one struct per family.
    type Controls: Copy + PartialEq + Send;

    /// The system a set of controls describes.
    type Model: System + Clone + Send + Borrow<<Self::Bounds as Sizes>::Source>;

    /// The worst-case sizes of this design, the figures its recurrence
    /// allocates from.
    type Bounds: Sizes;

    /// The family's limit on a control ([`Ceiling::range`]).
    type Range: Range;

    /// What runs this design's models: the recurrence [`Realized`] runs.
    type Recurrence: Recurrence<Model = Self::Model, Bounds = Self::Bounds>;

    /// The grid on which `theta` is sampled, and so whether a render depends
    /// on how audio is split into blocks ([`Cadence::block_independent`]). The
    /// contract's block-independence cases run iff it is.
    const CADENCE: Cadence = Cadence::BLOCK;

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
    /// declared ranges (and the limits in `ceiling`): what a recurrence
    /// allocates for and what the node declares as its tail ceiling.
    fn bounds(&self, ceiling: &Ceiling<Self::Range>) -> Self::Bounds;

    /// An unprepared recurrence for this design. Control side; allocates.
    fn recurrence(&self) -> Self::Recurrence;

    /// An unprepared runner of this design's recurrence, at rest on the design's blank
    /// model. Control side; allocates.
    fn realized(&self) -> Realized<Self::Recurrence> {
        Realized::new(self.recurrence(), &self.blank(SampleRate::DEFAULT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A split's outputs are its input's speakers once per band, so its width
    /// is the input's times the bands; any other design has one output bus.
    #[test]
    fn a_split_repeats_its_input_once_per_band() {
        let stereo = BusTopology::STEREO;
        let split = Channels::bands(stereo, 3).expect("three bands");
        assert_eq!((split.output(), split.band_count()), (stereo, 3));
        assert_eq!((split.input_width(), split.output_width()), (2, 6));
        assert_eq!(Channels::bands(stereo, 0), None);
        let same = Channels::same(stereo);
        assert_eq!((same.band_count(), same.output_width()), (1, 2));
    }
}

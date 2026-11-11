//! [`Design`]: the pure function from a family's controls to its model, and the
//! engineering that sizes what runs it ([`Sizes`]).

use std::borrow::Borrow;
use std::fmt::Debug;

use tutti_types::{ChannelLayout, Db, SampleRate};

use super::{Cadence, Ceiling, Range, Realization, Ring, System};

/// The channels a [`Design`] is made for: what its node's ports and its
/// realization's [`Setup`](super::Setup) are, and where they sit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channels {
    /// The input width.
    pub input: ChannelLayout,
    /// The output width.
    pub output: ChannelLayout,
    /// Where the channels sit: what a node declares as its main buses, so the
    /// graph's dry path (mix, bypass) folds the input onto the outputs by
    /// speaker. The graph refuses a dry path it would have to guess.
    pub placement: Placement,
}

impl Channels {
    /// Channels in the engine's speaker order for each width
    /// (`ChannelTopology::assumed`): a mono input is a centre, a stereo output a
    /// left and right pair.
    #[must_use]
    pub const fn speakers(input: ChannelLayout, output: ChannelLayout) -> Self {
        Self {
            input,
            output,
            placement: Placement::Speakers,
        }
    }

    /// The input's channels once per band, band-major (output
    /// `band * input + channel`): a crossover's. Band 0 sits where the input
    /// does, the main output; the further bands are not speaker feeds.
    ///
    /// # Panics
    ///
    /// If `input` times `bands` is wider than a layout holds.
    #[must_use]
    pub const fn bands(input: ChannelLayout, bands: u16) -> Self {
        let Some(width) = input.count().checked_mul(bands) else {
            panic!("bands wider than a layout holds");
        };
        Self {
            input,
            output: ChannelLayout::from_count(width),
            placement: Placement::Bands,
        }
    }
}

impl Channels {
    /// The output port of input channel `channel`'s output `output` (its band,
    /// for a crossover): band-major, `output * input + channel`. The realization
    /// writes through it ([`of_port`](Self::of_port) is its inverse), and a node
    /// declares [`main_output`](Self::main_output) from the same order: output
    /// 0's ports are the first `input`, so they are a bus.
    #[must_use]
    pub const fn port(&self, output: usize, channel: usize) -> usize {
        output * self.input.count() as usize + channel
    }

    /// The `(output, channel)` port `port` carries: [`port`](Self::port)'s
    /// inverse.
    #[must_use]
    pub const fn of_port(&self, port: usize) -> (usize, usize) {
        let width = self.input.count() as usize;
        (port / width, port % width)
    }

    /// The width of the main output: every output where the channels sit as
    /// speakers, output 0's ports (`port(0, c)`) for bands.
    #[must_use]
    pub const fn main_output(&self) -> ChannelLayout {
        match self.placement {
            Placement::Speakers => self.output,
            Placement::Bands => self.input,
        }
    }
}

/// Where a design's channels sit (see [`Channels::placement`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The engine's speaker order for each width, where the width has one: a
    /// mono source is a centre, which the graph's fold sends to both sides of a
    /// stereo output (`Fold::between`).
    Speakers,
    /// The output is the input's channels once per band ([`Channels::bands`]):
    /// band 0 is the main output, at the input's speakers.
    Bands,
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
    /// declared ranges (and the limits in `ceiling`): what a realization
    /// allocates for and what the node declares as its tail ceiling.
    fn bounds(&self, ceiling: &Ceiling<Self::Range>) -> Self::Bounds;

    /// An unprepared realization for this design. Control side; allocates.
    fn realization(&self) -> Self::Realization;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Output 0's ports are the main output, and `of_port` inverts `port`**,
    /// for speakers and for bands: what a node declares as its main bus
    /// (`main_output`, a prefix of the ports) is where the realization writes
    /// output 0, so the graph's dry path and the node's band 0 are one place.
    #[test]
    fn output_zeros_ports_are_the_main_output() {
        for channels in [
            Channels::speakers(ChannelLayout::MONO, ChannelLayout::MONO),
            Channels::speakers(ChannelLayout::STEREO, ChannelLayout::STEREO),
            Channels::bands(ChannelLayout::MONO, 3),
            Channels::bands(ChannelLayout::STEREO, 2),
            Channels::bands(ChannelLayout::from_count(6), 4),
        ] {
            let width = usize::from(channels.input.count());
            let main = usize::from(channels.main_output().count());
            for port in 0..usize::from(channels.output.count()) {
                let (output, channel) = channels.of_port(port);
                assert_eq!(channels.port(output, channel), port, "{channels:?}");
                assert!(channel < width, "{channels:?}");
                assert_eq!(output == 0, port < main, "{channels:?}: port {port}");
            }
        }
    }
}

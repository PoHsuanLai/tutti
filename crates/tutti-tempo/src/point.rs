use tutti_types::{Beat, Bpm};

/// What happens from a point to the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Hold this tempo until the next point.
    Step,
    /// Move to the next point's tempo, tempo linear in time.
    ///
    /// This is CLAP's `tempo_inc` (a constant increment per sample), and the
    /// default for import and export.
    RampTime,
    /// Move to the next point's tempo, tempo linear in beats.
    ///
    /// Time is a logarithm of the beat. This is the ramp Ardour draws.
    RampBeats,
}

/// A tempo from `beat` on.
///
/// A plain value: [`TempoMap::new`](crate::TempoMap::new) is where it is
/// checked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoPoint {
    /// Where it starts.
    pub beat: Beat,
    /// The tempo at `beat`.
    pub tempo: Bpm,
    /// How it gets to the next point.
    pub shape: Shape,
}

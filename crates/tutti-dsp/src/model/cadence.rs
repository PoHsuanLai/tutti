//! [`Cadence`]: how often a node samples `theta(t)`.

/// How often a node designs and retargets inside a block (design 024 section
/// 4.5). An engineering choice, not a property of a system.
///
/// Only [`Block`](Self::Block) exists: a family that needs a design every few
/// frames (a filter whose cutoff a lane moves inside a block) adds a variant
/// when its step lands, which is why the enum is `non_exhaustive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cadence {
    /// One design per block whose controls changed.
    Block,
}

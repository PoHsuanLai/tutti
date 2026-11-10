//! [`Cadence`]: how often a node samples `theta(t)`.

use std::num::NonZeroUsize;

/// How often a node designs and retargets inside a block (design 024 section
/// 4.5). An engineering choice, not a property of a system.
///
/// [`Block`](Self::Block) is one design per block, the realization ramping
/// between targets itself (the reverbs). [`Every`](Self::Every) is a design at the
/// end of every segment of at most `n` frames, for a family whose control a lane
/// may move **inside** a block (a filter's cutoff): the node reads the control at
/// the segment's last frame, designs, calls `retarget` and then `process` on the
/// segment's slices, and the realization ramps from the previous target to the new
/// one across that one call, landing on it at the call's last frame. The enum is
/// `non_exhaustive` for the next family that needs another schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cadence {
    /// One design per block whose controls changed.
    Block,
    /// A design at the end of every segment of at most this many frames
    /// (`COEFF_INTERVAL`, 16, for the filters).
    Every(NonZeroUsize),
}

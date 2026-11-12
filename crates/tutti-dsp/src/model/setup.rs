//! The [`Setup`] a runner is prepared for, and the [`Effort`] it declares.

use tutti_types::{MaxBlock, SampleRate};

/// What a runner is prepared for: the rate, and the longest block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setup {
    /// The rate every block runs at.
    pub rate: SampleRate,
    /// The longest block [`Realized::process`](crate::Realized::process) is handed.
    pub max_block: MaxBlock,
}

/// How costly a recurrence is to run, in the three classes a graph schedules by.
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
    /// Worth a thread of its own. What a recurrence that says nothing is.
    Heavy,
}

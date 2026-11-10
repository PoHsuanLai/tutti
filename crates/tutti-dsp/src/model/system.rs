//! [`System`]: what every processor answers, and [`Ring`], how long it rings.

use tutti_types::{Db, Samples, Seconds};

/// How long a system rings: [`System::tail`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ring {
    /// It falls to the floor in this long.
    Finite(Seconds),
    /// It never does: a lossless loop, or one that grows.
    Unbounded,
}

impl Ring {
    /// Whether this ring lasts at least as long as `other`.
    #[must_use]
    pub fn covers(self, other: Ring) -> bool {
        match (self, other) {
            (Ring::Unbounded, _) => true,
            (Ring::Finite(_), Ring::Unbounded) => false,
            (Ring::Finite(a), Ring::Finite(b)) => a.get() >= b.get(),
        }
    }

    /// The finite figure, or `None` for [`Unbounded`](Self::Unbounded).
    #[must_use]
    pub fn seconds(self) -> Option<Seconds> {
        match self {
            Ring::Finite(s) => Some(s),
            Ring::Unbounded => None,
        }
    }
}

/// A causal system from input signals to output signals (design 024 section 4.2):
/// the base of every processor class. It answers where the response to a unit
/// impulse starts and where it effectively ends.
pub trait System {
    /// The processing part of the onset of the impulse response, in frames: a
    /// lookahead or a convolver's partition. **Musical delay (a pre-delay, a delay
    /// time, a tap spacing) is not latency** and is never counted here.
    fn latency(&self) -> Samples;

    /// How long the response rings past its onset, until it is `floor` below
    /// what it carried. Musical delay counts toward it, as a reverb's pre-delay
    /// does. [`Ring::Unbounded`] when it never gets there.
    fn tail(&self, floor: Db) -> Ring;
}

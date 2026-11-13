//! [`Capacity`]: the lattice of sizes a recurrence allocates.

use core::fmt::Debug;

use tutti_types::Db;

use super::Ring;

/// The sizes a recurrence allocates for the worst case of a family: lines, taps,
/// an impulse response's length, a longest delay, a point
/// of the lattice 𝒦.
/// Engineering, not mathematics, so it is the recurrence's and not the member's.
///
/// [`need`](Self::need) is `need : M → 𝒦`, the sizes one member asks for;
/// [`dominates`](Self::dominates) is the order of the lattice. `Grows::reach` must
/// satisfy `need(φ(c)) ≤ reach(D)` for every control `c` in the domain `D`.
pub trait Capacity: Copy + Debug {
    /// What one set of sizes is measured from: the part of a family's member
    /// that the sizes depend on (the member borrows it).
    type Source;

    /// The sizes one `source` needs, its tail measured to `floor`: what
    /// `Grows::reach` must hold for every member the family can give.
    fn need(source: &Self::Source, floor: Db) -> Self;

    /// The longest [`System::tail`](super::System::tail) the family can give, at the
    /// reach's floor: what the node declares as its tail ceiling.
    fn tail(&self) -> Ring;

    /// Whether these sizes hold `other` in every figure.
    #[must_use]
    fn dominates(&self, other: &Self) -> bool;
}

/// A recurrence whose state does not grow: a filter allocates nothing from its
/// controls, so its one capacity holds every member. It rings as long as its Q or
/// resonance says, so the ring-out it bounds is [`Ring::Unbounded`].
impl Capacity for () {
    type Source = ();

    fn need(_: &(), _: Db) -> Self {}

    fn tail(&self) -> Ring {
        Ring::Unbounded
    }

    fn dominates(&self, _: &Self) -> bool {
        true
    }
}

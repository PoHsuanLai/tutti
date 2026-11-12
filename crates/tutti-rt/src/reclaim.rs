//! The control-side sweep of a published cell.

use arc_lend::ArcLend;

/// Something whose retired values a control-thread tick can free.
///
/// The one-sentence boundary: a holder of published cells answers "free what
/// the audio thread no longer holds". [`ArcLend::reclaim`] is its first
/// implementor, and a node's controls (a handle over one or more cells)
/// implement it by sweeping each. **Control thread only**, like `publish` and
/// `ArcLend::reclaim`: never call it from the audio thread. With nothing
/// retired it is a lock and a length check, so a tick can call it every time.
///
/// ```
/// use tutti_rt::{ArcLend, Reclaim};
///
/// let cell = ArcLend::new(1_u32);
/// cell.publish(2);
/// assert_eq!(Reclaim::reclaim(&cell), 0, "no reader holds the first value");
/// ```
pub trait Reclaim {
    /// Frees what has been retired and is no longer held; returns how many
    /// retired values are still pinned afterwards (by a reader that holds one
    /// or might be about to read it).
    fn reclaim(&self) -> usize;
}

impl<T> Reclaim for ArcLend<T> {
    fn reclaim(&self) -> usize {
        Self::reclaim(self)
    }
}

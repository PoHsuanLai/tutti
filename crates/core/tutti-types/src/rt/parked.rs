//! The engine's debug net for a parked [`Loan`](crate::Loan).
//!
//! `arc-lend` reports a stalled reclamation (`retired_len`, `epoch_stalls`)
//! and leaves the policy to its host. This is tutti's policy: a debug build
//! panics, at the publish, once a cell's retired list reaches
//! [`PARKED_LOAN_LIMIT`]. Release builds check nothing.
//!
//! A cell only gets there when loans are held across publishes (CLAUDE.md,
//! "Publishing to the audio thread": read once per block, never park a
//! `Loan`), so the call belongs after the `publish` of a cell the engine
//! republishes often, where a parked loan shows up within a test's run:
//! `tutti-nodes`' `LaneControls::publish_arc` (an automation lane,
//! republished on every program edit) and `tutti-sampler`'s
//! `VoiceNodeHandle::set_placement` (a voice's window, republished on every
//! clip move).

use arc_lend::ArcLend;

/// Retired values a cell may hold back before a debug build calls it a
/// parked [`Loan`](crate::Loan). One block's loans hold back a value or two;
/// this many means a loan has outlived many publishes.
pub const PARKED_LOAN_LIMIT: usize = 1024;

/// Panics, in a debug build, if `cell` holds back [`PARKED_LOAN_LIMIT`] or
/// more retired values. Call it on the control thread, after a `publish`.
#[track_caller]
#[inline]
pub fn debug_check_reclaiming<T, const SLOTS: usize, const EPOCHS: usize>(
    cell: &ArcLend<T, SLOTS, EPOCHS>,
) {
    debug_assert!(
        cell.retired_len() < PARKED_LOAN_LIMIT,
        "{} retired values are waiting on readers: a `Loan` is parked across \
         blocks. Lend once per block and drop the loan before the block ends \
         (CLAUDE.md, \"Publishing to the audio thread\")",
        cell.retired_len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Parks loans in every overflow epoch, so each publish after this one
    /// retires a value nothing can reclaim: the stall a parked `Loan` causes.
    fn park(cell: &ArcLend<u64>) -> Vec<crate::Loan<'_, u64>> {
        let mut parked = Vec::new();
        for round in 0..4 {
            parked.extend((0..16).map(|_| cell.lend()));
            cell.publish(Arc::new(round));
        }
        parked
    }

    /// Parked loans trip the check before the retired list passes the limit.
    ///
    /// Mutation: make `debug_check_reclaiming`'s body empty. The publishes
    /// all succeed and the test fails with "did not panic".
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a `Loan` is parked across blocks")]
    fn parked_loans_trip_the_check() {
        let cell = ArcLend::new(0u64);
        let _parked = park(&cell);
        for v in 0..PARKED_LOAN_LIMIT as u64 {
            cell.publish(Arc::new(v));
            debug_check_reclaiming(&cell);
        }
    }

    /// Loans dropped within their block never trip it, however many
    /// publishes there are.
    ///
    /// Mutation: compare `retired_len() < 1` instead of the limit. A held
    /// loan leaves one retired value at its publish, and the check panics.
    #[test]
    fn loans_dropped_each_block_never_trip_the_check() {
        let cell = ArcLend::new(0u64);
        for v in 0..2 * PARKED_LOAN_LIMIT as u64 {
            let loan = cell.lend();
            cell.publish(Arc::new(v));
            debug_check_reclaiming(&cell);
            drop(loan);
        }
    }
}

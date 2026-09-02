//! Progress accounting for a chunked state transfer.
//!
//! # Why a progress deadline and not a total
//!
//! Plugin state travels as a `StateChunk` sequence, and its size is chosen by
//! the plugin — up to [`MAX_STATE_BYTES`], a gigabyte. A single fixed budget for
//! the whole transfer makes that limit unreachable: streaming a gigabyte through
//! the control socket takes longer than any total short enough to be a useful
//! liveness check, so the cap and the timeout contradict each other and the
//! timeout always wins. The failure then reads as a hung plugin, because a
//! timeout is what a hung plugin produces — a large-but-legal preset is lost
//! under an explanation that blames the wrong component.
//!
//! Scaling a total by the declared size at some floor bandwidth was the other
//! candidate and is worse here for two reasons. The receive direction has no
//! declared size to scale by — `SaveState` learns the total only when the `last`
//! chunk lands, so the budget would have to be picked before the quantity it
//! depends on is known. And a floor bandwidth is a second tunable that is wrong
//! in both directions: too low and a genuinely wedged plugin is waited on for
//! minutes, too high and a slow disk-backed plugin is failed for being slow.
//!
//! A progress deadline needs neither. "No chunk in [`PROGRESS_TIMEOUT`]" is the
//! question worth asking — *is this transfer still moving?* — and it is the same
//! question in both directions, at any size, with no bandwidth assumption. A
//! transfer that advances is allowed to take as long as it takes; one that stops
//! fails one deadline after it stops, not one deadline after it started.
//!
//! # Shape
//!
//! The bridge thread owns the socket and the caller owns the wait, so progress
//! has to cross threads. [`StateProgress`] is that seam: the bridge thread calls
//! [`StateProgress::advance`] as each chunk lands, and the caller polls
//! [`StateProgress::wait`], which returns only when the reply arrives or when
//! the counter has stood still for the deadline.
//!
//! It carries the **deadline** as well as the counter, so both halves of a
//! transfer run on one figure. They did not, briefly, and the gap was not
//! cosmetic: the caller's wait was injectable while the bridge thread's reads
//! stayed on the constant, so a shortened test deadline exercised a timing
//! relationship production never has — and the bridge thread's own expiry, the
//! one that actually fires first, went untested.
//!
//! # A stall is not a disconnection
//!
//! `pump` treats every error out of `dispatch::handle` as connection-level and
//! calls `crash()`. A state deadline expiring is not that: the socket is
//! synchronised, nothing is malformed, and the peer has merely not spoken yet.
//! So the bridge thread reports a stall *to the caller* and stays alive, the way
//! the over-limit `LoadState` branch already does. Getting this wrong produced a
//! self-contradiction a caller could not defend against — [`StateError::Stalled`]
//! promises a healthy session and a reasonable retry, while the crash it rode in
//! on had already destroyed the session it would retry against.
//!
//! [`MAX_STATE_BYTES`]: crate::protocol::MAX_STATE_BYTES

use super::ask::Ask;
use crate::error::StateError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long a state transfer may make no progress before it is declared stalled.
///
/// Bounds the gap *between* chunks, never the transfer. A plugin serialising a
/// large state can legitimately pause between chunks — hitting disk, or
/// allocating — so this is generous relative to a socket write, and still far
/// short of a human's patience for a wedged UI.
///
/// **The production default, and the only place it is read.** Every wait takes
/// its deadline from the [`StateProgress`] it is serving, which
/// [`AudioBridge::new`](super::AudioBridge::new) seeds from here. Reading the
/// constant at a wait site instead would recreate the two-configuration bug
/// this shape exists to prevent: a test could shorten one half while the other
/// stayed on ten seconds, and the untouched half is the one carrying the
/// behaviour under test.
pub(super) const PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);

/// How often [`StateProgress::wait`] rechecks the counter.
///
/// The wait is a poll rather than a condvar because the reply and the progress
/// signal arrive through two different primitives — a one-shot channel and an
/// atomic — and a caller must wake for either. Short enough that a stalled
/// transfer is not detected appreciably late, long enough that the poll costs
/// nothing next to a transfer measured in seconds.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Shared byte counter for one in-flight state transfer.
///
/// Cloned into the [`Command`](super::messages::Command) so the bridge thread
/// can advance it while the caller watches. One counter per transfer, created at
/// the call and dropped with it, so a previous transfer's progress can never be
/// mistaken for this one's.
#[derive(Clone)]
pub(super) struct StateProgress {
    bytes: Arc<AtomicUsize>,
    /// The progress deadline for *this* transfer.
    ///
    /// Carried on the transfer rather than read from a constant at each wait
    /// site, so the caller's wait and the bridge thread's per-chunk reads are
    /// one configuration. They were two, and a test that shortened only the
    /// caller's half left the bridge thread on the 10 s constant — so the test
    /// exercised a timing relationship production never has.
    deadline: Duration,
}

impl StateProgress {
    pub(super) fn new(deadline: Duration) -> Self {
        Self {
            bytes: Arc::new(AtomicUsize::new(0)),
            deadline,
        }
    }

    /// The deadline this transfer runs under; see the field.
    pub(super) fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Record that `bytes` more have moved.
    ///
    /// `Relaxed` is sufficient: the counter guards nothing and publishes no
    /// other memory. The caller reads it only to decide whether to keep waiting,
    /// and the value it eventually acts on is delivered through the reply
    /// channel, which carries its own ordering.
    pub(super) fn advance(&self, bytes: usize) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The stall error for this transfer, as far as it got.
    ///
    /// One constructor rather than three literals, so the bridge thread's two
    /// stall sites and the caller's cannot describe the same event differently.
    pub(super) fn stalled(&self) -> StateError {
        StateError::Stalled {
            bytes: self.bytes(),
            after: self.deadline,
        }
    }

    /// Wait for `ask`'s reply, giving up only once progress has stalled.
    ///
    /// Returns the transfer's own answer when one arrives. Otherwise the counter
    /// stood still for [`PROGRESS_TIMEOUT`] and the result is
    /// [`StateError::Stalled`], carrying how far the transfer got.
    ///
    /// A disconnected channel is the bridge thread having dropped the `Reply`
    /// without sending, which happens when it exits — the plugin is gone, so
    /// that answers [`StateError::PluginCrashed`] rather than waiting out a
    /// deadline for a reply nobody will send.
    pub(super) fn wait<T>(
        &self,
        ask: Ask<std::result::Result<T, StateError>>,
    ) -> std::result::Result<T, StateError> {
        let deadline = self.deadline;
        let mut seen = self.bytes();
        let mut idle = Duration::ZERO;
        loop {
            match ask.recv_timeout_borrowed(POLL_INTERVAL) {
                Ok(value) => return value,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(StateError::PluginCrashed)
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            let now = self.bytes();
            if now == seen {
                idle += POLL_INTERVAL;
                if idle >= deadline {
                    return Err(self.stalled());
                }
            } else {
                // Progress: the transfer is alive, so the idle clock restarts.
                //
                // Both halves are load-bearing *together*, which is not obvious:
                // `idle` accumulates only inside the no-progress branch, so
                // either the guarded accumulate or this reset alone keeps the
                // deadline honest, and deleting just one leaves the behaviour
                // correct. Mutation-tested — the budget only degrades into a
                // total when the accumulate is unguarded *and* the reset is
                // gone, and that combination fails
                // `a_slow_but_progressing_state_transfer_completes` part-way
                // through the sequence, which is the production bug in
                // miniature. Kept explicit because relying on the branch
                // structure alone would make a later "simplification" of that
                // structure silently reintroduce a total budget.
                seen = now;
                idle = Duration::ZERO;
            }
        }
    }
}

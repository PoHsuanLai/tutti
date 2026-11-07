//! Shared transport state, grouped by what it is rather than by which
//! atomic it happens to be.
//!
//! The transport is *state* — a handful of atomics shared between the UI
//! thread and the audio thread — plus the commands that mutate it. These
//! types name the groups that always travel together, so consumers ask for
//! a concept (`SeekSlot`) instead of assembling one from loose atomics.
//!
//! Grouping is by **data-flow direction**:
//!
//! - [`ClockLinks`] — what `TransportClock` shares with the live transport.
//! - [`SeekSlot`] — the pending jump the FSM hands the clock.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use atomic_float::AtomicF64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use tutti_types::Beat;

/// A pending absolute jump. `pending` is the one-shot flag the clock
/// consumes; `target` is where to land.
///
/// Seek is an *absolute* jump, not an offset — the clock overwrites its
/// position with `target` and clears `pending`.
#[derive(Clone, Debug)]
pub struct SeekSlot {
    /// The absolute beat to land on. Meaningless unless `pending` is set.
    pub target: Arc<AtomicF64>,
    /// Whether a jump is due. Cleared by the clock when it takes the target.
    pub pending: Arc<AtomicBool>,
}

impl SeekSlot {
    /// An empty slot with no seek pending.
    #[must_use]
    pub fn new() -> Self {
        Self {
            target: Arc::new(AtomicF64::new(0.0)),
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Request a jump to `beat`. Applied by the clock on its next buffer.
    pub fn request(&self, beat: impl Into<Beat>) {
        self.target.store(beat.into().get(), Ordering::Release);
        self.pending.store(true, Ordering::Release);
    }

    /// Takes a pending target, clearing the flag. `None` if no seek is due.
    pub fn take(&self) -> Option<Beat> {
        self.pending
            .swap(false, Ordering::AcqRel)
            .then(|| Beat(self.target.load(Ordering::Acquire)))
    }

    /// Whether a jump is waiting for the clock. Advisory — the clock may
    /// consume it between this read and any action taken on the answer.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

impl Default for SeekSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything [`TransportClock`](super::TransportClock) shares with the live
/// transport, and — when it came from [`Transport::clock_links`] — the
/// transport's **one playhead writer**.
///
/// The membership rule is exactly a derived stream's cut
/// ([`TransportClock::starting_at`](super::TransportClock::starting_at), via
/// [`severed`](Self::severed)): every field here is `Arc`-shared, so an
/// offline render stepping a clone must drop all of them or it stomps live
/// playback. Fields the clock owns privately — its beat,
/// sample rate, cached increment — are deliberately *not* here; that is the
/// whole distinction the type draws.
///
/// Two are read to advance time (`tempo`, `paused`) and are public. The loop
/// region is the engine's, not a link (`LoopRegion`). The rest are the writer half, written every buffer: the seek it
/// consumes, and the playhead, segment generation, steady time and tempo in
/// force it publishes. Both directions travel together because a clock given
/// only the inputs advances a playhead nothing can read.
///
/// # One writer per transport
///
/// The writer half is private to the transport module, so the only
/// `ClockLinks` that writes a live playhead is one from
/// [`Transport::clock_links`], and that hands one out **once** per transport
/// (every clone of the transport shares the claim). A second clock over the
/// same transport is refused there, with [`PlayheadClaimed`], instead of two
/// clocks both consuming each seek and both writing the playhead. Cloning the
/// links (a clock's commit-clone) shares the one claim; [`severed`](Self::severed)
/// drops it; the claim is given back when the last holder is dropped.
///
/// ```compile_fail,E0451
/// # use std::sync::Arc;
/// # use tutti_transport::ClockLinks;
/// # use std::sync::atomic::AtomicBool;
/// # use atomic_float::AtomicF64;
/// # let t = tutti_transport::Transport::new(48_000.0);
/// // A writer assembled by hand, around the claim: refused.
/// let forged = ClockLinks {
///     tempo: Arc::new(AtomicF64::new(120.0)),
///     paused: Arc::new(AtomicBool::new(false)),
///     seek: t.motion.seek.clone(),
///     position_writeback: Some(Arc::new(AtomicF64::new(0.0))),
///     segment_generation: None,
///     steady_time: None,
///     tempo_in_force: None,
///     claim: None,
/// };
/// ```
///
/// [`Transport::clock_links`]: super::Transport::clock_links
#[derive(Clone, Debug)]
pub struct ClockLinks {
    /// Beats per minute, re-read every buffer so a tempo edit takes effect
    /// within one block.
    pub tempo: Arc<AtomicF64>,
    /// Whether to hold position. Written by the motion FSM, never set directly.
    pub paused: Arc<AtomicBool>,
    /// Pending absolute jump, consumed once per buffer. Writer half: the
    /// clock that consumes the live seek is the one that moves the playhead.
    pub(super) seek: SeekSlot,
    /// Where the clock publishes the playhead. `None` = writes nothing live.
    pub(super) position_writeback: Option<Arc<AtomicF64>>,
    /// Where the clock publishes its segment's generation
    /// ([`Timeline::segment_generation`](super::Timeline::segment_generation)),
    /// just before the playhead. `None` = writes nothing live.
    pub(super) segment_generation: Option<Arc<AtomicU64>>,
    /// Samples elapsed since the stream started — a free-running counter that
    /// does **not** reset on loop, seek, or stop.
    ///
    /// This is what the plugin ABIs call `continousTimeSamples` (VST3) and
    /// `steady_time` (CLAP). Free-running effects key their timing off it
    /// precisely *because* the musical playhead jumps, so it cannot be derived
    /// from the beat — which is why the clock, already running once per buffer,
    /// is the thing that counts it.
    ///
    /// `None` = writes nothing live, matching `position_writeback`.
    pub(super) steady_time: Option<Arc<AtomicI64>>,
    /// Where the clock publishes the tempo it runs at (its hysteresis
    /// applied), so a beat resolved elsewhere uses the same one. `None` =
    /// writes nothing live.
    pub(super) tempo_in_force: Option<Arc<AtomicF64>>,
    /// The transport's playhead-writer claim, when these are its live links:
    /// shared by clones of the links, given back when the last is dropped.
    pub(super) claim: Option<Arc<PlayheadClaim>>,
}

/// A transport's playhead-writer claim, held (in an `Arc`) by the one
/// [`ClockLinks`] that writes its playhead and by that links' clones. Its
/// drop gives the claim back, so a transport whose clock is gone (an engine
/// dropped for a new one) can hand out another.
#[derive(Debug)]
pub(super) struct PlayheadClaim(Arc<AtomicBool>);

impl PlayheadClaim {
    /// Take `flag`'s claim, if nobody holds it.
    pub(super) fn take(flag: &Arc<AtomicBool>) -> Result<Self, PlayheadClaimed> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self(Arc::clone(flag)))
            .map_err(|_| PlayheadClaimed)
    }
}

impl Drop for PlayheadClaim {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// [`Transport::clock_links`](super::Transport::clock_links) refused: a
/// clock over this transport already writes its playhead (an engine's, or a
/// clone of it). Two would both consume every seek and both write the
/// playhead. Drop the first (its engine) before building another over the
/// same transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayheadClaimed;

impl std::fmt::Display for PlayheadClaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a clock over this transport already writes its playhead")
    }
}

impl std::error::Error for PlayheadClaimed {}

impl ClockLinks {
    /// Links that follow a transport's live tempo and pausedness and share
    /// nothing else: a clock over them writes no playhead and takes no claim.
    /// For a test that needs a second clock over a transport an engine drives;
    /// built only under `cfg(test)` or the `test-support` feature.
    #[cfg(any(test, feature = "test-support"))]
    pub fn bare(tempo: Arc<AtomicF64>, paused: Arc<AtomicBool>) -> Self {
        Self {
            tempo,
            paused,
            seek: SeekSlot::new(),
            position_writeback: None,
            segment_generation: None,
            steady_time: None,
            tempo_in_force: None,
            claim: None,
        }
    }

    /// Move the playhead by hand: a host with no clock (a headless tool, a
    /// test standing in for the engine) publishes `beat` as the transport's
    /// playhead through the writer it holds. A jump, as far as a reader can
    /// tell, so the segment generation moves on first. A no-op on links
    /// that write no playhead ([`writes_playhead`](Self::writes_playhead)).
    ///
    /// ```
    /// use tutti_types::Beat;
    /// use tutti_transport::{Timeline, Transport};
    /// let transport = Transport::new(48_000.0);
    /// let playhead = transport.clock_links().expect("the only writer");
    /// playhead.set_playhead(Beat(4.0));
    /// assert_eq!(transport.beat(), Beat(4.0));
    /// // A jump: a reader keyed on the segment re-seats.
    /// assert_eq!(transport.segment_generation(), 1);
    /// // The writer is held: nobody else may store.
    /// assert!(transport.clock_links().is_err());
    /// ```
    pub fn set_playhead(&self, beat: impl Into<Beat>) {
        let beat = beat.into();
        if let Some(ref generation) = self.segment_generation {
            generation.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(ref writeback) = self.position_writeback {
            writeback.store(beat.get(), Ordering::Release);
        }
    }

    /// Whether these links write a transport's playhead: they came from
    /// [`Transport::clock_links`](super::Transport::clock_links) and were not
    /// [`severed`](Self::severed).
    pub fn writes_playhead(&self) -> bool {
        self.claim.is_some()
    }

    /// A copy sharing nothing with the live transport.
    ///
    /// Tempo is snapshotted into a private cell, playback forced unpaused with
    /// no pending seek, and the loop, writeback and claim dropped — so a clock
    /// built from this reads no live state and writes to nothing live.
    ///
    /// The destructure is exhaustive on purpose: adding another shared field
    /// becomes a compile error here rather than a field silently left shared,
    /// which is the bug class this cut exists to prevent.
    #[must_use]
    pub fn severed(&self) -> Self {
        let Self {
            tempo,
            paused: _,
            seek: _,
            position_writeback: _,
            segment_generation: _,
            steady_time: _,
            tempo_in_force: _,
            claim: _,
        } = self;

        Self {
            tempo: Arc::new(AtomicF64::new(tempo.load(Ordering::Acquire))),
            paused: Arc::new(AtomicBool::new(false)),
            seek: SeekSlot::new(),
            position_writeback: None,
            segment_generation: None,
            steady_time: None,
            tempo_in_force: None,
            claim: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_slot_take_is_one_shot() {
        let seek = SeekSlot::new();
        assert_eq!(seek.take(), None);

        seek.request(8.0);
        assert!(seek.is_pending());
        assert_eq!(seek.take(), Some(Beat(8.0)));
        // Consumed — a second take sees nothing.
        assert_eq!(seek.take(), None);
        assert!(!seek.is_pending());
    }
}

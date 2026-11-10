//! Shared transport state, grouped by what it is rather than by which
//! atomic it happens to be.
//!
//! The transport is *state* — a handful of atomics shared between the UI
//! thread and the audio thread — plus the commands that mutate it. These
//! types name the groups that always travel together, so consumers ask for
//! a concept instead of assembling one from loose atomics.
//!
//! Grouping is by **data-flow direction**:
//!
//! - [`ClockLinks`] — what `TransportClock` shares with the live transport.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use atomic_float::AtomicF64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use tutti_types::{Beat, Bpm};

/// A tempo shared between threads: only a [`Bpm`] goes in, so the audio thread
/// reads a valid tempo back whatever the writer did.
#[derive(Debug)]
pub(crate) struct AtomicBpm(AtomicF64);

impl AtomicBpm {
    pub(crate) fn new(tempo: Bpm) -> Self {
        Self(AtomicF64::new(tempo.get()))
    }

    #[inline]
    pub(crate) fn load(&self, order: Ordering) -> Bpm {
        // Only `new` and `store` write the cell, both from a `Bpm`.
        let raw = self.0.load(order);
        let tempo = Bpm::new(raw);
        debug_assert!(tempo.is_some(), "AtomicBpm holds {raw}, which is no tempo");
        tempo.unwrap_or_default()
    }

    #[inline]
    pub(crate) fn store(&self, tempo: Bpm, order: Ordering) {
        self.0.store(tempo.get(), order);
    }
}

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

/// Everything `TransportClock` shares with the live
/// transport.
///
/// The membership rule is exactly a derived stream's cut
/// (`TransportClock::starting_at`, via
/// `severed`): every field here is `Arc`-shared, so an
/// offline render stepping a clone must drop all of them or it stomps live
/// playback. Fields the clock owns privately are deliberately *not* here.
///
/// Crate-private: the only live links are the ones
/// [`Transport::audio`](super::Transport::audio) builds for the one
/// [`TransportAudio`](super::TransportAudio) that holds the motion machine,
/// so no second playhead writer can be assembled outside.
#[derive(Clone, Debug)]
pub(crate) struct ClockLinks {
    /// Beats per minute, re-read every buffer so a tempo edit takes effect
    /// within one block.
    pub(super) tempo: Arc<AtomicBpm>,
    /// Whether to hold position. Written by the motion FSM, never set directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) paused: Arc<AtomicBool>,
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
    pub(super) tempo_in_force: Option<Arc<AtomicBpm>>,
}

impl ClockLinks {
    /// Links that follow a transport's live tempo and pausedness and share
    /// nothing else: a clock over them writes no playhead. For tests.
    #[cfg(test)]
    pub(crate) fn bare(tempo: Arc<AtomicBpm>, paused: Arc<AtomicBool>) -> Self {
        Self {
            tempo,
            paused,
            seek: SeekSlot::new(),
            position_writeback: None,
            segment_generation: None,
            steady_time: None,
            tempo_in_force: None,
        }
    }

    /// Move the playhead by hand: publishes `beat` as the transport's
    /// playhead. A jump, as far as a reader can tell, so the segment
    /// generation moves on first. A no-op on links that write no playhead.
    pub(crate) fn set_playhead(&self, beat: impl Into<Beat>) {
        let beat = beat.into();
        if let Some(ref generation) = self.segment_generation {
            generation.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(ref writeback) = self.position_writeback {
            writeback.store(beat.get(), Ordering::Release);
        }
    }

    /// A copy sharing nothing with the live transport.
    ///
    /// Tempo is snapshotted into a private cell, playback forced unpaused with
    /// no pending seek, and the loop and writeback dropped — so a clock
    /// built from this reads no live state and writes to nothing live.
    ///
    /// The destructure is exhaustive on purpose: adding another shared field
    /// becomes a compile error here rather than a field silently left shared,
    /// which is the bug class this cut exists to prevent.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn severed(&self) -> Self {
        let Self {
            tempo,
            paused: _,
            seek: _,
            position_writeback: _,
            segment_generation: _,
            steady_time: _,
            tempo_in_force: _,
        } = self;

        Self {
            tempo: Arc::new(AtomicBpm::new(tempo.load(Ordering::Acquire))),
            paused: Arc::new(AtomicBool::new(false)),
            seek: SeekSlot::new(),
            position_writeback: None,
            segment_generation: None,
            steady_time: None,
            tempo_in_force: None,
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

        seek.request(Beat::new(8.0));
        assert!(seek.is_pending());
        assert_eq!(seek.take(), Some(Beat(8.0)));
        // Consumed — a second take sees nothing.
        assert_eq!(seek.take(), None);
        assert!(!seek.is_pending());
    }
}

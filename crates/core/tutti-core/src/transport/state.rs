//! Shared transport state, grouped by what it is rather than by which
//! atomic it happens to be.
//!
//! The transport is *state* — a handful of atomics shared between the UI
//! thread and the audio thread — plus the commands that mutate it. These
//! types name the groups that always travel together, so consumers ask for
//! a concept (`LoopSpan`) instead of assembling one from loose atomics.
//!
//! Grouping is by **data-flow direction**:
//!
//! - [`ClockLinks`] — what `TransportClock` shares with the live transport.
//! - [`Declick`] — the fade contract between the FSM and `Engine`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::params::{Beat, BeatDuration};
use crate::Samples;
use crate::{AtomicBool, AtomicF64, AtomicI64, AtomicU32};

/// Number of ports a beat signal occupies: whole beats, then fraction.
///
/// `TransportClock` emits the beat split across two channels because a single
/// `f32` cannot carry a musical position accurately — past beat 16384 its ULP
/// exceeds 0.002 beats, which is audible as automation stair-stepping. Port 0
/// carries the integer part and port 1 the fraction in `[0, 1)`, so precision
/// stays constant no matter how far into a session the playhead is.
///
/// Every beat-driven node uses this convention. Reconstruct with [`beat_from_ports`].
pub const BEAT_PORTS: usize = 2;

/// Rebuild a beat from the two port values written by `TransportClock`.
///
/// The inverse of the clock's split: `whole` is the integer part, `frac` the
/// remainder in `[0, 1)`.
/// Returns a [`Beat`], not a bare `f64`: the whole point of the two-port split
/// is that a beat position does not survive a single `f32`, and a scalar return
/// invites putting it back into one. The `f32` *parameters* are the audio ports
/// themselves and stay raw.
#[inline]
pub fn beat_from_ports(whole: f32, frac: f32) -> Beat {
    Beat(whole as f64 + frac as f64)
}

/// Musical time covered by one audio sample at `tempo` and `sample_rate`.
///
/// The conversion every beat-driven consumer needs: the clock caches it per
/// buffer, `BeatWindow` derives a block's span from it, `OfflineTimeline`
/// precomputes it once.
///
/// The association is load-bearing: `(tempo / 60) / sample_rate`, **not**
/// `tempo / (60 * sample_rate)`. The two round differently, and the offline
/// timeline is pinned to agree with the clock sample-for-sample.
#[inline]
pub fn beats_per_sample(
    tempo: impl Into<crate::Bpm>,
    sample_rate: impl Into<crate::SampleRate>,
) -> BeatDuration {
    BeatDuration((tempo.into().get() / 60.0) / sample_rate.into().get())
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

    /// Take a pending target, clearing the flag. `None` if no seek is due.
    pub fn take(&self) -> Option<Beat> {
        self.pending
            .swap(false, Ordering::AcqRel)
            .then(|| Beat(self.target.load(Ordering::Acquire)))
    }

    /// Whether a jump is waiting for the clock. Advisory — the clock may
    /// consume it between this read and any action taken on the answer.
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

impl Default for SeekSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// A loop region on the timeline: `start..end`, guaranteed non-empty and
/// correctly ordered.
///
/// The check lives in the constructor, so `None` means "not a usable loop"
/// rather than "a loop you must validate". A consumer holding one of these
/// needs no `end > start` guard of its own, and `wrap` relies on exactly that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopRange {
    start: Beat,
    end: Beat,
}

impl LoopRange {
    /// Build a region, or `None` if it is empty or inverted.
    pub fn new(start: impl Into<Beat>, end: impl Into<Beat>) -> Option<Self> {
        let (start, end) = (start.into(), end.into());
        (end > start).then_some(Self { start, end })
    }

    /// First beat of the region, inclusive.
    #[inline]
    pub fn start(&self) -> Beat {
        self.start
    }

    /// One past the last beat of the region, exclusive.
    #[inline]
    pub fn end(&self) -> Beat {
        self.end
    }

    /// Length in beats. Always positive, by construction.
    #[inline]
    pub fn len(&self) -> BeatDuration {
        self.end - self.start
    }

    /// Whether `beat` falls in `[start, end)`. The end beat is *not* contained
    /// — it is the first beat of the next pass.
    #[inline]
    pub fn contains(&self, beat: Beat) -> bool {
        beat >= self.start && beat < self.end
    }

    /// Wrap `beat` back into the region, preserving overshoot.
    ///
    /// The remainder needs no zero guard because `len()` is positive by
    /// construction. `rem_euclid` rather than `%` so a beat below `start` wraps
    /// *into* the region instead of landing outside it on the negative side.
    #[inline]
    pub fn wrap(&self, beat: Beat) -> Beat {
        if beat < self.end {
            return beat;
        }
        self.start + (beat - self.start).rem_euclid(self.len())
    }
}

/// The loop region, and whether looping is armed.
///
/// `start`/`end` retain their values while disabled, so toggling the loop
/// off and on again restores the same region.
#[derive(Clone, Debug)]
pub struct LoopSpan {
    /// Whether looping is armed. The bounds are kept either way.
    pub enabled: Arc<AtomicBool>,
    /// Region start, in beats. Unvalidated — may exceed `end` mid-drag.
    pub start: Arc<AtomicF64>,
    /// Region end, in beats. Unvalidated — may precede `start` mid-drag.
    pub end: Arc<AtomicF64>,
}

impl LoopSpan {
    /// A disarmed span over `start..end`. The bounds are stored as given and
    /// are not validated here; [`range`](Self::range) is where validity lives.
    pub fn new(start: impl Into<Beat>, end: impl Into<Beat>) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            start: Arc::new(AtomicF64::new(start.into().get())),
            end: Arc::new(AtomicF64::new(end.into().get())),
        }
    }

    /// Whether looping is armed. Says nothing about whether the bounds are
    /// usable — [`range`](Self::range) answers both at once.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Arm or disarm looping, leaving the bounds intact.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// The active region: `None` when looping is disarmed *or* when the stored
    /// bounds are not a usable range. A consumer gets a validated region or
    /// nothing, so it never re-checks `end > start` itself.
    pub fn range(&self) -> Option<LoopRange> {
        if !self.is_enabled() {
            return None;
        }
        let (start, end) = self.bounds();
        LoopRange::new(start, end)
    }

    /// The raw stored bounds, regardless of arming or validity. For UI that
    /// must render a brace the user is mid-drag on.
    ///
    /// Deliberately still two positions rather than a start-plus-length: an
    /// inverted pair is a legitimate mid-drag state, and a `BeatDuration` cannot
    /// express one. Typing both as `Beat` is a vocabulary fix, not a fix for
    /// transposing them — [`range`](Self::range) is still where validity lives.
    pub fn bounds(&self) -> (Beat, Beat) {
        (
            Beat(self.start.load(Ordering::Acquire)),
            Beat(self.end.load(Ordering::Acquire)),
        )
    }

    /// Store new bounds. Not validated and not ordered — an inverted pair is
    /// accepted, and simply yields no [`range`](Self::range).
    pub fn set_range(&self, start: impl Into<Beat>, end: impl Into<Beat>) {
        self.start.store(start.into().get(), Ordering::Release);
        self.end.store(end.into().get(), Ordering::Release);
    }
}

impl Default for LoopSpan {
    fn default() -> Self {
        Self::new(0.0, 16.0)
    }
}

/// Declick fade contract between the transport FSM and `Engine`.
///
/// The FSM arms a fade; the processor reads `remaining` every buffer to shape
/// the output gain and reports completion. Both halves are load-bearing — this
/// is a real two-thread handshake, not vestigial state.
///
/// # Frames narrow once, and saturate
///
/// Frames are `u32` in the atomics and [`Samples`] at the API, and `Samples` is
/// `usize`-backed, so the narrowing is real. It happens once, in
/// [`start`](Self::start), saturating rather than truncating: a fade longer
/// than `u32::MAX` frames (over a day) is nonsense, but wrapping it to a short
/// fade would click, which is the one thing this type exists to prevent.
#[derive(Clone, Debug)]
pub struct Declick {
    /// Frames left in the fade. 0 = no fade active.
    pub remaining: Arc<AtomicU32>,
    /// Fade length in frames, stamped when the fade starts.
    pub total: Arc<AtomicU32>,
}

impl Declick {
    /// An idle contract: no fade armed, no length stamped.
    pub fn new() -> Self {
        Self {
            remaining: Arc::new(AtomicU32::new(0)),
            total: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Arm a fade of `frames`, resetting the ramp to full gain.
    ///
    /// Takes [`Samples`] because that is what callers hold and what the
    /// processor compares against — `Seconds::to_samples_*` returns one, and
    /// the RT loop bounds itself by the block's frame count, so the narrowing
    /// happens here instead of at every call site.
    ///
    /// **Not for retargeting a fade in flight.** Resetting `remaining` mid-fade
    /// steps the gain back to 1.0, which clicks — the FSM keeps the ramp
    /// counting and swaps only its outcome instead.
    pub fn start(&self, frames: impl Into<Samples>) {
        let n = u32::try_from(frames.into().get()).unwrap_or(u32::MAX);
        self.total.store(n, Ordering::Release);
        self.remaining.store(n, Ordering::Release);
    }

    /// Frames left in the fade, 0 when inactive.
    #[inline]
    pub fn remaining(&self) -> Samples {
        Samples(self.remaining.load(Ordering::Acquire) as usize)
    }

    /// Fade length as armed, 0 when never started.
    #[inline]
    pub fn total(&self) -> Samples {
        Samples(self.total.load(Ordering::Acquire) as usize)
    }

    /// Whether a fade is still ramping.
    pub fn is_active(&self) -> bool {
        self.remaining.load(Ordering::Acquire) > 0
    }

    /// Abandon any fade in flight. `total` is left stamped, so the length of
    /// the last fade stays inspectable.
    pub fn clear(&self) {
        self.remaining.store(0, Ordering::Release);
    }
}

impl Default for Declick {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything [`TransportClock`](super::TransportClock) shares with the live
/// transport.
///
/// The membership rule is exactly `AudioUnit::isolate`'s cut: every field here
/// is `Arc`-shared, so an offline render ticking a clone must drop all of them
/// or it stomps live playback. Fields the clock owns privately — its beat,
/// sample rate, cached increment — are deliberately *not* here; that is the
/// whole distinction the type draws.
///
/// Four are read to advance time; `position_writeback` and `steady_time` are the
/// output half of the same handshake, written every buffer. Both directions
/// travel together because a clock given only the inputs advances a playhead
/// nothing can read.
#[derive(Clone, Debug)]
pub struct ClockLinks {
    /// Beats per minute, re-read every buffer so a tempo edit takes effect
    /// within one block.
    pub tempo: Arc<AtomicF64>,
    /// Whether to hold position. Written by the motion FSM, never set directly.
    pub paused: Arc<AtomicBool>,
    /// Pending absolute jump, consumed once per buffer.
    pub seek: SeekSlot,
    /// `None` = this clock ignores looping entirely (offline renders).
    pub loop_span: Option<LoopSpan>,
    /// Where the clock publishes the playhead. `None` = writes nothing live.
    pub position_writeback: Option<Arc<AtomicF64>>,
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
    pub steady_time: Option<Arc<AtomicI64>>,
}

impl ClockLinks {
    /// Minimal links for a clock under test: live tempo and pausedness, nothing
    /// else shared.
    #[cfg(test)]
    pub(crate) fn bare(tempo: Arc<AtomicF64>, paused: Arc<AtomicBool>) -> Self {
        Self {
            tempo,
            paused,
            seek: SeekSlot::new(),
            loop_span: None,
            position_writeback: None,
            steady_time: None,
        }
    }

    /// A copy sharing nothing with the live transport.
    ///
    /// Tempo is snapshotted into a private cell, playback forced unpaused with
    /// no pending seek, and the loop and writeback dropped — so a clock built
    /// from this reads no live state and writes to nothing live.
    ///
    /// The destructure is exhaustive on purpose: adding another shared field
    /// becomes a compile error here rather than a silently-forgotten `isolate`,
    /// which is the bug class this cut exists to prevent.
    pub fn severed(&self) -> Self {
        let Self {
            tempo,
            paused: _,
            seek: _,
            loop_span: _,
            position_writeback: _,
            steady_time: _,
        } = self;

        Self {
            tempo: Arc::new(AtomicF64::new(tempo.load(Ordering::Acquire))),
            paused: Arc::new(AtomicBool::new(false)),
            seek: SeekSlot::new(),
            loop_span: None,
            position_writeback: None,
            steady_time: None,
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

    #[test]
    fn loop_span_retains_bounds_while_disabled() {
        let span = LoopSpan::new(0.0, 16.0);
        span.set_range(2.0, 6.0);
        assert_eq!(span.range(), None, "disabled span yields no range");
        assert_eq!(
            span.bounds(),
            (Beat(2.0), Beat(6.0)),
            "bounds survive disarming"
        );

        span.set_enabled(true);
        assert_eq!(span.range(), LoopRange::new(2.0, 6.0));
    }

    #[test]
    fn declick_start_arms_both_halves() {
        let declick = Declick::new();
        assert!(!declick.is_active());

        declick.start(480);
        assert!(declick.is_active());
        assert_eq!(declick.total.load(Ordering::Acquire), 480);
        assert_eq!(declick.remaining.load(Ordering::Acquire), 480);

        declick.clear();
        assert!(!declick.is_active());
        // Total is retained so a fade's length stays inspectable.
        assert_eq!(declick.total.load(Ordering::Acquire), 480);
    }

    #[test]
    fn loop_range_rejects_empty_and_inverted() {
        assert!(LoopRange::new(0.0, 4.0).is_some());
        assert!(
            LoopRange::new(4.0, 4.0).is_none(),
            "an empty region is not a loop"
        );
        assert!(
            LoopRange::new(8.0, 4.0).is_none(),
            "an inverted region is not a loop"
        );
    }

    #[test]
    fn loop_range_wrap_preserves_overshoot() {
        let r = LoopRange::new(4.0, 8.0).unwrap();

        // Inside the region: untouched.
        assert_eq!(r.wrap(Beat(6.0)), Beat(6.0));
        // One beat past the end wraps to one beat past the start.
        assert_eq!(r.wrap(Beat(9.0)), Beat(5.0));
        // Exactly at the end wraps to the start.
        assert_eq!(r.wrap(Beat(8.0)), Beat(4.0));
        // More than one length past still lands inside.
        let far = r.wrap(Beat(4.0 + 4.0 * 3.5));
        assert!(far.get() >= 4.0 && far.get() < 8.0, "got {far:?}");
    }

    #[test]
    fn loop_span_range_is_none_when_bounds_are_unusable() {
        let span = LoopSpan::new(0.0, 0.0);
        span.set_enabled(true);
        assert!(
            span.range().is_none(),
            "armed but empty must not yield a region"
        );

        span.set_range(8.0, 4.0);
        assert!(span.range().is_none(), "armed but inverted likewise");

        span.set_range(0.0, 4.0);
        assert_eq!(span.range(), LoopRange::new(0.0, 4.0));
    }
}

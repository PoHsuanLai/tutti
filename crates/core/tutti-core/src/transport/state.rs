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
//! - [`ClockInputs`] — what `TransportClock` reads to advance time.
//! - [`Declick`] — the fade contract between the FSM and `GraphProcessor`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::params::Beat;
use crate::{AtomicBool, AtomicF64, AtomicU32};

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
#[inline]
pub fn beat_from_ports(whole: f32, frac: f32) -> f64 {
    whole as f64 + frac as f64
}

/// A pending absolute jump. `pending` is the one-shot flag the clock
/// consumes; `target` is where to land.
///
/// Seek is an *absolute* jump, not an offset — the clock overwrites its
/// position with `target` and clears `pending`.
#[derive(Clone, Debug)]
pub struct SeekSlot {
    pub target: Arc<AtomicF64>,
    pub pending: Arc<AtomicBool>,
}

impl SeekSlot {
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
/// The `(f64, f64)` tuple this replaces carried no invariant, so every
/// consumer re-checked `end > start` before using it — the clock did so in two
/// separate places. Constructing this type performs that check once, and
/// `None` means "not a usable loop" rather than "a loop you must validate".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopRange {
    start: Beat,
    end: Beat,
}

impl LoopRange {
    /// Build a region, or `None` if it is empty or inverted.
    pub fn new(start: impl Into<Beat>, end: impl Into<Beat>) -> Option<Self> {
        let (start, end) = (start.into(), end.into());
        (end.get() > start.get()).then_some(Self { start, end })
    }

    #[inline]
    pub fn start(&self) -> Beat {
        self.start
    }

    #[inline]
    pub fn end(&self) -> Beat {
        self.end
    }

    /// Length in beats. Always positive, by construction.
    #[inline]
    pub fn len(&self) -> f64 {
        self.end.get() - self.start.get()
    }

    #[inline]
    pub fn contains(&self, beat: Beat) -> bool {
        beat.get() >= self.start.get() && beat.get() < self.end.get()
    }

    /// Wrap `beat` back into the region, preserving overshoot.
    ///
    /// The division is safe because `len()` is positive by construction — the
    /// guard every caller used to write is now unnecessary.
    #[inline]
    pub fn wrap(&self, beat: Beat) -> Beat {
        if beat.get() < self.end.get() {
            return beat;
        }
        let offset = (beat.get() - self.start.get()) % self.len();
        Beat(self.start.get() + offset)
    }
}

/// The loop region, and whether looping is armed.
///
/// `start`/`end` retain their values while disabled, so toggling the loop
/// off and on again restores the same region.
#[derive(Clone, Debug)]
pub struct LoopSpan {
    pub enabled: Arc<AtomicBool>,
    pub start: Arc<AtomicF64>,
    pub end: Arc<AtomicF64>,
}

impl LoopSpan {
    pub fn new(start: f64, end: f64) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            start: Arc::new(AtomicF64::new(start)),
            end: Arc::new(AtomicF64::new(end)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// The active region: `None` when looping is disarmed *or* when the stored
    /// bounds are not a usable range. Consumers get a validated region or
    /// nothing, and no longer re-check `end > start` themselves.
    pub fn range(&self) -> Option<LoopRange> {
        if !self.is_enabled() {
            return None;
        }
        let (start, end) = self.bounds();
        LoopRange::new(start, end)
    }

    /// The raw stored bounds, regardless of arming or validity. For UI that
    /// must render a brace the user is mid-drag on.
    pub fn bounds(&self) -> (f64, f64) {
        (
            self.start.load(Ordering::Acquire),
            self.end.load(Ordering::Acquire),
        )
    }

    pub fn set_range(&self, start: f64, end: f64) {
        self.start.store(start, Ordering::Release);
        self.end.store(end, Ordering::Release);
    }
}

impl Default for LoopSpan {
    fn default() -> Self {
        Self::new(0.0, 16.0)
    }
}

/// Declick fade contract between the transport FSM and `GraphProcessor`.
///
/// The FSM arms a fade; the processor reads `remaining` every buffer to
/// shape the output gain and reports completion. Both halves are load-bearing
/// — this is a real two-thread handshake, not vestigial state.
#[derive(Clone, Debug)]
pub struct Declick {
    /// Samples left in the fade. 0 = no fade active.
    pub remaining: Arc<AtomicU32>,
    /// Fade length in samples, stamped when the fade starts.
    pub total: Arc<AtomicU32>,
}

impl Declick {
    pub fn new() -> Self {
        Self {
            remaining: Arc::new(AtomicU32::new(0)),
            total: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn start(&self, samples: u32) {
        self.total.store(samples, Ordering::Release);
        self.remaining.store(samples, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.remaining.load(Ordering::Acquire) > 0
    }

    pub fn clear(&self) {
        self.remaining.store(0, Ordering::Release);
    }
}

impl Default for Declick {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`TransportClock`](super::TransportClock) reads to advance time.
///
/// This is the clock's entire input surface. Handing one of these over
/// replaces eight separate `.clone()`s of loose atomics at every clock
/// construction site.
#[derive(Clone, Debug)]
pub struct ClockInputs {
    pub tempo: Arc<AtomicF64>,
    pub paused: Arc<AtomicBool>,
    pub seek: SeekSlot,
    pub loop_span: LoopSpan,
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
        assert_eq!(span.bounds(), (2.0, 6.0), "bounds survive disarming");

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

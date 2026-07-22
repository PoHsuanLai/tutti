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
//! - [`TransportState`] — what the clock publishes; what readers observe.
//! - [`Declick`] — the fade contract between the FSM and `GraphProcessor`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

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
    pub fn request(&self, beat: f64) {
        self.target.store(beat, Ordering::Release);
        self.pending.store(true, Ordering::Release);
    }

    /// Take a pending target, clearing the flag. `None` if no seek is due.
    pub fn take(&self) -> Option<f64> {
        self.pending
            .swap(false, Ordering::AcqRel)
            .then(|| self.target.load(Ordering::Acquire))
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

    /// The region, or `None` when looping is disabled. Callers that need the
    /// bounds regardless of arming should read `start`/`end` directly.
    pub fn range(&self) -> Option<(f64, f64)> {
        self.is_enabled().then(|| self.bounds())
    }

    /// The region regardless of whether looping is armed.
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

/// What the clock publishes and every reader observes — "what time is it".
///
/// This is the read-only half of the transport. Sources that only need to
/// know the position want this, not the command side.
#[derive(Clone, Debug)]
pub struct TransportState {
    pub beat: Arc<AtomicF64>,
    pub recording: Arc<AtomicBool>,
    pub in_preroll: Arc<AtomicBool>,
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
        assert_eq!(seek.take(), Some(8.0));
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
        assert_eq!(span.range(), Some((2.0, 6.0)));
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
}

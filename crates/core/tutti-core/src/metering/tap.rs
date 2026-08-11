//! The analysis tap — a lock-free copy of the master output for off-thread
//! consumers.
//!
//! Two of them, both off the audio thread: **analysis** (spectrum, pitch,
//! transients) drains the ring directly, and **recording** goes through
//! `tutti-io`'s `TapIn`, which adapts the consumer end into an `AudioIn` so a
//! pump can write what the graph is playing to a file.
//!
//! Opt-in: until someone calls [`AudioTap::open`], the audio thread's
//! [`push`](AudioTap::push) is a single atomic load and a return.
//!
//! This is the only way to observe master output. The RT callback takes no
//! host-supplied hook, so the alternative is an `AudioUnit` spliced into
//! `MasterSources` — which must leave its ring untouched in `reset` (fundsp
//! clones every vertex on `commit`, so a frontend clone shares the ring and
//! popping there races the backend's `tick`) and mint a unique `get_id`.

use crate::{AtomicBool, Ordering};
use parking_lot::Mutex;
use ringbuf::{
    traits::{Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::sync::Arc;

/// Ring capacity in stereo frames — ~3 seconds at 44.1 kHz.
const CAPACITY: usize = 131_072;

/// The producer end, swapped in on `open` and out on `close`. `Mutex` rather
/// than an atomic swap because only the cold path ever writes it; the audio
/// thread `try_lock`s and skips the buffer on contention.
type TapProducer = Arc<Mutex<Option<HeapProd<(f32, f32)>>>>;

/// The consumer end of an opened [`AudioTap`] — stereo frames, in the order the
/// audio thread pushed them.
///
/// A newtype rather than a bare `ringbuf::HeapCons` so that `ringbuf` stays an
/// implementation detail of this crate. Returning the raw type would put a
/// dependency this crate does not re-export into the signature of a public
/// method: a caller who wants to *name* what [`AudioTap::open`] returned — to
/// store it in a struct, or write a function over it — would have to add
/// `ringbuf` to their own manifest and keep the version in lockstep with ours
/// forever. `tutti-core` and `tutti-io` do not even pin the same feature set
/// (`default-features = false` here, defaults there), so "just add ringbuf"
/// is not reliably the same crate instantiation.
///
/// [`MicRing`](../../tutti_io/struct.MicRing.html) is the same decision for the
/// mic capture ring; this is the sibling that was missed.
pub struct TapCons(HeapCons<(f32, f32)>);

impl TapCons {
    /// Pop one stereo frame, or `None` when the ring is empty.
    ///
    /// Empty means "the callback has not pushed since the last poll", never
    /// "finished" — see `TapIn`'s `ON_EMPTY` for why that distinction is the
    /// whole reason the tap is not treated as a finite source.
    #[inline]
    pub fn try_pop(&mut self) -> Option<(f32, f32)> {
        use ringbuf::traits::Consumer;
        self.0.try_pop()
    }

    /// Frames readable right now.
    ///
    /// Advisory: the audio thread may push more between this call and the next
    /// [`try_pop`](Self::try_pop). Useful for sizing a drain, not for deciding
    /// that the tap is finished.
    #[inline]
    pub fn occupied_len(&self) -> usize {
        use ringbuf::traits::Observer;
        self.0.occupied_len()
    }
}

impl std::fmt::Debug for TapCons {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Occupancy is a live value the audio thread is writing; reporting it
        // here would make a `Debug` print race with the callback. The type's
        // identity is all a formatter needs.
        f.debug_struct("TapCons").finish_non_exhaustive()
    }
}

/// [`AudioTap::open`] was called on a tap that already has a consumer.
///
/// A ring has one reader. Returning this rather than minting a second ring is
/// what keeps the first consumer alive: the alternative orphans it in a way
/// nothing can detect, because a consumer whose producer was dropped reads
/// exactly like one whose producer is idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the analysis tap already has a consumer; close it before opening again")]
pub struct TapBusy;

/// Producer half of the analysis tap. Cheap to clone; the audio callback keeps
/// one and pushes every buffer through it.
#[derive(Clone, Default)]
pub struct AudioTap {
    on: Arc<AtomicBool>,
    producer: TapProducer,
}

impl std::fmt::Debug for AudioTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-rolled: the ring producer is not `Debug`. Reports whether the
        // tap is open, which is the whole of its observable state, and does not
        // touch the producer lock -- a `Debug` print must not contend with the
        // audio callback.
        f.debug_struct("AudioTap")
            .field("open", &self.on.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl AudioTap {
    /// A **closed** tap. No ring is allocated until [`open`](Self::open), and
    /// [`push`](Self::push) is one atomic load until then.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the tap, returning the consumer end. The caller owns it and drains
    /// it from its own thread.
    ///
    /// A ring has exactly one reader, so a tap has exactly one consumer:
    /// opening an already-open tap returns [`TapBusy`] rather than minting a
    /// second ring, and the incumbent keeps receiving. Displacing it would be
    /// undetectable — a `HeapCons` whose producer was dropped reads exactly
    /// like one that is merely idle, so both look like silence.
    ///
    /// Call [`close`](Self::close) first to hand the tap over deliberately.
    #[must_use = "the returned consumer is the only handle to the tap ring; drop it and the audio thread pushes into a ring nobody reads"]
    pub fn open(&self) -> Result<TapCons, TapBusy> {
        // Decide under the lock, not against `is_open`: `on` and `producer` are
        // separate, so a check-then-open would let two control threads both
        // pass the check and the loser's consumer would be orphaned — exactly
        // the bug this returns an error to prevent.
        let mut slot = self.producer.lock();
        if slot.is_some() {
            return Err(TapBusy);
        }

        let (prod, cons) = HeapRb::<(f32, f32)>::new(CAPACITY).split();
        *slot = Some(prod);
        // Release *after* the producer is in place: the audio thread checks
        // `on` first and only then tries the lock, so flipping this earlier
        // would let a callback find `on == true` with nothing to push into.
        self.on.store(true, Ordering::Release);
        Ok(TapCons(cons))
    }

    /// Close the tap and drop the producer. The consumer sees an empty ring.
    ///
    /// Also the way to hand the tap to a *different* consumer: [`open`](Self::open)
    /// refuses while one is live, so releasing it is an explicit step rather
    /// than a side effect of asking for a second.
    ///
    /// Idempotent — closing a closed tap is a no-op.
    pub fn close(&self) {
        // Flag down first: the audio thread reads `on` before touching the
        // lock, so this order means a callback can never find a live flag over
        // an absent producer.
        self.on.store(false, Ordering::Release);
        *self.producer.lock() = None;
    }

    /// Whether a consumer currently holds this tap.
    ///
    /// Advisory only. Between this returning `false` and a subsequent
    /// [`open`](Self::open), another thread may have opened it — which is why
    /// `open` decides under the lock and reports [`TapBusy`] rather than
    /// trusting a prior check.
    pub fn is_open(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// Push interleaved stereo samples into the ring.
    ///
    /// `frames` is a **frame** count, so `output` must hold at least
    /// `frames * 2` samples.
    ///
    /// Called from the audio callback. RT-safe and never blocking: it
    /// `try_lock`s, and drops frames if the ring is full or the producer is
    /// mid-swap. No-op while the tap is closed.
    #[inline]
    pub fn push(&self, output: &[f32], frames: usize) {
        if !self.on.load(Ordering::Acquire) {
            return;
        }
        // try_lock: skip this callback if the producer is being swapped.
        if let Some(ref mut guard) = self.producer.try_lock() {
            if let Some(ref mut prod) = **guard {
                output.chunks_exact(2).take(frames).for_each(|ch| {
                    let _ = prod.try_push((ch[0], ch[1]));
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh tap opens, and says so.
    #[test]
    fn a_fresh_tap_opens() {
        let tap = AudioTap::new();
        assert!(!tap.is_open());
        assert!(tap.open().is_ok());
        assert!(tap.is_open());
    }

    /// The contract this type gained: a second open is refused, and the FIRST
    /// consumer keeps receiving.
    ///
    /// The liveness half is what matters. `open` returning `Err` is easy to get
    /// right and easy to test; the bug being prevented is the first consumer
    /// going silent, and that only shows up by pushing after the refusal and
    /// checking the original still sees it.
    #[test]
    fn a_second_open_is_refused_and_the_first_consumer_survives() {
        let tap = AudioTap::new();
        let mut first = tap.open().expect("first open");

        // Matched rather than `unwrap_err`: `HeapCons` is not `Debug`, so the
        // Result cannot be unwrapped for its error side.
        assert!(
            matches!(tap.open(), Err(TapBusy)),
            "a second open must be refused"
        );

        tap.push(&[0.5, -0.5], 1);
        assert_eq!(
            first.try_pop(),
            Some((0.5, -0.5)),
            "the incumbent consumer must still be fed after a refused open"
        );
    }

    /// Closing releases the tap, so it can be handed to a different consumer.
    ///
    /// Without this, refusing a second open would make the tap single-use — a
    /// host that stopped analysing could never start recording.
    #[test]
    fn closing_releases_the_tap_for_a_new_consumer() {
        let tap = AudioTap::new();
        let first = tap.open().expect("first open");
        tap.close();
        assert!(!tap.is_open());
        drop(first);

        let mut second = tap.open().expect("a closed tap reopens");
        tap.push(&[0.25, 0.75], 1);
        assert_eq!(second.try_pop(), Some((0.25, 0.75)));
    }

    /// Closing twice is a no-op rather than a panic or a state flip.
    #[test]
    fn closing_is_idempotent() {
        let tap = AudioTap::new();
        let _c = tap.open().expect("open");
        tap.close();
        tap.close();
        assert!(!tap.is_open());
        assert!(tap.open().is_ok(), "still reopenable after a double close");
    }

    /// A closed tap drops pushes on the floor — the opt-in half of the design.
    #[test]
    fn a_closed_tap_pushes_nothing() {
        let tap = AudioTap::new();
        let mut cons = tap.open().expect("open");
        tap.close();

        tap.push(&[1.0, 1.0], 1);
        assert_eq!(
            cons.try_pop(),
            None,
            "a closed tap must not feed a consumer that outlived the close"
        );
    }
}

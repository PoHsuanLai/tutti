//! [`PosRing`]: frames a writer thread places ahead of a reader, indexed by
//! position rather than consumed in order.
//!
//! **The invariant.** A slot the reader's block in flight may read is never
//! written until that block is over: a position the block may read — inside
//! one of the ranges it claimed, and held by the window it took — keeps the
//! frame it had when the block took the window, for as long as the block
//! runs. Every write goes to the window's end; a position leaves the window
//! only by a shrink the writer counts as possibly still held (a *stale*
//! position) until the reader says it has moved on, or by being overwritten
//! round the ring, which the writer checks against the block's ranges first.
//!
//! One writer (a streaming thread, [`PosWriter`]) and one reader (the audio
//! thread, [`PosReader`]) share `N` frame slots. Slot `s mod N` holds position
//! `s` — a position on whatever line the two agree on; tutti-sampler's disk
//! ring uses a file counted straight on. The writer publishes the window
//! `[from, to)` of positions the slots hold as one packed word, appends at
//! `to`, and may shrink the window (a retraction, a raise, a reset); the
//! reader claims the positions its block may read, takes the window, and reads
//! any position the window holds inside its claim, in any order, as often as
//! it likes. Nothing is consumed and nothing on the reading side locks,
//! allocates or waits.
//!
//! **One of each, by type.** [`PosRing::new`] returns the one writer and the
//! one reader. Neither is `Clone`; the writer's methods take `&mut self`; a
//! read goes through a [`PosClaim`], which borrows the reader, so a claim
//! cannot outlive the next one.
//!
//! # Why a write never tears a read
//!
//! Every sample is an `AtomicU32`, so a slip in the protocol below is a wrong
//! sample, never undefined behaviour; the protocol is what makes it never a
//! wrong sample. The reader, per block ([`PosReader::claim`]):
//!
//! 1. stores where it plays, and the ranges `[a, e)` its block may read
//!    (`Release`);
//! 2. a `SeqCst` fence (**F1**);
//! 3. loads the shrink generation `g` (`Acquire`);
//! 4. loads the window (`Acquire`);
//! 5. echoes `g` as the generation it holds (`Release`).
//!
//! The writer never writes a slot the block in flight may read:
//!
//! - **Going round the ring.** Writing position `s` reuses the slot of every
//!   `s ± kN`. Before a write the writer raises the window's start past
//!   `s - N` (`Release`), then a `SeqCst` fence (**F2**), then it loads the
//!   reader's ranges (`Acquire`). By the fence rule (C++20 [atomics.fences]),
//!   F1 and F2 order the reader's range stores against the writer's raise:
//!   either the reader's window load sees the raised start, or the writer's
//!   range load sees the reader's ranges and skips any `s` whose slot another
//!   position of a range holds ([`first_alias`]). A write that is cut short
//!   leaves the start where the write it did make needs it, no higher: every
//!   position it takes out of the window is one it overwrote, and one it
//!   raised past and then did not overwrite is put back.
//! - **Shrinking.** A retraction, a raise or a reset stores the smaller window
//!   (`Release`), *then* bumps the generation (`Release`). The positions it
//!   removed are stale: a block that took the window before it may still read
//!   them. The writer does not write a stale position inside the reader's
//!   ranges until the reader echoes the new generation. That echo is a
//!   `Release` the writer loads with `Acquire`, and it comes after the reader
//!   loaded the new generation (`Acquire`, pairing with the bump) and then a
//!   window no older than the shrink; and after every read of the block
//!   before, in program order. So once the writer sees it, no block that could
//!   hold a stale position is still reading.
//!
//! Which orderings carry that: F1/F2 (going round); the window's
//! `Release`/`Acquire` (a claimed window's frames were stored before it was
//! published); the generation's `Release`/`Acquire` (the echo implies the
//! shrunk window); the echo's `Release`/`Acquire` (the block before it is
//! over). The ranges' `Release`/`Acquire` are redundant beside F1/F2, and
//! close load buffering formally. A position *in* a range but never in any
//! window the reader could hold is free to write: the reader reads it only
//! once a later window holds it.
//!
//! A reader that stops claiming ([`PosReader::idle`]) clears its ranges and
//! echoes the generation, so a paused reader holds nothing back.
//!
//! `tests/pos_ring_loom.rs` checks this code under loom (its atomics become
//! loom's under `--cfg loom`), with models that fail without each part.

use std::sync::atomic::Ordering;

use sync::{fence, Arc, AtomicU32, AtomicU64};

/// The atomics: `std`'s in a real build, `loom`'s under `--cfg loom`, so the
/// model in `tests/pos_ring_loom.rs` checks this code rather than a replica.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::{
        atomic::{fence, AtomicU32, AtomicU64},
        Arc,
    };
    #[cfg(not(loom))]
    pub(super) use std::sync::{
        atomic::{fence, AtomicU32, AtomicU64},
        Arc,
    };
}

/// Bits of the packed window word that hold its length; the rest its end.
const LEN_BITS: u32 = 24;

/// The most frames a ring holds: its window's length is packed in 24 bits.
/// (The window's end takes the other 40: a position must stay below `2^40`,
/// 290 days of frames at 44.1 kHz.)
pub const MAX_POS_RING_FRAMES: usize = (1 << LEN_BITS) - 1;

/// A window of positions `[from, to)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingWindow {
    /// The first position held.
    pub from: u64,
    /// One past the last position held.
    pub to: u64,
}

impl RingWindow {
    fn pack(self) -> u64 {
        debug_assert!(self.to < 1 << (64 - LEN_BITS), "a position past 2^40");
        debug_assert!(self.to - self.from <= MAX_POS_RING_FRAMES as u64);
        (self.to << LEN_BITS) | (self.to - self.from)
    }

    fn unpack(word: u64) -> Self {
        let to = word >> LEN_BITS;
        Self {
            from: to - (word & ((1 << LEN_BITS) - 1)),
            to,
        }
    }

    /// Whether position `s` is held.
    #[inline]
    pub fn holds(&self, s: u64) -> bool {
        s >= self.from && s < self.to
    }
}

/// The state a [`PosWriter`] and a [`PosReader`] share (see the module docs).
///
/// Only its glances are public — the slots are read through a [`PosClaim`]
/// and written through the [`PosWriter`]. A writer hands one out with
/// [`PosWriter::watch`], for status and tests.
pub struct PosRing {
    stride: usize,
    frames: usize,
    /// Positions the writer keeps behind where the reader plays.
    keep_behind: u64,
    /// `frames * stride` samples, as `f32` bits.
    slots: Box<[AtomicU32]>,
    /// The packed [`RingWindow`].
    window: AtomicU64,
    /// Where the reader plays (the reader's claims, or the writer before one).
    play: AtomicU64,
    /// Reader → writer: the ranges its block may read, `[a0, e0)`, `[a1, e1)`.
    reads: [AtomicU64; 4],
    /// Writer → reader: bumped after every shrink.
    generation: AtomicU64,
    /// Reader → writer: the generation its latest claim loaded.
    claimed: AtomicU64,
}

impl std::fmt::Debug for PosRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosRing")
            .field("frames", &self.frames)
            .field("stride", &self.stride)
            .field("window", &self.window())
            .finish_non_exhaustive()
    }
}

impl PosRing {
    /// A ring of `frames` slots (at least one, at most
    /// [`MAX_POS_RING_FRAMES`]) of `stride` samples (at least one), holding
    /// nothing, that keeps `keep_behind` positions behind where the reader
    /// plays (the taps a position reads behind itself): its one writer and
    /// its one reader.
    #[allow(clippy::new_ret_no_self)] // the ring is its two ends
    pub fn new(frames: usize, stride: usize, keep_behind: u64) -> (PosWriter, PosReader) {
        let frames = frames.clamp(1, MAX_POS_RING_FRAMES);
        let stride = stride.max(1);
        let ring = Arc::new(Self {
            stride,
            frames,
            keep_behind,
            slots: (0..frames * stride).map(|_| AtomicU32::new(0)).collect(),
            window: AtomicU64::new(0),
            play: AtomicU64::new(0),
            reads: std::array::from_fn(|_| AtomicU64::new(0)),
            generation: AtomicU64::new(0),
            claimed: AtomicU64::new(0),
        });
        let writer = PosWriter {
            ring: Arc::clone(&ring),
            stale: (u64::MAX, 0),
        };
        (writer, PosReader { ring })
    }

    /// Slots, in frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Samples per frame.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The window now: a glance, not a claim (nothing may be read by it).
    #[inline]
    pub fn window(&self) -> RingWindow {
        RingWindow::unpack(self.window.load(Ordering::Acquire))
    }

    /// Where the reader plays (or the writer last said it would). Advisory:
    /// what the writer fills ahead of.
    pub fn play(&self) -> u64 {
        self.play.load(Ordering::Relaxed)
    }

    /// Shrinks so far: the generation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The reader's ranges, `Acquire` (the writer has fenced, or is about to
    /// only cap with them).
    fn in_flight(&self) -> [(u64, u64); 2] {
        let r = |i: usize| self.reads[i].load(Ordering::Acquire);
        [(r(0), r(1)), (r(2), r(3))]
    }
}

/// The one writer of a [`PosRing`]: a streaming thread. Not `Clone`.
pub struct PosWriter {
    ring: Arc<PosRing>,
    /// The span of positions shrinks have removed since the reader last
    /// echoed the generation, `[lo, hi)`: what a block that took an older
    /// window may still hold. Writer-local.
    stale: (u64, u64),
}

impl std::fmt::Debug for PosWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosWriter")
            .field("ring", &self.ring)
            .field("stale", &self.stale)
            .finish()
    }
}

impl PosWriter {
    /// A read-only handle on the shared state, for glances (the window, the
    /// play position, the generation). It reads no sample.
    pub fn watch(&self) -> Arc<PosRing> {
        Arc::clone(&self.ring)
    }

    /// Slots, in frames.
    pub fn frames(&self) -> usize {
        self.ring.frames
    }

    /// Samples per frame.
    pub fn stride(&self) -> usize {
        self.ring.stride
    }

    /// The window now.
    #[inline]
    pub fn window(&self) -> RingWindow {
        self.ring.window()
    }

    /// Where the reader plays (or this writer last said it would).
    pub fn play(&self) -> u64 {
        self.ring.play()
    }

    /// Shrinks so far.
    pub fn generation(&self) -> u64 {
        self.ring.generation()
    }

    /// One past the last position the reader's block in flight may read, as
    /// of a `SeqCst` fence now (0 for an idle reader).
    pub fn in_flight_end(&self) -> u64 {
        fence(Ordering::SeqCst);
        let [(_, e0), (_, e1)] = self.ring.in_flight();
        e0.max(e1)
    }

    /// Say where the reader plays before it has (so the window is filled
    /// there).
    pub fn set_play(&mut self, play: u64) {
        self.ring.play.store(play, Ordering::Relaxed);
    }

    /// Whether the reader's latest claim has seen every shrink so far.
    fn seen(&self) -> bool {
        self.ring.claimed.load(Ordering::Acquire) == self.ring.generation.load(Ordering::Relaxed)
    }

    /// Shrink to `window`, removing `[removed.0, removed.1)`, then bump the
    /// generation (the order is the module docs' argument).
    fn shrink(&mut self, window: RingWindow, removed: (u64, u64)) {
        // What earlier shrinks removed stays stale only until the reader has
        // seen them: an echo of the current generation says it has.
        if self.seen() {
            self.stale = (u64::MAX, 0);
        }
        self.ring.window.store(window.pack(), Ordering::Release);
        if removed.0 < removed.1 {
            self.stale = (self.stale.0.min(removed.0), self.stale.1.max(removed.1));
        }
        self.ring.generation.fetch_add(1, Ordering::Release);
    }

    /// Drop every position at and past `x` (a rewrite from `x` follows).
    pub fn retract_to(&mut self, x: u64) {
        let w = self.window();
        if x < w.to {
            self.shrink(
                RingWindow {
                    from: w.from.min(x),
                    to: x,
                },
                (x.max(w.from), w.to),
            );
        }
    }

    /// Drop every position below `y`.
    pub fn raise_from(&mut self, y: u64) {
        let w = self.window();
        if y > w.from {
            self.shrink(
                RingWindow {
                    from: y.min(w.to),
                    to: w.to,
                },
                (w.from, y.min(w.to)),
            );
        }
    }

    /// An empty window at `start` (the reader moved outside the old one).
    pub fn reset(&mut self, start: u64) {
        let w = self.window();
        self.shrink(
            RingWindow {
                from: start,
                to: start,
            },
            (w.from, w.to),
        );
    }

    /// Append frames at the window's end, returning how many **frames**
    /// landed — short where a slot the reader's block in flight may read
    /// would be written (see the module docs), or where the ring is full
    /// ahead of where the reader plays. A trailing partial frame is ignored.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let ring = &*self.ring;
        let ch = ring.stride;
        let n = (samples.len() / ch) as u64;
        let frames = ring.frames as u64;
        let RingWindow { from, to } = self.window();
        // Never reuse the slot of a position the reader still stands on.
        let room = (ring.play().saturating_sub(ring.keep_behind) + frames).saturating_sub(to);
        let mut end = to + n.min(room);
        // A first cap from the ranges as they stand, so the raise below is
        // not needlessly high. Not the check: that is after the fence.
        for (a, e) in ring.in_flight() {
            end = end.min(first_alias(to, a, e, frames));
        }
        let raised = from.max(end.saturating_sub(frames));
        if raised != from {
            ring.window
                .store(RingWindow { from: raised, to }.pack(), Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let held = !self.seen();
        let stale = self.stale;
        for (a, e) in ring.in_flight() {
            end = end.min(first_alias(to, a, e, frames));
            // A block that took a window from before a shrink may hold the
            // positions it removed.
            let (lo, hi) = (a.max(to).max(stale.0), e.min(stale.1));
            if held && lo < hi {
                end = end.min(lo);
            }
        }
        if !held {
            self.stale = (u64::MAX, 0);
        }
        // The start the write needs: only what it overwrites leaves the
        // window. What the raise took out and the write does not reach is put
        // back (its slots were not touched).
        let needed = from.max(end.saturating_sub(frames));
        if end <= to {
            if raised != from {
                ring.window
                    .store(RingWindow { from, to }.pack(), Ordering::Release);
            }
            return 0;
        }
        for (i, frame) in samples
            .chunks_exact(ch)
            .take((end - to) as usize)
            .enumerate()
        {
            let slot = ((to + i as u64) % frames) as usize;
            for (c, &s) in frame.iter().enumerate() {
                ring.slots[slot * ch + c].store(s.to_bits(), Ordering::Relaxed);
            }
        }
        ring.window.store(
            RingWindow {
                from: needed,
                to: end,
            }
            .pack(),
            Ordering::Release,
        );
        (end - to) as usize
    }
}

/// The one reader of a [`PosRing`]: the audio thread. Not `Clone`.
pub struct PosReader {
    ring: Arc<PosRing>,
}

impl std::fmt::Debug for PosReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosReader")
            .field("ring", &self.ring)
            .finish()
    }
}

impl PosReader {
    /// Samples per frame.
    pub fn stride(&self) -> usize {
        self.ring.stride
    }

    /// Once per block before any read: where the reader plays, the ranges the
    /// block may read, and the window it may read them in (see the module
    /// docs for the order). The claim borrows the reader: it ends before the
    /// next one starts.
    #[inline]
    pub fn claim(&mut self, play: u64, reads: [(u64, u64); 2]) -> PosClaim<'_> {
        let ring = &*self.ring;
        ring.play.store(play, Ordering::Relaxed);
        for (i, (a, e)) in reads.into_iter().enumerate() {
            ring.reads[2 * i].store(a, Ordering::Release);
            ring.reads[2 * i + 1].store(e, Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let generation = ring.generation.load(Ordering::Acquire);
        let window = RingWindow::unpack(ring.window.load(Ordering::Acquire));
        ring.claimed.store(generation, Ordering::Release);
        PosClaim {
            ring,
            window,
            reads,
        }
    }

    /// A block that reads nothing: no ranges, and the generation echoed, so a
    /// paused reader holds no write back (the ranges of its last block would
    /// otherwise stand, and a shrink would wait on an echo that never comes).
    #[inline]
    pub fn idle(&mut self) {
        let ring = &*self.ring;
        for r in &ring.reads {
            r.store(0, Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let generation = ring.generation.load(Ordering::Acquire);
        ring.claimed.store(generation, Ordering::Release);
    }
}

/// One block's claim: the window it took and the ranges it may read.
#[derive(Debug)]
pub struct PosClaim<'a> {
    ring: &'a PosRing,
    window: RingWindow,
    reads: [(u64, u64); 2],
}

impl<'a> PosClaim<'a> {
    /// The window this block may read.
    #[inline]
    pub fn window(&self) -> RingWindow {
        self.window
    }

    /// Whether position `s` may be read this block: the window holds it (the
    /// caller keeps to its ranges).
    #[inline]
    pub fn holds(&self, s: u64) -> bool {
        self.window.holds(s)
    }

    /// The frame at position `s`, which the window and the claimed ranges
    /// hold: its slot, found once for all its channels.
    #[inline]
    pub fn frame(&self, s: u64) -> PosFrame<'a> {
        debug_assert!(self.window.holds(s), "position {s} outside the window");
        debug_assert!(
            self.reads.iter().any(|&(a, e)| (a..e).contains(&s)),
            "position {s} outside the claimed ranges"
        );
        let ch = self.ring.stride;
        let slot = (s % self.ring.frames as u64) as usize;
        PosFrame(&self.ring.slots[slot * ch..(slot + 1) * ch])
    }

    /// Channel `c` of position `s` (see [`frame`](Self::frame)).
    #[inline]
    pub fn sample(&self, s: u64, c: usize) -> f32 {
        self.frame(s).get(c)
    }
}

/// One claimed frame's samples.
#[derive(Clone, Copy, Debug)]
pub struct PosFrame<'a>(&'a [AtomicU32]);

impl PosFrame<'_> {
    /// Channel `c` (below the ring's stride; 0 past it in a release build).
    #[inline]
    pub fn get(&self, c: usize) -> f32 {
        debug_assert!(c < self.0.len(), "channel {c} past the stride");
        self.0
            .get(c)
            .map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)))
    }
}

/// The first position at or past `to` whose slot also holds a position of the
/// range `[a, e)` other than itself — an alias `[a + kN, e + kN)`, `k != 0`,
/// of a ring `n` slots long. Writing a position *in* the range is no conflict
/// of this kind (see [`PosWriter::push`] for the other kind).
fn first_alias(to: u64, a: u64, e: u64, n: u64) -> u64 {
    if e <= a {
        return u64::MAX;
    }
    // A position `s` of the range has another of the range in its slot when
    // the range reaches `s + n`, or reaches back to `s - n`.
    let inside = |s: u64| if s + n < e { s } else { s.max(a + n) };
    if (a..e).contains(&to) {
        return inside(to);
    }
    let len = e - a;
    let d = (to as i128 - a as i128).rem_euclid(n as i128) as u64;
    if d < len {
        return to;
    }
    let next = to + (n - d);
    if next == a {
        inside(a)
    } else {
        next
    }
}

#[cfg(all(test, not(loom)))]
mod tests;

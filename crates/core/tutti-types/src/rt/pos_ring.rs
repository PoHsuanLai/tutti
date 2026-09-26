//! [`PosRing`]: frames a writer thread places ahead of a reader, indexed by
//! position rather than consumed in order.
//!
//! One writer (a streaming thread) and one reader (the audio thread) share
//! `N` frame slots. Slot `s mod N` holds position `s` — a position on whatever
//! line the two agree on; tutti-sampler's disk ring uses a file counted
//! straight on. The writer publishes the window `[from, to)` of positions the
//! slots hold as one packed word, appends at `to`, and may shrink the window
//! (a retraction, a reset); the reader claims the positions its block may
//! read, takes the window, and reads any position the window holds, in any
//! order, as often as it likes. Nothing is consumed and nothing on the
//! reading side locks, allocates or waits.
//!
//! # Why a write never tears a read
//!
//! Every sample is an `AtomicU32`, so a slip in the protocol below is a wrong
//! sample, never undefined behaviour; the protocol is what makes it never a
//! wrong sample. The reader, per block ([`claim`](PosRing::claim)):
//!
//! 1. stores where it plays and the ranges `[a, e)` its block may read;
//! 2. a `SeqCst` fence;
//! 3. loads the shrink generation `g` (`Acquire`);
//! 4. loads the window (`Acquire`: the frames it holds were stored before the
//!    writer's `Release` of it);
//! 5. echoes `g` as the generation it holds (`Release`).
//!
//! It then reads only positions the window holds, inside its ranges.
//!
//! The writer never writes a slot the block in flight may read:
//!
//! - **Going round the ring.** Writing position `s` reuses the slot of every
//!   `s ± kN`. Before a write the writer raises the window's start past
//!   `s - N`, then a `SeqCst` fence, then it loads the reader's ranges. The
//!   two fences order the reader's range stores against the writer's raise
//!   (C++20's fence rule, [atomics.fences]): either the reader's window load
//!   sees the raised start, or the writer's range load sees the reader's
//!   ranges, and skips any `s` whose slot another position of a range holds
//!   ([`first_alias`]).
//! - **Shrinking.** A retraction (the window's end lowered, to rewrite what
//!   lies past it) or a reset (the window emptied elsewhere) stores the shrunk
//!   window, *then* bumps the generation with `Release`. Positions the shrink
//!   removed may be in a block that took the window before it; the writer
//!   treats the in-range ones among them as taken until the reader echoes the
//!   new generation. The echo is `Release` and the writer's
//!   load of it `Acquire`, and the reader loaded the new generation with
//!   `Acquire` before its window: the claim that echoed it took the window
//!   after the shrink.
//!
//! A position *in* a range but never in any window the reader could hold is
//! free to write: the reader reads it only once a later window holds it.
//!
//! `tests/pos_ring_loom.rs` checks this code under loom (its atomics become
//! loom's under `--cfg loom`), including a model that fails without the
//! generation.

use std::sync::atomic::Ordering;

use sync::{fence, AtomicU32, AtomicU64};

/// The atomics: `std`'s in a real build, `loom`'s under `--cfg loom`, so the
/// model in `tests/pos_ring_loom.rs` checks this code rather than a replica.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{fence, AtomicU32, AtomicU64};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{fence, AtomicU32, AtomicU64};
}

/// Bits of the packed window word that hold its length; the rest its end.
const LEN_BITS: u32 = 24;

/// The most frames a ring holds: its window's length is packed in 24 bits.
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

/// Frames one writer places ahead of one reader, by position (see the module
/// docs).
///
/// The reader's methods are [`claim`](Self::claim) and
/// [`sample`](Self::sample); the rest are the writer's, and only one thread
/// may call them.
pub struct PosRing {
    stride: usize,
    frames: usize,
    /// Positions the writer keeps behind where the reader plays.
    keep_behind: u64,
    /// `frames * stride` samples, as `f32` bits.
    slots: Box<[AtomicU32]>,
    /// The packed [`RingWindow`].
    window: AtomicU64,
    /// Reader → writer: where the reader plays.
    play: AtomicU64,
    /// Reader → writer: the ranges its block may read, `[a0, e0)`, `[a1, e1)`.
    reads: [AtomicU64; 4],
    /// Writer → reader: bumped after every shrink.
    generation: AtomicU64,
    /// Reader → writer: the generation its latest claim loaded.
    claimed: AtomicU64,
    /// Writer only: the span of positions shrinks have removed since the
    /// reader last echoed the generation, `[stale_lo, stale_hi)` — what a
    /// block that took an older window may still hold.
    stale_lo: AtomicU64,
    stale_hi: AtomicU64,
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
    /// plays (the taps a position reads behind itself).
    pub fn new(frames: usize, stride: usize, keep_behind: u64) -> Self {
        let frames = frames.clamp(1, MAX_POS_RING_FRAMES);
        let stride = stride.max(1);
        Self {
            stride,
            frames,
            keep_behind,
            slots: (0..frames * stride).map(|_| AtomicU32::new(0)).collect(),
            window: AtomicU64::new(0),
            play: AtomicU64::new(0),
            reads: std::array::from_fn(|_| AtomicU64::new(0)),
            generation: AtomicU64::new(0),
            claimed: AtomicU64::new(0),
            stale_lo: AtomicU64::new(u64::MAX),
            stale_hi: AtomicU64::new(0),
        }
    }

    /// Slots, in frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Samples per frame.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The window now (for the writer, and for a reader's glance outside a
    /// claim; a claim returns the window it may read).
    #[inline]
    pub fn window(&self) -> RingWindow {
        RingWindow::unpack(self.window.load(Ordering::Acquire))
    }

    /// Reader, once per block before any read: where it plays, the ranges the
    /// block may read, and the window it may read them in (see the module
    /// docs for the order).
    #[inline]
    pub fn claim(&self, play: u64, reads: [(u64, u64); 2]) -> RingWindow {
        self.play.store(play, Ordering::Relaxed);
        for (i, (a, e)) in reads.into_iter().enumerate() {
            self.reads[2 * i].store(a, Ordering::Relaxed);
            self.reads[2 * i + 1].store(e, Ordering::Relaxed);
        }
        fence(Ordering::SeqCst);
        let generation = self.generation.load(Ordering::Acquire);
        let window = RingWindow::unpack(self.window.load(Ordering::Acquire));
        self.claimed.store(generation, Ordering::Release);
        window
    }

    /// Channel `c` of position `s`, which the caller's claimed window and
    /// ranges hold.
    #[inline]
    pub fn sample(&self, s: u64, c: usize) -> f32 {
        let slot = (s % self.frames as u64) as usize;
        f32::from_bits(self.slots[slot * self.stride + c].load(Ordering::Relaxed))
    }

    /// Where the reader plays (or the writer last said it would). Advisory:
    /// what the writer fills ahead of.
    pub fn play(&self) -> u64 {
        self.play.load(Ordering::Relaxed)
    }

    /// One past the last position the reader's block in flight may read, as
    /// of a `SeqCst` fence now.
    pub fn in_flight_end(&self) -> u64 {
        fence(Ordering::SeqCst);
        self.reads[1]
            .load(Ordering::Relaxed)
            .max(self.reads[3].load(Ordering::Relaxed))
    }

    /// Shrinks and moves so far: the generation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The reader's ranges; the caller has fenced.
    fn in_flight(&self) -> [(u64, u64); 2] {
        let r = |i: usize| self.reads[i].load(Ordering::Relaxed);
        [(r(0), r(1)), (r(2), r(3))]
    }

    /// Writer: say where the reader plays before it has (so the window is
    /// filled there).
    pub fn set_play(&self, play: u64) {
        self.play.store(play, Ordering::Relaxed);
    }

    /// Writer: shrink to `window`, removing `[removed.0, removed.1)`, then
    /// bump the generation (the order is the module docs' argument).
    fn shrink(&self, window: RingWindow, removed: (u64, u64)) {
        // What earlier shrinks removed stays taken only until the reader has
        // seen them: an echo of the current generation says it has.
        if self.claimed.load(Ordering::Acquire) == self.generation.load(Ordering::Relaxed) {
            self.stale_lo.store(u64::MAX, Ordering::Relaxed);
            self.stale_hi.store(0, Ordering::Relaxed);
        }
        self.window.store(window.pack(), Ordering::Release);
        if removed.0 < removed.1 {
            self.stale_lo.fetch_min(removed.0, Ordering::Relaxed);
            self.stale_hi.fetch_max(removed.1, Ordering::Relaxed);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Writer: drop every position at and past `x` (a rewrite from `x`
    /// follows).
    pub fn retract_to(&self, x: u64) {
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

    /// Writer: drop every position below `y`.
    pub fn raise_from(&self, y: u64) {
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

    /// Writer: an empty window at `start` (the reader moved outside the old
    /// one).
    pub fn reset(&self, start: u64) {
        let w = self.window();
        self.shrink(
            RingWindow {
                from: start,
                to: start,
            },
            (w.from, w.to),
        );
    }

    /// Writer: append frames at the window's end, returning how many
    /// **frames** landed — short where a slot the reader's block in flight may
    /// read would be written (see the module docs), or where the ring is full
    /// ahead of where the reader plays. A trailing partial frame is ignored.
    pub fn push(&self, samples: &[f32]) -> usize {
        let ch = self.stride;
        let n = (samples.len() / ch) as u64;
        let frames = self.frames as u64;
        let w = self.window();
        let (from, to) = (w.from, w.to);
        let play = self.play();
        // Never reuse the slot of a position the reader still stands on.
        let room = (play.saturating_sub(self.keep_behind) + frames).saturating_sub(to);
        let mut end = to + n.min(room);
        let new_from = from.max(end.saturating_sub(frames));
        if new_from != from {
            self.window
                .store(RingWindow { from: new_from, to }.pack(), Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let current = self.generation.load(Ordering::Relaxed);
        let held = self.claimed.load(Ordering::Acquire) != current;
        let stale = (
            self.stale_lo.load(Ordering::Relaxed),
            self.stale_hi.load(Ordering::Relaxed),
        );
        for (a, e) in self.in_flight() {
            end = end.min(first_alias(to, a, e, frames));
            // A block that took a window from before a shrink may hold the
            // positions it removed.
            let (lo, hi) = (a.max(to).max(stale.0), e.min(stale.1));
            if held && lo < hi {
                end = end.min(lo);
            }
        }
        if !held {
            self.stale_lo.store(u64::MAX, Ordering::Relaxed);
            self.stale_hi.store(0, Ordering::Relaxed);
        }
        if end <= to {
            return 0;
        }
        for (i, frame) in samples
            .chunks_exact(ch)
            .take((end - to) as usize)
            .enumerate()
        {
            let slot = ((to + i as u64) % frames) as usize;
            for (c, &s) in frame.iter().enumerate() {
                self.slots[slot * ch + c].store(s.to_bits(), Ordering::Relaxed);
            }
        }
        self.window.store(
            RingWindow {
                from: new_from.min(end),
                to: end,
            }
            .pack(),
            Ordering::Release,
        );
        (end - to) as usize
    }
}

/// The first position at or past `to` whose slot also holds a position of the
/// range `[a, e)` other than itself — an alias `[a + kN, e + kN)`, `k != 0`,
/// of a ring `n` slots long. Writing a position *in* the range is no conflict
/// of this kind (see [`PosRing::push`] for the other kind).
fn first_alias(to: u64, a: u64, e: u64, n: u64) -> u64 {
    if e <= a {
        return u64::MAX;
    }
    let len = e - a;
    let d = (to as i128 - a as i128).rem_euclid(n as i128) as u64;
    if d < len {
        return if (a..e).contains(&to) { a + n } else { to };
    }
    let next = to + (n - d);
    if next == a {
        a + n
    } else {
        next
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn indexed(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| i as f32).collect()
    }

    fn frame_at(ring: &PosRing, s: u64) -> Vec<f32> {
        (0..ring.stride()).map(|c| ring.sample(s, c)).collect()
    }

    /// **The window packs into one word and back**, up to its largest length
    /// and far along the line.
    ///
    /// Mutation (run): the length mask one bit short → the longest window
    /// unpacks short → fails.
    #[test]
    fn a_window_packs_into_one_word_and_back() {
        let far = 1u64 << 39;
        for (from, to) in [(0, 0), (5, 4_100), (far, far + MAX_POS_RING_FRAMES as u64)] {
            let w = RingWindow { from, to };
            assert_eq!(RingWindow::unpack(w.pack()), w);
        }
    }

    /// **Frames land at the window's end and read back by position**, every
    /// channel in its place at six channels, a trailing partial frame
    /// dropped.
    ///
    /// Mutation (run): the slot index taken in samples (`slot * ch` → `slot`)
    /// → channels rotate → fails.
    #[test]
    fn frames_land_at_the_windows_end_and_read_back_by_position() {
        let ring = PosRing::new(64, 6, 4);
        let mut data = indexed(10, 6);
        data.extend_from_slice(&[99.0, 99.0, 99.0]);
        assert_eq!(ring.push(&data), 10);
        assert_eq!(ring.window(), RingWindow { from: 0, to: 10 });
        for s in 0..10 {
            let want: Vec<f32> = (0..6).map(|c| (s as usize * 6 + c) as f32).collect();
            assert_eq!(frame_at(&ring, s), want, "frame {s}");
        }
    }

    /// **A write never goes round the ring onto the reader**: with the reader
    /// at 1 000, a ring of 4 096 that keeps 4 behind takes positions up to
    /// 1 000 - 4 + 4 096 and no further.
    ///
    /// Mutation (run): the room check removed → the push wraps onto 996.. →
    /// fails.
    #[test]
    fn a_write_never_goes_round_the_ring_onto_the_reader() {
        let ring = PosRing::new(4_096, 1, 4);
        ring.push(&indexed(1_000, 1));
        ring.set_play(1_000);
        assert_eq!(ring.push(&indexed(8_000, 1)), 4_096 - 4);
        assert_eq!(ring.window().to, 1_000 - 4 + 4_096);
        assert_eq!(frame_at(&ring, 996), [996.0]);
    }

    /// **A write never reuses a slot the block in flight reads**: a block
    /// reading `[3 997, 4 069)` whose jump copy reads `[100, 200)` keeps both
    /// — the writer stops at 4 196, the first position whose slot is 100's —
    /// while it writes the block's own positions freely.
    ///
    /// Mutation (run): the alias check removed → 4 196.. overwrite 100.. →
    /// fails. Mutation (run): a position *in* the range counted as its own
    /// alias → the writer stops at once → fails.
    #[test]
    fn a_write_never_reuses_a_slot_the_block_in_flight_reads() {
        let ring = PosRing::new(4_096, 1, 4);
        ring.push(&indexed(4_000, 1));
        let window = ring.claim(3_997, [(3_997, 4_069), (100, 200)]);
        assert_eq!(window, RingWindow { from: 0, to: 4_000 });
        assert_eq!(ring.push(&[-1.0; 300]), 196, "stops at 4 196");
        assert_eq!(frame_at(&ring, 150), [150.0]);
    }

    /// **A shrink does not free what a block in flight holds** (the second
    /// review of #48, B3): a block claims `[600, 680)` in the window `[0,
    /// 1 000)`; the writer retracts to 620 and pushes a rewrite — it must not
    /// touch 620.. until the reader claims again, after which it may. The
    /// same for a reset below the old end.
    ///
    /// Mutation (run): the generation check removed from `push` → frame 650
    /// changes under the claim → fails. And a reset that moves the window
    /// away from a claim frees the new place at once: what it removed is the
    /// old window, not everything below its end (a first cut blocked a seek's
    /// refill until the reader claimed again, and the refill reset the window
    /// again every cycle). Mutation (run): `shrink` bumping the
    /// generation *before* storing the window → no single-threaded change
    /// (the loom model `retract_then_push` catches the order).
    #[test]
    fn a_shrink_does_not_free_what_a_block_in_flight_holds() {
        for reset in [false, true] {
            let ring = PosRing::new(4_096, 1, 4);
            ring.push(&indexed(1_000, 1));
            let window = ring.claim(600, [(600, 680), (0, 0)]);
            assert!(window.holds(650));
            if reset {
                ring.reset(620);
            } else {
                ring.retract_to(620);
            }
            assert_eq!(
                ring.push(&[-1.0; 100]),
                0,
                "reset {reset}: wrote under the claim"
            );
            assert_eq!(frame_at(&ring, 650), [650.0], "reset {reset}");
            // The next claim sees the shrink: the rewrite may land.
            let window = ring.claim(600, [(600, 680), (0, 0)]);
            assert!(!window.holds(650));
            assert_eq!(ring.push(&[-1.0; 100]), 100, "reset {reset}");
        }
        // A reset away from a claim (a seek back, below the old window) frees
        // the new place at once: the old window was `[5 000, 6 000)`.
        let ring = PosRing::new(4_096, 1, 4);
        ring.reset(5_000);
        ring.set_play(5_000);
        ring.push(&indexed(1_000, 1));
        ring.claim(1_000, [(997, 1_069), (0, 0)]);
        ring.reset(996);
        ring.set_play(1_000);
        assert_eq!(ring.push(&[-1.0; 100]), 100, "a seek's refill was held");
    }

    /// **Raising the window's start drops the positions below it**, so a
    /// reader that jumps back there finds the window without them — and once
    /// the reader has claimed since, a reset there refills at once: what the
    /// raise removed is no longer counted as held.
    ///
    /// Mutation (run): the stale span not cleared at a shrink the reader has
    /// seen → the reset's refill is held for a cycle → fails.
    #[test]
    fn raising_the_start_drops_what_lies_below() {
        let ring = PosRing::new(4_096, 1, 4);
        ring.push(&indexed(1_000, 1));
        ring.raise_from(400);
        assert_eq!(
            ring.window(),
            RingWindow {
                from: 400,
                to: 1_000
            }
        );
        assert_eq!(ring.generation(), 1);
        ring.claim(500, [(497, 569), (0, 0)]);
        // The reader jumps back below the raise; the writer moves the window.
        ring.claim(100, [(97, 169), (0, 0)]);
        ring.reset(96);
        ring.set_play(100);
        assert_eq!(ring.push(&[-1.0; 100]), 100, "the refill was held");
    }

    /// `first_alias` against a brute-force search, over ranges before, around
    /// and after the write's start, near and across a lap of the ring.
    ///
    /// Mutation (run): `a + n` returned as `a` for a range containing the
    /// start → fails.
    #[test]
    fn first_alias_is_the_first_slot_another_position_of_the_range_holds() {
        let n = 16u64;
        for to in 0..64u64 {
            for a in 0..64u64 {
                for len in 0..8u64 {
                    let e = a + len;
                    let brute = (to..to + 3 * n)
                        .find(|&s| (a..e).any(|q| q != s && q % n == s % n))
                        .unwrap_or(u64::MAX);
                    let got = first_alias(to, a, e, n);
                    assert!(
                        got == brute || (brute == u64::MAX && got >= to + 3 * n),
                        "to {to} range [{a}, {e}): {got}, brute {brute}"
                    );
                }
            }
        }
    }

    /// **A writer thread and a reader thread, free-running**: the writer
    /// pushes, retracts and resets at random; the reader claims random ranges
    /// and checks every sample it reads in its window is the tag of that
    /// position (both channels, twice), so no write ever lands under a claim.
    /// Real threads, not a model: it complements `tests/pos_ring_loom.rs` at a
    /// scale loom cannot explore.
    ///
    /// Mutation (run): the generation check removed from `push` → a torn or
    /// rewritten frame under a claim within the run → fails.
    #[test]
    fn a_writer_and_a_reader_thread_never_tear() {
        use std::sync::atomic::{AtomicBool, Ordering as O};
        use std::sync::Arc;
        const N: usize = 64;
        let ring = Arc::new(PosRing::new(N, 2, 2));
        let done = Arc::new(AtomicBool::new(false));
        // A tag: the position, the rewrite generation, the channel — kept
        // under 2^24, where an `f32` holds every integer exactly.
        let tag =
            |pos: u64, gen: u64, c: usize| ((pos % 8_192) * 128 + (gen % 32) * 2 + c as u64) as f32;
        let reader = {
            let (ring, done) = (Arc::clone(&ring), Arc::clone(&done));
            std::thread::spawn(move || {
                let mut seed = 0x9e37_79b9_u64;
                let mut checked = 0u64;
                while !done.load(O::Relaxed) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let w = ring.window();
                    let a = w.from + seed % (w.to - w.from + 1).max(1);
                    let e = a + 1 + (seed >> 20) % 8;
                    let window = ring.claim(a, [(a, e), (0, 0)]);
                    for s in a..e {
                        if !window.holds(s) {
                            continue;
                        }
                        let (x0, x1, y0) =
                            (ring.sample(s, 0), ring.sample(s, 1), ring.sample(s, 0));
                        let pos = (x0 as u64) / 128;
                        assert_eq!(pos, s % 8_192, "position {s} held another's frame");
                        assert_eq!(x1, x0 + 1.0, "position {s} torn across channels");
                        assert_eq!(y0, x0, "position {s} rewritten under the claim");
                        checked += 1;
                    }
                }
                checked
            })
        };
        let mut seed = 0x2545_f491_u64;
        let mut gen = 0u64;
        let mut buf = Vec::new();
        for _ in 0..200_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let w = ring.window();
            match seed % 16 {
                0 => {
                    gen += 1;
                    ring.retract_to(w.from + (seed >> 8) % (w.to - w.from + 1));
                }
                1 => {
                    gen += 1;
                    ring.reset(ring.play().saturating_sub(2) + (seed >> 8) % 8);
                }
                _ => {
                    let to = ring.window().to;
                    let n = 1 + (seed >> 8) as usize % 16;
                    buf.clear();
                    for i in 0..n as u64 {
                        buf.push(tag(to + i, gen, 0));
                        buf.push(tag(to + i, gen, 1));
                    }
                    ring.push(&buf);
                }
            }
        }
        done.store(true, O::Relaxed);
        let checked = reader.join().expect("the reader never saw a torn frame");
        assert!(checked > 1_000, "the reader checked only {checked} frames");
    }
}

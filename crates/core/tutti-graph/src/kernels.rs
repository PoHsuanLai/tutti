//! The graph's own DSP: PDC rings for audio and for events.
//!
//! Doc 013 §"SIMD and DOD": the graph's own work is a handful of kernels on
//! planar slices. These are written as straight loops over `&[f32]` so they
//! auto-vectorize where the shape allows; nothing here is on a hot enough path
//! yet to earn explicit SIMD.
//!
//! # No summing kernel, yet
//!
//! There is no audio fan-in kernel here because audio fan-in is
//! unrepresentable (one source per port; summing is a node). When doc 013's
//! compiler-owned `Sum` op lands (the `ChannelSumNode` verdict), it should
//! accumulate in `f64` and store `f32`: a many-input bus is where `f32`
//! accumulation error compounds, and the widening is cheap next to the loads.
//! The event merge is not arithmetic and has no such question.
//!
//! # Retuning keeps history
//!
//! A ring's state is keyed by its [`DelayKey`](crate::DelayKey) and survives a
//! recompile. When the recompile changes its *length* from `n` to `m`, it keeps
//! the most recent `min(n, m)` inputs and zero-fills the older end — exactly
//! the state a ring of length `m` would have had if it had seen the same
//! inputs, as far as the kept history goes. The reference interpreter states
//! the same rule independently, and the recompile differential test holds the
//! two to it.

use std::collections::VecDeque;

use tutti_types::Samples;

use crate::event::Event;
use crate::time::Offset;

/// An audio delay line of fixed length.
pub(crate) struct AudioRing {
    buf: Vec<f32>,
    pos: usize,
    /// Consecutive silent input frames seen, saturating. Once it reaches the
    /// ring length the ring holds only zeros, so a silent input gives a silent
    /// output.
    quiet: usize,
}

impl AudioRing {
    pub(crate) fn new(len: Samples) -> Self {
        Self {
            buf: vec![0.0; len.get().max(1)],
            pos: 0,
            quiet: usize::MAX,
        }
    }

    /// The ring's inputs, oldest first.
    fn history(&self) -> impl Iterator<Item = f32> + '_ {
        self.buf[self.pos..]
            .iter()
            .chain(&self.buf[..self.pos])
            .copied()
    }

    /// A ring of `len` already full of `value`, as if it had been fed
    /// `value` forever: a param source's delay that appears under a running
    /// port (see the `param` module docs).
    pub(crate) fn filled(len: Samples, value: f32) -> Self {
        Self {
            buf: vec![value; len.get().max(1)],
            pos: 0,
            quiet: if value == 0.0 { usize::MAX } else { 0 },
        }
    }

    /// Change the length, keeping the most recent `min(old, new)` inputs.
    /// Control side: allocates.
    pub(crate) fn retune(&mut self, len: Samples) {
        self.retune_padded(len, 0.0);
    }

    /// [`retune`](Self::retune), padding a longer ring's oldest end with
    /// `pad` instead of silence.
    pub(crate) fn retune_padded(&mut self, len: Samples, pad: f32) {
        let m = len.get().max(1);
        let n = self.buf.len();
        if m == n {
            return;
        }
        if pad != 0.0 {
            self.quiet = 0;
        }
        let mut next = vec![pad; m];
        let keep = n.min(m);
        for (slot, x) in next[m - keep..]
            .iter_mut()
            .zip(self.history().skip(n - keep))
        {
            *slot = x;
        }
        self.buf = next;
        self.pos = 0;
    }

    /// Delay `src` into `dst`. Returns whether `dst` is known silent.
    pub(crate) fn run(&mut self, src: &[f32], dst: &mut [f32], src_silent: bool) -> bool {
        let n = self.buf.len();
        for (o, &x) in dst.iter_mut().zip(src) {
            *o = self.buf[self.pos];
            self.buf[self.pos] = x;
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
        }
        self.account(src.len(), src_silent)
    }

    /// Delay `io` in place. Returns whether it is known silent afterwards.
    pub(crate) fn run_in_place(&mut self, io: &mut [f32], src_silent: bool) -> bool {
        let n = self.buf.len();
        for x in io.iter_mut() {
            std::mem::swap(&mut self.buf[self.pos], x);
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
        }
        self.account(io.len(), src_silent)
    }

    fn account(&mut self, frames: usize, src_silent: bool) -> bool {
        // The output is silent when the input is AND every sample still in
        // the ring from before this block was silent too.
        let silent = src_silent && self.quiet >= self.buf.len();
        self.quiet = if src_silent {
            self.quiet.saturating_add(frames)
        } else {
            0
        };
        silent
    }
}

impl AudioRing {
    /// Copy the oldest `dst.len()` samples, without consuming them — the
    /// read half of a fixed-length feedback delay, whose write half
    /// ([`push`](Self::push)) runs later in the same block. Returns whether
    /// they are known silent.
    pub(crate) fn peek_oldest(&self, dst: &mut [f32]) -> bool {
        let n = self.buf.len();
        debug_assert!(dst.len() <= n, "a feedback read is at most one ring long");
        for (i, o) in dst.iter_mut().enumerate() {
            *o = self.buf[(self.pos + i) % n];
        }
        self.quiet >= n
    }

    /// Append `src`, displacing the oldest `src.len()` samples.
    pub(crate) fn push(&mut self, src: &[f32], src_silent: bool) {
        let n = self.buf.len();
        for &x in src {
            self.buf[self.pos] = x;
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
        }
        self.account(src.len(), src_silent);
    }
}

/// An event delay: a FIFO of `(input time, event)` on the delay's own clock.
///
/// Input time rather than due time is stored so a retune can reschedule: an
/// event already past due under the new length is delivered at offset 0 of the
/// next block rather than dropped — a dropped note-off is a stuck note.
///
/// # Capacity, and what is never dropped
///
/// Sized on the control side from a **declared rate**: at most `cap` events
/// per port per `max_block` frames (the same `cap` every event slot holds per
/// block). A delay of `len` frames then holds at most
/// `cap × (⌈len / max_block⌉ + 2)` events; that is the `limit`. Above it,
/// a quarter again is **reserved for note-offs**. Past the limit an incoming
/// non-note-off is dropped (and counted); a note-off uses the reserve, and if
/// even that is full it evicts the oldest non-note-off instead. A note-off is
/// dropped only when the whole FIFO is note-offs.
///
/// The queue never holds more than `limit + reserve` ([`bound`](Self::bound)),
/// and the compiler prices the delay's output slot at that same bound
/// (`EventSlotCapacity::fifo`), so everything that falls due in a block —
/// events input across several source blocks, or a backlog a retune made
/// overdue — is delivered in it. Delivery never drops: should the slot be
/// full anyway (a FIFO carried across a recompile that shrank its bound
/// while holding more than the new one), the due events stay queued and go
/// out at offset 0 of the next block — late, not lost.
pub(crate) struct EventFifo {
    q: VecDeque<(u64, Event)>,
    len: u64,
    clock: u64,
    limit: usize,
    reserve: usize,
}

impl EventFifo {
    /// A FIFO for a delay of `len`, sized from the declared rate (see the
    /// type docs). Control side: allocates.
    pub(crate) fn sized(len: Samples, cap: usize, max_block: usize) -> Self {
        let (limit, reserve) = Self::bounds(len.get(), cap, max_block);
        Self {
            q: VecDeque::with_capacity(limit + reserve),
            len: len.get() as u64,
            clock: 0,
            limit,
            reserve,
        }
    }

    /// The most a FIFO for a delay of `len` at this rate ever holds: its
    /// limit plus the note-off reserve. What its output slot is sized for.
    pub(crate) fn bound(len: usize, cap: usize, max_block: usize) -> usize {
        let (limit, reserve) = Self::bounds(len, cap, max_block);
        limit + reserve
    }

    /// `⌈len / max_block⌉ + 2`: how many declared blocks' worth of events a
    /// delay of `len` holds at most (see the type docs). The compiler prices
    /// with it (`EventSlotCapacity::fifo`), so it and [`bounds`](Self::bounds)
    /// are one formula.
    pub(crate) fn blocks(len: usize, max_block: usize) -> usize {
        len.div_ceil(max_block.max(1)) + 2
    }

    /// `(limit, note-off reserve)` for a delay of `len` at the declared rate.
    fn bounds(len: usize, cap: usize, max_block: usize) -> (usize, usize) {
        let limit = cap.max(1) * Self::blocks(len, max_block);
        (limit, limit / 4 + 8)
    }

    pub(crate) fn retune(&mut self, len: Samples) {
        self.len = len.get() as u64;
    }

    /// Re-derive the limit for the current length at this rate and maximum
    /// block — after a retune, a re-prepare that shrank `MaxBlock` (more
    /// blocks fit in the same delay, so more events may be in flight), or a
    /// source that now declares a different rate (`Shape::event_capacity`,
    /// a unit hard-replaced by one with another declaration). Keeps every
    /// queued event; grows the queue when the new bounds need it, never
    /// shrinks it. Control side: may allocate.
    ///
    /// The limit is the new bound, not the new bound plus what is queued:
    /// the output slot is priced at the bound, so a queue past it could not
    /// be delivered in one block. Queued events past a smaller new bound are
    /// kept (and go out late if they all fall due at once); new ones wait
    /// for room.
    pub(crate) fn resize(&mut self, cap: usize, max_block: usize) {
        let (limit, reserve) = Self::bounds(self.len as usize, cap, max_block);
        self.limit = limit;
        self.reserve = reserve;
        let want = limit + reserve;
        if self.q.capacity() < want {
            self.q.reserve_exact(want - self.q.len());
        }
    }

    /// Every event still queued, oldest first.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> impl Iterator<Item = Event> + '_ {
        self.q.iter().map(|&(_, e)| e)
    }

    /// The queued events for a flush, when the delay itself goes away: each
    /// at its due time relative to the earliest, so a note-on/note-off pair
    /// keeps its length. Control side (allocates).
    pub(crate) fn flushed(&self) -> Vec<Event> {
        let first = self.q.front().map_or(0, |&(t, _)| t + self.len);
        // Raw offsets: the spacing, not yet inside any block — the block
        // that delivers them clamps them (`Offset::clamp_to`).
        self.q
            .iter()
            .map(|&(t, e)| Event {
                offset: Offset::raw((t + self.len - first).min(u64::from(u32::MAX)) as u32),
                ..e
            })
            .collect()
    }

    /// Queue this block's `input`. Returns how many were dropped.
    pub(crate) fn push(&mut self, input: &[Event]) -> u32 {
        let mut dropped = 0;
        let start = self.clock;
        for e in input {
            let item = (start + u64::from(e.offset.get()), *e);
            if self.q.len() < self.limit {
                self.q.push_back(item);
            } else if !e.is_note_off() {
                dropped += 1;
            } else if self.q.len() < self.limit + self.reserve {
                self.q.push_back(item);
            } else if let Some(i) = self.q.iter().position(|(_, x)| !x.is_note_off()) {
                // `remove` shifts in place and never reallocates, but it is
                // O(n) in the queue length, as is the `position` scan. That
                // is paid only on overflow — the rate the FIFO was sized for
                // has already been exceeded — so it buys note-off safety at
                // the one moment it is needed, not every block.
                self.q.remove(i);
                self.q.push_back(item);
                dropped += 1;
            } else {
                dropped += 1;
            }
        }
        dropped
    }

    /// Emit into `out` what falls due in the block of `frames` starting at
    /// the FIFO's clock. Stops, without dropping, when `out` is full.
    pub(crate) fn pop_due(&mut self, out: &mut Vec<Event>, frames: usize) {
        let start = self.clock;
        let end = start + frames as u64;
        while let Some(&(t, e)) = self.q.front() {
            let due = t + self.len;
            if due >= end || out.len() >= out.capacity() {
                break;
            }
            self.q.pop_front();
            // `start <= … < end`: inside this block by the loop condition.
            out.push(Event {
                offset: Offset::raw(due.saturating_sub(start) as u32),
                ..e
            });
        }
    }

    /// Move the clock past a block of `frames`.
    pub(crate) fn advance(&mut self, frames: usize) {
        self.clock += frames as u64;
    }

    /// A PDC delay's whole block: emit what was already queued and falls
    /// due, queue the block's input, emit what of it falls due too, advance.
    ///
    /// The backlog goes out **before** the input is queued: the limit bounds
    /// events in flight, and an event leaving in this very block is not. A
    /// FIFO a recompile retuned shorter holds a backlog that is all due at
    /// once; queued first, the input would have been refused against room
    /// the backlog was about to free (the reference interpreter, which
    /// bounds nothing, delivered it). Order is kept: every queued event was
    /// input before this block's, at the same length, so it falls due no
    /// later.
    pub(crate) fn run(&mut self, input: &[Event], out: &mut Vec<Event>, frames: usize) -> u32 {
        self.pop_due(out, frames);
        let dropped = self.push(input);
        self.pop_due(out, frames);
        self.advance(frames);
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring of `n` outputs its input `n` samples later, across blocks.
    ///
    /// Mutation: write the input before reading the output in `run` → the
    /// ring passes its input straight through → fails.
    #[test]
    fn ring_delays_by_its_length() {
        let mut r = AudioRing::new(Samples(3));
        let src: Vec<f32> = (1..=8).map(|x| x as f32).collect();
        let mut dst = vec![0.0; 8];
        r.run(&src[..5], &mut dst[..5], false);
        r.run(&src[5..], &mut dst[5..], false);
        assert_eq!(dst, vec![0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    /// Retuning keeps the newest inputs and zero-pads the oldest end.
    ///
    /// Mutation: keep the *oldest* `keep` inputs instead (`.take(keep)`) →
    /// shrinking to 2 outputs [1, 2] instead of [3, 4] → fails.
    #[test]
    fn retune_keeps_recent_history() {
        let mut r = AudioRing::new(Samples(4));
        let mut sink = vec![0.0; 4];
        r.run(&[1.0, 2.0, 3.0, 4.0], &mut sink, false);
        r.retune(Samples(2));
        let mut out = vec![0.0; 2];
        r.run(&[0.0, 0.0], &mut out, false);
        assert_eq!(out, vec![3.0, 4.0]);
        r.retune(Samples(4));
        let mut out = vec![0.0; 4];
        r.run(&[0.0; 4], &mut out, false);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.0]);
    }

    /// A filled ring outputs its value for its length, then its input; a
    /// padded retune outputs the pad before the kept history.
    ///
    /// Mutation (run): `filled` filling with 0 → the first three outputs
    /// are 0 → fails. `retune_padded` padding with 0 → the grown ring's
    /// first output is 0 → fails.
    #[test]
    fn a_filled_ring_holds_its_value_and_a_padded_retune_its_pad() {
        let mut r = AudioRing::filled(Samples(3), 0.5);
        let mut out = vec![0.0; 5];
        r.run(&[1.0, 2.0, 3.0, 4.0, 5.0], &mut out, false);
        assert_eq!(out, vec![0.5, 0.5, 0.5, 1.0, 2.0]);
        r.retune_padded(Samples(5), 9.0);
        let mut out = vec![0.0; 5];
        r.run(&[0.0; 5], &mut out, false);
        assert_eq!(out, vec![9.0, 9.0, 3.0, 4.0, 5.0]);
    }

    /// An event is delivered `len` frames later, in whichever block that is.
    ///
    /// Mutation: `due >= end` → `due > end` → the event at the block boundary
    /// is emitted one block early with offset == frames → fails.
    #[test]
    fn fifo_delays_events_across_blocks() {
        let mut f = EventFifo::sized(Samples(6), 8, 8);
        let mut out = Vec::with_capacity(8);
        f.run(&[Event::midi(Offset::raw(2), [7, 0, 0, 0])], &mut out, 8);
        assert!(out.is_empty(), "due at 8, which is the next block");
        f.run(&[], &mut out, 8);
        assert_eq!(out, vec![Event::midi(Offset::ZERO, [7, 0, 0, 0])]);
    }

    /// A backlog due this block frees its room before the block's input is
    /// queued: a FIFO holding its limit's worth of overdue events (a retune
    /// made them due at once) takes a full block of new events without
    /// refusing one, and delivers all of them in order.
    ///
    /// Mutation (run): queue the input before emitting the backlog in `run`
    /// (the old order) → the input past the limit is refused → fails. Found
    /// as `recompiles_preserve_state_identically`'s seed
    /// 3280887136571273968 (the executor dropped an event the reference
    /// delivered).
    #[test]
    fn a_due_backlog_makes_room_for_the_blocks_input() {
        let mut f = EventFifo::sized(Samples(64), 2, 64);
        let limit = f.limit;
        let backlog: Vec<Event> = (0..limit as u32).map(|i| note(i, false, i)).collect();
        f.push(&backlog);
        // Shorter: the whole backlog is overdue.
        f.retune(Samples(1));
        f.resize(2, 64);
        let input = [note(0, false, 100), note(1, false, 101)];
        let mut out = Vec::with_capacity(EventFifo::bound(1, 2, 64));
        let dropped = f.run(&input, &mut out, 64);
        assert_eq!(dropped, 0, "nothing refused");
        let tags: Vec<u32> = out
            .iter()
            .map(|e| match e.kind {
                crate::event::EventKind::Midi(crate::event::Ump(w)) => (w[0] >> 8) & 0x7f,
                _ => unreachable!("notes only"),
            })
            .collect();
        let want: Vec<u32> = (0..limit as u32).chain([100, 101]).collect();
        assert_eq!(
            tags, want,
            "the backlog first, then the input, all this block"
        );
    }

    /// A feedback ring read before it is written delays by exactly its
    /// length, whatever the block sizes.
    ///
    /// Mutation: in `peek_oldest`, read from `pos + n - dst.len()` (the
    /// newest samples) → the ragged schedule reads the wrong samples → fails.
    #[test]
    fn a_feedback_ring_delays_by_its_length_across_ragged_blocks() {
        let mut r = AudioRing::new(Samples(8));
        let src: Vec<f32> = (1..=40).map(|x| x as f32).collect();
        let mut got = Vec::new();
        let mut at = 0;
        for n in [8, 3, 1, 8, 5, 7, 8] {
            let mut out = vec![0.0; n];
            r.peek_oldest(&mut out);
            r.push(&src[at..at + n], false);
            got.extend(out);
            at += n;
        }
        let want: Vec<f32> = (0..at)
            .map(|i| if i < 8 { 0.0 } else { src[i - 8] })
            .collect();
        assert_eq!(got, want);
    }

    fn note(offset: u32, off: bool, tag: u32) -> Event {
        // MIDI 1.0 channel voice in UMP: 0x2 group 0, status 0x8 (off) or
        // 0x9 (on), note `tag`, velocity 100.
        let status = if off { 0x80 } else { 0x90 };
        Event::midi(
            Offset::raw(offset),
            [
                0x2000_0000 | (status << 16) | ((tag & 0x7f) << 8) | 100,
                0,
                0,
                0,
            ],
        )
    }

    /// A note-off survives an overflow: past the limit it takes the reserve,
    /// and when even that is full it evicts the oldest non-note-off.
    ///
    /// Mutation: drop the note-off branches in `push` (treat every event
    /// alike) → the note-off is refused like the note-ons → fails.
    #[test]
    fn a_note_off_survives_overflow() {
        let mut f = EventFifo::sized(Samples(4), 1, 4);
        let limit = f.limit;
        let cap = f.q.capacity();
        let ons: Vec<Event> = (0..cap as u32 + 5).map(|i| note(0, false, i)).collect();
        let dropped = f.push(&ons);
        assert_eq!(f.q.len(), limit, "note-ons stop at the limit");
        assert_eq!(dropped as usize, ons.len() - limit);
        // Fill the reserve with note-offs, then one more.
        let offs: Vec<Event> = (0..(cap - limit) as u32 + 1)
            .map(|i| note(0, true, i))
            .collect();
        f.push(&offs);
        let kept_offs = f.pending().filter(Event::is_note_off).count();
        assert_eq!(kept_offs, offs.len(), "every note-off is kept");
        assert!(f.q.len() <= cap, "and the FIFO never grew");
    }

    /// The queue never passes its bound, however much room its allocation
    /// has: a FIFO resized from a longer delay keeps its larger allocation,
    /// but its note-off reserve is counted against the new bound, which is
    /// what its output slot is priced at.
    ///
    /// Mutation: check the reserve against `q.capacity()` in `push` → the
    /// shrunk FIFO takes note-offs up to its old allocation → fails.
    #[test]
    fn a_fifo_never_holds_more_than_its_bound() {
        let mut f = EventFifo::sized(Samples(64), 4, 8);
        f.retune(Samples(1));
        f.resize(4, 8);
        let offs: Vec<Event> = (0..200).map(|i| note(0, true, i)).collect();
        f.push(&offs);
        assert_eq!(f.pending().count(), EventFifo::bound(1, 4, 8));
    }

    /// Delivery never drops: when the output slot is full, due events stay
    /// queued and go out at offset 0 of the next block — late, not lost.
    ///
    /// Mutation: in `pop_due`, pop and discard when `out` is full instead of
    /// stopping → three of the five events vanish → fails.
    #[test]
    fn a_full_output_slot_delays_events_rather_than_dropping_them() {
        let mut f = EventFifo::sized(Samples(1), 16, 8);
        let burst: Vec<Event> = (0..5)
            .map(|i| Event::midi(Offset::ZERO, [i, 0, 0, 0]))
            .collect();
        f.push(&burst);
        let mut out = Vec::with_capacity(2);
        f.pop_due(&mut out, 8);
        f.advance(8);
        assert_eq!(out.len(), 2, "the slot takes two");
        let mut got = out.clone();
        for _ in 0..3 {
            out.clear();
            f.pop_due(&mut out, 8);
            f.advance(8);
            assert!(
                out.iter().all(|e| e.offset == Offset::ZERO),
                "late events land at 0"
            );
            got.extend(out.iter().copied());
        }
        let tags: Vec<u32> = got
            .iter()
            .map(|e| match e.kind {
                crate::event::EventKind::Midi(crate::event::Ump(w)) => w[0],
                crate::event::EventKind::Ramp(_) => unreachable!(),
            })
            .collect();
        assert_eq!(tags, vec![0, 1, 2, 3, 4], "all five, in order");
    }
}

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

    /// Change the length, keeping the most recent `min(old, new)` inputs.
    /// Control side: allocates.
    pub(crate) fn retune(&mut self, len: Samples) {
        let m = len.get().max(1);
        let n = self.buf.len();
        if m == n {
            return;
        }
        let mut next = vec![0.0; m];
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

/// An event delay: a FIFO of `(input time, event)` on the delay's own clock.
///
/// Input time rather than due time is stored so a retune can reschedule: an
/// event already past due under the new length is delivered at offset 0 of the
/// next block rather than dropped — a dropped note-off is a stuck note.
pub(crate) struct EventFifo {
    q: VecDeque<(u64, Event)>,
    len: u64,
    clock: u64,
}

impl EventFifo {
    pub(crate) fn new(len: Samples, cap: usize) -> Self {
        Self {
            q: VecDeque::with_capacity(cap),
            len: len.get() as u64,
            clock: 0,
        }
    }

    pub(crate) fn retune(&mut self, len: Samples) {
        self.len = len.get() as u64;
    }

    /// Take `input` (this block's events), emit what falls due in the block
    /// into `out`. Returns how many events did not fit.
    pub(crate) fn run(&mut self, input: &[Event], out: &mut Vec<Event>, frames: usize) -> u32 {
        let mut dropped = 0;
        let start = self.clock;
        for e in input {
            if self.q.len() < self.q.capacity() {
                self.q.push_back((start + e.offset as u64, *e));
            } else {
                dropped += 1;
            }
        }
        let end = start + frames as u64;
        while let Some(&(t, e)) = self.q.front() {
            let due = t + self.len;
            if due >= end {
                break;
            }
            self.q.pop_front();
            let offset = due.saturating_sub(start) as u32;
            if out.len() < out.capacity() {
                out.push(Event { offset, ..e });
            } else {
                dropped += 1;
            }
        }
        self.clock = end;
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

    /// An event is delivered `len` frames later, in whichever block that is.
    ///
    /// Mutation: `due >= end` → `due > end` → the event at the block boundary
    /// is emitted one block early with offset == frames → fails.
    #[test]
    fn fifo_delays_events_across_blocks() {
        let mut f = EventFifo::new(Samples(6), 8);
        let mut out = Vec::with_capacity(8);
        f.run(&[Event::midi(2, [7, 0, 0, 0])], &mut out, 8);
        assert!(out.is_empty(), "due at 8, which is the next block");
        f.run(&[], &mut out, 8);
        assert_eq!(out, vec![Event::midi(0, [7, 0, 0, 0])]);
    }
}

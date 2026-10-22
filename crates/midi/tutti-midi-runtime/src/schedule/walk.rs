//! The walk a beat-scheduled event source makes through each block's
//! transport: which of its sorted events playback reaches, on which frame,
//! and where playback jumps. Shared by [`MidiClipNode`](super::MidiClipNode)
//! and [`HarmonyNode`](super::HarmonyNode).
//!
//! It keeps **no play cursor**. Which events fall in a segment is a binary
//! search over the sorted events and the engine's one beat→frame rule
//! ([`Env::due`]), so a seek, a loop wrap or a tempo change inside a block
//! needs no bookkeeping, and an offline fork walks its render's `Env`. What it
//! keeps is where the next segment starts if playback is continuous, which is
//! how it tells a jump (a start, a seek, a loop wrap, a restart) from the
//! next block of the same run.

use tutti_core::At;
use tutti_graph::{Due, Env, Offset};

/// An event scheduled at a beat.
pub(crate) trait Beated {
    /// The beat it is scheduled at.
    fn beat(&self) -> f64;
}

/// What a source does as the walk goes: [`Walk::segment`] calls these in
/// frame order.
pub(crate) trait Visit<T> {
    /// Playback is stopped from `at` (or has no usable tempo).
    fn stop(&mut self, at: Option<Offset>);
    /// Playback jumped to `beat` at `at`: a start, a seek, a loop wrap, or a
    /// walk told to [`forget`](Walk::forget). `events` are the source's.
    fn jump(&mut self, at: Option<Offset>, beat: f64, events: &[T]);
    /// `event` is due at `at`.
    fn event(&mut self, at: Offset, event: &T);
}

/// Where playback was: the beat the next segment starts on if it is
/// continuous. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    /// `None` when stopped, before the first block, and after
    /// [`forget`](Self::forget).
    expected: Option<f64>,
}

impl Walk {
    /// Make the next segment read as a jump (its source's events changed).
    pub(crate) fn forget(&mut self) {
        self.expected = None;
    }

    /// Walk `seg`, the block's piece from `start`: every event of `events`
    /// (sorted by beat) that playback reaches in it, and where it stops or
    /// jumps.
    pub(crate) fn segment<T: Beated>(
        &mut self,
        events: &[T],
        block: &Env,
        start: Offset,
        seg: &Env,
        v: &mut impl Visit<T>,
    ) {
        let t = seg.transport;
        let len = seg.block_len.get();
        let at = |k: usize| Offset::new(start.index() + k, block.block_len);
        let Some(fpb) = frames_per_beat(seg).filter(|_| t.playing) else {
            v.stop(at(0));
            self.expected = None;
            return;
        };
        let now = t.beat().get();
        let frame = 1.0 / fpb;
        // A jump is a start more than half a frame away from where the last
        // segment left off (a seek, a wrap at a block edge, a restart).
        let continuous = self.expected.is_some_and(|e| ((now - e) * fpb).abs() < 0.5);
        if !continuous {
            v.jump(at(0), now, events);
        }

        // Where the loop, if the playhead is inside it, first wraps in this
        // segment: `len` when it does not.
        let looping = t
            .looping
            .filter(|l| l.start.get() < l.end.get() && now < l.end.get());
        let reach = looping.map(|l| {
            let k = tutti_core::first_frame_at_or_after((l.end.get() - now) * fpb).max(0);
            usize::try_from(k).unwrap_or(usize::MAX)
        });
        let wrap = reach.map_or(len, |k| k.min(len));
        // Whether the loop wraps exactly at this segment's end: the next
        // segment then starts on the loop's start, which reads as
        // continuous, so the jump a wrap is is made there (`expected`
        // forgotten below).
        let mut wraps_at_end = reach == Some(len);

        // Up to the wrap: a beat less than a frame behind the playhead falls
        // on the first frame (`Env::due`), so the range starts a frame back.
        let hi = now + (wrap as f64 + 1.0) * frame;
        emit(events, now - frame, hi, seg, v, &at, |k| k < wrap);
        if let (Some(l), true) = (looping, wrap < len) {
            wraps_at_end =
                Passes::new(l.start.get(), l.end.get(), now, fpb).walk(events, wrap, len, v, &at);
        }

        // Where the next segment starts if nothing jumps.
        let mut next = now + len as f64 * frame;
        if let Some(l) = looping {
            let (start, end) = (l.start.get(), l.end.get());
            if next >= end {
                next = start + (next - end).rem_euclid(end - start);
            }
        }
        self.expected = (!wraps_at_end).then_some(next);
    }
}

/// The passes of a loop after its first wrap in a segment: pass `p`, counted
/// from one, starts `(end - now) + (p - 1) × span` beats after the segment's
/// first frame, so a loop shorter than the segment plays every pass it
/// holds, each event on the frame playback reaches it in that pass (the one
/// beat→frame rule, `first_frame_at_or_after`, as `Env::due` places the
/// first pass).
struct Passes {
    start: f64,
    end: f64,
    /// Beats from the segment's first frame to the loop's first wrap.
    first: f64,
    fpb: f64,
}

impl Passes {
    fn new(start: f64, end: f64, now: f64, fpb: f64) -> Self {
        Self {
            start,
            end,
            first: end - now,
            fpb,
        }
    }

    /// Beats from the segment's first frame to where pass `p` starts.
    fn ahead(&self, p: u64) -> f64 {
        self.first + (p - 1) as f64 * (self.end - self.start)
    }

    /// The frame playback reaches `beat` (inside the loop) on in pass `p`.
    fn frame(&self, p: u64, beat: f64) -> i64 {
        tutti_core::first_frame_at_or_after((self.ahead(p) + (beat - self.start)) * self.fpb)
    }

    /// The first frame of pass `p`.
    fn begins(&self, p: u64) -> i64 {
        tutti_core::first_frame_at_or_after(self.ahead(p) * self.fpb)
    }

    /// The pass in force at frame `k`: the last that begins at or before it
    /// (passes shorter than a frame that begin on one frame play the last).
    fn at_frame(&self, k: usize) -> u64 {
        let span = (self.end - self.start) * self.fpb;
        let guess = ((k as f64 - self.first * self.fpb) / span).floor() + 1.0;
        // `as` saturates: a NaN or negative guess is pass 1.
        let mut p = (guess as u64).max(1);
        let k = k as i64;
        while p > 1 && self.begins(p) > k {
            p -= 1;
        }
        while self.begins(p + 1) <= k {
            p += 1;
        }
        p
    }

    /// From frame `from` (the first wrap) to `len`: at each pass's first
    /// frame, the jump to the loop's start, then the pass's events. Returns
    /// whether a pass ends exactly at `len` (the next segment starts on a
    /// wrap). At most one iteration a frame.
    fn walk<T: Beated>(
        &self,
        events: &[T],
        from: usize,
        len: usize,
        v: &mut impl Visit<T>,
        at: &impl Fn(usize) -> Option<Offset>,
    ) -> bool {
        let in_loop = {
            let lo = events.partition_point(|e| e.beat() < self.start);
            let hi = events.partition_point(|e| e.beat() < self.end);
            &events[lo..hi.max(lo)]
        };
        let mut k = from;
        while k < len {
            let p = self.at_frame(k);
            let next = usize::try_from(self.begins(p + 1).max(0)).unwrap_or(usize::MAX);
            let until = next.min(len).max(k + 1);
            v.jump(at(k), self.start, events);
            // The pass's beats from frame `k` up to `until`, a frame either
            // side for rounding; each kept only on a frame inside.
            let (k0, k1) = (k as f64, until as f64);
            let pass_at = self.ahead(p) * self.fpb;
            let lo = self.start + (k0 - 1.0 - pass_at) / self.fpb;
            let hi = self.start + (k1 + 1.0 - pass_at) / self.fpb;
            let first = in_loop.partition_point(|e| e.beat() < lo);
            for e in &in_loop[first..] {
                if e.beat() >= hi {
                    break;
                }
                let f = self.frame(p, e.beat());
                if (k as i64..until as i64).contains(&f) {
                    if let Some(o) = at(f as usize) {
                        v.event(o, e);
                    }
                }
            }
            if next == len {
                return true;
            }
            k = until;
        }
        false
    }
}

/// Visit every event with a beat in `[lo, hi)` that playback reaches in `seg`
/// at an offset `keep` accepts.
fn emit<T: Beated>(
    events: &[T],
    lo: f64,
    hi: f64,
    seg: &Env,
    v: &mut impl Visit<T>,
    at: &impl Fn(usize) -> Option<Offset>,
    keep: impl Fn(usize) -> bool,
) {
    let first = events.partition_point(|e| e.beat() < lo);
    for e in &events[first..] {
        if e.beat() >= hi || e.beat().is_nan() {
            break;
        }
        let Due::In(k) = seg.due(At::Beat(tutti_core::Beat(e.beat()))) else {
            continue;
        };
        if !keep(k.index()) {
            continue;
        }
        if let Some(offset) = at(k.index()) {
            v.event(offset, e);
        }
    }
}

/// Frames per beat in `seg`, when its rate and tempo are usable.
fn frames_per_beat(seg: &Env) -> Option<f64> {
    let (rate, tempo) = (seg.sample_rate.get(), seg.transport.tempo.get());
    let fpb = rate * 60.0 / tempo;
    (rate > 0.0 && tempo > 0.0 && fpb.is_finite()).then_some(fpb)
}

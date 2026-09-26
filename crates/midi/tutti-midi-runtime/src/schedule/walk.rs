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

        // Where the loop, if the playhead is inside it, wraps in this
        // segment: `len` when it does not.
        let looping = t
            .looping
            .filter(|l| l.start.get() < l.end.get() && now < l.end.get());
        let reach = looping.map(|l| {
            let k = tutti_core::first_frame_at_or_after((l.end.get() - now) * fpb).max(0);
            usize::try_from(k).unwrap_or(usize::MAX)
        });
        let wrap = reach.map_or(len, |k| k.min(len));
        // The loop wraps exactly at this segment's end: the next segment
        // starts on the loop's start, which reads as continuous, so the jump
        // a wrap is is made there (`expected` forgotten below).
        let wraps_at_end = reach == Some(len);

        // Up to the wrap: a beat less than a frame behind the playhead falls
        // on the first frame (`Env::due`), so the range starts a frame back.
        let hi = now + (wrap as f64 + 1.0) * frame;
        emit(events, now - frame, hi, seg, v, &at, |k| k < wrap);
        if let (Some(l), true) = (looping, wrap < len) {
            let from = l.start.get();
            v.jump(at(wrap), from, events);
            let hi = from + ((len - wrap) as f64 + 1.0) * frame;
            emit(events, from, hi, seg, v, &at, |k| k >= wrap);
        }

        // Where the next segment starts if nothing jumps.
        let mut next = now + len as f64 * frame;
        if let Some(l) = looping {
            if next >= l.end.get() {
                next = l.start.get() + (next - l.end.get());
            }
        }
        self.expected = (!wraps_at_end).then_some(next);
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

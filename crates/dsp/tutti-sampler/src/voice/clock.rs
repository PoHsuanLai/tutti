//! The transport over one block, as a placed voice reads it: from the block's
//! [`Env`], never from a shared timeline (doc 013, items 8 and 9).
//!
//! A placed voice's position is a function of the playhead, so what it needs
//! from a block is the playhead's beat at every frame. The block's `Env`
//! carries the transport at its first frame and at every change inside it (a
//! start, a stop, a seek); between them the playhead rolls on the host's own
//! clock ([`FrameClock`], rebuilt from the transport as
//! [`Env::transport_at`] and `Env::for_each_beat` rebuild it), wrapping at
//! the loop's end. So a block is a handful of **runs**: stretches over which
//! the playhead moves linearly, cut at each change and at each loop wrap.
//! [`BlockClock::runs`] walks them; a reader asks a run for the beat at any
//! of its frames in closed form ([`Run::beat_at`]), so the voice enters and
//! leaves its window on the frame the engine's one beat→frame rule puts it
//! on, not a block (or a 64-frame chunk) late.
//!
//! # Jumps
//!
//! Buffered state — a stretch filter's FIFOs, the live disk reader's
//! crossfade — needs to know where the playhead moved **discontinuously**,
//! which one block cannot say: a block starting at beat 4 continues a block
//! that ended at beat 4, and jumps from one that ended at 7. So the node that
//! owns the read keeps a [`Clock`] across blocks: where the playhead would
//! be at the first frame after the last block, had nothing moved it. A run is
//! a **jump** ([`Run::jump`]) when it starts somewhere else while rolling — a
//! seek, at a block's start or inside it — and every loop wrap is one. A
//! start, a stop, and a seek while stopped are not (the playhead resumes
//! where it stands), as the timeline cursor they replace (`BeatCursor`) had
//! it. Each jump moves the [generation](Run::generation) on: a live disk
//! read's segment.
//!
//! A seek at a block's first frame to exactly where the playhead would have
//! been anyway is invisible here (the `Env` says only where the playhead
//! is), and so not a jump: nothing buffered is stale after it. A seek inside
//! a block is a transport change, and is compared the same way.
//!
//! Nothing here allocates: runs are computed on demand, a few closed-form
//! beats each.

use std::ops::Range;

use tutti_core::{Beat, Bpm, FrameClock, LoopRange, SampleRate, Samples};
use tutti_graph::{Env, Transport};

/// How far from where the playhead would have been, in frames, a run may
/// start and still continue it. Far below anything audible (a thousandth of
/// a frame), far above the rounding of a host that accumulates its beat.
const CONTINUES_WITHIN: f64 = 1e-3;

/// Where the playhead would be at the first frame after a run, had nothing
/// moved it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Next {
    beat: Beat,
    /// Whether it got there across a loop wrap (a jump, whatever comes next).
    wrapped: bool,
}

/// A stretch of a block over which the playhead moves linearly (or stands):
/// no transport change and no loop wrap inside it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    /// The run's first frame, as an offset into the block.
    pub(crate) start: usize,
    /// One past its last frame.
    pub(crate) end: usize,
    /// The host's clock at `start`.
    clock: FrameClock,
    /// Whether the playhead moves over the run: the transport plays, at a
    /// usable tempo.
    rolling: bool,
    /// Jumps since the owning [`Clock`] was built: the same for runs that
    /// continue one another.
    pub(crate) generation: u64,
    /// Whether the playhead jumped onto this run's first frame. See the
    /// module docs.
    pub(crate) jump: bool,
}

impl Run {
    /// Whether the playhead moves over the run.
    #[inline]
    pub(crate) fn rolling(&self) -> bool {
        self.rolling
    }

    /// The run's tempo.
    #[inline]
    pub(crate) fn tempo(&self) -> Bpm {
        self.clock.tempo()
    }

    /// The rate its frames are counted at: the block's.
    #[inline]
    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.clock.sample_rate()
    }

    /// The playhead's beat at block frame `frame` (inside the run), in
    /// closed form: the host's own beat there, to the bit.
    #[inline]
    pub(crate) fn beat_at(&self, frame: usize) -> Beat {
        debug_assert!(frame >= self.start, "a frame before the run");
        if !self.rolling {
            return self.clock.beat();
        }
        let mut c = self.clock;
        c.advance(Samples(frame - self.start), None);
        c.beat()
    }

    /// Frames per beat at the block's rate (`None` when it does not roll).
    #[inline]
    pub(crate) fn frames_per_beat(&self) -> Option<f64> {
        let (tempo, rate) = (self.tempo().get(), self.sample_rate().get());
        (self.rolling && tempo.is_finite() && tempo > 0.0).then(|| rate * 60.0 / tempo)
    }

    /// This run cut to `range`: `None` when they do not meet. A run cut
    /// after its first frame does not start with its jump.
    fn clipped(mut self, range: &Range<usize>) -> Option<Self> {
        let start = self.start.max(range.start);
        let end = self.end.min(range.end);
        if start >= end {
            return None;
        }
        if start > self.start {
            if self.rolling {
                self.clock.advance(Samples(start - self.start), None);
            }
            self.jump = false;
        }
        self.start = start;
        self.end = end;
        Some(self)
    }
}

/// Whether `t` moves: it plays, at a finite positive tempo.
fn rolls(t: &Transport) -> bool {
    let tempo = t.tempo.get();
    t.playing && tempo.is_finite() && tempo > 0.0
}

/// The loop `t` wraps at, if any.
fn region(t: &Transport) -> Option<LoopRange> {
    t.looping.and_then(|l| LoopRange::new(l.start, l.end))
}

/// Whether the playhead, which would have been at `prev`, jumped to where
/// `to` puts it. Only a rolling transport jumps (a stop, and a seek while
/// stopped, do not); a loop wrap always does.
fn breaks(prev: Next, to: &Transport, rate: SampleRate) -> bool {
    if !rolls(to) {
        return false;
    }
    if prev.wrapped {
        return true;
    }
    let fpb = rate.get() * 60.0 / to.tempo.get();
    ((to.beat() - prev.beat).get() * fpb).abs() > CONTINUES_WITHIN
}

/// The beat `clock` reads `k` frames on, no loop.
#[inline]
fn beat_after(clock: &FrameClock, k: usize) -> Beat {
    let mut c = *clock;
    c.advance(Samples(k), None);
    c.beat()
}

/// The first frame after `clock`'s, fewer than `limit` on, whose beat is at
/// or past `end` — where [`FrameClock::advance`] wraps a loop ending there —
/// or `None` when there is none. The beat is below `end` at the clock.
fn wrap_offset(clock: &FrameClock, end: Beat, limit: usize) -> Option<usize> {
    if limit <= 1 || beat_after(clock, limit - 1) < end {
        return None;
    }
    let fpb = clock.sample_rate().get() * 60.0 / clock.tempo().get();
    let est = ((end - clock.beat()).get() * fpb).ceil();
    let mut k = if est.is_finite() && est >= 1.0 {
        (est as usize).min(limit - 1)
    } else {
        1
    };
    while k > 1 && beat_after(clock, k - 1) >= end {
        k -= 1;
    }
    while beat_after(clock, k) < end {
        k += 1;
    }
    Some(k)
}

/// A placed read's clock, kept across blocks by the node that owns the read:
/// where the playhead would be next, and the jumps so far. See the module
/// docs.
#[derive(Clone, Debug, Default)]
pub(crate) struct Clock {
    generation: u64,
    /// `None` before the first block, and after a reset: nothing to
    /// continue, so no jump.
    next: Option<Next>,
}

impl Clock {
    /// A clock with no history.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Forget where the playhead was: the next block is no jump.
    pub(crate) fn reset(&mut self) {
        self.next = None;
    }

    /// The block `env` describes, and where it leaves the playhead for the
    /// next. Call once per block, before reading it.
    pub(crate) fn observe<'e>(&mut self, env: &'e Env) -> BlockClock<'e> {
        let jump = self
            .next
            .is_some_and(|n| breaks(n, &env.transport, env.sample_rate));
        let block = BlockClock {
            env,
            generation: self.generation + u64::from(jump),
            jump,
        };
        let mut last = None;
        for run in block.runs() {
            last = Some(run);
        }
        if let Some(last) = last {
            self.generation = last.generation;
            self.next = Some(continuation(&last, &env_transport_of(env, last.start)));
        }
        block
    }
}

/// The transport in force at block frame `frame`.
fn env_transport_of(env: &Env, frame: usize) -> Transport {
    env.changes
        .as_slice()
        .iter()
        .rev()
        .find(|c| c.at.index() <= frame)
        .map_or(env.transport, |c| c.to)
}

/// Where the playhead would be at the first frame after `run`, whose
/// transport is `t`.
fn continuation(run: &Run, t: &Transport) -> Next {
    if !run.rolling {
        return Next {
            beat: run.clock.beat(),
            wrapped: false,
        };
    }
    let mut c = run.clock;
    c.advance(Samples(run.end - run.start), region(t));
    Next {
        beat: c.beat(),
        wrapped: c.generation() != run.clock.generation(),
    }
}

/// One block's transport, as [`Clock::observe`] hands it to the readers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockClock<'e> {
    env: &'e Env,
    /// The generation at the block's first frame.
    generation: u64,
    /// Whether the block's first frame is a jump.
    jump: bool,
}

impl<'e> BlockClock<'e> {
    /// The block's runs, in order, covering it.
    pub(crate) fn runs(&self) -> Runs<'e> {
        let t = self.env.transport;
        Runs {
            env: self.env,
            seg: 0,
            cur: 0,
            clock: t.clock(self.env.sample_rate),
            transport: t,
            generation: self.generation,
            jump: self.jump,
        }
    }

    /// The runs meeting frames `range`, cut to it.
    pub(crate) fn runs_in(&self, range: Range<usize>) -> impl Iterator<Item = Run> + 'e {
        self.runs()
            .take_while(move |r| r.start < range.end)
            .filter_map(move |r| r.clipped(&range))
    }
}

/// [`BlockClock::runs`].
pub(crate) struct Runs<'e> {
    env: &'e Env,
    /// Segments entered: 0 is the block's own transport, `i` the
    /// transport of change `i - 1`.
    seg: usize,
    /// The next frame to hand out.
    cur: usize,
    /// The host's clock at `cur`.
    clock: FrameClock,
    /// The transport of the current segment.
    transport: Transport,
    generation: u64,
    /// Whether the next run starts with a jump.
    jump: bool,
}

impl Runs<'_> {
    /// Where the current segment ends.
    fn seg_end(&self) -> usize {
        let len = self.env.block_len.get();
        self.env
            .changes
            .as_slice()
            .get(self.seg)
            .map_or(len, |c| c.at.index().min(len))
    }
}

impl Iterator for Runs<'_> {
    type Item = Run;

    fn next(&mut self) -> Option<Run> {
        loop {
            let end = self.seg_end();
            if self.cur >= end {
                let change = *self.env.changes.as_slice().get(self.seg)?;
                self.seg += 1;
                // The clock stands at the change's frame: where the
                // playhead would have been (or stood, stopped).
                let prev = Next {
                    beat: self.clock.beat(),
                    wrapped: false,
                };
                self.transport = change.to;
                self.clock = change.to.clock(self.env.sample_rate);
                if breaks(prev, &change.to, self.env.sample_rate) {
                    self.jump = true;
                    self.generation += 1;
                }
                if self.cur >= self.env.block_len.get() {
                    return None;
                }
                continue;
            }
            let rolling = rolls(&self.transport);
            let region = region(&self.transport);
            let wrap = region
                .filter(|r| rolling && self.clock.beat() < r.end())
                .and_then(|r| wrap_offset(&self.clock, r.end(), end - self.cur));
            let stop = wrap.map_or(end, |k| self.cur + k);
            let run = Run {
                start: self.cur,
                end: stop,
                clock: self.clock,
                rolling,
                generation: self.generation,
                jump: self.jump,
            };
            if rolling {
                self.clock.advance(Samples(stop - self.cur), region);
            }
            self.cur = stop;
            self.jump = wrap.is_some();
            if wrap.is_some() {
                self.generation += 1;
            }
            return Some(run);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::Frame;
    use tutti_graph::{Offset, TransportChanges};

    /// 120 BPM at 48 kHz: 24 000 frames a beat.
    const FPB: f64 = 24_000.0;
    const BLOCK: usize = 64;

    fn env(transport: Transport, changes: &[(usize, Transport)]) -> Env {
        let mut c = TransportChanges::NONE;
        for &(at, to) in changes {
            c.push(Offset::new(at, Samples(BLOCK)).expect("inside"), to)
                .expect("room");
        }
        Env {
            frame: Frame(0),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(BLOCK),
            transport,
            changes: c,
        }
    }

    fn at(playing: bool, frame: f64) -> Transport {
        Transport::new(playing, Bpm(120.0), Beat(frame / FPB), None)
    }

    /// `(start, end, jump, generation)` of each run of the block.
    fn runs(clock: &mut Clock, env: &Env) -> Vec<(usize, usize, bool, u64)> {
        clock
            .observe(env)
            .runs()
            .map(|r| (r.start, r.end, r.jump, r.generation))
            .collect()
    }

    /// **A block that starts where the last one left the playhead is no
    /// jump; one that starts anywhere else is**, on its first frame, and
    /// moves the generation on. The first block continues nothing.
    ///
    /// Mutation (run): `breaks` answering `false` for a rolling transport
    /// → the seek back is no jump → fails. Mutation (run): `continuation`
    /// not advancing over the run (the next beat its start) → the second
    /// block reads as a jump → fails.
    #[test]
    fn a_block_that_continues_the_last_is_no_jump_and_a_seek_is() {
        let mut clock = Clock::new();
        assert_eq!(
            runs(&mut clock, &env(at(true, 0.0), &[])),
            [(0, 64, false, 0)]
        );
        assert_eq!(
            runs(&mut clock, &env(at(true, 64.0), &[])),
            [(0, 64, false, 0)],
            "continues"
        );
        assert_eq!(
            runs(&mut clock, &env(at(true, 0.0), &[])),
            [(0, 64, true, 1)],
            "a seek back to the start"
        );
        assert_eq!(
            runs(&mut clock, &env(at(true, 64.0), &[])),
            [(0, 64, false, 1)]
        );
    }

    /// **A stop, a seek while stopped, and a restart where the playhead
    /// stands are no jump** (the playhead resumes where it stands, as the
    /// `BeatCursor` this replaces had it): nothing buffered is stale.
    ///
    /// Mutation (run): `continuation` treating a stopped run as rolling
    /// (the `!run.rolling` return removed) → the restart reads the playhead
    /// 64 frames on, and is a jump → fails.
    #[test]
    fn a_stop_a_seek_while_stopped_and_a_restart_are_no_jump() {
        let mut clock = Clock::new();
        runs(&mut clock, &env(at(true, 0.0), &[]));
        assert_eq!(
            runs(&mut clock, &env(at(false, 64.0), &[])),
            [(0, 64, false, 0)],
            "the stop"
        );
        assert_eq!(
            runs(&mut clock, &env(at(false, 5_000.0), &[])),
            [(0, 64, false, 0)],
            "a seek while stopped"
        );
        assert_eq!(
            runs(&mut clock, &env(at(true, 5_000.0), &[])),
            [(0, 64, false, 0)],
            "the restart where it stands"
        );
    }

    /// **A seek inside a block is a run of its own, a jump on its frame**,
    /// and the next block continues from it.
    ///
    /// Mutation (run): `Runs::seg_end` ignoring `Env::changes` (the block
    /// one segment) → one run → fails.
    #[test]
    fn a_seek_inside_a_block_is_a_jump_on_its_frame() {
        let mut clock = Clock::new();
        let seek = env(at(true, 0.0), &[(32, at(true, 10_000.0))]);
        assert_eq!(
            runs(&mut clock, &seek),
            [(0, 32, false, 0), (32, 64, true, 1)]
        );
        let next = env(at(true, 10_032.0), &[]);
        let block = clock.observe(&next);
        let run = block.runs().next().expect("a run");
        assert!(!run.jump, "the next block continues the seek's run");
        assert_eq!(run.beat_at(0), Beat(10_032.0 / FPB));
    }

    /// **A loop wrap inside a block is a jump on the wrap's frame**, the
    /// run after it starting at the loop's start.
    ///
    /// Mutation (run): `Runs` not cutting at a wrap (`wrap_offset`
    /// answering `None`) → one run, no jump → fails.
    #[test]
    fn a_loop_wrap_inside_a_block_is_a_jump_on_its_frame() {
        let looping = Some(tutti_graph::LoopRange {
            start: Beat(0.0),
            end: Beat(1.0),
        });
        let t = Transport::new(true, Bpm(120.0), Beat(1.0 - 10.0 / FPB), looping);
        let mut clock = Clock::new();
        let e = env(t, &[]);
        let block = clock.observe(&e);
        let got: Vec<_> = block.runs().collect();
        assert_eq!(
            got.iter()
                .map(|r| (r.start, r.end, r.jump, r.generation))
                .collect::<Vec<_>>(),
            [(0, 10, false, 0), (10, 64, true, 1)]
        );
        assert_eq!(got[1].beat_at(10), Beat(0.0), "the loop's start");
    }

    /// **A range cut out of a run keeps the run's jump only at its first
    /// frame** (`runs_in`), and reads the same beats.
    #[test]
    fn a_range_starts_with_the_jump_only_where_the_jump_is() {
        let mut clock = Clock::new();
        runs(&mut clock, &env(at(true, 0.0), &[]));
        let e = env(at(true, 5_000.0), &[]);
        let block = clock.observe(&e);
        let first: Vec<_> = block.runs_in(0..16).map(|r| (r.start, r.jump)).collect();
        let later: Vec<_> = block.runs_in(16..32).map(|r| (r.start, r.jump)).collect();
        assert_eq!(first, [(0, true)]);
        assert_eq!(later, [(16, false)]);
        let r = block.runs_in(16..32).next().expect("a run");
        assert_eq!(r.beat_at(20), Beat(5_020.0 / FPB));
    }
}

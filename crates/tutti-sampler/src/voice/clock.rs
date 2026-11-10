//! The transport over one block, as a placed voice reads it: from the block's
//! [`Env`], never from a shared timeline.
//!
//! A placed voice's position is a function of the playhead, so what it needs
//! from a block is the playhead's beat at every frame. The block's `Env`
//! carries the transport at its first frame and at every change inside it (a
//! start, a stop, a seek); between them the playhead rolls on the host's own
//! clock ([`FrameClock`], rebuilt from the transport as
//! [`Env::transport_at`] and `Env::for_each_beat` rebuild it), wrapping at
//! the loop's end. So a block is a handful of runs: stretches over which
//! the playhead moves linearly, cut at each change and at each loop wrap.
//! [`BlockClock::runs`] walks them; a reader asks a run for the beat at any
//! of its frames in closed form ([`Run::beat_at`]), so the voice enters and
//! leaves its window on the frame the engine's one beat→frame rule puts it
//! on, not a block (or a 64-frame chunk) late.
//!
//! # Jumps
//!
//! Buffered state — a stretch filter's FIFOs, the live disk reader's
//! crossfade — needs to know where the playhead moved discontinuously,
//! which one block cannot say: a block starting at beat 4 continues a block
//! that ended at beat 4, and jumps from one that ended at 7. So the node that
//! owns the read keeps a [`Clock`] across blocks: where the playhead would
//! be at the first frame after the last block, had nothing moved it. A run is
//! a jump ([`Run::jump`]) when it starts somewhere else while rolling — a
//! seek, at a block's start or inside it — and every loop wrap is one. A
//! start, a stop, and a seek while stopped are not (the playhead resumes
//! where it stands). Each jump moves the [generation](Run::generation) on: a live disk
//! read's segment.
//!
//! # Wrap crossfade
//!
//! A wrap is a jump in the playhead and a cut in a voice that plays through
//! it. So a run that follows a wrap carries a [`Wrap`]: for [`WRAP_FADE`]
//! after the wrap (at most half a pass of the loop), the voice also sounds
//! where the playhead would be had it run on past the loop's end, a loop's
//! length ahead of it, and blends that tail out under the loop's start. The
//! crossfade starts on the wrap's frame and moves nothing: the loop's start
//! is on the same frame it was. It outlasts the block ([`Clock`] carries it)
//! and a seek or a stop ends it.
//!
//! The head is the clip's own start when the voice begins on or after the
//! loop's start; it then keeps unity (the tail fades out under it, and any
//! step at the join is the clip's own). Otherwise it is mid-sound, and the
//! two cross at equal power: an in-phase join sums the gains, up to root 2
//! (+3.01 dB) mid-fade and 4/pi (+2.1 dB) on average; a unity head peaks at
//! twice a tone when head and tail are both near full level. Nothing limits
//! it. A clip that starts before the loop's start with an attack exactly on
//! it is mid-sound to this, which reads the voice's position and not its
//! audio, and gets the ramp.
//!
//! A tempo change inside a fade re-derives the tail's position from the new
//! tempo and the wrap's loop length in beats, so the tail jumps there (a
//! glitch of the size of the change, for the rest of the fade); a loop edit
//! does not move it, and a new wrap restarts the fade. The fade's length is
//! a frame count at the wrap's rate, so a rate change inside it runs those
//! frames at the new rate.
//! A seek at a block's first frame to exactly where the playhead would have
//! been anyway is invisible here (the `Env` says only where the playhead
//! is), and so not a jump: nothing buffered is stale after it. A seek inside
//! a block is a transport change, and is compared the same way.
//!
//! Nothing here allocates: runs are computed on demand, a few closed-form
//! beats each.

use std::ops::Range;

use tutti_graph::{Env, Transport};
use tutti_types::{Beat, BeatDuration, Bpm, FrameClock, LoopRange, SampleRate, Samples, Seconds};

/// How long a placed memory voice crossfades across a loop wrap: the tail it
/// was playing past the loop's end fades out as the loop's start fades in.
/// A time, so it is the same 5 ms at every sample rate; short enough that a
/// hit on the loop's first frame keeps its attack.
pub(crate) const WRAP_FADE: Seconds = Seconds(0.005);

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
    /// The wrap crossfade still running at that frame.
    wrap: Option<Wrap>,
}

/// A loop wrap whose crossfade is still running: the playhead jumped back by
/// the loop's length, and for [`len`](Self::len) frames the voice sounds
/// both where the playhead is and where it would have been.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Wrap {
    /// Frames since the wrap, at the run's first frame.
    pub(crate) ago: usize,
    /// The crossfade's length in frames: [`WRAP_FADE`], at most half a pass
    /// so a short loop still has a pass of its own.
    pub(crate) len: usize,
    /// How far back the playhead jumped: the loop's length.
    pub(crate) back: BeatDuration,
}

impl Wrap {
    /// The crossfade of a wrap of `region` at `tempo`, `None` when it has no
    /// frame to run over.
    fn new(region: LoopRange, tempo: Bpm, rate: SampleRate) -> Option<Self> {
        let pass = region.len().get() * rate.get() * 60.0 / tempo.get();
        let len = WRAP_FADE.to_samples(rate).0.min((pass / 2.0) as usize);
        (len > 0).then_some(Self {
            ago: 0,
            len,
            back: region.len(),
        })
    }

    /// Frames of the crossfade still to run.
    #[inline]
    pub(crate) fn left(&self) -> usize {
        self.len - self.ago
    }

    /// The crossfade `frames` on, `None` once it is done.
    fn after(self, frames: usize) -> Option<Self> {
        let ago = self.ago + frames;
        (ago < self.len).then_some(Self { ago, ..self })
    }
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
    /// The wrap crossfade running at `start`, if one is.
    pub(crate) wrap: Option<Wrap>,
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
        tutti_types::frames_per_beat(self.tempo(), self.sample_rate()).filter(|_| self.rolling)
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
            self.wrap = self.wrap.and_then(|w| w.after(start - self.start));
        }
        self.start = start;
        self.end = end;
        Some(self)
    }
}

/// Whether `t` moves: it plays (a `Bpm` is finite and above zero).
fn rolls(t: &Transport) -> bool {
    t.playing
}

/// The loop `t` wraps at, if any.
fn region(t: &Transport) -> Option<LoopRange> {
    t.looping.and_then(|l| LoopRange::new(l.start, l.end))
}

/// Whether the playhead, which would have been at `prev`, jumped to where
/// `to` puts it. Only a rolling transport jumps (a stop, and a seek while
/// stopped, do not); a loop wrap always does.
fn breaks(prev: Next, to: &Transport, rate: SampleRate) -> bool {
    rolls(to) && (prev.wrapped || !continues(prev, to, rate))
}

/// Whether `to` is where the playhead would have been at `prev`.
fn continues(prev: Next, to: &Transport, rate: SampleRate) -> bool {
    let fpb = rate.get() * 60.0 / to.tempo.get();
    ((to.beat() - prev.beat).get() * fpb).abs() <= CONTINUES_WITHIN
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
        // A crossfade goes on across a block edge, unless a seek broke it
        // (a wrap on the edge is a jump, but a seek after it is not the
        // wrap); a wrap on the edge starts one.
        let wrap = self
            .next
            .filter(|n| !jump || n.wrapped)
            .filter(|n| !rolls(&env.transport) || continues(*n, &env.transport, env.sample_rate))
            .and_then(|n| n.wrap);
        let mut block = BlockClock {
            env,
            generation: self.generation + u64::from(jump),
            jump,
            jumps: false,
            wrap,
            fades: false,
        };
        let mut last = None;
        let mut jumps = false;
        let mut fades = false;
        for run in block.runs() {
            jumps |= run.jump;
            fades |= run.wrap.is_some();
            last = Some(run);
        }
        block.jumps = jumps;
        block.fades = fades;
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
            wrap: None,
        };
    }
    let mut c = run.clock;
    c.advance(Samples(run.end - run.start), region(t));
    let wrapped = c.generation() != run.clock.generation();
    let wrap = if wrapped {
        region(t).and_then(|r| Wrap::new(r, t.tempo, run.sample_rate()))
    } else {
        run.wrap.and_then(|w| w.after(run.end - run.start))
    };
    Next {
        beat: c.beat(),
        wrapped,
        wrap,
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
    /// Whether any run of the block starts with a jump.
    jumps: bool,
    /// The wrap crossfade running at the block's first frame.
    wrap: Option<Wrap>,
    /// Whether a wrap crossfade runs over any frame of the block.
    fades: bool,
}

impl<'e> BlockClock<'e> {
    /// Whether any run of the block starts with a [jump](Run::jump): when
    /// not, no range of it holds one, and a reader that cuts its read at
    /// jumps need not walk the runs to find none.
    #[inline]
    pub(crate) fn jumps(&self) -> bool {
        self.jumps
    }

    /// Whether a wrap crossfade runs over any frame of the block: when not,
    /// a reader has no tail to read.
    #[inline]
    pub(crate) fn fades(&self) -> bool {
        self.fades
    }

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
            wrap: self.wrap.filter(|_| rolls(&t)),
        }
    }

    /// The runs meeting frames `range`, cut to it.
    pub(crate) fn runs_in(&self, range: Range<usize>) -> impl Iterator<Item = Run> + 'e + use<'e> {
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
    /// The wrap crossfade running at `cur`.
    wrap: Option<Wrap>,
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
                    wrap: None,
                };
                self.transport = change.to;
                self.clock = change.to.clock(self.env.sample_rate);
                let broke = breaks(prev, &change.to, self.env.sample_rate);
                if broke {
                    self.jump = true;
                    self.generation += 1;
                }
                // A seek or a stop ends the crossfade on its frame.
                if broke || !rolls(&change.to) {
                    self.wrap = None;
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
                wrap: self.wrap,
            };
            if rolling {
                self.clock.advance(Samples(stop - self.cur), region);
            }
            self.wrap = match region.filter(|_| wrap.is_some()) {
                Some(r) => Wrap::new(r, self.transport.tempo, self.env.sample_rate),
                None => self.wrap.and_then(|w| w.after(stop - self.cur)),
            };
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
    use tutti_graph::{Offset, TransportChanges};
    use tutti_types::Frame;

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
            play: tutti_graph::PlayClock::ZERO,
        }
    }

    fn at(playing: bool, frame: f64) -> Transport {
        Transport::new(playing, Bpm::of(120.0), Beat(frame / FPB), None)
    }

    /// **A tempo too slow to count frames at has no frames per beat** (the
    /// run was `Some(inf)` before the one rule).
    ///
    /// Mutation (run): `Run::frames_per_beat` computing `rate * 60 / tempo`
    /// unfiltered → `Some(inf)` → fails.
    #[test]
    fn the_slowest_tempo_has_no_frames_per_beat() {
        let slow = Transport::new(true, Bpm::of(f64::from_bits(1)), Beat(0.0), None);
        let mut clock = Clock::new();
        let run = clock.observe(&env(slow, &[])).runs().next().expect("a run");
        assert_eq!(run.frames_per_beat(), None);
        let mut clock = Clock::new();
        let run = clock
            .observe(&env(at(true, 0.0), &[]))
            .runs()
            .next()
            .expect("a run");
        assert_eq!(run.frames_per_beat(), Some(FPB));
    }

    /// `(start, end, jump, generation)` of each run of the block.
    fn runs(clock: &mut Clock, env: &Env) -> Vec<(usize, usize, bool, u64)> {
        clock
            .observe(env)
            .runs()
            .map(|r| (r.start, r.end, r.jump, r.generation))
            .collect()
    }

    /// A block that starts where the last one left the playhead is no
    /// jump; one that starts anywhere else is, on its first frame, and
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

    /// A stop, a seek while stopped, and a restart where the playhead
    /// stands are no jump (the playhead resumes where it stands): nothing
    /// buffered is stale.
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

    /// A seek inside a block is a run of its own, a jump on its frame,
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

    /// A loop wrap inside a block is a jump on the wrap's frame, the
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
        let t = Transport::new(true, Bpm::of(120.0), Beat(1.0 - 10.0 / FPB), looping);
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

    /// `BlockClock::jumps` is whether any run of the block jumps: the
    /// slot walks a range's runs for a reset only when it says so, so a
    /// `false` over a jump would leave a stretch filter or a live read
    /// holding stale state. Over a continuation, a seek back, a seek inside
    /// a block, a stop and a restart.
    ///
    /// Mutation (run): `jumps` always `false` → the seek blocks fail.
    /// Mutation (run): `jumps` set from the first run only → the in-block
    /// seek fails.
    #[test]
    fn jumps_is_whether_any_run_jumps() {
        let mut clock = Clock::new();
        let blocks = [
            env(at(true, 0.0), &[]),
            env(at(true, 64.0), &[]),
            env(at(true, 0.0), &[]),
            env(at(true, 64.0), &[(32, at(true, 9_000.0))]),
            env(at(true, 9_032.0), &[]),
            env(at(false, 9_096.0), &[]),
            env(at(true, 9_096.0), &[]),
        ];
        let mut jumped = 0;
        for (i, e) in blocks.iter().enumerate() {
            let block = clock.observe(e);
            let any = block.runs().any(|r| r.jump);
            assert_eq!(block.jumps(), any, "block {i}");
            jumped += usize::from(any);
        }
        assert_eq!(jumped, 2, "the seek back and the seek inside a block");
    }

    /// A range cut out of a run keeps the run's jump only at its first
    /// frame (`runs_in`), and reads the same beats.
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

    /// 120 BPM: a loop of one beat is 24 000 frames, and the fade 240.
    fn looping(from: f64) -> Transport {
        let looping = Some(tutti_graph::LoopRange {
            start: Beat(0.0),
            end: Beat(1.0),
        });
        Transport::new(true, Bpm::of(120.0), Beat(from / FPB), looping)
    }

    /// `(ago, len)` of the wrap crossfade at each run of the block.
    fn fades(block: &BlockClock<'_>) -> Vec<Option<(usize, usize)>> {
        block
            .runs()
            .map(|r| r.wrap.map(|w| (w.ago, w.len)))
            .collect()
    }

    /// A wrap starts a crossfade on its frame, and it goes on across the
    /// blocks that follow until it is done.
    ///
    /// Mutation (run): `continuation` not carrying the run's crossfade
    /// (`wrap: None` for a run without a wrap) → the second block has none →
    /// fails. `Wrap::after` not adding the frames → the `ago` stops at 0 →
    /// fails.
    #[test]
    fn a_wrap_starts_a_crossfade_that_outlasts_the_block() {
        let mut clock = Clock::new();
        let e = env(looping(24_000.0 - 10.0), &[]);
        let block = clock.observe(&e);
        assert_eq!(fades(&block), [None, Some((0, 240))]);
        assert!(block.fades());
        let mut from = 54.0;
        for ago in [54, 118, 182] {
            let e = env(looping(from), &[]);
            let block = clock.observe(&e);
            assert_eq!(fades(&block), [Some((ago, 240))], "ago {ago}");
            from += 64.0;
        }
        let e = env(looping(from), &[]);
        let block = clock.observe(&e);
        assert_eq!(fades(&block), [None], "done after 240 frames");
        assert!(!block.fades());
    }

    /// A wrap on a block's last frame starts its crossfade on the next
    /// block's first.
    ///
    /// Mutation (run): `observe` taking the carried crossfade only when
    /// `!jump` (the `|| n.wrapped` removed) → none → fails.
    #[test]
    fn a_wrap_on_the_block_edge_starts_the_crossfade_on_the_next_block() {
        let mut clock = Clock::new();
        clock.observe(&env(looping(24_000.0 - 64.0), &[]));
        let e = env(looping(0.0), &[]);
        let block = clock.observe(&e);
        assert_eq!(fades(&block), [Some((0, 240))]);
    }

    /// A seek between blocks ends the crossfade.
    ///
    /// Mutation (run): `observe` taking the carried crossfade across a jump
    /// → the seek's block has one → fails.
    #[test]
    fn a_seek_between_blocks_ends_the_crossfade() {
        let mut clock = Clock::new();
        clock.observe(&env(looping(24_000.0 - 10.0), &[]));
        let e = env(looping(5_000.0), &[]);
        assert_eq!(fades(&clock.observe(&e)), [None]);
    }

    /// The crossfade is [`WRAP_FADE`] at the rate, at most half a pass.
    ///
    /// Mutation (run): `Wrap::new` without the `min` → a 120 frame pass
    /// fades over 240 → fails.
    #[test]
    fn the_crossfade_is_the_fade_or_half_a_pass() {
        let rate = SampleRate(48_000.0);
        let loop_of =
            |beats: f64| LoopRange::new(Beat::new(0.0), Beat::new(beats)).expect("a loop");
        let len = |beats| Wrap::new(loop_of(beats), Bpm::of(120.0), rate).map(|w| w.len);
        assert_eq!(len(1.0), Some(240));
        assert_eq!(len(0.005), Some(60), "a pass of 120 frames");
        assert_eq!(len(0.00005), None, "a pass of 1.2 frames");
        let fast = SampleRate(96_000.0);
        assert_eq!(
            Wrap::new(loop_of(1.0), Bpm::of(120.0), fast).map(|w| w.len),
            Some(480)
        );
    }

    /// A seek on the block right after an edge wrap is a seek, not the wrap:
    /// it starts no crossfade (the wrap's own jump is not a seek's).
    ///
    /// Mutation (run): `observe` without the `continues` filter on the carried
    /// crossfade → the seek's block fades a ghost tail → fails.
    #[test]
    fn a_seek_right_after_an_edge_wrap_starts_no_crossfade() {
        let mut clock = Clock::new();
        clock.observe(&env(looping(24_000.0 - 64.0), &[]));
        let e = env(looping(5_000.0), &[]);
        assert_eq!(fades(&clock.observe(&e)), [None]);
    }
}

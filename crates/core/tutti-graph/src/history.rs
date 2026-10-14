//! [`TransportHistory`]: the transport the graph played over its last
//! frames, so a node behind a latent path reads the transport its input
//! audio was rendered at ([`Cx::transport_at`](crate::Cx::transport_at)).

use tutti_types::{Frame, Latency, SampleRate, Samples};

use crate::node::{Env, Transport, MAX_TRANSPORT_CHANGES};
use crate::time::Offset;

/// One piece of a past block: from `frame` on, the transport was
/// `transport`, counted at `rate`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Piece {
    frame: Frame,
    rate: SampleRate,
    transport: Transport,
}

/// The transport over the graph's last frames: what a node behind a latent
/// path reads its transport from ([`Env::transport_before`],
/// [`Cx::transport_at`](crate::Cx::transport_at)).
///
/// # Why a history, and not a closed form
///
/// A node whose compiled arrival latency is `a` hears, at frame `F` of its
/// block, the audio of timeline frame `F - a`. When `a` reaches before the
/// block's first frame, the transport there is not in the block's [`Env`].
/// Rewinding the block-start transport in closed form (the beat minus
/// `a / frames_per_beat`) is right only while nothing happened in between:
/// a seek, a start or stop, a tempo or loop edit, or a loop wrap in the last
/// `a` frames all make it wrong, and a loop wrap cannot even be detected
/// from the block that follows it (a playhead at the loop's start may have
/// wrapped there or been started there).
///
/// So the executor keeps what it was told. Every block it renders adds its
/// pieces ([`Env::segments`]: the block's first frame, and every
/// [transport change](Env::changes) inside it), each as the frame it starts
/// on and the transport the host reported there. The transport at an earlier
/// frame is then the latest piece at or before it, rolled forward to the
/// frame with the host's own clock — exactly what
/// [`Env::transport_at`] answered for that frame in the block that held it,
/// to the bit, through any wrap, seek or tempo change since.
///
/// # Size, memory, and where it is allocated
///
/// A history answers for frames up to its **reach** before the block being
/// rendered. Each block first drops the oldest piece while the next one
/// already covers the earliest frame in reach, so what is kept is the piece
/// covering that frame and the pieces starting after it: at most `reach` of
/// those, since pieces start on distinct frames, plus the current block's
/// `1 + MAX_TRANSPORT_CHANGES`. So at most `reach + 1 + MAX_TRANSPORT_CHANGES`
/// pieces are ever live (that worst case is one-frame blocks); a host
/// rendering the device's quantum keeps about `reach / quantum + 2`.
///
/// The ring is a buffer with a moving front: dropped pieces advance it, and
/// the live pieces are moved back to the start once the dropped prefix is
/// as long as they are, so only the first twice-the-peak-live slots are
/// ever written. The buffer is **reserved** for the worst case,
/// `2 * (reach + 1 + MAX_TRANSPORT_CHANGES)` pieces of about 90 bytes (a
/// 10-second lookahead at 48 kHz reserves about 85 MB of address space),
/// because the audio thread can never grow it; where the allocator maps a
/// large block lazily (glibc, macOS) pages that are never written are never
/// resident, so resident memory follows the pieces actually kept.
///
/// The executor's reach is its plan's longest arrival
/// ([`Plan::max_arrival`](crate::Plan::max_arrival)). A commit whose plan
/// changes it carries a new ring, allocated on the control thread; applying
/// it moves the pieces across and sends the old ring back in the box, so
/// the audio thread neither allocates nor frees one. A block only appends,
/// drops and moves pieces in place.
///
/// # Where it stops
///
/// A frame older than the oldest piece kept is answered with that piece's
/// transport, **unmoved** (a [`TransportHistory::new`], which keeps
/// nothing, answers with the block's own [`Env::transport`]). That happens in
/// three places, all where the audio the node hears there is a delay line's
/// initial silence or a pipeline refilling, rather than anything the
/// timeline played:
///
/// - **before the graph's first block**: the first `a` frames a node with
///   arrival `a` renders;
/// - **after a recompile raised the reach**: the new ring starts with what
///   the old one kept, which may not go back the new reach yet (the
///   path that raised the arrival is refilling too);
/// - **after a sample-rate change**: frames counted at the old rate do not
///   name frames at the new one, so the history starts over.
///
/// # Who keeps one
///
/// The executor keeps one and hands it to every node through
/// [`Cx::history`](crate::Cx::history). A host that drives a node by hand
/// keeps its own: [`with_reach`](Self::with_reach) on the control thread,
/// then [`observe`](Self::observe) every block's [`Env`] before rendering
/// it.
#[derive(Clone, Debug, Default)]
pub struct TransportHistory {
    /// The pieces, oldest first, from `head` on; `buf[..head]` are dropped
    /// pieces not yet moved over. Reserved for `2 * capacity`, which the
    /// compaction rule never exceeds (see `observe`).
    buf: Vec<Piece>,
    head: usize,
    /// Most pieces live at once: `reach + 1 + MAX_TRANSPORT_CHANGES`, or 0
    /// for no reach.
    capacity: usize,
    reach: Latency,
}

/// A history with no reach, for a [`Cx`](crate::Cx) built where nothing is
/// kept (a node driven by hand, the contract harness).
static NONE: TransportHistory = TransportHistory::new();

impl TransportHistory {
    /// A history that keeps nothing: every frame before the block is answered
    /// with the block's own transport. Allocates nothing.
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
            capacity: 0,
            reach: Latency::ZERO,
        }
    }

    /// The history that keeps nothing ([`new`](Self::new)), borrowed for
    /// as long as anything needs it.
    pub fn none() -> &'static TransportHistory {
        &NONE
    }

    /// A history answering exactly for frames up to `reach` before the block
    /// being rendered. Allocates its ring here: control thread.
    pub fn with_reach(reach: Latency) -> Self {
        let capacity = if reach.is_zero() {
            0
        } else {
            reach.samples().get() + 1 + MAX_TRANSPORT_CHANGES
        };
        Self {
            buf: Vec::with_capacity(2 * capacity),
            head: 0,
            capacity,
            reach,
        }
    }

    /// The pieces kept, oldest first.
    fn live(&self) -> &[Piece] {
        &self.buf[self.head..]
    }

    /// How far back it answers exactly.
    pub fn reach(&self) -> Latency {
        self.reach
    }

    /// Records the block about to be rendered: its first frame and each
    /// transport change inside it, as [`Env::segments`] cuts them. Call it
    /// once per block, before any node reads the history, in frame order.
    /// Allocation-free: first the oldest pieces no frame in reach needs are
    /// dropped, then the block's are appended.
    ///
    /// A block must start after every piece kept. The executor's clock only
    /// moves forward, and the one time it moves back (a sample-rate change
    /// rescales it) the history is cleared first, so this is
    /// debug-asserted rather than repaired.
    pub fn observe(&mut self, env: &Env) {
        if self.capacity == 0 {
            return;
        }
        debug_assert!(
            self.live().last().is_none_or(|p| p.frame < env.frame),
            "a block observed at or before a piece already kept"
        );
        // The earliest frame a node can ask for this block is `reach` before
        // its first; the front piece is needed only while the next one starts
        // after that frame.
        let reach = self.reach.samples();
        while self.live().len() >= 2
            && env
                .frame
                .since(self.live()[1].frame)
                .is_some_and(|d| d >= reach)
        {
            self.head += 1;
        }
        for (_, piece) in env.segments() {
            if self.live().len() == self.capacity {
                // Unreachable while the bound in the type docs holds; kept
                // so a break in it costs the oldest piece, not an allocation.
                // No test can reach it (`if false` here passes every one):
                // the trim above is what keeps the bound, and
                // `the_capacity_holds_exactly_the_worst_case` pins it.
                self.head += 1;
            }
            // Move the live pieces to the front once the dropped prefix is
            // as long as they are: amortised one move a push, and the buffer
            // never holds more than twice the live pieces, so it never grows.
            if self.head > 0 && self.head >= self.buf.len() - self.head {
                self.buf.drain(..self.head);
                self.head = 0;
            }
            self.buf.push(Piece {
                frame: piece.frame,
                rate: env.sample_rate,
                transport: piece.transport,
            });
        }
    }

    /// Forgets every piece (a sample-rate change: see the type docs).
    pub fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }

    /// Takes `older`'s pieces, the newest that fit, leaving it empty with
    /// its buffer: applying a commit that carries a new ring. No allocation:
    /// `with_reach` reserved room for twice what fits.
    pub(crate) fn take_from(&mut self, older: &mut TransportHistory) {
        self.clear();
        let live = older.live();
        let skip = live.len().saturating_sub(self.capacity);
        self.buf.extend_from_slice(&live[skip..]);
        older.clear();
    }

    /// The transport `back` frames before frame `start`, when a piece kept
    /// covers that frame; `None` when it is older than every piece.
    fn before(&self, start: Frame, back: Samples) -> Option<Transport> {
        // Pieces are in frame order, so "starts at or before the frame" holds
        // for a prefix. A piece too far back for `since` to count (only on a
        // 32-bit target, a day of audio) is before it too.
        let covers = |p: &Piece| p.frame <= start && start.since(p.frame).is_none_or(|d| d >= back);
        let live = self.live();
        let i = live.partition_point(covers).checked_sub(1)?;
        let p = &live[i];
        let into = start.since(p.frame)?.checked_sub(back)?;
        Some(roll(p.transport, p.rate, into))
    }

    /// The oldest piece's transport, at its own frame.
    fn oldest(&self) -> Option<Transport> {
        self.live().first().map(|p| p.transport)
    }
}

/// `t` rolled `frames` on at `rate` while it plays, wrapping at its loop:
/// the host's clock ([`Transport::clock`]) advanced and read back. The one
/// walk [`Env::transport_at`] and the history share.
pub(crate) fn roll(t: Transport, rate: SampleRate, frames: Samples) -> Transport {
    let (tempo, r) = (t.tempo.get(), rate.get());
    // `is_finite` and `> 0.0` together also refuse a NaN.
    let usable = tempo.is_finite() && tempo > 0.0 && r.is_finite() && r > 0.0;
    if !usable || !t.playing {
        return t;
    }
    let mut clock = t.clock(rate);
    let region = t
        .looping
        .and_then(|l| tutti_types::LoopRange::new(l.start, l.end));
    clock.advance(frames, region);
    Transport::counted(t.playing, t.tempo, clock.origin(), t.looping).with_recording(t.recording())
}

impl Env {
    /// The transport `back` frames before frame `offset` of this block: the
    /// transport the audio a node with arrival latency `back` hears at
    /// `offset` was rendered at. Nodes read it through
    /// [`Cx::transport_at`](crate::Cx::transport_at).
    ///
    /// Inside the block it is [`transport_at`](Self::transport_at) of the
    /// earlier offset. Before the block's first frame it is read from
    /// `history`, which the executor fed every block it rendered: the
    /// transport [`transport_at`](Self::transport_at) reported for that
    /// frame in the block that held it, through any seek, start, stop, tempo
    /// or loop change, and any loop wrap, since. A frame older than anything
    /// `history` keeps is answered with the oldest transport it has,
    /// unmoved (see [`TransportHistory`] for where that happens).
    pub fn transport_before(
        &self,
        offset: Offset,
        back: Latency,
        history: &TransportHistory,
    ) -> Transport {
        let back = back.samples();
        let here = Samples(offset.index());
        match here.checked_sub(back) {
            Some(earlier) => self.transport_at(Offset::raw(earlier.get() as u32)),
            None => {
                // Frames before the block's first one.
                let before = back.checked_sub(here).unwrap_or(Samples::ZERO);
                history
                    .before(self.frame, before)
                    .or_else(|| history.oldest())
                    .unwrap_or(self.transport)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{LoopRange, TransportChanges};
    use tutti_types::{Beat, Bpm};

    const SR: SampleRate = SampleRate(48_000.0);
    /// Frames per beat at 120 BPM and 48 kHz.
    const FPB: f64 = 24_000.0;

    fn env(frame: u64, len: usize, transport: Transport, changes: TransportChanges) -> Env {
        Env {
            frame: Frame(frame),
            sample_rate: SR,
            block_len: Samples(len),
            transport,
            changes,
        }
    }

    fn rolling(beat: f64) -> Transport {
        Transport::new(true, Bpm(120.0), Beat(beat), None)
    }

    fn off(i: usize) -> Offset {
        Offset::raw(i as u32)
    }

    fn lat(n: usize) -> Latency {
        Latency::new(Samples(n))
    }

    /// The blocks a host played, kept so a test can ask what the block that
    /// held a frame said about it.
    struct Played {
        blocks: Vec<Env>,
        history: TransportHistory,
    }

    impl Played {
        fn new(reach: usize) -> Self {
            Self {
                blocks: Vec::new(),
                history: TransportHistory::with_reach(lat(reach)),
            }
        }

        fn play(&mut self, e: Env) {
            self.history.observe(&e);
            self.blocks.push(e);
        }

        /// What the block holding absolute frame `f` reported for it.
        fn truth(&self, f: u64) -> Transport {
            let e = self
                .blocks
                .iter()
                .find(|e| e.offset_of(Frame(f)).is_some())
                .expect("a block played that frame");
            e.transport_at(e.offset_of(Frame(f)).expect("inside"))
        }
    }

    /// Inside the block, `transport_before` is `transport_at` of the earlier
    /// offset, whatever the history holds.
    ///
    /// Mutation: answer `self.transport_at(offset)` (ignore `back`) → the
    /// beat at offset 40 is 40 frames late → fails.
    #[test]
    fn inside_the_block_it_is_the_earlier_offset() {
        let e = env(1_000, 64, rolling(2.0), TransportChanges::NONE);
        let h = TransportHistory::new();
        assert_eq!(
            e.transport_before(off(40), lat(16), &h),
            e.transport_at(off(24))
        );
        assert_eq!(
            e.transport_before(off(40), lat(40), &h),
            e.transport_at(off(0))
        );
        assert_eq!(
            e.transport_before(off(40), Latency::ZERO, &h),
            e.transport_at(off(40))
        );
    }

    /// Across block boundaries, through a seek at a block start, a seek
    /// inside a block, a loop that wraps, a tempo change and a stop: every
    /// frame back to the reach reads what the block that held it said.
    ///
    /// Mutation: `before` answers `None` (every frame before the block reads
    /// the oldest piece, unmoved) → fails. Mutation: in `transport_before`,
    /// answer `transport_at(offset)` inside the block (ignore `back`) →
    /// fails.
    #[test]
    fn every_frame_in_reach_reads_what_its_block_said() {
        const REACH: usize = 200;
        let mut p = Played::new(REACH);
        let loop_t = Transport::new(
            true,
            Bpm(120.0),
            Beat(8.0),
            Some(LoopRange {
                start: Beat(8.0),
                end: Beat(8.0 + 50.0 / FPB),
            }),
        );
        let mut mid_seek = TransportChanges::NONE;
        mid_seek.push(off(20), rolling(32.0)).expect("inside");
        let mut tempo = TransportChanges::NONE;
        tempo
            .push(off(10), Transport::new(true, Bpm(90.0), Beat(40.0), None))
            .expect("inside");
        let scenes = [
            (rolling(1.0), TransportChanges::NONE),
            (rolling(1.0 + 64.0 / FPB), TransportChanges::NONE),
            // A seek at the block start.
            (rolling(16.0), TransportChanges::NONE),
            // A seek inside the block.
            (rolling(16.0 + 64.0 / FPB), mid_seek),
            // A 50-frame loop: wraps inside this block and the next.
            (loop_t, TransportChanges::NONE),
            (
                env(0, 64, loop_t, TransportChanges::NONE).transport_at(off(63)),
                TransportChanges::NONE,
            ),
            (rolling(40.0), tempo),
            (
                Transport::new(false, Bpm(90.0), Beat(41.0), None),
                TransportChanges::NONE,
            ),
            (rolling(41.0), TransportChanges::NONE),
        ];
        for (b, (t, c)) in scenes.into_iter().enumerate() {
            let e = env(b as u64 * 64, 64, t, c);
            p.play(e);
            let start = e.frame.get();
            for o in 0..64 {
                for back in [1usize, 17, 63, 64, 65, 100, 150, REACH] {
                    let Some(f) = (start + o as u64).checked_sub(back as u64) else {
                        continue;
                    };
                    assert_eq!(
                        e.transport_before(off(o), lat(back), &p.history),
                        p.truth(f),
                        "block {b}, offset {o}, back {back}"
                    );
                }
            }
        }
    }

    /// A loop wrap behind the block: the closed form from the block's start
    /// (inside the loop) cannot tell a wrap from a start, the history can.
    ///
    /// Mutation: `before` answers `None` → reads the oldest piece, the
    /// loop's start, instead of 24 frames into its second pass → fails.
    #[test]
    fn a_loop_wrap_before_the_block_is_unwound() {
        let looping = Some(LoopRange {
            start: Beat(4.0),
            end: Beat(4.0 + 32.0 / FPB),
        });
        let mut p = Played::new(64);
        let first = env(
            0,
            64,
            Transport::new(true, Bpm(120.0), Beat(4.0), looping),
            TransportChanges::NONE,
        );
        p.play(first);
        let second = env(64, 64, first.transport_at(off(63)), TransportChanges::NONE);
        p.play(second);
        // 8 frames before the second block: frame 56 of the first, 24 frames
        // into the loop's second pass.
        let got = second.transport_before(off(0), lat(8), &p.history);
        assert_eq!(got, first.transport_at(off(56)));
        assert!((got.beat().get() - (4.0 + 24.0 / FPB)).abs() < 1e-12);
    }

    /// Before the first block, and past what the ring keeps: the oldest
    /// transport kept, unmoved. With no history at all: the block's own.
    ///
    /// Mutation: roll the oldest piece back (or forward) instead of
    /// answering it unmoved → fails. Mutation: capacity `reach` instead of
    /// `reach + 1 + MAX_TRANSPORT_CHANGES` → with one-frame blocks the
    /// piece covering frame `start - reach` is dropped → the in-reach check
    /// fails.
    #[test]
    fn older_than_the_history_is_the_oldest_transport_unmoved() {
        let e = env(0, 64, rolling(2.0), TransportChanges::NONE);
        let mut h = TransportHistory::with_reach(lat(100));
        h.observe(&e);
        assert_eq!(e.transport_before(off(10), lat(50), &h), rolling(2.0));
        let none = TransportHistory::none();
        let later = env(640, 64, rolling(9.0), TransportChanges::NONE);
        assert_eq!(later.transport_before(off(0), lat(10), none), rolling(9.0));

        // One-frame blocks, each a seek: the ring is full of pieces, and the
        // frame exactly `reach` back is still covered.
        const REACH: usize = 20;
        let mut p = Played::new(REACH);
        for f in 0..100u64 {
            let e1 = env(f, 1, rolling(f as f64), TransportChanges::NONE);
            p.play(e1);
            if f as usize >= REACH {
                assert_eq!(
                    e1.transport_before(off(0), lat(REACH), &p.history),
                    p.truth(f - REACH as u64),
                    "frame {f}"
                );
            }
        }
    }

    /// A ring that replaces another keeps the newest pieces that fit, in
    /// order, and leaves the old one empty.
    ///
    /// Mutation: `.take(self.capacity)` for `.skip(skip)` in `take_from` →
    /// it keeps the oldest pieces, and the frame before the last block
    /// reads stale → fails. Mutation: skip 0 → it keeps all 20, past its
    /// capacity → fails.
    #[test]
    fn a_new_ring_takes_the_newest_pieces() {
        let mut big = Played::new(1_000);
        for b in 0..20u64 {
            big.play(env(
                b * 32,
                32,
                rolling(b as f64 * 10.0),
                TransportChanges::NONE,
            ));
        }
        let mut small = TransportHistory::with_reach(lat(1));
        small.take_from(&mut big.history);
        assert!(big.history.live().is_empty());
        assert_eq!(small.live().len(), 1 + 1 + MAX_TRANSPORT_CHANGES);
        let last = *big.blocks.last().expect("played");
        assert_eq!(
            last.transport_before(off(0), lat(1), &small),
            last.transport_before(off(0), lat(1), &{
                let mut h = TransportHistory::with_reach(lat(1_000));
                for e in &big.blocks {
                    h.observe(e);
                }
                h
            })
        );
    }

    /// The capacity is tight: `reach` one-frame blocks, each a seek, then a
    /// block carrying `MAX_TRANSPORT_CHANGES` changes. Nothing can be
    /// dropped (every piece is within reach), so the ring holds exactly
    /// `reach + 1 + MAX_TRANSPORT_CHANGES` pieces, and the frame `reach`
    /// back, the very first one, still reads what its block said.
    ///
    /// Mutation: capacity `reach + MAX_TRANSPORT_CHANGES` in `with_reach` →
    /// the first piece is dropped, the frame reads the clamp (frame 1's
    /// seek) → fails.
    #[test]
    fn the_capacity_holds_exactly_the_worst_case() {
        const REACH: usize = 12;
        let mut p = Played::new(REACH);
        for f in 0..REACH as u64 {
            p.play(env(f, 1, rolling(f as f64 * 3.0), TransportChanges::NONE));
        }
        let len = MAX_TRANSPORT_CHANGES + 1;
        let mut changes = TransportChanges::NONE;
        for k in 1..len {
            changes
                .push(off(k), rolling(100.0 + k as f64))
                .expect("room for every change");
        }
        let last = env(REACH as u64, len, rolling(50.0), changes);
        p.play(last);
        assert_eq!(p.history.live().len(), REACH + 1 + MAX_TRANSPORT_CHANGES);
        assert_eq!(
            last.transport_before(off(0), lat(REACH), &p.history),
            p.truth(0)
        );
    }

    /// The ring keeps what is in reach, not what it could hold: blocks of a
    /// device quantum leave about `reach / quantum + 2` pieces live however
    /// long they run, and the buffer is written no further than twice that,
    /// while every frame in reach still reads its block.
    ///
    /// Mutation: drop the trim loop in `observe` → the live pieces climb to
    /// the capacity (1 009) → fails. Mutation: compact only when the buffer
    /// is full (`self.buf.len() == self.buf.capacity()`) → the buffer is
    /// written to its end → fails.
    #[test]
    fn the_ring_keeps_only_what_is_in_reach() {
        const REACH: usize = 1_000;
        const QUANTUM: usize = 64;
        let mut p = Played::new(REACH);
        let mut most = (0, 0);
        for b in 0..5_000u64 {
            let e = env(
                b * QUANTUM as u64,
                QUANTUM,
                rolling(b as f64),
                TransportChanges::NONE,
            );
            p.play(e);
            most = (
                most.0.max(p.history.live().len()),
                most.1.max(p.history.buf.len()),
            );
            if b > 20 {
                assert_eq!(
                    e.transport_before(off(5), lat(REACH), &p.history),
                    p.truth(b * QUANTUM as u64 + 5 - REACH as u64),
                    "block {b}"
                );
            }
            if p.blocks.len() > 64 {
                p.blocks.remove(0);
            }
        }
        let live = REACH / QUANTUM + 2;
        assert!(most.0 <= live, "{} pieces live, {live} needed", most.0);
        assert!(most.1 <= 2 * live, "{} slots written", most.1);
    }
}

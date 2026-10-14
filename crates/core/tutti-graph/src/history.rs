//! [`TransportHistory`]: the transport the graph played over its last
//! frames, so a node behind a latent path reads the transport its input
//! audio was rendered at ([`Cx::transport_at`](crate::Cx::transport_at)).

use std::collections::VecDeque;

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
/// # Size, and where it is allocated
///
/// A history answers for frames up to its **reach** before the block being
/// rendered. Pieces start on distinct frames, so at most `reach` of them
/// start inside that window, one more covers its first frame, and the
/// current block adds at most `1 + MAX_TRANSPORT_CHANGES`: the ring holds
/// `reach + 1 + MAX_TRANSPORT_CHANGES` pieces and drops the oldest past
/// that. A host that counts frames and blocks of the device's quantum fills
/// a few of them; the bound is for blocks one frame long.
///
/// The executor's reach is its plan's longest arrival
/// ([`Plan::max_arrival`](crate::Plan::max_arrival)). A commit whose plan
/// changes it carries a new ring, allocated on the control thread; applying
/// it moves the pieces across and sends the old ring back in the box, so
/// the audio thread neither allocates nor frees one. A block only appends
/// and drops pieces in place.
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
    pieces: VecDeque<Piece>,
    /// Pieces kept: `reach + 1 + MAX_TRANSPORT_CHANGES`, or 0 for no reach.
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
            pieces: VecDeque::new(),
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
            pieces: VecDeque::with_capacity(capacity),
            capacity,
            reach,
        }
    }

    /// How far back it answers exactly.
    pub fn reach(&self) -> Latency {
        self.reach
    }

    /// Records the block about to be rendered: its first frame and each
    /// transport change inside it, as [`Env::segments`] cuts them. Call it
    /// once per block, before any node reads the history, in frame order.
    /// Allocation-free: past its capacity the oldest piece is dropped.
    ///
    /// A block that starts at or before a piece already kept (a clock that
    /// went back) first drops every piece from that frame on.
    pub fn observe(&mut self, env: &Env) {
        if self.capacity == 0 {
            return;
        }
        while self.pieces.back().is_some_and(|p| p.frame >= env.frame) {
            self.pieces.pop_back();
        }
        for (_, piece) in env.segments() {
            if self.pieces.len() == self.capacity {
                self.pieces.pop_front();
            }
            self.pieces.push_back(Piece {
                frame: piece.frame,
                rate: env.sample_rate,
                transport: piece.transport,
            });
        }
    }

    /// Forgets every piece (a sample-rate change: see the module docs).
    pub fn clear(&mut self) {
        self.pieces.clear();
    }

    /// Takes `older`'s pieces, the newest that fit, leaving it empty with
    /// its buffer: applying a commit that carries a new ring. No allocation
    /// while this one's buffer holds its capacity, which `with_reach`
    /// reserved.
    pub(crate) fn take_from(&mut self, older: &mut TransportHistory) {
        self.pieces.clear();
        let skip = older.pieces.len().saturating_sub(self.capacity);
        self.pieces.extend(older.pieces.drain(..).skip(skip));
    }

    /// The transport `back` frames before frame `start`, when a piece kept
    /// covers that frame; `None` when it is older than every piece.
    fn before(&self, start: Frame, back: Samples) -> Option<Transport> {
        // Pieces are in frame order, so "starts at or before the frame" holds
        // for a prefix. A piece too far back for `since` to count (only on a
        // 32-bit target, a day of audio) is before it too.
        let covers = |p: &Piece| p.frame <= start && start.since(p.frame).is_none_or(|d| d >= back);
        let i = self.pieces.partition_point(covers).checked_sub(1)?;
        let p = &self.pieces[i];
        let into = start.since(p.frame)?.checked_sub(back)?;
        Some(roll(p.transport, p.rate, into))
    }

    /// The oldest piece's transport, at its own frame.
    fn oldest(&self) -> Option<Transport> {
        self.pieces.front().map(|p| p.transport)
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
        assert!(big.history.pieces.is_empty());
        assert_eq!(small.pieces.len(), 1 + 1 + MAX_TRANSPORT_CHANGES);
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
}

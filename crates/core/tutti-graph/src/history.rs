//! [`TransportHistory`]: the transport's jumps over the graph's recent
//! past, so a node behind a latent path reads the transport its input audio
//! was rendered at ([`Cx::transport_at`](crate::Cx::transport_at)).

use tutti_types::latency::MAX_NODE_LATENCY;
use tutti_types::{Frame, Latency, SampleRate, Samples};

use crate::node::{Env, Transport, MAX_TRANSPORT_CHANGES};
use crate::time::Offset;
use tutti_types::PlayReading;

/// From `frame` on, the transport was `transport`, counted at `rate`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Piece {
    frame: Frame,
    rate: SampleRate,
    transport: Transport,
    /// The play clock at `frame`. A play start changes `transport.playing`,
    /// so it is always a piece of its own, and between pieces the clock
    /// advances one per frame: the piece's reading rolled forward is exact.
    play: PlayReading,
}

/// An unused slot.
const EMPTY: Piece = Piece {
    frame: Frame::ZERO,
    rate: SampleRate(0.0),
    transport: Transport::new(false, tutti_types::Bpm(120.0), tutti_types::Beat(0.0), None),
    play: PlayReading {
        epoch: 0,
        play: Frame::ZERO,
    },
};

/// Pieces a history keeps: see [`TransportHistory`], "Size".
const JUMPS: usize = 256;

/// The longest continuous stretch between pieces: see
/// [`TransportHistory::CHECKPOINT`].
const CHECKPOINT: usize = 8_192;

// One block's pieces (its start and every change) and the one covering the
// frame before it always fit.
const _: () = assert!(JUMPS > MAX_TRANSPORT_CHANGES + 1);
// Continuous playback is exact back to `(JUMPS - 1) * CHECKPOINT` frames:
// at least four maxed-out nodes in series.
const _: () = assert!((JUMPS - 1) * CHECKPOINT >= 4 * MAX_NODE_LATENCY.get());

/// The transport's jumps over the graph's recent past: what a node behind a
/// latent path reads its transport from ([`Env::transport_before`],
/// [`Cx::transport_at`](crate::Cx::transport_at)).
///
/// # Why a history, and not a closed form
///
/// A node whose compiled arrival latency is `a` hears, at frame `F` of its
/// block, the audio of timeline frame `F - a`. When that is before the
/// block's first frame, the transport there is not in the block's [`Env`].
/// Rewinding the block-start transport in closed form is right only while
/// nothing happened in between: a seek, a start or stop, a tempo or loop
/// edit in the last `a` frames makes it wrong, and a loop wrap cannot be
/// told from a start by the block after it.
///
/// # What is kept: the jumps
///
/// The frame is the source of truth: away from a jump, the transport at a
/// frame is the last jump before it rolled forward in closed form with the
/// host's clock ([`FrameClock`](tutti_types::FrameClock), the walk
/// [`Env::transport_at`] uses, loop wraps included). So the history keeps a
/// **piece** (a frame, and the transport the host reported there) only where
/// the transport is not the previous piece rolled forward:
///
/// - a block's first frame or a [transport change](Env::changes) inside it
///   whose transport differs from that roll: a start, stop, seek, tempo or
///   loop edit, an untimed command landing on a block start, a host whose
///   position is not a frame count ([`Transport::new`]), the first block;
/// - a **checkpoint**: a block's first frame when the newest piece is
///   [`CHECKPOINT`](Self::CHECKPOINT) frames or more behind it, so no roll
///   (whose cost grows with the loop wraps it walks) is ever longer than
///   that plus a block.
///
/// A query takes the newest piece at or before its frame and rolls it
/// forward. That is **exactly** what [`Env::transport_at`] said for that
/// frame in the block that held it: a piece is skipped only when the block's
/// transport *equals* the roll, and the clock's closed form makes rolling
/// `a` then `b` frames land on the bits rolling `a + b` does
/// ([`TimelineSegment::beat_at`](tutti_types::TimelineSegment::beat_at) is
/// relative to its segment's origin, and a wrap lands on the unique frame
/// that reaches the loop's end). Not a tolerance: the tests compare
/// `Transport`s with `==`.
///
/// # Size, and the cost of a query
///
/// Fixed, whatever the reach: [`JUMPS`](Self::JUMPS) = 256 pieces inline,
/// about 104 bytes each (some 26 KiB, no heap). Nothing is sized from the
/// plan, so a recompile changes nothing here, and the audio thread never
/// allocates or frees.
///
/// A query is a binary search (8 steps) and one roll of under
/// `CHECKPOINT` + `MaxBlock` frames. The roll is closed form except for loop
/// wraps, which it walks one by one: the worst case, a loop one frame long,
/// is about 8 200 wraps (one per frame) for a query at a 64-frame maximum
/// block; a loop of a beat at 120 BPM and 48 kHz wraps at most once.
///
/// # Where it stops
///
/// A frame older than the oldest piece kept is answered with that piece's
/// transport, **unmoved** ([`TransportHistory::new`] before anything is
/// observed: the block's own [`Env::transport`]). That happens:
///
/// - **before the graph's first block**: the first `a` frames a node with
///   arrival `a` renders, where its input is a delay line's silence;
/// - **after a sample-rate change**: frames counted at the old rate name
///   nothing at the new one, so the history is [cleared](Self::clear);
/// - **past more than `JUMPS` pieces**: when more than 256 pieces fall
///   inside a node's arrival, the oldest are dropped. Continuous playback
///   spends one checkpoint per `CHECKPOINT` frames, so with no jump at all
///   it is exact back to `(JUMPS - 1) * CHECKPOINT` = **2 088 960 frames**
///   (43.5 s at 48 kHz), and clamps past that. An arrival is a sum of node
///   latencies, each clamped to `MAX_NODE_LATENCY` (480 000), so that is
///   four maxed-out nodes in series and some; a fifth goes past it. Jumps
///   inside the arrival spend pieces too: a 10-second lookahead at 48 kHz
///   holds 59 checkpoints and leaves room for some 196 jumps (a burst of
///   seeks, or a host reporting a hand-built transport or a new tempo, a
///   ramp, every block).
#[derive(Clone, Debug)]
pub struct TransportHistory {
    /// A ring of pieces in frame order: `len` of them from `head`.
    pieces: [Piece; JUMPS],
    head: usize,
    len: usize,
}

impl Default for TransportHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// A history that keeps nothing, for a [`Cx`](crate::Cx) built where nothing
/// is observed (a node driven by hand, the contract harness).
static NONE: TransportHistory = TransportHistory::new();

impl TransportHistory {
    /// How many pieces it keeps.
    pub const JUMPS: usize = JUMPS;

    /// The longest continuous stretch, in frames, between pieces: a block
    /// starting this far (or further) past the newest piece is recorded as a
    /// checkpoint.
    pub const CHECKPOINT: Samples = Samples(CHECKPOINT);

    /// An empty history. Nothing is allocated, now or later.
    pub const fn new() -> Self {
        Self {
            pieces: [EMPTY; JUMPS],
            head: 0,
            len: 0,
        }
    }

    /// An empty history, borrowed for as long as anything needs it.
    pub fn none() -> &'static TransportHistory {
        &NONE
    }

    fn get(&self, i: usize) -> &Piece {
        &self.pieces[(self.head + i) % JUMPS]
    }

    /// Records the block about to be rendered: of its pieces as
    /// [`Env::segments`] cuts them (its first frame, each change), those
    /// that are not the newest piece rolled forward, and a checkpoint (see
    /// the type docs). Call it once per block, in frame order, before any
    /// node reads the history. Allocation-free; past [`JUMPS`](Self::JUMPS)
    /// pieces the oldest is dropped.
    ///
    /// A block must start after every piece kept. The executor's clock only
    /// moves back on a sample-rate change, and the history is cleared
    /// first, so this is debug-asserted rather than repaired.
    pub fn observe(&mut self, env: &Env) {
        debug_assert!(
            self.newest().is_none_or(|p| p.frame < env.frame),
            "a block observed at or before a piece already kept"
        );
        for (start, piece) in env.segments() {
            let t = piece.transport;
            let keep = match self.newest() {
                None => true,
                Some(p) => {
                    let gap = piece.frame.since(p.frame).unwrap_or(Samples::ZERO);
                    p.rate != env.sample_rate
                        || roll(p.transport, p.rate, gap) != t
                        || (start == Offset::ZERO && gap.get() >= Self::CHECKPOINT.get())
                }
            };
            if keep {
                self.push(Piece {
                    frame: piece.frame,
                    rate: env.sample_rate,
                    transport: t,
                    play: piece.play.start,
                });
            }
        }
    }

    fn push(&mut self, p: Piece) {
        if self.len == JUMPS {
            // Full: the oldest goes, and frames before the new oldest clamp.
            self.head = (self.head + 1) % JUMPS;
            self.len -= 1;
        }
        self.pieces[(self.head + self.len) % JUMPS] = p;
        self.len += 1;
    }

    fn newest(&self) -> Option<&Piece> {
        self.len.checked_sub(1).map(|i| self.get(i))
    }

    /// Forgets every piece (a sample-rate change: see the type docs).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// The transport `back` frames before frame `start`, when a piece kept
    /// covers that frame; `None` when it is older than every piece.
    fn before(&self, start: Frame, back: Samples) -> Option<Transport> {
        self.piece_before(start, back)
            .map(|(p, into)| roll(p.transport, p.rate, into))
    }

    /// The play clock `back` frames before frame `start`, as
    /// [`before`](Self::before) answers for the transport.
    fn play_before(&self, start: Frame, back: Samples) -> Option<PlayReading> {
        self.piece_before(start, back)
            .map(|(p, into)| p.play.after(into))
    }

    /// The newest piece at or before the frame `back` before `start`, and
    /// how far past it that frame is.
    fn piece_before(&self, start: Frame, back: Samples) -> Option<(&Piece, Samples)> {
        // In frame order, so "starts at or before the frame" holds for a
        // prefix: binary search for its end. A piece too far back for
        // `since` to count (a 32-bit target, a day of audio) is before it.
        let covers = |p: &Piece| p.frame <= start && start.since(p.frame).is_none_or(|d| d >= back);
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if covers(self.get(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let p = self.get(lo.checked_sub(1)?);
        let into = start.since(p.frame)?.checked_sub(back)?;
        Some((p, into))
    }

    /// The oldest piece, at its own frame.
    fn oldest(&self) -> Option<&Piece> {
        (self.len > 0).then(|| self.get(0))
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
                    .or_else(|| history.oldest().map(|p| p.transport))
                    .unwrap_or(self.transport)
            }
        }
    }

    /// The play clock `back` frames before frame `offset` of this block:
    /// what a node with arrival latency `back` reads at `offset`
    /// ([`Cx::play_at`](crate::Cx::play_at)). Inside the block it is
    /// [`play_at`](Self::play_at) of the earlier offset; before it, read
    /// from `history` as [`transport_before`](Self::transport_before) reads
    /// the transport, with the same reach. A frame older than anything kept
    /// reads the oldest piece's clock, unmoved, or with nothing kept the
    /// block's first frame's.
    pub fn play_before(
        &self,
        offset: Offset,
        back: Latency,
        history: &TransportHistory,
    ) -> PlayReading {
        let back = back.samples();
        let here = Samples(offset.index());
        match here.checked_sub(back) {
            Some(earlier) => self.play_at(Offset::raw(earlier.get() as u32)),
            None => {
                let before = back.checked_sub(here).unwrap_or(Samples::ZERO);
                history
                    .play_before(self.frame, before)
                    .or_else(|| history.oldest().map(|p| p.play))
                    .unwrap_or(self.play.start)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{LoopRange, TransportChanges};
    use tutti_types::{Beat, Bpm, SegmentOrigin};

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
            play: crate::PlayClock::ZERO,
        }
    }

    fn rolling(beat: f64) -> Transport {
        Transport::new(true, Bpm(120.0), Beat(beat), None)
    }

    /// A host that counts frames, at `beat` (its segment's first frame).
    fn counted(beat: f64, looping: Option<LoopRange>) -> Transport {
        Transport::counted(
            true,
            Bpm(120.0),
            SegmentOrigin {
                beat: Beat(beat),
                frame: Frame(0),
                sample_rate: SR,
            },
            looping,
        )
    }

    /// The transport at `e`'s end: where a host that counts frames starts
    /// the next block.
    fn end_of(e: &Env) -> Transport {
        let longer = Env {
            block_len: Samples(e.block_len.get() + 1),
            ..*e
        };
        longer.transport_at(off(e.block_len.get()))
    }

    fn off(i: usize) -> Offset {
        Offset::raw(i as u32)
    }

    fn lat(n: usize) -> Latency {
        Latency::new(Samples(n))
    }

    /// The blocks a host played, kept so a test can ask what the block that
    /// held a frame said about it.
    #[derive(Default)]
    struct Played {
        blocks: Vec<Env>,
        history: TransportHistory,
    }

    impl Played {
        fn play(&mut self, e: Env) {
            self.history.observe(&e);
            self.blocks.push(e);
        }

        /// What the block holding absolute frame `f` reported for it.
        fn truth(&self, f: u64) -> Transport {
            let i = self.blocks.partition_point(|e| e.frame.get() <= f) - 1;
            let e = &self.blocks[i];
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
    /// frame back to 200 reads what the block that held it said.
    ///
    /// Mutation: `before` answers `None` (every frame before the block reads
    /// the oldest piece, unmoved) → fails. Mutation: in `before`, take the
    /// piece *after* the newest one at or before the frame
    /// (`self.get(lo.min(self.len - 1))`) → fails. Mutation: take the one
    /// before it (`lo.checked_sub(2)`) → fails.
    #[test]
    fn every_frame_in_reach_reads_what_its_block_said() {
        const REACH: usize = 200;
        let mut p = Played::default();
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
        let mut p = Played::default();
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

    /// Before the first block: the oldest transport kept, unmoved. With no
    /// history at all: the block's own.
    ///
    /// Mutation: roll the oldest piece forward a frame instead of answering
    /// it unmoved → fails.
    #[test]
    fn older_than_the_history_is_the_oldest_transport_unmoved() {
        let e = env(0, 64, rolling(2.0), TransportChanges::NONE);
        let mut h = TransportHistory::new();
        h.observe(&e);
        assert_eq!(e.transport_before(off(10), lat(50), &h), rolling(2.0));
        let none = TransportHistory::none();
        let later = env(640, 64, rolling(9.0), TransportChanges::NONE);
        assert_eq!(later.transport_before(off(0), lat(10), none), rolling(9.0));
    }

    /// A host that counts frames plays a 10-second lookahead's worth and
    /// more in ordinary blocks, through a 1-beat loop that wraps every
    /// 24 000 frames and a seek every 1 000 blocks: the history stays its
    /// fixed size, keeps only the jumps and the checkpoints, and every frame
    /// 480 000 back (a 10-second lookahead at 48 kHz) reads what its block
    /// said, to the bit.
    ///
    /// Mutation: never keep a block start after the first (drop the
    /// `roll(..) != t` term) → the seeks are lost → fails. Mutation: keep
    /// every block start (`keep = true`) → 256 pieces cover 65 536 frames,
    /// and frames 480 000 back read the clamp → fails. Mutation: drop the
    /// checkpoint term → the pieces kept are the 12 jumps and nothing else
    /// (the count check fails).
    #[test]
    fn a_counting_host_keeps_only_the_jumps_whatever_the_reach() {
        const REACH: usize = 480_000;
        const BLOCK: usize = 256;
        let looping = LoopRange {
            start: Beat(0.0),
            end: Beat(1.0),
        };
        let mut p = Played::default();
        let mut next = counted(0.0, Some(looping));
        let mut checked = 0;
        for b in 0..12_000u64 {
            if b % 1_000 == 999 {
                // A seek to a beat of its own, counted from here.
                next = counted(0.25 + (b / 1_000) as f64 * 0.01, Some(looping));
            }
            let e = env(b * BLOCK as u64, BLOCK, next, TransportChanges::NONE);
            p.play(e);
            next = end_of(&e);
            if b % 97 == 0 && e.frame.get() >= REACH as u64 {
                for o in [0usize, 100] {
                    let f = e.frame.get() + o as u64 - REACH as u64;
                    assert_eq!(
                        e.transport_before(off(o), lat(REACH), &p.history),
                        p.truth(f),
                        "block {b}, offset {o}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 100);
        // 12 000 blocks of 256 frames: 3 072 000 frames, a checkpoint at
        // least every 8 192 (every 32 blocks), and a seek every 1 000.
        assert_eq!(p.history.len, JUMPS, "the ring is full, and no larger");
        let kept: Vec<u64> = (0..p.history.len)
            .map(|i| p.history.get(i).frame.get())
            .collect();
        assert!(
            kept.windows(2).all(|w| w[1] - w[0] <= 8_192 + BLOCK as u64),
            "no gap between pieces is longer than a checkpoint and a block"
        );
        assert_eq!(
            std::mem::size_of::<TransportHistory>(),
            std::mem::size_of::<[Piece; JUMPS]>() + 2 * std::mem::size_of::<usize>()
        );
        // The documented figure: about 104 bytes a piece (the transport and
        // the play clock), some 26 KiB.
        let size = std::mem::size_of::<TransportHistory>();
        assert!((24 * 1024..=28 * 1024).contains(&size), "{size} bytes");
    }

    /// A burst of more than `JUMPS` seeks, one per one-frame block: the ring
    /// keeps the newest 256, a frame the newest 256 still cover reads its
    /// block, and an older one reads the oldest kept, unmoved: exactly at
    /// the boundary.
    ///
    /// Mutation: when full, overwrite the newest piece instead of dropping
    /// the oldest (skip the `head` advance, write at `len - 1`) → the
    /// boundary moves → fails.
    #[test]
    fn a_burst_past_the_capacity_clamps_where_the_oldest_kept_starts() {
        const SEEKS: u64 = 400;
        let mut p = Played::default();
        for f in 0..SEEKS {
            p.play(env(f, 1, rolling(f as f64 * 3.0), TransportChanges::NONE));
        }
        let e = env(SEEKS, 64, rolling(1_000.0), TransportChanges::NONE);
        p.play(e);
        // Kept: blocks SEEKS - 255 .. SEEKS, and `e`.
        let oldest = SEEKS - (JUMPS as u64 - 1);
        assert_eq!(p.history.get(0).frame.get(), oldest);
        let at = |back: u64| e.transport_before(off(0), lat(back as usize), &p.history);
        // The clamp hands back the piece as the host gave it (a bare beat);
        // a roll reports it counted. The same transport either way.
        let same = |a: Transport, b: Transport| {
            (a.playing, a.tempo, a.beat(), a.looping, a.recording())
                == (b.playing, b.tempo, b.beat(), b.looping, b.recording())
        };
        assert_eq!(at(SEEKS - oldest), p.truth(oldest));
        assert!(
            same(at(SEEKS - oldest + 1), p.truth(oldest)),
            "clamped, unmoved"
        );
        assert!(same(at(SEEKS), p.truth(oldest)), "clamped, unmoved");
        assert!(!same(p.truth(oldest - 1), p.truth(oldest)));
    }

    /// The bound for continuous playback, exactly: blocks of `CHECKPOINT`
    /// frames make every block start a checkpoint, so the newest 256 pieces
    /// reach back `(JUMPS - 1) * CHECKPOINT` = 2 088 960 frames from the
    /// last block's start (at least four `MAX_NODE_LATENCY`s). That frame
    /// reads its block; the one before it reads the clamp (the oldest piece,
    /// unmoved).
    ///
    /// Mutation: checkpoint on `gap > CHECKPOINT` (not `>=`) → a checkpoint
    /// every other block, twice the reach, and the frame past the bound is
    /// exact → fails. Mutation: `JUMPS` 255 → the bound moves a checkpoint
    /// closer (the compile-time bound still holds) → fails.
    #[test]
    fn continuous_playback_is_exact_back_to_the_documented_bound() {
        let block = TransportHistory::CHECKPOINT.get();
        let bound = (JUMPS as u64 - 1) * block as u64;
        assert_eq!(bound, 2_088_960);
        assert!(bound >= 4 * MAX_NODE_LATENCY.get() as u64);
        let mut p = Played::default();
        let mut next = counted(0.0, None);
        for b in 0..300u64 {
            let e = env(b * block as u64, block, next, TransportChanges::NONE);
            p.play(e);
            next = end_of(&e);
        }
        let last = *p.blocks.last().expect("played");
        let back = |n: u64| last.transport_before(off(0), lat(n as usize), &p.history);
        let edge = last.frame.get() - bound;
        assert_eq!(back(bound), p.truth(edge), "just inside: exact");
        assert_eq!(back(bound + 1), p.truth(edge), "just past: the clamp");
        assert_ne!(p.truth(edge - 1), p.truth(edge));
    }

    /// A host that counts frames changes one field and keeps its position:
    /// a stop in place, recording armed, a loop armed around the playhead
    /// (it wraps 40 frames on), a tempo step on the same origin. Each runs
    /// on its own rolling host, at a block start and inside a block (at
    /// offset 20); each is a piece (the roll is not the block's transport),
    /// so every frame back reads its block.
    ///
    /// Mutations (each fails here, and nowhere else): in `observe`, compare
    /// the roll `r` with `t` ignoring one field, through a copy `u` of `t`:
    /// `u.playing = r.playing`, `u.looping = r.looping`,
    /// `u.tempo = r.tempo`, and `r != t.with_recording(r.recording())`.
    #[test]
    fn a_change_of_one_field_is_a_jump() {
        let one_field = |t: Transport, k: usize| -> Transport {
            let origin = t.origin().expect("counted");
            match k {
                0 => Transport::counted(false, t.tempo, origin, t.looping),
                1 => t.with_recording(true),
                2 => {
                    let b = t.beat().get();
                    Transport::counted(
                        t.playing,
                        t.tempo,
                        origin,
                        Some(LoopRange {
                            start: Beat(b - 0.001),
                            end: Beat(b + 40.0 / FPB),
                        }),
                    )
                }
                _ => Transport::counted(t.playing, Bpm(97.0), origin, t.looping),
            }
        };
        for k in 0..4 {
            for inside in [false, true] {
                let mut p = Played::default();
                let mut next = counted(2.0, None);
                for b in 0..8u64 {
                    let mut changes = TransportChanges::NONE;
                    if b == 3 {
                        if inside {
                            let probe = env(b * 64, 64, next, TransportChanges::NONE);
                            changes
                                .push(off(20), one_field(probe.transport_at(off(20)), k))
                                .expect("inside");
                        } else {
                            next = one_field(next, k);
                        }
                    }
                    let e = env(b * 64, 64, next, changes);
                    p.play(e);
                    next = end_of(&e);
                    for o in [0usize, 30, 63] {
                        for back in [1usize, 10, 25, 64, 100, 200] {
                            let Some(f) = (e.frame.get() + o as u64).checked_sub(back as u64)
                            else {
                                continue;
                            };
                            assert_eq!(
                                e.transport_before(off(o), lat(back), &p.history),
                                p.truth(f),
                                "field {k}, inside {inside}, block {b}, offset {o}, back {back}"
                            );
                        }
                    }
                }
            }
        }
    }
}

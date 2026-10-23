//! [`EnvelopeFollowerNode`]: an audio input's level, sent to control lanes
//! on the play-clock grid (design doc 014 §6).

use tutti_core::{ChannelLayout, SampleRate, Seconds};
use tutti_graph::{
    Cost, Cx, Event, Io, Node, Offset, ParamSegment, Prepare, SegmentShape, Shape, Status,
};
use tutti_mod::{SignalKind, SourceId, SourceNode};
use tutti_types::{SamplePosition, Samples};

use super::PlayWalk;
use crate::automation::CONTROL_STEP;

/// Segments a follower sends per block, at most: one per grid point of a
/// 4096-frame block (128), and as many again for play starts. A block past
/// 8192 frames sends its first 256 pieces; each piece carries both its
/// ends, so the next piece sent is exact again.
pub const FOLLOWER_EVENT_CAPACITY: u32 = 256;

/// Below this the envelope is 0: a release decays to silence in finite
/// time (`ln(1e7) ≈ 16.1` release time constants from full scale, at any
/// `τ`) rather than through subnormals forever, and a silent follower
/// sends nothing.
const FLOOR: f64 = 1e-7;

/// An **envelope follower**: the level of its audio input, sent raw in
/// `[0, 1]` as linear pieces on the graph's play-clock grid, addressed
/// `ParamAddr::Id(source.0)` for every control lane that reads `source`.
///
/// - **The level** of a frame is its **peak over channels**, `max |x_c|`,
///   clamped to `[0, 1]` (a NaN sample reads 0). Peak, not RMS: a frame's
///   RMS over channels is not a level in time but a width-dependent
///   average, so a hard-panned kick would read `1/√2` of itself in stereo
///   and all of itself in mono; the peak reads the same at any width, as a
///   compressor's linked sidechain does.
/// - **One-pole attack and release**, per frame:
///   `env ← (1 − c) · env + c · level`, `c = 1 − exp(−1 / (τ · rate))`
///   ([`Seconds::one_pole_coeff`]), the envelope in `f64`, with `τ` the
///   attack while the level is above the envelope and the release
///   otherwise. A `τ` of zero (or below) follows the level exactly, and
///   an infinite one holds (the envelope never moves that way). Below
///   `1e-7` the envelope is 0, so a release reaches silence in finite
///   time. The pieces it sends are `f32`.
/// - **On the grid, one step late.** On every frame `g` whose play count is
///   a multiple of [`CONTROL_STEP`] it sends a piece from `env(g − step)`
///   to `env(g)` over `[g, g + step)`, where `env(g)` is the envelope after
///   the audio before `g`. So what it sends depends only on the audio up to
///   each grid point, never on where blocks fall. The step's lag is musical
///   delay, not latency: nothing is reported to PDC.
/// - **Silent, it sends nothing**: a piece from 0 to 0 after one that
///   ended at 0 is not sent (a lane reads an input that has not sent as 0).
/// - **It resets at play start**: on the frame the play clock restarts the
///   envelope is 0, and it sends a piece from 0 to 0 there (if its last
///   piece ended elsewhere), so every lane it feeds reads 0 from that frame.
///
/// Insert it `ForkByClone(node)`: it shares nothing with a clone, and a
/// fork's (the node as inserted, reset) starts at 0, as a live play start
/// leaves it.
#[derive(Clone, Debug)]
pub struct EnvelopeFollowerNode {
    source: u32,
    attack: Seconds,
    release: Seconds,
    input: ChannelLayout,
    attack_coeff: f32,
    release_coeff: f32,
    /// In `f64`, so a long time constant decays at its own rate: in `f32`,
    /// `1 − c` would round to 1 past about 640 s at 48 kHz and the envelope
    /// would never decay.
    env: f64,
    /// `env` at the last grid point: the next piece's start.
    from: f64,
    /// The end of the last piece sent (0 before any; NaN for a
    /// [`replacing`](Self::replacing) node before its first).
    sent: f32,
    /// The play clock's epoch at the last frame read; `None` while fresh.
    epoch: Option<u32>,
}

impl EnvelopeFollowerNode {
    /// A follower of an `input`-wide signal, sending as `source`, with
    /// time constants `attack` and `release`.
    pub fn new(source: SourceId, attack: Seconds, release: Seconds, input: ChannelLayout) -> Self {
        let rate = SampleRate::DEFAULT;
        Self {
            source: source.0,
            attack,
            release,
            input,
            attack_coeff: attack.one_pole_coeff(rate),
            release_coeff: release.one_pole_coeff(rate),
            env: 0.0,
            from: 0.0,
            sent: 0.0,
            epoch: None,
        }
    }

    /// The node `decl` describes (what `tutti_mod::compile` gives each
    /// follower), following an `input`-wide signal: `None` when `decl` is
    /// not a follower. The rate comes at `prepare`.
    pub fn from_decl(decl: &SourceNode, input: ChannelLayout) -> Option<Self> {
        match decl.kind {
            SignalKind::Follower { attack, release } => {
                Some(Self::new(decl.id, attack, release, input))
            }
            SignalKind::NoteEnvelope { .. } => None,
        }
    }

    /// This follower, sending its first grid piece even while silent: for
    /// one that replaces another of the same [`SourceId`] (a declaration
    /// edit rebuilds it), since a lane holds the old node's last piece until
    /// this one sends, and a silent follower otherwise sends nothing. A
    /// fork (the node reset) does not keep it.
    pub fn replacing(mut self) -> Self {
        // NaN is no value it could have sent: the first grid piece goes out.
        self.sent = f32::NAN;
        self
    }

    /// The source id its pieces are addressed to.
    pub fn source(&self) -> SourceId {
        SourceId(self.source)
    }

    /// One frame's level into the envelope.
    #[inline]
    fn follow(&mut self, level: f64) {
        let c = f64::from(if level > self.env {
            self.attack_coeff
        } else {
            self.release_coeff
        });
        let env = (1.0 - c) * self.env + c * level;
        self.env = if env < FLOOR { 0.0 } else { env };
    }

    fn idle(&mut self) {
        self.env = 0.0;
        self.from = 0.0;
    }
}

impl Node for EnvelopeFollowerNode {
    /// Its input's width in, no audio out; one event output (the pieces).
    ///
    /// `Moderate`: a frame's peak over its channels and a one-pole step,
    /// with a piece every 32 frames, as the dynamics processors' detectors
    /// cost.
    fn shape(&self) -> Shape {
        Shape::audio(self.input, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(FOLLOWER_EVENT_CAPACITY)
            .with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, prepare: &Prepare) {
        let rate = prepare.sample_rate();
        self.attack_coeff = self.attack.one_pole_coeff(rate);
        self.release_coeff = self.release.one_pole_coeff(rate);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let len = io.frames();
        if len == 0 {
            return Status::Modified;
        }
        let walk = PlayWalk::new(cx, len);
        let channels = io.input_count();
        let step = u64::from(CONTROL_STEP);
        for i in 0..len {
            let r = walk.at(cx, i);
            if self.epoch.is_some_and(|e| e != r.epoch) {
                // A play start: idle on its frame.
                self.idle();
            }
            self.epoch = Some(r.epoch);
            if r.play.get().is_multiple_of(step) {
                let (from, to) = (self.from as f32, self.env as f32);
                if from != 0.0 || to != 0.0 || self.sent != 0.0 {
                    let seg = ParamSegment::foreign(
                        self.source,
                        from,
                        to,
                        Samples(0),
                        SegmentShape::Linear,
                    )
                    .resumed(SamplePosition(0.0), SamplePosition(f64::from(CONTROL_STEP)));
                    if let Some(at) = Offset::new(i, cx.env.block_len) {
                        // Past the capacity (a block past 8192 frames) the
                        // piece is dropped; the next one carries both ends.
                        if io.event_out(0).push(Event::segment(at, seg)).is_ok() {
                            self.sent = to;
                        }
                    }
                }
                self.from = self.env;
            }
            let mut level = 0.0f32;
            for c in 0..channels {
                // `max` ignores a NaN operand: a NaN sample reads 0.
                level = level.max(io.input(c)[i].abs());
            }
            self.follow(f64::from(level.min(1.0)));
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.idle();
        self.sent = 0.0;
        self.epoch = None;
    }
}

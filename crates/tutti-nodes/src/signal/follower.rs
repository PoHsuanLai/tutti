//! [`EnvelopeFollowerNode`]: an audio input's level, sent to control lanes
//! on the play-clock grid.

use tutti_graph::{Cost, Io};
use tutti_modulation::{LevelFollower, SignalKind, SignalSource, SourceId};
use tutti_types::{ChannelLayout, Seconds};

use super::{SourceNode, SourcePorts};

/// An **envelope follower**: the level of its audio input, followed by a
/// one-pole attack and release ([`LevelFollower`]) and sent raw in `[0, 1]`
/// as linear pieces on the play-clock grid, one step late, addressed
/// `ParamAddr::Id(source.0)` for every control lane that reads `source`.
///
/// - **The level** of a frame is its **peak over channels**, `max |x_c|`,
///   clamped to `[0, 1]` (a NaN sample reads 0). Peak, not RMS: a frame's
///   RMS over channels is not a level in time but a width-dependent
///   average, so a hard-panned kick would read `1/√2` of itself in stereo
///   and all of itself in mono; the peak reads the same at any width, as a
///   compressor's linked sidechain does.
/// - **Silent, it sends nothing**, and **it resets at play start**: on the
///   frame the play clock restarts the envelope is 0, and it sends a 0 there
///   (if its last piece ended elsewhere). The rest is [`SourceNode`]'s.
///
/// Insert it `ForkByClone(node)`: it shares nothing with a clone, and a
/// fork's (the node as inserted, reset) starts at 0, as a live play start
/// leaves it.
pub type EnvelopeFollowerNode = SourceNode<LevelFollower>;

impl SourceNode<LevelFollower> {
    /// A follower of an `input`-wide signal, sending as `source`, with
    /// time constants `attack` and `release`.
    #[must_use]
    pub fn new(source: SourceId, attack: Seconds, release: Seconds, input: ChannelLayout) -> Self {
        Self::with_source(source, LevelFollower::new(attack, release), input)
    }

    /// The node `decl` describes (what `tutti_modulation::compile` gives each
    /// follower), following an `input`-wide signal: `None` when `decl` is
    /// not a follower. The rate comes at `prepare`.
    #[must_use]
    pub fn from_decl(decl: &SignalSource, input: ChannelLayout) -> Option<Self> {
        match decl.kind {
            SignalKind::Follower { attack, release } => {
                Some(Self::new(decl.id, attack, release, input))
            }
            SignalKind::NoteEnvelope { .. } => None,
        }
    }
}

impl SourcePorts for LevelFollower {
    /// A frame's peak over its channels and a one-pole step, with a piece
    /// every 32 frames, as the dynamics processors' detectors cost.
    const COST: Cost = Cost::Moderate;
    const MIDI_IN: bool = false;

    fn hear(&mut self, io: &Io<'_>, from: usize, to: usize) {
        let channels = io.input_count();
        for i in from..to {
            let mut level = 0.0f32;
            for c in 0..channels {
                // `max` ignores a NaN operand: a NaN sample reads 0.
                level = level.max(io.input(c)[i].abs());
            }
            self.follow(level);
        }
    }
}

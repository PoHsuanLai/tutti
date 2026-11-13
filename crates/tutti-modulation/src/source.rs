//! A **control source**: a control value produced as pieces on the graph's play
//! clock. The source holds the math (a detector, a note envelope); `tutti-nodes`'
//! `SourceNode` sends what it produces.
//!
//! Two clocks meet at a control. An effect solves on the **stream** clock (frames since
//! its reset), because audio runs through it while the transport is stopped. A source
//! produces on the **play** clock, which counts one per frame whether the transport is
//! rolling or stopped and restarts at 0 on every play start. A control an effect samples
//! is exact at any frame, so the two meet without loss; only the glide between two
//! solves approximates, by at most one period.

mod follower;
mod note_adsr;

pub use follower::LevelFollower;
pub use note_adsr::NoteAdsr;

use std::num::NonZeroU32;

use tutti_curve::{Law, Piece};
use tutti_types::{Frame, SampleRate, Samples};
use tutti_ump::Ump;

/// A stride of the play clock's grid, in play frames: a source on it sends a
/// piece on every frame whose play count is a multiple of the stride.
///
/// Its own type, so a play stride is never taken for an effect's stream
/// period (`tutti_dsp::Cadence`): the play clock restarts at every play start,
/// the stream clock does not. Every `NonZeroU32` is a stride.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayGrid(NonZeroU32);

impl PlayGrid {
    /// A grid every `stride` play frames.
    #[must_use]
    pub const fn new(stride: NonZeroU32) -> Self {
        Self(stride)
    }

    /// The stride.
    #[must_use]
    pub const fn get(self) -> NonZeroU32 {
        self.0
    }

    /// Returns the stride in play frames, as a plain count.
    #[must_use]
    pub const fn stride(self) -> u32 {
        self.0.get()
    }
}

impl From<PlayGrid> for NonZeroU32 {
    fn from(grid: PlayGrid) -> Self {
        grid.0
    }
}

/// The control grid: 32 play frames, the step every control lane and the
/// envelope follower send on.
pub const CONTROL_GRID: PlayGrid = PlayGrid(NonZeroU32::new(32).expect("32 is not zero"));

/// One piece a source sends: its values and law, and its length in frames,
/// after which it holds `piece.to`. A length of 0 is a step to `piece.to`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leg {
    /// The values and the law between them.
    pub piece: Piece,
    /// Frames from `piece.from` to `piece.to`.
    pub frames: Samples,
}

impl Leg {
    /// A linear leg from `from` to `to` over `frames`.
    #[must_use]
    pub const fn linear(from: f32, to: f32, frames: Samples) -> Self {
        Self {
            piece: Piece::new(from, to, Law::Linear),
            frames,
        }
    }

    /// A step to `value`, held.
    #[must_use]
    pub const fn hold(value: f32) -> Self {
        Self::linear(value, value, Samples(0))
    }
}

/// A source of a control value as [`Leg`]s on the play clock: what each of its
/// stops (a play start, a grid point, a corner of its own, a MIDI message)
/// sends.
///
/// The sender walks the play clock and asks at each stop, in this order on one
/// frame: a play start ([`start`](Self::start)), a corner the source named
/// ([`corner`](Self::corner)), a grid point ([`grid`](Self::grid)), the MIDI
/// on that frame ([`heard`](Self::heard)). A leg the sender sends replaces one
/// sent earlier on the same frame. A source reads audio through its node
/// (the node reduces it to what the source takes, a level for a follower), so
/// the trait names no audio.
pub trait ControlSource: Clone + Send + 'static {
    /// The grid it sends on, or `None` for a source that sends only at its
    /// corners and on MIDI.
    const GRID: Option<PlayGrid>;

    /// Takes the sample rate: what it derives in frames.
    fn prepare(&mut self, rate: SampleRate);

    /// A play start, or a reset: the state of a source that has heard nothing,
    /// and the leg to send there, if it sends one.
    fn start(&mut self) -> Option<Leg>;

    /// The play frame of its next corner: a frame where it sends a leg of its
    /// own, such as a stage's end. `None` if none is due.
    fn next_corner(&self) -> Option<Frame> {
        None
    }

    /// Its corner at play frame `t` is reached: the leg to send, if any.
    fn corner(&mut self, _t: Frame) -> Option<Leg> {
        None
    }

    /// A grid point at play frame `t`, the next `frames` on: the leg to send
    /// over those frames, if any.
    fn grid(&mut self, _t: Frame, _frames: Samples) -> Option<Leg> {
        None
    }

    /// A MIDI message at play frame `t`, where the value already sent stands
    /// at `level`: the leg to send, if any.
    fn heard(&mut self, _t: Frame, _level: f32, _ump: Ump) -> Option<Leg> {
        None
    }
}

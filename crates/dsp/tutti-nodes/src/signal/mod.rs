//! Signal-driven modulation sources (design doc 014 §6): graph nodes that
//! send a raw value in `[0, 1]` into a control lane's event input,
//! addressed `ParamAddr::Id(source.0)`.
//!
//! - [`EnvelopeFollowerNode`]: an audio input's level, on the play-clock
//!   grid, one grid step late.
//! - [`NoteEnvelopeNode`]: an ADSR on a MIDI input, as exact segments.
//!
//! Both **reset at play start**, on the frame the graph's play clock
//! (`Env::play`) restarts, read at the node's input time (`Cx::play_at`),
//! so a live play from beat X and an export from beat X agree (doc 014 §6).
//! A fork's node starts in that state: both are inserted
//! `ForkByClone(node)`, whose fork is the node as inserted, reset. Neither
//! holds a cell, a handle or anything else a clone would share, and neither
//! has controls: everything it does follows from its declaration, its input
//! and the play clock, so there is nothing for a `ForkSource` to gather.

mod follower;
mod note_envelope;

pub use follower::{EnvelopeFollowerNode, FOLLOWER_EVENT_CAPACITY};
pub use note_envelope::{NoteEnvelopeNode, NOTE_ENVELOPE_EVENT_CAPACITY};

use tutti_graph::{Cx, Offset, PlayReading};
use tutti_types::Samples;

/// The play clock at each frame of a block, at a node's input time
/// (`Cx::play_at`): read per frame only in a block with a play start
/// inside it, otherwise counted on from the block's first frame.
struct PlayWalk {
    first: PlayReading,
    /// No play start after the first frame: every frame is `first` counted
    /// on. Epochs are fresh at every start, so a reading at the last frame
    /// in the first's epoch, the right count on, means none.
    straight: bool,
}

impl PlayWalk {
    fn new(cx: &Cx<'_>, len: usize) -> Self {
        let first = cx.play_at(Offset::ZERO);
        let last = Offset::new(len - 1, cx.env.block_len).unwrap_or(Offset::ZERO);
        let straight = cx.play_at(last) == first.after(Samples(len - 1));
        Self { first, straight }
    }

    /// The reading at frame `i` of the block.
    fn at(&self, cx: &Cx<'_>, i: usize) -> PlayReading {
        if self.straight {
            self.first.after(Samples(i))
        } else {
            cx.play_at(Offset::new(i, cx.env.block_len).unwrap_or(Offset::ZERO))
        }
    }

    /// The block's play starts, in order: each frame whose epoch is not
    /// the one before it. `prev` is the epoch on the last block's last
    /// frame, `None` for a fresh node: a fresh node is already idle, so a
    /// start on its first frame is none, but one later in its first block
    /// is. One scan per block, however often the caller asks.
    fn starts<'w, 'c, 'a>(
        &'w self,
        cx: &'c Cx<'a>,
        len: usize,
        prev: Option<u32>,
    ) -> Starts<'w, 'c, 'a> {
        Starts {
            walk: self,
            cx,
            i: 0,
            len,
            prev: prev.unwrap_or(self.first.epoch),
        }
    }
}

/// [`PlayWalk::starts`]: scans each frame once.
struct Starts<'w, 'c, 'a> {
    walk: &'w PlayWalk,
    cx: &'c Cx<'a>,
    /// The next frame to read.
    i: usize,
    len: usize,
    /// The epoch on frame `i - 1`.
    prev: u32,
}

impl Iterator for Starts<'_, '_, '_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.walk.straight {
            // No start after the first frame: at most one, on it.
            let first = std::mem::replace(&mut self.i, self.len) == 0;
            return (first && self.walk.first.epoch != self.prev).then_some(0);
        }
        while self.i < self.len {
            let (i, e) = (self.i, self.walk.at(self.cx, self.i).epoch);
            self.i += 1;
            if e != self.prev {
                self.prev = e;
                return Some(i);
            }
        }
        None
    }
}

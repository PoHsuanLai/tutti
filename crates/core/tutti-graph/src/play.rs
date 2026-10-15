//! The graph's **play clock**: a frame count that runs one per frame,
//! rolling or stopped, restarts at 0 on every play start, and ignores seeks,
//! loops and tempo. What a free-running (seconds-clocked) modulation layer
//! reads its phase from (doc 014 §4.3), so its phase is a function of time
//! since the play start, equal live and in an export from the same beat.
//!
//! - [`PlayReading`]: the clock at one frame, `(epoch, play)`.
//! - [`PlayClock`]: the clock as a block carries it ([`Env::play`]), from
//!   which [`Env::play_at`] answers at any frame of the block, through every
//!   play start inside it.
//! - [`Cx::play_at`](crate::Cx::play_at): the clock at a node's **input**
//!   time, through the executor's [`TransportHistory`](crate::TransportHistory)
//!   before the block's first frame.
//!
//! **The rules.** The executor keeps it: it starts at 0 in every executor
//! (a fork's included); it resets to 0 on the frame of every stopped→playing
//! transition (between blocks, or a start inside a block's
//! [changes](Env::changes)) and takes a new **epoch** there; nothing else
//! resets it. Epochs come from one process-wide counter, so no fork's
//! epoch equals a live one's, and a reading taken from one executor never
//! names a play start of another.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_types::{Frame, PlayReading, Samples};

use crate::node::Env;
use crate::time::Offset;

/// The epoch mask: epochs live below [`PlayReading::EPOCHS`].
const EPOCH_MASK: u32 = PlayReading::EPOCHS - 1;

/// The process-wide epoch counter. Starts at 1, so the [`PlayClock::ZERO`]
/// of a hand-built [`Env`] (epoch 0) is no executor's until it wraps.
static EPOCHS: AtomicU32 = AtomicU32::new(1);

/// `n` fresh epochs, consecutive modulo [`PlayReading::EPOCHS`]: the first.
/// Wait-free; called on the audio thread at a play start.
pub(crate) fn fresh_epochs(n: u32) -> u32 {
    EPOCHS.fetch_add(n, Ordering::Relaxed) & EPOCH_MASK
}

/// The play clock as a block carries it ([`Env::play`]): the reading at the
/// block's first frame, and the epoch the block's first play start inside
/// it takes (each later start one more), reserved by the executor.
///
/// Read it through [`Env::play_at`], which applies the starts in
/// [`Env::changes`]; the fields are what the executor hands every node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlayClock {
    /// The clock at the block's first frame.
    pub start: PlayReading,
    /// The epoch of the first play start *inside* the block (a change from
    /// stopped to playing); the `k`-th takes `next_epoch + k`, modulo
    /// [`PlayReading::EPOCHS`]. Unused in a block with none.
    pub next_epoch: u32,
}

impl PlayClock {
    /// Epoch 0, play 0: for an [`Env`] built by hand (a test, a node driven
    /// outside a graph). Executors take their epochs from the process-wide
    /// counter, which starts at 1.
    pub const ZERO: Self = Self {
        start: PlayReading {
            epoch: 0,
            play: Frame::ZERO,
        },
        next_epoch: 0,
    };
}

impl Env {
    /// The play clock at frame `offset` of this block: [`play`](Self::play)'s
    /// reading advanced one per frame, restarted at 0 in a new epoch on the
    /// frame of each stopped→playing change in [`changes`](Self::changes)
    /// at or before `offset`. A start at the block's first frame is already
    /// in `play`; the executor put it there. Allocation-free.
    pub fn play_at(&self, offset: Offset) -> PlayReading {
        let mut at = self.play.start;
        let mut from = 0usize;
        let mut playing = self.transport.playing;
        let mut k = 0u32;
        for c in self.changes.as_slice() {
            if c.at > offset {
                break;
            }
            if !playing && c.to.playing {
                at = PlayReading {
                    epoch: self.play.next_epoch.wrapping_add(k) & EPOCH_MASK,
                    play: Frame::ZERO,
                };
                from = c.at.index();
                k += 1;
            }
            playing = c.to.playing;
        }
        at.after(Samples(offset.index() - from))
    }

    /// How many play starts (stopped→playing changes) lie inside the block,
    /// after its first frame: the epochs the executor reserves for it.
    pub(crate) fn play_starts(&self) -> u32 {
        let mut playing = self.transport.playing;
        let mut n = 0;
        for c in self.changes.as_slice() {
            n += u32::from(!playing && c.to.playing);
            playing = c.to.playing;
        }
        n
    }

    /// Whether the transport is playing on the block's last frame.
    pub(crate) fn playing_at_end(&self) -> bool {
        self.changes
            .as_slice()
            .last()
            .map_or(self.transport.playing, |c| c.to.playing)
    }
}

/// What an executor keeps of the clock between blocks, and publishes for
/// the control side.
#[derive(Debug)]
pub(crate) struct PlayState {
    /// The reading at the next block's first frame (before a start there).
    next: PlayReading,
    /// Whether the transport was playing on the last block's last frame.
    playing: bool,
    /// The last block's end reading, packed ([`PlayReading::pack`]), for
    /// [`Editor::play_reading`](crate::Editor::play_reading).
    published: Arc<AtomicU64>,
}

impl PlayState {
    /// A fresh clock: play 0, in a fresh epoch, stopped.
    pub(crate) fn new(published: Arc<AtomicU64>) -> Self {
        let next = PlayReading {
            epoch: fresh_epochs(1),
            play: Frame::ZERO,
        };
        published.store(next.pack(), Ordering::Relaxed);
        Self {
            next,
            playing: false,
            published,
        }
    }

    /// The clock for a block starting with `playing`, with `starts` play
    /// starts inside it: a start at its first frame (stopped before, playing
    /// now) restarts the clock in a fresh epoch here. Wait-free.
    pub(crate) fn begin(&self, playing: bool, starts: u32) -> PlayClock {
        let start = if playing && !self.playing {
            PlayReading {
                epoch: fresh_epochs(1),
                play: Frame::ZERO,
            }
        } else {
            self.next
        };
        PlayClock {
            start,
            next_epoch: if starts > 0 { fresh_epochs(starts) } else { 0 },
        }
    }

    /// Records the block `env` (whose clock [`begin`](Self::begin) made):
    /// the next block starts where it ends. Publishes that reading. A block
    /// of no frames advances nothing: the next block starts at its reading.
    pub(crate) fn end(&mut self, env: &Env) {
        let len = env.block_len.get();
        self.next = match len.checked_sub(1) {
            Some(last) => env.play_at(Offset::raw(last as u32)).after(Samples(1)),
            None => env.play.start,
        };
        self.playing = env.playing_at_end();
        self.published.store(self.next.pack(), Ordering::Release);
    }

    /// The reading at the next block's first frame.
    pub(crate) fn reading(&self) -> PlayReading {
        self.next
    }

    /// Rescales the count to the same wall-clock time at a new rate (a
    /// re-prepare's rate change, as the executor's frame is).
    pub(crate) fn rescale(&mut self, ratio: f64) {
        self.next.play = crate::command::rescale(self.next.play, ratio);
    }
}

/// The control side's handle on an executor's play clock.
#[derive(Clone, Debug)]
pub(crate) struct PlayReader(pub(crate) Arc<AtomicU64>);

impl PlayReader {
    pub(crate) fn read(&self) -> PlayReading {
        PlayReading::unpack(self.0.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Transport, TransportChanges};
    use tutti_types::{Beat, Bpm, SampleRate};

    fn t(playing: bool) -> Transport {
        Transport::new(playing, Bpm(120.0), Beat(0.0), None)
    }

    fn env(first: bool, changes: &[(usize, bool)], play: PlayClock) -> Env {
        let mut ch = TransportChanges::NONE;
        for &(at, p) in changes {
            ch.push(Offset::raw(at as u32), t(p)).expect("fits");
        }
        Env {
            frame: Frame(1_000),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(64),
            transport: t(first),
            changes: ch,
            play,
        }
    }

    /// A reading round-trips through its word.
    ///
    /// Mutation (run): `unpack` shifting by `EPOCH_BITS` instead of
    /// `64 - EPOCH_BITS` → the epoch reads back wrong → fails.
    #[test]
    fn a_reading_packs_into_one_word() {
        let r = PlayReading {
            epoch: PlayReading::EPOCHS - 1,
            play: Frame(123_456_789_012),
        };
        assert_eq!(PlayReading::unpack(r.pack()), r);
        let z = PlayReading::default();
        assert_eq!(PlayReading::unpack(z.pack()), z);
    }

    /// Inside a block the clock runs one per frame, and a start inside it
    /// restarts it at 0 on the start's frame, in the next reserved epoch; a
    /// stop or a seek while playing restarts nothing.
    ///
    /// Mutation (run): restart on every change to playing (drop the
    /// `!playing &&`) → the seek at 40 restarts the clock → fails.
    /// Mutation (run): restart one frame late (`from = c.at.index() + 1`)
    /// → frame 20 reads 0 at 21 → fails.
    #[test]
    fn a_start_inside_a_block_restarts_the_clock_on_its_frame() {
        let clock = PlayClock {
            start: PlayReading {
                epoch: 7,
                play: Frame(500),
            },
            next_epoch: 30,
        };
        // Stopped, starts at 20, a seek (playing → playing) at 40, stops at
        // 50, starts again at 60.
        let e = env(
            false,
            &[(20, true), (40, true), (50, false), (60, true)],
            clock,
        );
        let at = |i: usize| e.play_at(Offset::raw(i as u32));
        assert_eq!(
            at(0),
            PlayReading {
                epoch: 7,
                play: Frame(500)
            }
        );
        assert_eq!(
            at(19),
            PlayReading {
                epoch: 7,
                play: Frame(519)
            }
        );
        assert_eq!(
            at(20),
            PlayReading {
                epoch: 30,
                play: Frame(0)
            }
        );
        assert_eq!(
            at(45),
            PlayReading {
                epoch: 30,
                play: Frame(25)
            }
        );
        assert_eq!(
            at(55),
            PlayReading {
                epoch: 30,
                play: Frame(35)
            }
        );
        assert_eq!(
            at(60),
            PlayReading {
                epoch: 31,
                play: Frame(0)
            }
        );
        assert_eq!(
            at(63),
            PlayReading {
                epoch: 31,
                play: Frame(3)
            }
        );
        assert_eq!(e.play_starts(), 2);
        assert!(e.playing_at_end());
    }

    /// The executor's state across blocks: the clock continues from block
    /// to block, a start between blocks restarts it at the next block's
    /// first frame in a fresh epoch, and the published reading is the next
    /// block's start.
    ///
    /// Mutation (run): `begin` ignoring `self.playing` (restart whenever the
    /// block plays) → the second playing block restarts → fails.
    #[test]
    fn a_start_between_blocks_restarts_the_next_block() {
        let published = Arc::new(AtomicU64::new(0));
        let mut s = PlayState::new(Arc::clone(&published));
        let reader = PlayReader(Arc::clone(&published));
        let e1 = env(false, &[], s.begin(false, 0));
        assert_eq!(e1.play.start.play, Frame(0));
        s.end(&e1);
        assert_eq!(reader.read().play, Frame(64));
        let e2 = env(true, &[], s.begin(true, 0));
        assert_eq!(e2.play.start.play, Frame(0), "a start restarts it");
        assert_ne!(e2.play.start.epoch, e1.play.start.epoch, "in a new epoch");
        s.end(&e2);
        let e3 = env(true, &[], s.begin(true, 0));
        assert_eq!(e3.play.start, e2.play.start.after(Samples(64)));
        s.end(&e3);
        assert_eq!(reader.read(), e3.play.start.after(Samples(64)));
    }

    /// A block of no frames advances the clock by nothing.
    ///
    /// Mutation (run): `end` reading the last frame as
    /// `len.saturating_sub(1)` and adding one (as it first did) → the empty
    /// block advances the clock by 1 → fails.
    #[test]
    fn an_empty_block_advances_nothing() {
        let published = Arc::new(AtomicU64::new(0));
        let mut s = PlayState::new(Arc::clone(&published));
        let e1 = env(false, &[], s.begin(false, 0));
        s.end(&e1);
        let before = s.reading();
        for k in 0..2 {
            let mut empty = env(false, &[], s.begin(false, 0));
            empty.block_len = Samples(0);
            s.end(&empty);
            assert_eq!(s.reading(), before, "empty block {k}");
            assert_eq!(PlayReader(Arc::clone(&published)).read(), before);
        }
    }
}

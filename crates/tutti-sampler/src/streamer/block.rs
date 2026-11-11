//! One block's read of a stream's ring: [`Block`], lent by a [`StreamReader`](crate::StreamReader).
//!
//! The audio thread reads a ring under two rules: it claims the ranges it may
//! read before it reads them (the position ring's protocol), and it lends the
//! stream's published control once per block and never parks the loan. A
//! `Block` is both at once, so a block that forgot to lend, or a loan that
//! outlived its block, cannot be written (doc 025 §2.3).

use std::marker::PhantomData;
use std::ops::Range;

use tutti_rt::{Loan, PosClaim, PosFrame, PosReader, Reader};
use tutti_types::Samples;

use super::loops::RingControl;
use crate::{Direction, LoopSetting};

/// The audio side's two handles on a ring: its position reader and the
/// reader of its published control, registered together on the control side.
/// A voice holds one, by value, and reads a block through it.
#[derive(Debug)]
pub(crate) struct RingReader {
    pos: PosReader,
    /// Registered on the control side (registering allocates).
    control: Reader<RingControl>,
}

impl RingReader {
    pub(crate) fn new(pos: PosReader, control: Reader<RingControl>) -> Self {
        Self { pos, control }
    }

    /// Claim `reads` (up to two ranges, `0..0` for an unused one) and lend
    /// the control, for one block that plays at `play`.
    #[inline]
    pub(crate) fn block(&mut self, play: Samples, reads: [Range<Samples>; 2]) -> Block<'_> {
        let [a, b] = reads;
        let claim = self.pos.claim(
            play.get() as u64,
            [
                (a.start.get() as u64, a.end.get() as u64),
                (b.start.get() as u64, b.end.get() as u64),
            ],
        );
        Block {
            claim,
            control: self.control.lend(),
            _not_send: PhantomData,
        }
    }

    /// A block that reads nothing: the reader's ranges are cleared so a
    /// paused voice holds no refill back.
    #[inline]
    pub(crate) fn idle(&mut self) {
        self.pos.idle();
    }
}

/// One block of a stream's ring: the slot claim and the lent control, which
/// both end with this value.
///
/// Made by [`StreamReader::block`](crate::StreamReader::block), which it
/// borrows mutably: the next block cannot start while this one is alive, so
/// a block's frames are never read after the next claim. It is not `Send`:
/// the claim and the loan belong to the thread that took them. It must be
/// used: a dropped block read nothing.
///
/// ```compile_fail
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn two(reader: &mut StreamReader) {
/// let first = reader.block(Samples::new(0), [none(), none()]);
/// let second = reader.block(Samples::new(0), [none(), none()]); // the first is alive
/// first.holds(0);
/// # }
/// ```
///
/// Its compiling twin ends the first before the second begins:
///
/// ```
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn two(reader: &mut StreamReader) {
/// {
///     let first = reader.block(Samples::new(0), [none(), none()]);
///     first.holds(0);
/// } // the first block ends (its loan is a destructor)
/// let second = reader.block(Samples::new(0), [none(), none()]);
/// second.holds(0);
/// # }
/// ```
///
/// A block cannot be sent to another thread:
///
/// ```compile_fail
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn away(reader: &mut StreamReader) {
/// let block = reader.block(Samples::new(0), [none(), none()]);
/// std::thread::scope(|s| {
///     s.spawn(move || block.holds(0));
/// });
/// # }
/// ```
///
/// Its compiling twin reads on the thread that made it:
///
/// ```
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn here(reader: &mut StreamReader) {
/// let block = reader.block(Samples::new(0), [none(), none()]);
/// std::thread::scope(|s| {
///     s.spawn(|| ());
/// });
/// block.holds(0);
/// # }
/// ```
/// A block that is dropped unread is a mistake, and the compiler says so:
///
/// ```compile_fail
/// # #![deny(unused_must_use)]
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn dropped(reader: &mut StreamReader) {
/// reader.block(Samples::new(0), [none(), none()]);
/// # }
/// ```
///
/// Its compiling twin keeps it and reads it:
///
/// ```
/// # #![deny(unused_must_use)]
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn kept(reader: &mut StreamReader) {
/// let block = reader.block(Samples::new(0), [none(), none()]);
/// block.holds(0);
/// # }
/// ```
///
/// What the block lent is the block's: a reading of it cannot outlive it.
///
/// ```compile_fail
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn parked(reader: &mut StreamReader) {
/// let loop_ = {
///     let block = reader.block(Samples::new(0), [none(), none()]);
///     block.applied_loop() // borrows the block's loan
/// };
/// drop(loop_);
/// # }
/// ```
///
/// Its compiling twin copies what it needs before the block ends:
///
/// ```
/// # use tutti_sampler::StreamReader;
/// # use tutti_types::Samples;
/// # fn none() -> std::ops::Range<Samples> { Samples::new(0)..Samples::new(0) }
/// # fn copied(reader: &mut StreamReader) {
/// let loop_ = {
///     let block = reader.block(Samples::new(0), [none(), none()]);
///     *block.applied_loop()
/// };
/// drop(loop_);
/// # }
/// ```
#[derive(Debug)]
#[must_use = "a block that is dropped unread claimed and lent for nothing"]
pub struct Block<'r> {
    claim: PosClaim<'r>,
    control: Loan<'r, RingControl>,
    /// The claim and the loan stay on the audio thread (`Loan` is `!Send`
    /// already; this says it where the rule is).
    _not_send: PhantomData<*const ()>,
}

impl Block<'_> {
    /// Whether position `pos` may be read this block: the ring's window holds
    /// it.
    #[inline]
    pub fn holds(&self, pos: u64) -> bool {
        self.claim.holds(pos)
    }

    /// The frame at position `pos`, found once for all its channels. The
    /// caller keeps to the ranges it claimed.
    #[inline]
    pub fn frame(&self, pos: u64) -> PosFrame<'_> {
        self.claim.frame(pos)
    }

    /// The direction the stream plays in, as published.
    #[inline]
    pub fn direction(&self) -> Direction {
        self.control.direction
    }

    /// The loop the stream runs, as the streamer applied it.
    #[inline]
    pub fn applied_loop(&self) -> &LoopSetting {
        &self.control.loop_
    }

    /// What the streamer published about the ring: the map, the direction and
    /// the applied loop.
    #[inline]
    pub(crate) fn control(&self) -> &RingControl {
        &self.control
    }
}

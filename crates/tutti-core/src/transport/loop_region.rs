//! The loop region: owned by the engine, read everywhere else as a snapshot.
//!
//! The engine is the only writer. A control-side edit is a
//! [`TransportCommand::Loop`](super::TransportCommand::Loop) scheduled at an
//! [`At`](tutti_types::At), so the region changes whole, on a frame the
//! engine picks, and the block's loop is never half of two edits. The engine
//! keeps the region as plain fields; after each edit it publishes a copy
//! through a [`SeqWriter`], which any thread reads as one write's value.
//!
//! A brace being dragged is the caller's state. A [`LoopRange`] is valid by
//! construction, so the engine never holds an inverted pair.

use tutti_rt::{SeqReader, SeqWriter};
use tutti_types::{Beat, LoopRange};

/// The loop region and whether looping is armed.
///
/// The bounds are kept while looping is disarmed, so arming again restores
/// the same region. Read the engine's current one with
/// [`MotionFsm::loop_region`](super::MotionFsm::loop_region).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopRegion {
    bounds: LoopRange,
    armed: bool,
}

impl LoopRegion {
    /// A region over `bounds`, armed or not.
    #[must_use]
    pub const fn new(bounds: LoopRange, armed: bool) -> Self {
        Self { bounds, armed }
    }

    /// The bounds, kept whether or not looping is armed.
    #[must_use]
    pub const fn bounds(&self) -> LoopRange {
        self.bounds
    }

    /// Whether looping is armed.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// The region playback wraps at: the bounds while armed, else `None`.
    #[must_use]
    pub const fn active(&self) -> Option<LoopRange> {
        if self.armed { Some(self.bounds) } else { None }
    }

    /// This region after `edit`: the fields it names replaced, the rest
    /// kept.
    #[must_use]
    pub const fn with(self, edit: LoopEdit) -> Self {
        let bounds = match edit.bounds {
            Some(bounds) => bounds,
            None => self.bounds,
        };
        let armed = match edit.armed {
            Some(armed) => armed,
            None => self.armed,
        };
        Self { bounds, armed }
    }
}

impl Default for LoopRegion {
    /// Beats 0 to 16, disarmed.
    fn default() -> Self {
        let bounds = LoopRange::new(0.0, 16.0).expect("0 < 16");
        Self::new(bounds, false)
    }
}

/// One change to the loop region, the payload of
/// [`TransportCommand::Loop`](super::TransportCommand::Loop).
///
/// Each field is either a new value or "leave it": moving the bounds does
/// not arm looping, and disarming does not forget the bounds. One edit
/// carries both, so setting the bounds and arming is one command that lands
/// whole.
///
/// # Send on release, not per drag step
///
/// An edit holds one of the schedule's [`SCHEDULE_CAPACITY`](super::SCHEDULE_CAPACITY)
/// credits from [`schedule`](super::MotionFsm::schedule) until the engine
/// applies it, and the credits are shared with play, stop and seek. A UI
/// dragging a brace should send the edit when the drag ends (or throttle it
/// to a few a second), not one per frame: with the engine down or restarting
/// the credits run out and a later `Play` is refused with
/// [`ScheduleFull`](super::ScheduleFull). Edits are not coalesced: a pending
/// command lives in a lock-free ring that the control side can only push to,
/// and merging into it would need a second shared structure beside the
/// ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopEdit {
    bounds: Option<LoopRange>,
    armed: Option<bool>,
}

impl LoopEdit {
    /// Move the bounds to `bounds`. Whether looping is armed is unchanged.
    #[must_use]
    pub const fn bounds(bounds: LoopRange) -> Self {
        Self {
            bounds: Some(bounds),
            armed: None,
        }
    }

    /// Arm or disarm looping. The bounds are unchanged, so arming again
    /// restores the region.
    #[must_use]
    pub const fn armed(armed: bool) -> Self {
        Self {
            bounds: None,
            armed: Some(armed),
        }
    }

    /// Loop over `bounds`: move the bounds and arm, in one edit.
    #[must_use]
    pub const fn over(bounds: LoopRange) -> Self {
        Self {
            bounds: Some(bounds),
            armed: Some(true),
        }
    }
}

/// The region as the sequence cell stores it: start bits, end bits, armed.
type Wire = [u64; 3];

fn encode(region: LoopRegion) -> Wire {
    [
        region.bounds.start().get().to_bits(),
        region.bounds.end().get().to_bits(),
        u64::from(region.armed),
    ]
}

/// `None` for bounds `LoopRange` refuses, which only a cell nobody wrote to
/// (all zero) holds.
fn decode([start, end, armed]: Wire) -> Option<LoopRegion> {
    let bounds = LoopRange::new(Beat(f64::from_bits(start)), Beat(f64::from_bits(end)))?;
    Some(LoopRegion::new(bounds, armed != 0))
}

/// The engine's end of the region: the plain copy its blocks read, and the
/// cell it publishes each edit to. One per transport, handed to whichever
/// engine runs over it ([`MotionFsm::audio`](super::MotionFsm::audio)).
pub(crate) struct LoopOwner {
    region: LoopRegion,
    out: SeqWriter<Wire>,
}

impl LoopOwner {
    /// The default region, already published, and a read side of its cell.
    pub(crate) fn new() -> (Self, LoopReader) {
        let mut out = SeqWriter::new();
        let region = LoopRegion::default();
        out.write(encode(region));
        let reader = LoopReader(out.reader());
        (Self { region, out }, reader)
    }

    /// The region as the engine holds it.
    pub(crate) fn region(&self) -> LoopRegion {
        self.region
    }

    /// Applies `edit` and publishes the result. Wait-free, no allocation.
    pub(crate) fn apply(&mut self, edit: LoopEdit) {
        self.region = self.region.with(edit);
        self.out.write(encode(self.region));
    }
}

/// A read side of the published region, for any thread.
#[derive(Clone, Debug)]
pub(crate) struct LoopReader(SeqReader<Wire>);

impl LoopReader {
    /// The region the engine last published: one write's value, never a
    /// start of one and an end of another. Spins with `yield_now` while a
    /// write is in flight (a few stores long), so it is for control and UI
    /// threads, not real-time ones.
    pub(crate) fn read(&self) -> LoopRegion {
        loop {
            match self.0.read(READ_RETRIES) {
                Ok(published) => return published.and_then(decode).unwrap_or_default(),
                Err(_) => std::thread::yield_now(),
            }
        }
    }
}

/// Copies a read attempts before it yields the thread.
const READ_RETRIES: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    /// A region survives the wire, and a cell nobody wrote to (all zero:
    /// start equal to end) decodes to nothing rather than an invalid range.
    ///
    /// Mutation (run): `encode` storing the end bits in the start word → the
    /// round trip fails.
    #[test]
    fn the_wire_round_trips_a_region() {
        let region = LoopRegion::new(LoopRange::new(1.5, 9.25).expect("ordered"), true);
        assert_eq!(decode(encode(region)), Some(region));
        assert_eq!(decode([0; 3]), None);
    }
}

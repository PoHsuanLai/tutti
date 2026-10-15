//! Fixed-capacity point storage: [`PointBuf`], and the shared pool of
//! larger buffers a set of them borrows from ([`PointPool`]).
//!
//! The sizes are runtime ones (a plugin's parameter count, a chunk's room),
//! so neither a const-generic array nor an `ArrayVec<_, N>` can hold them;
//! a buffer sized once, off the audio thread, and never grown can. Nothing
//! here allocates after construction except [`Clone`].

use crate::ParameterPoint;

/// A fixed-capacity buffer of [`ParameterPoint`]s: its storage is sized when
/// it is built, off the audio thread, and nothing on it can grow it.
///
/// It reads as the slice of the points it holds (`len`, `iter`, indexing,
/// `sort_by_key` all go through [`Deref`](core::ops::Deref)). A point is
/// added only within the capacity: [`try_push`](Self::try_push) and
/// [`try_push_front`](Self::try_push_front) hand the point back when it is
/// full, and [`push_or_replace_last`](Self::push_or_replace_last) is **the
/// one policy** for a full buffer, which every producer that cannot drop a
/// point uses: the buffer keeps its latest value.
///
/// On the wire it is the sequence of its points (a length, then the points),
/// byte for byte what a `Vec` of them is; the spare capacity is not on it.
#[derive(Debug, Clone, Default)]
pub struct PointBuf {
    /// The storage, every slot pre-filled; `..len` are the points.
    slots: Box<[ParameterPoint]>,
    len: usize,
}

impl PointBuf {
    /// An empty buffer with room for exactly `capacity` points. Allocates;
    /// call it off the audio thread.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: vec![ParameterPoint::default(); capacity].into_boxed_slice(),
            len: 0,
        }
    }

    /// A full buffer of exactly `points`: its capacity is their count. For
    /// a builder sized by its data off the audio thread (the wire's decode,
    /// a test); may reallocate to drop `points`' spare capacity.
    pub fn from_points(points: Vec<ParameterPoint>) -> Self {
        let len = points.len();
        Self {
            slots: points.into_boxed_slice(),
            len,
        }
    }

    /// The points this buffer can hold.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Whether one more point fits.
    pub fn has_room(&self) -> bool {
        self.len < self.slots.len()
    }

    /// Appends `point`, or hands it back when the buffer is full.
    pub fn try_push(&mut self, point: ParameterPoint) -> Result<(), ParameterPoint> {
        match self.slots.get_mut(self.len) {
            Some(slot) => {
                *slot = point;
                self.len += 1;
                Ok(())
            }
            None => Err(point),
        }
    }

    /// Puts `point` ahead of every point, or hands it back when the buffer
    /// is full.
    pub fn try_push_front(&mut self, point: ParameterPoint) -> Result<(), ParameterPoint> {
        if !self.has_room() {
            return Err(point);
        }
        self.slots.copy_within(..self.len, 1);
        self.slots[0] = point;
        self.len += 1;
        Ok(())
    }

    /// Drops every point, keeping the capacity.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Appends `point`, and when the buffer is full applies **the policy for
    /// a full queue**: keep the latest value. `true` when `point` was
    /// appended; `false` when it was not (it replaced the latest point, or
    /// was dropped), for the caller to count. See
    /// [`replace_latest`](Self::replace_latest).
    pub fn push_or_replace_last(&mut self, point: ParameterPoint) -> bool {
        match self.try_push(point) {
            Ok(()) => true,
            Err(point) => {
                self.replace_latest(point);
                false
            }
        }
    }

    /// The full-buffer half of
    /// [`push_or_replace_last`](Self::push_or_replace_last), for a producer
    /// whose room ends before the buffer's capacity: `point` takes the place
    /// of the **latest** point in time (the one with the largest
    /// `sample_offset`, the last of them when several share it) when it is
    /// no earlier than that one, so the buffer ends on its latest value
    /// whatever order its points were added in; an earlier `point` is
    /// dropped, the buffer already holding a later value. Returns the index
    /// `point` was stored at, or `None` when it was dropped (always, in an
    /// empty buffer).
    pub fn replace_latest(&mut self, point: ParameterPoint) -> Option<usize> {
        // `max_by_key` returns the last of equal maxima.
        let (i, latest) = self
            .iter()
            .enumerate()
            .max_by_key(|(_, p)| p.sample_offset)?;
        if point.sample_offset < latest.sample_offset {
            return None;
        }
        self.slots[i] = point;
        Some(i)
    }
}

impl core::ops::Deref for PointBuf {
    type Target = [ParameterPoint];

    fn deref(&self) -> &[ParameterPoint] {
        &self.slots[..self.len]
    }
}

impl core::ops::DerefMut for PointBuf {
    fn deref_mut(&mut self) -> &mut [ParameterPoint] {
        &mut self.slots[..self.len]
    }
}

impl<'a> IntoIterator for &'a PointBuf {
    type Item = &'a ParameterPoint;
    type IntoIter = core::slice::Iter<'a, ParameterPoint>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// The points as a sequence, exactly as a `Vec<ParameterPoint>` encodes
/// them: the capacity past them is not on the wire.
#[cfg(feature = "serde")]
impl serde::Serialize for PointBuf {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter())
    }
}

/// **The one place point storage is sized from data**: the wire's decode
/// allocates exactly the decoded length, as the per-block decode of a `Vec`
/// did (the plugin-server's receive path, off the plugin's process call).
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for PointBuf {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::<ParameterPoint>::deserialize(d).map(Self::from_points)
    }
}

/// A pool of point buffers of one room, lent to a fixed number of holders
/// of smaller ones (by index) and given back all at once: how a set of
/// parameter queues stores its points as the points a chunk carries rather
/// than the parameter count
/// ([`ParameterChanges::reserved`](crate::ParameterChanges::reserved)).
///
/// [`lend`](Self::lend) swaps a block into a holder's buffer, the holder's
/// points moved into it, and keeps the holder's own buffer;
/// [`give_back`](Self::give_back) swaps every one back. Neither allocates or
/// frees: the record of loans was reserved for every block.
#[derive(Debug, Clone, Default)]
pub struct PointPool {
    /// Empty blocks of `block_room` points each.
    free: Vec<PointBuf>,
    /// Each holder of a block, by index, with the buffer it gets back.
    lent: Vec<(usize, PointBuf)>,
    /// Whether each holder, by index, holds a block.
    holds: Vec<bool>,
    /// The points a block holds.
    block_room: usize,
}

impl PointPool {
    /// A pool of `blocks` blocks of `room` points each, for holders
    /// `0..holders`. Allocates; call it off the audio thread.
    pub fn new(blocks: usize, room: usize, holders: usize) -> Self {
        Self {
            free: (0..blocks).map(|_| PointBuf::with_capacity(room)).collect(),
            lent: Vec::with_capacity(blocks),
            holds: vec![false; holders],
            block_room: room,
        }
    }

    /// Lends holder `index` a block: `points`' contents move into it, and it
    /// takes `points`' place until [`give_back`](Self::give_back). `false`,
    /// changing nothing, when the holder already holds one (or is not one of
    /// the pool's), the pool is spent, or a block would not hold `points`.
    /// Allocation-free.
    pub fn lend(&mut self, index: usize, points: &mut PointBuf) -> bool {
        let Some(holds) = self.holds.get_mut(index) else {
            return false;
        };
        if *holds || points.len() > self.block_room {
            return false;
        }
        let Some(mut block) = self.free.pop() else {
            return false;
        };
        for p in points.iter() {
            // Within the block's room, checked above.
            let _ = block.try_push(*p);
        }
        core::mem::swap(points, &mut block);
        *holds = true;
        // One entry per block lent, reserved for them all.
        self.lent.push((index, block));
        true
    }

    /// Takes back every block lent: `swap(index, own)` puts holder `index`'s
    /// own buffer back in its place (`own` then holds the block) and returns
    /// `true`, or returns `false` for a holder that is gone, which keeps its
    /// block. Each block comes back emptied. Allocation-free.
    pub fn give_back(&mut self, mut swap: impl FnMut(usize, &mut PointBuf) -> bool) {
        for (i, mut own) in self.lent.drain(..) {
            self.holds[i] = false;
            if swap(i, &mut own) {
                own.clear();
                self.free.push(own);
            }
        }
    }

    /// The blocks not lent, for tests that follow a block by its storage.
    #[cfg(test)]
    pub(crate) fn free_blocks(&self) -> &[PointBuf] {
        &self.free
    }

    /// The loans outstanding.
    #[cfg(test)]
    pub(crate) fn lent(&self) -> usize {
        self.lent.len()
    }

    /// Every point the pool has storage for: its free blocks and the
    /// holders' own buffers it keeps while their blocks are lent.
    pub fn storage(&self) -> usize {
        let free: usize = self.free.iter().map(PointBuf::capacity).sum();
        let kept: usize = self.lent.iter().map(|(_, b)| b.capacity()).sum();
        free + kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Normalized;

    fn at(offset: i32, value: f64) -> ParameterPoint {
        ParameterPoint {
            sample_offset: offset,
            value: Normalized::new(value),
        }
    }

    fn points(buf: &PointBuf) -> Vec<(i32, f64)> {
        buf.iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect()
    }

    /// **`try_push` on a full buffer hands the point back without
    /// allocating**, and the buffer keeps its points and its storage.
    ///
    /// Mutation (run): `try_push` growing past the capacity (rebuilding the
    /// storage one larger and pushing, as a `Vec` would) → the gate aborts.
    #[test]
    fn try_push_on_a_full_buffer_returns_the_point_without_allocating() {
        let mut buf = PointBuf::with_capacity(3);
        for k in 0..3 {
            assert!(buf.try_push(at(k, 0.25)).is_ok());
        }
        let storage = buf.as_ptr();
        let refused = assert_no_alloc::assert_no_alloc(|| {
            let refused = buf.try_push(at(9, 0.75));
            let inserted = buf.try_push_front(at(0, 0.5));
            (refused, inserted)
        });
        assert_eq!(refused.0.map_err(|p| p.sample_offset), Err(9));
        assert_eq!(refused.1.map_err(|p| p.sample_offset), Err(0));
        assert_eq!(points(&buf), vec![(0, 0.25), (1, 0.25), (2, 0.25)]);
        assert_eq!(buf.capacity(), 3);
        assert_eq!(buf.as_ptr(), storage);
    }

    /// **`push_or_replace_last` keeps the latest value**: on a full buffer
    /// a later point takes the latest point's place, wherever that is in the
    /// buffer (points from two sources need not be in time order), and an
    /// earlier one is dropped; either way `false`, and nothing grows.
    ///
    /// Mutations (run), each fails: replacing the last point *added* rather
    /// than the latest in time (`len - 1`) → the out-of-order buffer loses
    /// its frame-30 point; replacing the latest even with an earlier point
    /// (no `>=` check) → the buffer ends on frame 5.
    #[test]
    fn push_or_replace_last_keeps_the_latest_value() {
        let mut buf = PointBuf::with_capacity(3);
        assert!(buf.push_or_replace_last(at(0, 0.0)));
        assert!(buf.push_or_replace_last(at(30, 0.5)));
        assert!(buf.push_or_replace_last(at(10, 0.25)));
        // Later than all: replaces frame 30 (the latest), not frame 10 (the
        // last added).
        assert!(!buf.push_or_replace_last(at(40, 1.0)));
        assert_eq!(points(&buf), vec![(0, 0.0), (40, 1.0), (10, 0.25)]);
        // Earlier than the latest: dropped.
        assert!(!buf.push_or_replace_last(at(5, 0.75)));
        assert_eq!(points(&buf), vec![(0, 0.0), (40, 1.0), (10, 0.25)]);
        // At the same frame as the latest: the later write wins.
        assert!(!buf.push_or_replace_last(at(40, 0.125)));
        assert_eq!(buf.iter().map(|p| p.sample_offset).max(), Some(40));
        assert_eq!(buf[1].value.get(), 0.125);
        assert_eq!(buf.capacity(), 3);
        // An empty buffer of no room stores nothing.
        let mut none = PointBuf::default();
        assert!(!none.push_or_replace_last(at(0, 0.5)));
        assert!(none.is_empty());
    }

    /// **`clear` keeps the capacity**, and the storage: a buffer cleared and
    /// refilled block after block is the same allocation.
    ///
    /// Mutation (run): `clear` rebuilding the storage (`*self =
    /// Self::default()`) → the capacity is 0 → fails.
    #[test]
    fn clear_keeps_the_capacity() {
        let mut buf = PointBuf::with_capacity(4);
        let storage = buf.as_ptr();
        for _ in 0..3 {
            for k in 0..4 {
                assert!(buf.try_push(at(k, 0.5)).is_ok());
            }
            buf.clear();
            assert!(buf.is_empty());
            assert_eq!(buf.capacity(), 4);
        }
        assert!(buf.try_push(at(0, 0.5)).is_ok());
        assert_eq!(buf.as_ptr(), storage);
        assert_eq!(points(&buf), vec![(0, 0.5)]);
    }

    /// **`try_push_front` puts the point ahead of every point**, shifting
    /// them, within the capacity.
    ///
    /// Mutation (run): `try_push_front` writing slot 0 without shifting
    /// (no `copy_within`) → the frame-10 point is overwritten → fails.
    #[test]
    fn try_push_front_shifts_the_points_after_it() {
        let mut buf = PointBuf::with_capacity(3);
        assert!(buf.try_push(at(10, 0.5)).is_ok());
        assert!(buf.try_push(at(20, 0.75)).is_ok());
        assert!(buf.try_push_front(at(0, 0.25)).is_ok());
        assert_eq!(points(&buf), vec![(0, 0.25), (10, 0.5), (20, 0.75)]);
        assert!(buf.try_push_front(at(0, 1.0)).is_err());
    }
}

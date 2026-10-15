//! Stable sorts by a per-item key that never allocate, for lists the audio
//! thread puts in time order: [`insertion_sort_by_key`] for short lists, and
//! [`StableOrder`] for long ones.
//!
//! Std's stable sort (`slice::sort_by_key`) borrows a scratch buffer as long
//! as the slice: on the stack up to 4 KiB of elements, and **from the heap
//! past that**. That is 73 elements of 56 bytes (a VST3 event), or 256 of 16
//! (a parameter point), so a block one event past the line allocates on the
//! audio thread. Std's unstable sort never allocates, but it is free to
//! reorder equal keys, and two events on one frame (a note-off, then the
//! note-on that reuses its key; the two points of a VST3 jump) mean their
//! order. Both sorts here keep equal keys in the order they came in.

/// Sort `items` by `key`, stably, in place: an insertion sort. Never
/// allocates.
///
/// Linear in the length plus the number of out-of-order pairs, so it is for
/// lists that are short, or long and nearly sorted (runs that are each in
/// order already). Each item is moved into place with one `rotate_right`
/// over the items it passes. For a long list that may arrive in any order,
/// use [`StableOrder`], which is `n log n` either way.
pub fn insertion_sort_by_key<T, K: Ord>(items: &mut [T], mut key: impl FnMut(&T) -> K) {
    for i in 1..items.len() {
        let k = key(&items[i]);
        // The first slot, scanning back, whose item is not after `k`: the
        // item goes just past it, behind every equal key already there.
        let mut j = i;
        while j > 0 && key(&items[j - 1]) > k {
            j -= 1;
        }
        if j < i {
            items[j..=i].rotate_right(1);
        }
    }
}

/// The scratch for a stable, allocation-free sort of a long list by a
/// `u32` key: one `(key, index)` word per item, reserved off the audio thread
/// with the list it sorts.
///
/// [`sort_by_key`](Self::sort_by_key) sorts the words with std's unstable
/// sort (which never allocates; the index makes every word distinct, so the
/// order it finds is the stable one) and then moves the items into that order
/// in place, one permutation cycle at a time. `n log n` whatever order the
/// items arrive in, and a list already in order is left as it is.
#[derive(Debug, Clone, Default)]
pub struct StableOrder {
    keys: Vec<u64>,
}

impl StableOrder {
    /// Scratch for sorting up to `items` items. Allocates; call it off the
    /// audio thread.
    pub fn with_capacity(items: usize) -> Self {
        Self {
            keys: Vec::with_capacity(items),
        }
    }

    /// Grow the scratch to sort up to `items` items. Allocates; call it off
    /// the audio thread.
    pub fn reserve(&mut self, items: usize) {
        self.keys.reserve(items.saturating_sub(self.keys.len()));
    }

    /// The most items [`sort_by_key`](Self::sort_by_key) sorts in
    /// `n log n`.
    pub fn capacity(&self) -> usize {
        self.keys.capacity()
    }

    /// Sort `items` by `key`, stably, in place. Never allocates.
    ///
    /// A list longer than the reserved [`capacity`](Self::capacity) is still
    /// sorted, by [`insertion_sort_by_key`], rather than grow the scratch on
    /// the audio thread: the caller caps its list at the room it reserved, so
    /// that is a caller bug, and it costs time, never memory.
    pub fn sort_by_key<T>(&mut self, items: &mut [T], mut key: impl FnMut(&T) -> u32) {
        if items.is_sorted_by_key(&mut key) {
            return;
        }
        if items.len() > self.keys.capacity() || u32::try_from(items.len()).is_err() {
            insertion_sort_by_key(items, key);
            return;
        }
        self.keys.clear();
        self.keys.extend(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| (u64::from(key(item)) << 32) | i as u64),
        );
        self.keys.sort_unstable();
        // `keys[i]` is now the index of the item that belongs at `i`.
        for k in &mut self.keys {
            *k &= u64::from(u32::MAX);
        }
        // Apply the permutation in place: walk each cycle once, marking each
        // position done by pointing it at itself.
        for start in 0..self.keys.len() {
            let mut at = start;
            loop {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "masked to an item index, which is below the list's length"
                )]
                let from = self.keys[at] as usize;
                self.keys[at] = at as u64;
                if from == start || from == at {
                    break;
                }
                items.swap(at, from);
                at = from;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 56 bytes, as a VST3 event is: std's stable sort keeps 73 of these on
    /// the stack and allocates for 74.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Item {
        key: u32,
        seq: u32,
        _pad: [u64; 6],
    }

    /// `n` items whose keys interleave several ascending runs (as a list
    /// staged from several sources does) and repeat, `seq` their arrival.
    fn interleaved(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| Item {
                key: ((i * 7) % 64) as u32,
                seq: i as u32,
                _pad: [0; 6],
            })
            .collect()
    }

    /// Ascending by key, and arrival order among equal keys.
    fn assert_stably_sorted(items: &[Item]) {
        for w in items.windows(2) {
            assert!(
                (w[0].key, w[0].seq) < (w[1].key, w[1].seq),
                "{:?} then {:?}",
                (w[0].key, w[0].seq),
                (w[1].key, w[1].seq)
            );
        }
    }

    /// **Both sorts are stable, and sort.** Every key repeats several
    /// times, out of order.
    ///
    /// Mutations (run): the insertion sort's `>` to `>=` (an equal key
    /// moves ahead of the one before it), and `StableOrder`'s key word
    /// without the index (`key << 32` alone, so equal words leave the
    /// unstable sort free and every index masks to 0): each fails here.
    #[test]
    fn both_sorts_keep_equal_keys_in_arrival_order() {
        let mut a = interleaved(300);
        insertion_sort_by_key(&mut a, |i| i.key);
        assert_stably_sorted(&a);

        let mut b = interleaved(300);
        StableOrder::with_capacity(300).sort_by_key(&mut b, |i| i.key);
        assert_stably_sorted(&b);
        assert_eq!(a, b);
    }

    /// **Neither sort allocates**, at a length std's stable sort allocates
    /// for (300 items of 56 bytes, past its 73 on the stack), nor past
    /// `StableOrder`'s reserved room.
    ///
    /// Mutations (run): `insertion_sort_by_key` calling
    /// `items.sort_by_key(key)` first aborts inside the gate, and so does
    /// `StableOrder::sort_by_key` without its capacity check (the scratch
    /// grows for the list past its room).
    #[test]
    fn neither_sort_allocates() {
        let mut a = interleaved(300);
        let mut b = interleaved(300);
        let mut c = interleaved(300);
        let mut order = StableOrder::with_capacity(300);
        let mut small = StableOrder::with_capacity(16);
        assert_no_alloc::assert_no_alloc(|| {
            insertion_sort_by_key(&mut a, |i| i.key);
            order.sort_by_key(&mut b, |i| i.key);
            small.sort_by_key(&mut c, |i| i.key);
        });
        assert_stably_sorted(&a);
        assert_stably_sorted(&b);
        assert_stably_sorted(&c);
        assert_eq!(small.capacity(), 16, "the scratch did not grow");
    }
}

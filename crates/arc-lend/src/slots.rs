//! Every reader's slot, and what the writer remembers of each between
//! scans. A slot is one word on a cache line of its own. Nothing here
//! depends on the published type.

use std::ptr::NonNull;
use std::sync::Arc;

use crate::sync::{AtomicBool, AtomicUsize, Ordering};

/// Slot word meaning no loan. Every other value is either a *claim* (odd: the
/// reader's generation, shifted left, with the low bit set) or an
/// *announcement* (even and nonzero: the address of the value the reader
/// holds). An `Arc`'s data pointer is never 0 and always even: it points past
/// the two `usize` counts at the start of a heap block that is itself aligned
/// to at least a `usize`, so it cannot collide with either.
pub(crate) const FREE: usize = 0;

/// A slot word, decoded, as a scan reads it.
#[derive(Clone, Copy)]
pub(crate) enum Word {
    /// [`FREE`]: no loan.
    Free,
    /// A reader between claim and announce. Carries the whole word, so a scan
    /// can tell this claim from a later one by the same reader.
    Claim(usize),
    /// Address of the value a reader holds.
    Announced(usize),
}

impl Word {
    /// Returns the claim word for a reader's `generation`.
    ///
    /// Odd, so never [`FREE`] or an announcement. The shift drops the
    /// generation's top bit; generations wrap anyway (`docs::design`, §5.1).
    #[inline]
    pub(crate) fn claim(generation: usize) -> usize {
        (generation << 1) | 1
    }

    /// Returns the announcement of `ptr`, which came from [`Arc::into_raw`]
    /// and so is even and nonzero.
    #[inline]
    pub(crate) fn announcement<T>(ptr: NonNull<T>) -> usize {
        let addr = ptr.as_ptr().addr();
        debug_assert!(addr & 1 == 0, "an `Arc`'s data pointer is even");
        addr
    }

    fn decode(word: usize) -> Self {
        match word {
            FREE => Self::Free,
            claim if claim & 1 == 1 => Self::Claim(claim),
            addr => Self::Announced(addr),
        }
    }
}

/// One cache line (two on x86-64 and aarch64, whose adjacent-line prefetcher
/// pulls lines in pairs; the same choice as `crossbeam-utils`' `CachePadded`).
/// Each reader's slot and the `current` pointer get one of their own, so a
/// lend writes a line no other reader reads or writes.
#[cfg_attr(any(target_arch = "x86_64", target_arch = "aarch64"), repr(align(128)))]
#[cfg_attr(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    repr(align(64))
)]
pub(crate) struct Padded<T>(pub(crate) T);

/// One registered reader's slot.
pub(crate) struct Slot {
    /// [`FREE`], a claim, or an announcement. Written only by the
    /// [`Reader`](crate::Reader) that owns the slot (and reset to `FREE` by
    /// the registration that hands the slot to a new reader, under the
    /// control lock).
    pub(crate) word: AtomicUsize,
    /// Whether a live [`Reader`](crate::Reader) owns the slot. Set by
    /// registration, under the control lock; cleared by the reader's drop,
    /// with one store.
    pub(crate) owned: AtomicBool,
    /// Counts the owning reader's lends, for its claim words. Only the owner
    /// touches it (registration hands it on through `owned`), so it is not
    /// part of the protocol: a `std` atomic even under loom, used `Relaxed`
    /// as a plain counter. It lives here, on the slot's own line, so a lend
    /// writes nothing outside it.
    pub(crate) generation: std::sync::atomic::AtomicUsize,
}

impl Slot {
    fn new() -> Padded<Self> {
        Padded(Self {
            word: AtomicUsize::new(FREE),
            owned: AtomicBool::new(false),
            generation: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

/// A claim seen by the last scan: its word (which carries the reader's
/// generation), and the oldest sequence number the claiming reader can load.
#[derive(Clone, Copy)]
struct Claim {
    word: usize,
    floor: u64,
}

/// Every reader's slot, and what the writer remembers of each between scans.
///
/// Nothing here depends on the published type: a slot holds a word, not a
/// value.
pub(crate) struct Slots {
    /// Append-only; a block's address never changes, and blocks are freed
    /// only with the cell, so a [`Reader`](crate::Reader) can keep a pointer
    /// to its slot. Each block doubles the list: block 0 holds slot 0, and
    /// block `k > 0` holds slots `2^(k-1)..2^k`. A cell with one reader pays
    /// for one slot, and one with many grows in few allocations.
    blocks: Vec<Arc<[Padded<Slot>]>>,
    /// Per slot, the claim the last scan saw there, if it saw one.
    claims: Vec<Option<Claim>>,
    /// Where the next registration starts looking for an unowned slot.
    hint: usize,
    /// Addresses the last scan found announced, sorted. Reused across scans;
    /// its capacity is kept at the slot count, so a scan never grows it.
    announced: Vec<usize>,
}

impl Slots {
    pub(crate) fn new() -> Self {
        // No capacity reserved: only the publishing side touches these, and
        // growing them there is allowed.
        Self {
            blocks: Vec::new(),
            claims: Vec::new(),
            hint: 0,
            announced: Vec::new(),
        }
    }

    /// Returns how many slots there are: the most readers ever registered at
    /// once, rounded up to a power of two.
    pub(crate) fn len(&self) -> usize {
        self.claims.len()
    }

    /// Returns slot `i`, by the block layout on [`blocks`](Self::blocks).
    fn slot(&self, i: usize) -> &Slot {
        let (block, offset) = match i.checked_ilog2() {
            None => (0, 0),
            Some(k) => (k as usize + 1, i - (1 << k)),
        };
        &self.blocks[block][offset].0
    }

    /// Hands an unowned slot to a new reader, doubling the list if every
    /// slot is owned.
    pub(crate) fn register(&mut self) -> NonNull<Slot> {
        let len = self.len();
        for i in (self.hint..len).chain(0..self.hint) {
            // `Acquire`, pairing with the previous owner's `Release` in
            // `Reader::drop`: its last store to the slot happens before the
            // new owner's first.
            if !self.slot(i).owned.load(Ordering::Acquire) {
                return self.take(i);
            }
        }
        let grow = len.max(1);
        self.blocks.push((0..grow).map(|_| Slot::new()).collect());
        self.claims.resize(len + grow, None);
        // Keep the scan's buffer able to hold one address per slot.
        self.announced.reserve(len + grow - self.announced.len());
        self.take(len)
    }

    fn take(&mut self, i: usize) -> NonNull<Slot> {
        self.hint = i + 1;
        let slot = self.slot(i);
        // Only registration sets `owned`, and only under the lock.
        slot.owned.store(true, Ordering::Relaxed);
        // A previous owner that `mem::forget`ed a loan left its announcement
        // here; the new owner starts from nothing. (Read only under the lock,
        // and by the new owner, which registration happens before.)
        slot.word.store(FREE, Ordering::Relaxed);
        NonNull::from(slot)
    }

    /// Reads every slot once, for a scan, and returns the oldest sequence
    /// number a reader caught mid-lend can load (`u64::MAX` if none is).
    ///
    /// Runs after the writer's fence. `scanned_at` is the sequence number
    /// current at the last scan. Leaves the announced addresses sorted for
    /// [`is_announced`](Self::is_announced). Neither allocates nor drops.
    pub(crate) fn read(&mut self, scanned_at: u64) -> u64 {
        self.announced.clear();
        let mut floor = u64::MAX;
        let slots = self.blocks.iter().flat_map(|b| b.iter());
        for (slot, claim) in slots.zip(self.claims.iter_mut()) {
            // `Acquire`, pairing with the reader's `Release` stores (and the
            // `Release` half of its fence), so whatever the reader did through
            // an earlier loan happens before anything this scan frees.
            match Word::decode(slot.0.word.load(Ordering::Acquire)) {
                Word::Free => *claim = None,
                Word::Claim(word) => {
                    // A reader between claim and announce. The publisher does
                    // not wait for it. If the last scan saw this very claim
                    // (same word, so same generation), the reader has been
                    // mid-lend since then and keeps the floor recorded then.
                    // Otherwise it claimed after the last scan read this
                    // slot, so it loads a value no older than the one current
                    // at that scan.
                    let since = match *claim {
                        Some(seen) if seen.word == word => seen.floor,
                        _ => scanned_at,
                    };
                    *claim = Some(Claim { word, floor: since });
                    floor = floor.min(since);
                }
                Word::Announced(addr) => {
                    *claim = None;
                    // Cannot allocate: its capacity is the slot count.
                    self.announced.push(addr);
                }
            }
        }
        self.announced.sort_unstable();
        floor
    }

    /// Returns whether the last [`read`](Self::read) found a slot announcing
    /// `addr`.
    pub(crate) fn is_announced(&self, addr: usize) -> bool {
        self.announced.binary_search(&addr).is_ok()
    }
}

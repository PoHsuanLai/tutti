//! Runtime-checked split borrows of one slice across threads: [`SplitMut`]
//! (exclusive claims on single elements) and [`SplitRw`] (shared or
//! exclusive claims on fixed-size chunks, through a per-thread [`RwView`]).
//!
//! # Why
//!
//! A parallel audio executor runs several nodes at once, each on its own
//! buffers, and every buffer lives in one arena. Which buffers are disjoint
//! is decided at run time (by a compiled plan), so `split_at_mut` cannot
//! prove it, and a raw pointer plus "the plan says so" is a soundness claim
//! resting on the plan's verifier. These types move that claim into a type:
//! every element (or chunk) has a claim word, a borrow takes the word first,
//! and a borrow that would alias one held elsewhere is **refused** — a panic
//! from the panicking methods, an `Err` from the `try_` ones — never a second
//! reference. A wrong schedule is therefore a panic, not undefined behaviour.
//!
//! # Not a lock
//!
//! Nothing here waits. A claim is one atomic read-modify-write that succeeds
//! or reports [`ClaimConflict`]; there is no queue, no parking and no retry.
//! A caller whose schedule is right never sees a conflict; the claims exist
//! so that one whose schedule is wrong cannot alias.
//!
//! # Ordering
//!
//! A claim is an `Acquire` read-modify-write and a release is a `Release`
//! one, so whatever a thread wrote through a claim happens-before whatever
//! the next claimer of that word reads. The `loom` model in
//! `tests/split_loom.rs` checks this against the shipped code, and that a
//! refused claim leaves the word as it found it once it returns. A shared
//! claim adds one to the word before it looks for the exclusive bit and
//! subtracts it again on refusal, so for that instant a concurrent exclusive
//! claim on the same word is refused too; only a schedule that already
//! conflicts can see it.
//!
//! # Cost
//!
//! One uncontended atomic per borrow and one per release. The claim words are
//! a separate table ([`ClaimTable`]), preallocated on the control thread and
//! reset per [`SplitMut::new`] / [`SplitRw::new`] (plain stores through
//! `&mut`), so building a split per block never allocates.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use sync::{AtomicU32, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU32, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU32, Ordering};
}

/// The exclusive bit of a claim word. Below it: the number of shared claims.
const WRITE: u32 = 1 << 31;

/// A borrow refused because another claim on the same element (or chunk)
/// is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimConflict {
    /// The element or chunk index.
    pub index: usize,
}

impl std::fmt::Display for ClaimConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "element {} is claimed elsewhere: two borrows would alias",
            self.index
        )
    }
}

impl std::error::Error for ClaimConflict {}

/// One claim word per element or chunk, allocated on the control thread and
/// reused by every [`SplitMut`] / [`SplitRw`] built over it.
pub struct ClaimTable {
    words: Box<[AtomicU32]>,
}

impl ClaimTable {
    /// A table for `len` elements or chunks. Allocates.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            words: (0..len).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    /// How many words it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The first `n` words, all released. `&mut` proves no claim through an
    /// earlier split is alive (every guard and view borrows its split, which
    /// borrowed this table), so clearing them discards only claims whose
    /// holders were leaked with `mem::forget`.
    fn reset(&mut self, n: usize) -> &[AtomicU32] {
        assert!(
            n <= self.words.len(),
            "a claim table of {} words for {n} elements",
            self.words.len()
        );
        let words = &self.words[..n];
        for w in words {
            w.store(0, Ordering::Relaxed);
        }
        words
    }
}

impl std::fmt::Debug for ClaimTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimTable")
            .field("len", &self.words.len())
            .finish()
    }
}

fn claim_shared(word: &AtomicU32, index: usize) -> Result<(), ClaimConflict> {
    let prev = word.fetch_add(1, Ordering::Acquire);
    if prev & WRITE != 0 {
        // Leave the word as found: the writer's release subtracts only its
        // own bit.
        word.fetch_sub(1, Ordering::Relaxed);
        return Err(ClaimConflict { index });
    }
    assert!(prev + 1 < WRITE, "shared claim count overflow");
    Ok(())
}

fn claim_exclusive(word: &AtomicU32, index: usize) -> Result<(), ClaimConflict> {
    word.compare_exchange(0, WRITE, Ordering::Acquire, Ordering::Relaxed)
        .map(drop)
        .map_err(|_| ClaimConflict { index })
}

/// A sole shared claim, made exclusive.
fn upgrade(word: &AtomicU32, index: usize) -> Result<(), ClaimConflict> {
    word.compare_exchange(1, WRITE, Ordering::Acquire, Ordering::Relaxed)
        .map(drop)
        .map_err(|_| ClaimConflict { index })
}

fn release_shared(word: &AtomicU32) {
    word.fetch_sub(1, Ordering::Release);
}

fn release_exclusive(word: &AtomicU32) {
    word.fetch_sub(WRITE, Ordering::Release);
}

/// A slice whose elements can be claimed one at a time, exclusively, from
/// any thread holding a `&SplitMut`. See the module docs
/// (`src/split.rs`).
#[derive(Debug)]
pub struct SplitMut<'a, T> {
    base: *mut T,
    len: usize,
    words: &'a [AtomicU32],
    _borrow: PhantomData<&'a mut [T]>,
}

// SAFETY: a `&SplitMut` hands out `&mut T` to one thread at a time per
// element (the claim word enforces it), which is what `Mutex<T>: Sync`
// needs — `T: Send`, not `T: Sync`. Sending the split sends the `&mut [T]`
// it was built from.
unsafe impl<T: Send> Sync for SplitMut<'_, T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for SplitMut<'_, T> {}

impl<'a, T> SplitMut<'a, T> {
    /// Split `items`, one claim word each from `table`, every word released.
    ///
    /// # Panics
    ///
    /// If `table` has fewer words than `items` has elements.
    pub fn new(items: &'a mut [T], table: &'a mut ClaimTable) -> Self {
        let words = table.reset(items.len());
        Self {
            base: items.as_mut_ptr(),
            len: items.len(),
            words,
            _borrow: PhantomData,
        }
    }

    /// Elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Element `i`, exclusively, until the guard drops.
    ///
    /// # Panics
    ///
    /// If `i` is out of range, or claimed elsewhere ([`ClaimConflict`]).
    #[inline]
    #[track_caller]
    pub fn claim(&self, i: usize) -> Claimed<'_, T> {
        self.try_claim(i).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Element `i`, exclusively, or [`ClaimConflict`] if claimed elsewhere.
    ///
    /// # Errors
    ///
    /// [`ClaimConflict`] if element `i` is claimed elsewhere.
    ///
    /// # Panics
    ///
    /// If `i` is out of range.
    #[inline]
    pub fn try_claim(&self, i: usize) -> Result<Claimed<'_, T>, ClaimConflict> {
        assert!(i < self.len, "index {i} of {}", self.len);
        claim_exclusive(&self.words[i], i)?;
        // SAFETY: `i < len`, so the pointer is inside the slice the split
        // borrows mutably for `'a`; the exclusive claim just taken is the
        // only one on element `i` until `Claimed` drops, so this is the only
        // reference to it.
        let elem = unsafe { &mut *self.base.add(i) };
        Ok(Claimed {
            elem,
            word: &self.words[i],
        })
    }

    /// Elements `range`, exclusively, until the guard drops.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, or any element in it is claimed
    /// elsewhere (the ones already taken are released first).
    #[track_caller]
    pub fn claim_run(&self, range: std::ops::Range<usize>) -> ClaimedRun<'_, T> {
        assert!(
            range.start <= range.end && range.end <= self.len,
            "run {range:?} of {}",
            self.len
        );
        for i in range.clone() {
            if let Err(e) = claim_exclusive(&self.words[i], i) {
                for w in &self.words[range.start..i] {
                    release_exclusive(w);
                }
                panic!("{e}");
            }
        }
        // SAFETY: in bounds (checked), and every element in it is exclusively
        // claimed until `ClaimedRun` drops.
        let run = unsafe {
            std::slice::from_raw_parts_mut(self.base.add(range.start), range.end - range.start)
        };
        ClaimedRun {
            run,
            words: &self.words[range],
        }
    }
}

/// One element of a [`SplitMut`], claimed exclusively.
#[must_use = "the claim is released when it drops"]
pub struct Claimed<'s, T> {
    elem: &'s mut T,
    word: &'s AtomicU32,
}

impl<T> std::fmt::Debug for Claimed<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claimed").finish_non_exhaustive()
    }
}

impl<T> Deref for Claimed<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.elem
    }
}

impl<T> DerefMut for Claimed<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.elem
    }
}

impl<T> Drop for Claimed<'_, T> {
    fn drop(&mut self) {
        release_exclusive(self.word);
    }
}

/// A contiguous run of a [`SplitMut`], every element claimed exclusively.
#[must_use = "the claims are released when it drops"]
pub struct ClaimedRun<'s, T> {
    run: &'s mut [T],
    words: &'s [AtomicU32],
}

impl<T> std::fmt::Debug for ClaimedRun<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimedRun")
            .field("len", &self.run.len())
            .finish_non_exhaustive()
    }
}

impl<T> Deref for ClaimedRun<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.run
    }
}

impl<T> DerefMut for ClaimedRun<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.run
    }
}

impl<T> Drop for ClaimedRun<'_, T> {
    fn drop(&mut self) {
        for w in self.words {
            release_exclusive(w);
        }
    }
}

/// A slice cut into chunks of `stride` elements, each of which can be read
/// by several threads at once or written by one, through an [`RwView`]. See
/// the module docs (`src/split.rs`).
#[derive(Debug)]
pub struct SplitRw<'a, T> {
    base: *mut T,
    chunks: usize,
    stride: usize,
    words: &'a [AtomicU32],
    _borrow: PhantomData<&'a mut [T]>,
}

// SAFETY: a `&SplitRw` hands out `&T` to several threads at once (so `T:
// Sync`) and `&mut T` to one at a time (so `T: Send`), per chunk, as
// `RwLock<T>: Sync` requires. The claim words enforce the exclusion.
unsafe impl<T: Send + Sync> Sync for SplitRw<'_, T> {}
// SAFETY: sending the split sends the `&mut [T]` it was built from.
unsafe impl<T: Send> Send for SplitRw<'_, T> {}

impl<'a, T> SplitRw<'a, T> {
    /// Split `items` into chunks of `stride` (a trailing partial chunk is not
    /// reachable), one claim word each from `table`, every word released.
    ///
    /// # Panics
    ///
    /// If `stride` is zero, or `table` has fewer words than there are
    /// chunks.
    pub fn new(items: &'a mut [T], stride: usize, table: &'a mut ClaimTable) -> Self {
        assert!(stride > 0, "a zero stride");
        let chunks = items.len() / stride;
        let words = table.reset(chunks);
        Self {
            base: items.as_mut_ptr(),
            chunks,
            stride,
            words,
            _borrow: PhantomData,
        }
    }

    /// Chunks.
    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// Elements per chunk.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// A view that records the claims it takes in `held` and releases them
    /// all when it drops. One thread's; see [`RwView`].
    ///
    /// # Panics
    ///
    /// If `held` is already in use by another live view.
    pub fn view<'v>(&'v self, held: &'v Held) -> RwView<'v, T> {
        // Two live views on one list would each find the other's claims and
        // hand out a second reference to a chunk the first holds.
        assert!(
            !held.in_use.replace(true),
            "a held list already in use by a view"
        );
        held.len.set(0);
        RwView { split: self, held }
    }

    /// # Safety
    ///
    /// `i < chunks`, and the caller holds a claim on chunk `i` that allows
    /// the reference it makes of the result, for as long as it lives.
    unsafe fn chunk_ptr(&self, i: usize) -> *mut T {
        debug_assert!(i < self.chunks);
        // SAFETY: `i < chunks` and `chunks * stride <= items.len()`.
        unsafe { self.base.add(i * self.stride) }
    }
}

/// Where an [`RwView`] records the claims it holds: a fixed capacity,
/// allocated on the control thread, so a view never allocates. One thread's
/// at a time (`!Sync`).
pub struct Held {
    entries: Box<[Cell<u32>]>,
    len: Cell<usize>,
    /// A view is using it.
    in_use: Cell<bool>,
}

impl Held {
    /// Room for `capacity` claims at once. Allocates.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: (0..capacity).map(|_| Cell::new(0)).collect(),
            len: Cell::new(0),
            in_use: Cell::new(false),
        }
    }

    /// How many claims a view can hold at once.
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }
}

impl std::fmt::Debug for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Held")
            .field("capacity", &self.entries.len())
            .field("len", &self.len.get())
            .finish()
    }
}

/// One thread's access to a [`SplitRw`]: the same shape of API as a plain
/// `&mut [T]` cut into chunks — read chunks through `&self`, write through
/// `&mut self`, several at once through [`split`](Self::split) — with each
/// chunk claimed the first time the view touches it and released when the
/// view drops. A chunk the view already holds is not claimed again.
///
/// `!Send` and `!Sync` (it holds `&Held`, whose cells are one thread's).
#[derive(Debug)]
#[must_use = "its claims are released when it drops"]
pub struct RwView<'v, T> {
    split: &'v SplitRw<'v, T>,
    held: &'v Held,
}

/// An entry's mode bit in [`Held`].
const HELD_WRITE: u32 = 1 << 31;

impl<'v, T> RwView<'v, T> {
    fn find(&self, i: usize) -> Option<usize> {
        let n = self.held.len.get();
        self.held.entries[..n]
            .iter()
            .position(|e| (e.get() & !HELD_WRITE) as usize == i)
    }

    /// Panic unless the view has room for one more claim. Called before a
    /// claim is taken, so a full `Held` never leaves a claim with no entry.
    fn ensure_room(&self) {
        assert!(
            self.held.len.get() < self.held.entries.len(),
            "a view holding more than its {} claims",
            self.held.entries.len()
        );
    }

    fn record(&self, entry: u32) {
        let n = self.held.len.get();
        self.ensure_room();
        self.held.entries[n].set(entry);
        self.held.len.set(n + 1);
    }

    fn check(&self, i: usize) {
        assert!(i < self.split.chunks, "chunk {i} of {}", self.split.chunks);
        assert!(i < HELD_WRITE as usize, "chunk index {i} too large");
    }

    /// Take (or find) a shared claim on chunk `i`.
    fn acquire_read(&self, i: usize) -> Result<(), ClaimConflict> {
        self.check(i);
        if self.find(i).is_none() {
            self.ensure_room();
            claim_shared(&self.split.words[i], i)?;
            self.record(i as u32);
        }
        Ok(())
    }

    /// Take (or find, or upgrade to) an exclusive claim on chunk `i`.
    fn acquire_write(&self, i: usize) -> Result<(), ClaimConflict> {
        self.check(i);
        match self.find(i) {
            Some(k) => {
                let e = self.held.entries[k].get();
                if e & HELD_WRITE == 0 {
                    upgrade(&self.split.words[i], i)?;
                    self.held.entries[k].set(e | HELD_WRITE);
                }
            }
            None => {
                self.ensure_room();
                claim_exclusive(&self.split.words[i], i)?;
                self.record(i as u32 | HELD_WRITE);
            }
        }
        Ok(())
    }

    /// Chunk `i`, shared, or [`ClaimConflict`] if another view holds it
    /// exclusively.
    ///
    /// # Errors
    ///
    /// [`ClaimConflict`] if another view holds chunk `i` exclusively.
    ///
    /// # Panics
    ///
    /// If `i` is out of range, or the view already holds as many claims as
    /// its [`Held`] has room for.
    #[inline]
    pub fn try_get(&self, i: usize) -> Result<&[T], ClaimConflict> {
        self.acquire_read(i)?;
        // SAFETY: `i < chunks` (checked), and this view holds a claim on
        // chunk `i` until it drops. Any exclusive claim it holds is its own,
        // and every `&mut` this view hands out borrows it mutably, so none is
        // alive while `&self` is borrowed here.
        Ok(unsafe { std::slice::from_raw_parts(self.split.chunk_ptr(i), self.split.stride) })
    }

    /// Chunk `i`, shared.
    ///
    /// # Panics
    ///
    /// As [`try_get`](Self::try_get), and on a [`ClaimConflict`].
    #[inline]
    #[track_caller]
    pub fn get(&self, i: usize) -> &[T] {
        self.try_get(i).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Chunk `i`, exclusively, or [`ClaimConflict`] if another view holds
    /// any claim on it.
    ///
    /// # Errors
    ///
    /// [`ClaimConflict`] if another view holds any claim on chunk `i`.
    ///
    /// # Panics
    ///
    /// As [`try_get`](Self::try_get).
    #[inline]
    pub fn try_get_mut(&mut self, i: usize) -> Result<&mut [T], ClaimConflict> {
        self.acquire_write(i)?;
        // SAFETY: `i < chunks`, and this view holds the only claim on chunk
        // `i` (an exclusive one) until it drops. `&mut self` means no other
        // reference this view handed out is alive.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.split.chunk_ptr(i), self.split.stride) })
    }

    /// Chunk `i`, exclusively.
    ///
    /// # Panics
    ///
    /// As [`try_get_mut`](Self::try_get_mut), and on a [`ClaimConflict`].
    #[inline]
    #[track_caller]
    pub fn get_mut(&mut self, i: usize) -> &mut [T] {
        self.try_get_mut(i).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Several chunks at once: each request `(chunk, tag)` is claimed as a
    /// write when `is_write(tag)`, else as a read, and then handed to
    /// `write(tag, ..)` or `read(tag, ..)`.
    ///
    /// `is_write` is called exactly once per request, in order, and that
    /// answer alone decides the check, the claim and the reference, so a
    /// closure that answers differently on each call cannot alias.
    ///
    /// `reqs` must be **sorted by chunk**. Several reads of one chunk share
    /// it; a written chunk may appear once.
    ///
    /// # Panics
    ///
    /// If `reqs` is unsorted, a written chunk appears twice or is also read
    /// (never an aliased reference), or on a [`ClaimConflict`], an
    /// out-of-range chunk or a full [`Held`]. Requests are handled in order,
    /// so `read` and `write` have already run for the requests before the one
    /// that panics.
    #[track_caller]
    pub fn split<'s, R: Copy>(
        &'s mut self,
        reqs: &[(u32, R)],
        is_write: impl Fn(R) -> bool,
        mut read: impl FnMut(R, &'s [T]),
        mut write: impl FnMut(R, &'s mut [T]),
    ) {
        let split = self.split;
        // The previous request's chunk and mode: sorted order puts every
        // request on one chunk next to each other, so adjacent pairs cover
        // them all.
        let mut prev: Option<(u32, bool)> = None;
        for &(i, tag) in reqs {
            let write_it = is_write(tag);
            if let Some((pi, pw)) = prev {
                assert!(pi <= i, "split requests are not sorted");
                assert!(
                    pi != i || !(pw || write_it),
                    "chunk {i} is written and also borrowed elsewhere in one split"
                );
            }
            prev = Some((i, write_it));
            let claimed = if write_it {
                self.acquire_write(i as usize)
            } else {
                self.acquire_read(i as usize)
            };
            if let Err(e) = claimed {
                panic!("{e}");
            }
            // SAFETY: chunk `i` is in range and this view holds a claim on it
            // in the mode `write_it` (just taken). Requests before this one
            // were checked pairwise in sorted order, and this one against its
            // predecessor, so no reference already made in this call overlaps
            // this one: a written chunk is named by no request before it, and
            // a later duplicate is refused before it makes a second
            // reference. `&'s mut self` keeps every other reference from this
            // view dead for `'s`. `write_it` is the one answer from
            // `is_write`, used for all three decisions.
            let p = unsafe { split.chunk_ptr(i as usize) };
            if write_it {
                write(tag, unsafe {
                    std::slice::from_raw_parts_mut(p, split.stride)
                });
            } else {
                read(tag, unsafe { std::slice::from_raw_parts(p, split.stride) });
            }
        }
    }

    /// Claims held right now.
    pub fn held(&self) -> usize {
        self.held.len.get()
    }
}

impl<T> Drop for RwView<'_, T> {
    fn drop(&mut self) {
        let n = self.held.len.get();
        for e in &self.held.entries[..n] {
            let e = e.get();
            let word = &self.split.words[(e & !HELD_WRITE) as usize];
            if e & HELD_WRITE != 0 {
                release_exclusive(word);
            } else {
                release_shared(word);
            }
        }
        self.held.len.set(0);
        self.held.in_use.set(false);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Exclusive claims exclude, and release on drop.
    ///
    /// Mutation: make `Claimed::drop` a no-op → the second claim of 1 after
    /// the first guard drops conflicts → fails.
    #[test]
    fn claims_exclude_and_release() {
        let mut v = [1, 2, 3];
        let mut t = ClaimTable::new(3);
        let s = SplitMut::new(&mut v, &mut t);
        let mut a = s.claim(1);
        assert_eq!(s.try_claim(1).err(), Some(ClaimConflict { index: 1 }));
        *a += 10;
        let b = s.claim(2);
        assert_eq!(*b, 3);
        drop(a);
        assert_eq!(*s.claim(1), 12);
    }

    /// A run claims every element, and a conflict inside it releases the ones
    /// already taken before panicking.
    ///
    /// Mutation: drop the release loop in `claim_run`'s conflict branch →
    /// element 0 stays claimed after the caught panic → fails.
    #[test]
    fn a_refused_run_releases_what_it_took() {
        let mut v = [0; 4];
        let mut t = ClaimTable::new(4);
        let s = SplitMut::new(&mut v, &mut t);
        let held = s.claim(2);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _run = s.claim_run(0..4);
        }));
        assert!(r.is_err());
        drop(s.claim(0));
        drop(held);
        let mut run = s.claim_run(0..4);
        run[3] = 7;
        drop(run);
        assert_eq!(*s.claim(3), 7);
    }

    /// Views share reads, exclude writes, find their own claims, and upgrade
    /// a sole read.
    ///
    /// Mutation: in `acquire_write`, claim afresh instead of finding the
    /// view's own claim → `get_mut` after `get` of chunk 0 in one view
    /// conflicts with itself → fails.
    #[test]
    fn views_share_reads_and_exclude_writes() {
        let mut v = [0u8; 8];
        let mut t = ClaimTable::new(4);
        let s = SplitRw::new(&mut v, 2, &mut t);
        let (h1, h2) = (Held::new(4), Held::new(4));
        let mut a = s.view(&h1);
        let b = s.view(&h2);
        assert_eq!(a.get(0).len(), 2);
        assert_eq!(b.get(1), &[0, 0]);
        // Both read 1: neither may write it.
        let _ = a.get(1);
        assert!(a.try_get_mut(1).is_err());
        // `a` alone reads 0: upgrade.
        a.get_mut(0)[0] = 5;
        assert!(b.try_get(0).is_err());
        drop(a);
        assert_eq!(b.get(0), &[5, 0]);
        assert_eq!(b.held(), 2);
    }

    /// One `Held` backs one live view at a time.
    ///
    /// Mutation: drop the `in_use` assert in `SplitRw::view` → the second
    /// view is built, finds the first's exclusive claim on chunk 0 as its
    /// own, and hands out a second `&mut` → the `should_panic` fails.
    #[test]
    #[should_panic(expected = "already in use")]
    fn a_held_list_backs_one_view_at_a_time() {
        let mut v = [0u8; 2];
        let mut t = ClaimTable::new(2);
        let s = SplitRw::new(&mut v, 1, &mut t);
        let h = Held::new(2);
        let mut a = s.view(&h);
        let _x = a.get_mut(0);
        let mut b = s.view(&h);
        let _y = b.get_mut(0);
    }

    /// `split` hands out disjoint chunks and refuses an aliasing request.
    ///
    /// Mutation: delete the written-twice `assert!` in `split` → the request
    /// list below gets two `&mut` to chunk 1 → the `should_panic` fails.
    #[test]
    #[should_panic(expected = "written and also borrowed")]
    fn split_refuses_a_write_that_aliases() {
        let mut v = [0u8; 4];
        let mut t = ClaimTable::new(4);
        let s = SplitRw::new(&mut v, 1, &mut t);
        let h = Held::new(4);
        let mut view = s.view(&h);
        view.split(&[(1, true), (1, false)], |w| w, |_, _| {}, |_, _| {});
    }

    /// `split` asks `is_write` once per request, so a closure whose answers
    /// change cannot make the check and the hand-out disagree: whatever it
    /// answers, no chunk is handed out as `&mut` twice, or as `&mut` and `&`.
    ///
    /// Mutation: restore the old shape (call `is_write` in a separate pair
    /// check, claim loop and hand-out loop) -> the counter answers `false`
    /// for the pair check, then `true` for the claims and the hand-out, and
    /// the view hands `write` chunk 0 twice -> `writes` is 2 and the
    /// assertion fails.
    #[test]
    fn a_stateful_is_write_never_aliases() {
        let mut v = [0u8; 2];
        let mut t = ClaimTable::new(2);
        let s = SplitRw::new(&mut v, 1, &mut t);
        let h = Held::new(2);
        let mut view = s.view(&h);
        let calls = Cell::new(0u32);
        let (mut reads, mut writes) = (0, 0);
        view.split(
            &[(0, ()), (0, ())],
            |()| {
                calls.set(calls.get() + 1);
                calls.get() > 2
            },
            |_, _| reads += 1,
            |_, _| writes += 1,
        );
        assert!(
            writes <= 1 && (writes == 0 || reads == 0),
            "{writes} writes, {reads} reads of one chunk"
        );
        assert_eq!(calls.get(), 2, "one answer per request");
    }

    /// A full `Held` refuses a claim before taking it, so the panic leaves
    /// no claim behind with no entry to release it.
    ///
    /// Mutation (run): drop the `ensure_room` call in `acquire_read` -> the
    /// read of chunk 1 claims it, then `record` panics, and chunk 1 keeps a
    /// shared count after the view drops -> the last `try_get_mut` conflicts
    /// -> fails.
    #[test]
    fn a_full_held_leaves_no_claim_behind() {
        let mut v = [0u8; 2];
        let mut t = ClaimTable::new(2);
        let s = SplitRw::new(&mut v, 1, &mut t);
        let h = Held::new(1);
        let mut view = s.view(&h);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            view.split(&[(0, ()), (1, ())], |()| false, |_, _| {}, |_, _| {});
        }));
        assert!(r.is_err(), "the second claim does not fit");
        drop(view);
        let mut view = s.view(&h);
        assert_eq!(view.try_get_mut(1).map(|c| c.len()), Ok(1));
    }

    /// `split`'s reads and writes land on the right chunks.
    ///
    /// Mutation: in `split`, hand `write` the chunk at `i + 1` → the written
    /// value lands in chunk 3 → fails.
    #[test]
    fn split_hands_each_request_its_chunk() {
        let mut v = [0u8, 1, 2, 3];
        let mut t = ClaimTable::new(4);
        let s = SplitRw::new(&mut v, 1, &mut t);
        let h = Held::new(4);
        let mut view = s.view(&h);
        let mut seen = [0u8; 2];
        view.split(
            &[(0, (0, false)), (2, (1, true)), (3, (1, false))],
            |(_, w)| w,
            |(k, _), c| seen[k] = c[0],
            |_, c| c[0] = 9,
        );
        assert_eq!(seen, [0, 3]);
        drop(view);
        assert_eq!(v, [0, 1, 9, 3]);
    }
}

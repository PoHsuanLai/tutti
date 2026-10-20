//! [`SeqWriter`] and [`SeqReader`]: a value of `W` words that one thread overwrites in place and
//! any thread reads by copy, without either side allocating, locking or
//! freeing. A sequence lock.
//!
//! Its own module because what it adds over `W` atomics is the reason it
//! exists: a read returns the words of one write, never words of two. The
//! `loom` model in `tests/seq_cell_loom.rs` runs this code and checks exactly
//! that.
//!
//! # Why not [`ArcLend`](crate::ArcLend)
//!
//! `ArcLend` hands the audio thread a value the control thread built and
//! frees the retired one on the control thread. A sequence cell is for the other
//! direction: a value the *audio thread* writes each block (a VST2 plugin's
//! `audioMasterGetTime` snapshot), where publishing a fresh allocation and
//! retiring the old one would allocate and free inside the callback.
//!
//! # The protocol
//!
//! The sequence is even when the words are stable and odd while a write is
//! copying them; it starts at zero, "never written". The words are atomics,
//! each stored and loaded `Relaxed`, so a read racing a write copies stale
//! or mixed words, and the sequence check discards them: there is no data
//! race to make undefined, and no `unsafe`.
//!
//! - **Write:** mark the sequence odd, `Release` fence, store the words,
//!   store the even sequence with `Release`. The fence keeps every word store
//!   after the odd mark for any reader that sees one of them.
//! - **Read:** load the sequence with `Acquire` (odd → retry), load the
//!   words, `Acquire` fence, load the sequence again; the copy stands only if
//!   it is unchanged. A reader that loaded a word of a later write is
//!   synchronised by the two fences with that write's odd mark, so its second
//!   load cannot return the first.
//!
//! Plain `Release`/`Acquire` fences, not `SeqCst`: the protocol has no
//! store-then-load pair on either side (the writer stores then stores, the
//! reader loads then loads), which is the pattern CLAUDE.md's `SeqCst`-fence
//! rule covers. The model's mutation record shows each fence is needed.

use std::sync::Arc;

use sync::{fence, AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{fence, AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{fence, AtomicU64, Ordering};

    #[cfg(loom)]
    pub(super) use loom::hint::spin_loop;
    #[cfg(not(loom))]
    pub(super) use std::hint::spin_loop;
}

/// How many times [`SeqReader::read`] retries a copy a write overlapped
/// before it gives up.
///
/// A write is `W` relaxed stores, so a reader that loses the race this many
/// times was descheduled rather than raced, and more retries would not help;
/// a bounded read keeps the reader wait-free.
pub const SEQ_CELL_READ_RETRIES: usize = 8;

/// The shared state: the sequence and the words.
#[derive(Debug)]
struct Cell<const W: usize> {
    /// Zero until the first write; then even when stable, odd mid-write.
    seq: AtomicU64,
    words: [AtomicU64; W],
}

/// The one writer of a sequence cell of `W` words: it overwrites them in
/// place, and every [`SeqReader`] it hands out copies them whole.
///
/// The words are bare `u64`s: the caller packs its value into them and
/// unpacks what [`SeqReader::read`] returns (`f64::to_bits`, two `i32`s to a
/// word), so the cell needs no `unsafe` to hold an arbitrary `Copy` type.
///
/// **One writer, structurally.** A `SeqWriter` is not `Clone`, and
/// [`write`](Self::write) takes `&mut self`, so two writes can never
/// overlap: a second writer would let a reader accept words of two writes
/// (the sequence going 1, 2, 3, 2 under two writers looks stable to a
/// reader). Readers are unlimited.
///
/// Wait-free on both sides; no allocation after [`new`](Self::new).
///
/// ```
/// use tutti_types::SeqWriter;
///
/// let mut writer = SeqWriter::<2>::new();
/// let reader = writer.reader();
/// assert_eq!(reader.read(), None, "never written");
/// writer.write([1, 2]);
/// assert_eq!(reader.read(), Some([1, 2]));
/// ```
///
/// A second writer cannot be made:
///
/// ```compile_fail
/// let writer = tutti_types::SeqWriter::<2>::new();
/// let second = writer.clone();
/// ```
///
/// Nor can a shared writer write:
///
/// ```compile_fail
/// fn write_shared(writer: &tutti_types::SeqWriter<2>) {
///     writer.write([1, 2]);
/// }
/// ```
// Mutations (run) for the two `compile_fail` doctests: `SeqWriter` deriving
// `Clone` → the first compiles → fails; `write(&self)` keeping the sequence in
// the cell (load, then store, as the first version did) → the second compiles
// → fails.
#[derive(Debug)]
pub struct SeqWriter<const W: usize> {
    cell: Arc<Cell<W>>,
    /// The sequence of the last completed write: the writer's own copy, so a
    /// write never loads it.
    seq: u64,
    /// What the last write stored, for [`last`](Self::last).
    last: Option<[u64; W]>,
}

impl<const W: usize> SeqWriter<W> {
    /// A cell nothing has been written to, and its writer. Allocates the
    /// shared cell once.
    pub fn new() -> Self {
        Self {
            cell: Arc::new(Cell {
                seq: AtomicU64::new(0),
                words: std::array::from_fn(|_| AtomicU64::new(0)),
            }),
            seq: 0,
            last: None,
        }
    }

    /// A read side of this cell. Clone it for more readers.
    pub fn reader(&self) -> SeqReader<W> {
        SeqReader(Arc::clone(&self.cell))
    }

    /// Overwrites the words in place. Wait-free; allocates and frees nothing.
    ///
    /// A reader on the writer's own thread, after this returns, reads
    /// `value`.
    pub fn write(&mut self, value: [u64; W]) {
        let cell = &*self.cell;
        cell.seq.store(self.seq.wrapping_add(1), Ordering::Relaxed);
        // Orders the odd mark before the word stores, for a reader that sees
        // any of them (its `Acquire` fence in `read` pairs with this).
        fence(Ordering::Release);
        for (word, v) in cell.words.iter().zip(value) {
            word.store(v, Ordering::Relaxed);
        }
        self.seq = self.seq.wrapping_add(2);
        cell.seq.store(self.seq, Ordering::Release);
        self.last = Some(value);
    }

    /// What the last [`write`](Self::write) stored, or `None` before the
    /// first: the writer's own copy, so it is never torn and never retries.
    pub fn last(&self) -> Option<[u64; W]> {
        self.last
    }
}

impl<const W: usize> Default for SeqWriter<W> {
    /// Never written.
    fn default() -> Self {
        Self::new()
    }
}

/// A read side of a sequence cell: copies the words of one completed
/// [`SeqWriter::write`], never words of two. `Clone` for more readers; any
/// thread.
#[derive(Debug, Clone)]
pub struct SeqReader<const W: usize>(Arc<Cell<W>>);

impl<const W: usize> SeqReader<W> {
    /// The words of the last completed write, or `None` when nothing has been
    /// written yet or every one of [`SEQ_CELL_READ_RETRIES`] copies
    /// overlapped a write. Wait-free.
    pub fn read(&self) -> Option<[u64; W]> {
        let cell = &*self.0;
        for _ in 0..SEQ_CELL_READ_RETRIES {
            let before = cell.seq.load(Ordering::Acquire);
            if before == 0 {
                return None;
            }
            if !before.is_multiple_of(2) {
                sync::spin_loop();
                continue;
            }
            let copy: [u64; W] = std::array::from_fn(|i| cell.words[i].load(Ordering::Relaxed));
            // Orders the word loads before the re-check: a word from a later
            // write makes that write's odd mark visible to the load below.
            fence(Ordering::Acquire);
            if cell.seq.load(Ordering::Relaxed) == before {
                return Some(copy);
            }
            sync::spin_loop();
        }
        None
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Nothing before the first write; afterwards, the last write's words.
    ///
    /// Mutation (run): `read` without the `before == 0` check → the first
    /// read returns `Some([0, 0])` → fails.
    #[test]
    fn a_read_returns_the_last_write() {
        let mut writer = SeqWriter::<3>::new();
        let cell = writer.reader();
        assert_eq!(cell.read(), None);
        assert_eq!(writer.last(), None);
        writer.write([1, 2, 3]);
        assert_eq!(cell.read(), Some([1, 2, 3]));
        writer.write([4, 5, 6]);
        assert_eq!(cell.read(), Some([4, 5, 6]));
        assert_eq!(cell.read(), Some([4, 5, 6]), "a read takes nothing");
        assert_eq!(writer.last(), Some([4, 5, 6]));
    }

    /// A concurrent reader never accepts a copy mixing two writes. Every
    /// write stores one value in all words, so a mix shows as unequal words.
    /// A stress run, not a proof: the proof is the loom model.
    ///
    /// Mutation (run): `read` returning `copy` without the sequence
    /// re-check → mixed copies are accepted → fails.
    #[test]
    #[cfg_attr(miri, ignore = "a threaded stress run; the loom model covers it")]
    fn a_concurrent_reader_never_sees_a_mixed_copy() {
        use std::sync::atomic::AtomicBool;

        let mut writer = SeqWriter::<8>::new();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let cell = writer.reader();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut mixed = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(w) = cell.read() {
                        if w.iter().any(|&x| x != w[0]) {
                            mixed += 1;
                        }
                    }
                }
                mixed
            })
        };
        for i in 1..=200_000u64 {
            writer.write([i; 8]);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(reader.join().unwrap(), 0, "a reader accepted a mixed copy");
    }
}

//! [`SeqWriter`] and [`SeqReader`]: a value of type `T` that one thread
//! overwrites in place and any thread reads by copy, without either side
//! allocating, locking or freeing. A sequence lock.
//!
//! Its own module because what it adds over a run of atomics is the reason it
//! exists: a read returns the value of one write, never bytes of two. The
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
//! # The storage
//!
//! The value is held as its bytes, in `u64` words: `size_of::<T>()` rounded
//! up to a whole word, the tail of the last word zero. `T` is
//! [`NoUninit`] (its bytes can be copied out: no padding) and
//! [`AnyBitPattern`] (any bytes are a `T`: what a read copies back is one),
//! so the conversion both ways is `bytemuck`'s, and the module has no
//! `unsafe`.
//!
//! # The protocol
//!
//! The sequence is even when the words are stable and odd while a write is
//! copying them; it starts at zero, "never written". The words are atomics,
//! each stored and loaded `Relaxed`, so a read racing a write copies stale
//! or mixed words, and the sequence check discards them: there is no data
//! race to make undefined.
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

use bytemuck::{AnyBitPattern, NoUninit};
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

/// Bytes per storage word.
const WORD: usize = std::mem::size_of::<u64>();

/// The shared state: the sequence, then `T`'s bytes in whole words.
///
/// One allocation, the sequence first, so it shares a line with the first
/// words. Zero until the first write; then even when stable, odd mid-write.
type Cell = Arc<[AtomicU64]>;

/// The sequence word and the value words of a cell.
#[inline]
fn split(cell: &Cell) -> (&AtomicU64, &[AtomicU64]) {
    let (seq, words) = cell.split_first().expect("a cell holds its sequence");
    (seq, words)
}

/// A chunk of at most [`WORD`] bytes as one word, its tail zero.
#[inline]
fn word_of(chunk: &[u8]) -> u64 {
    let mut bytes = [0; WORD];
    bytes[..chunk.len()].copy_from_slice(chunk);
    u64::from_ne_bytes(bytes)
}

/// The one writer of a sequence cell holding a `T`: it overwrites the value
/// in place, and every [`SeqReader`] it hands out copies it whole.
///
/// **One writer, structurally.** A `SeqWriter` is not `Clone`, and
/// [`write`](Self::write) takes `&mut self`, so two writes can never
/// overlap: a second writer would let a reader accept bytes of two writes
/// (the sequence going 1, 2, 3, 2 under two writers looks stable to a
/// reader). Readers are unlimited.
///
/// Wait-free on both sides; no allocation after [`new`](Self::new).
///
/// ```
/// use tutti_types::SeqWriter;
///
/// let mut writer = SeqWriter::<[f64; 2]>::new();
/// let reader = writer.reader();
/// assert_eq!(reader.read(0), Ok(None), "never written");
/// writer.write([1.0, 2.0]);
/// assert_eq!(reader.read(0), Ok(Some([1.0, 2.0])));
/// ```
///
/// A second writer cannot be made:
///
/// ```compile_fail
/// let writer = tutti_types::SeqWriter::<u64>::new();
/// let second = writer.clone();
/// ```
///
/// Nor can a shared writer write:
///
/// ```compile_fail
/// fn write_shared(writer: &tutti_types::SeqWriter<u64>) {
///     writer.write(1);
/// }
/// ```
// Mutations (run) for the `compile_fail` doctests: `SeqWriter` deriving
// `Clone` → the first compiles → fails; `write(&self)` keeping the sequence in
// the cell (load, then store, as the first version did) → the second compiles
// → fails.
pub struct SeqWriter<T: NoUninit + AnyBitPattern> {
    cell: Cell,
    /// The sequence of the last completed write: the writer's own copy, so a
    /// write never loads it.
    seq: u64,
    /// What the last write stored, for [`last`](Self::last).
    last: Option<T>,
}

impl<T: NoUninit + AnyBitPattern> SeqWriter<T> {
    /// A cell nothing has been written to, and its writer. Allocates the
    /// shared cell once: one word for the sequence and
    /// `size_of::<T>()` rounded up to whole words for the value.
    pub fn new() -> Self {
        let words = std::mem::size_of::<T>().div_ceil(WORD);
        Self {
            cell: (0..=words).map(|_| AtomicU64::new(0)).collect(),
            seq: 0,
            last: None,
        }
    }

    /// A read side of this cell. Clone it for more readers.
    pub fn reader(&self) -> SeqReader<T> {
        SeqReader {
            cell: Arc::clone(&self.cell),
            _value: std::marker::PhantomData,
        }
    }

    /// Overwrites the value in place. Wait-free; allocates and frees nothing.
    ///
    /// A reader on the writer's own thread, after this returns, reads
    /// `value` on its first copy.
    pub fn write(&mut self, value: T) {
        let (seq, words) = split(&self.cell);
        seq.store(self.seq.wrapping_add(1), Ordering::Relaxed);
        // Orders the odd mark before the word stores, for a reader that sees
        // any of them (its `Acquire` fence in `read` pairs with this).
        fence(Ordering::Release);
        for (word, chunk) in words.iter().zip(bytemuck::bytes_of(&value).chunks(WORD)) {
            word.store(word_of(chunk), Ordering::Relaxed);
        }
        // Wraps to 0 ("never written") after 2^63 writes: unreachable (a
        // write per microsecond takes ~290,000 years).
        self.seq = self.seq.wrapping_add(2);
        seq.store(self.seq, Ordering::Release);
        self.last = Some(value);
    }

    /// What the last [`write`](Self::write) stored, or `None` before the
    /// first: the writer's own copy, so it is never torn and never retries.
    pub fn last(&self) -> Option<T> {
        self.last
    }
}

impl<T: NoUninit + AnyBitPattern> Default for SeqWriter<T> {
    /// Never written.
    fn default() -> Self {
        Self::new()
    }
}

impl<T: NoUninit + AnyBitPattern + std::fmt::Debug> std::fmt::Debug for SeqWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeqWriter")
            .field("seq", &self.seq)
            .field("last", &self.last)
            .finish()
    }
}

/// A [`SeqReader::read`] gave up: every copy it was allowed overlapped a
/// write. A write has begun (perhaps only the first, still in flight); a
/// later read can succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contended;

impl std::fmt::Display for Contended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("every copy of the sequence cell overlapped a write")
    }
}

impl std::error::Error for Contended {}

/// A read side of a sequence cell: copies the value of one completed
/// [`SeqWriter::write`], never bytes of two. `Clone` for more readers; any
/// thread.
pub struct SeqReader<T> {
    cell: Cell,
    /// `fn() -> T`: a reader hands out `T`s and holds none, so it is `Send`
    /// and `Sync` whatever `T` is (the cell is atomics).
    _value: std::marker::PhantomData<fn() -> T>,
}

impl<T: NoUninit + AnyBitPattern> SeqReader<T> {
    /// The value of the last completed write. Wait-free: at most
    /// `1 + retries` copies, each discarded if a write overlapped it.
    ///
    /// - `Ok(None)`: nothing has been written yet (decided on the first
    ///   load; no retry is spent on it).
    /// - `Ok(Some(value))`: the value one write stored.
    /// - `Err(Contended)`: every copy overlapped a write. A write is a few
    ///   relaxed stores, so a reader that loses this often was descheduled
    ///   rather than raced; the caller chooses how long that is worth trying
    ///   on its thread. A reader on the writer's own thread never sees it.
    pub fn read(&self, retries: usize) -> Result<Option<T>, Contended> {
        let (seq, words) = split(&self.cell);
        for _ in 0..=retries {
            let before = seq.load(Ordering::Acquire);
            if before == 0 {
                return Ok(None);
            }
            if !before.is_multiple_of(2) {
                sync::spin_loop();
                continue;
            }
            let mut copy = T::zeroed();
            for (chunk, word) in bytemuck::bytes_of_mut(&mut copy)
                .chunks_mut(WORD)
                .zip(words)
            {
                let bytes = word.load(Ordering::Relaxed).to_ne_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            // Orders the word loads before the re-check: a word from a later
            // write makes that write's odd mark visible to the load below.
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == before {
                return Ok(Some(copy));
            }
            sync::spin_loop();
        }
        Err(Contended)
    }
}

impl<T> Clone for SeqReader<T> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
            _value: std::marker::PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for SeqReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeqReader")
            .field("words", &(self.cell.len() - 1))
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Nothing before the first write; afterwards, the last write's value.
    ///
    /// Mutation (run): `read` without the `before == 0` check → the first
    /// read returns `Ok(Some([0, 0, 0]))` → fails.
    #[test]
    fn a_read_returns_the_last_write() {
        let mut writer = SeqWriter::<[u64; 3]>::new();
        let cell = writer.reader();
        assert_eq!(cell.read(0), Ok(None));
        assert_eq!(writer.last(), None);
        writer.write([1, 2, 3]);
        assert_eq!(cell.read(0), Ok(Some([1, 2, 3])));
        writer.write([4, 5, 6]);
        assert_eq!(cell.read(0), Ok(Some([4, 5, 6])));
        assert_eq!(cell.read(0), Ok(Some([4, 5, 6])), "a read takes nothing");
        assert_eq!(writer.last(), Some([4, 5, 6]));
    }

    /// A `T` whose size is not a whole number of words round-trips every
    /// byte, through a last word it fills only partly, and a one-byte and a
    /// zero-sized `T` work too.
    ///
    /// Mutation (run): `new` sizing the words with `size_of / WORD`
    /// (rounding down) → the 13-byte value gets one word and reads back with
    /// its last five bytes zero → fails.
    #[test]
    fn a_value_short_of_a_whole_word_round_trips() {
        let value: [u8; 13] = std::array::from_fn(|i| 0xA0 | i as u8);
        let mut writer = SeqWriter::<[u8; 13]>::new();
        let reader = writer.reader();
        assert_eq!(reader.cell.len(), 1 + 2, "the sequence and two words");
        writer.write(value);
        assert_eq!(reader.read(0), Ok(Some(value)));
        // The padding is zero: the last word holds bytes 8..13 and nothing
        // else, so a stale tail never leaks between writes.
        let tail = reader.cell[2].load(Ordering::Relaxed).to_ne_bytes();
        assert_eq!(tail[..5], value[8..]);
        assert_eq!(tail[5..], [0; 3]);

        let mut byte = SeqWriter::<u8>::new();
        byte.write(0xFE);
        assert_eq!(byte.reader().read(0), Ok(Some(0xFE)));

        let mut unit = SeqWriter::<()>::new();
        assert_eq!(unit.reader().read(0), Ok(None));
        unit.write(());
        assert_eq!(unit.reader().read(0), Ok(Some(())));
    }

    /// A cell whose write never completes (a writer preempted mid-write)
    /// is `Contended` after the retries, never "never written"; one never
    /// written is `Ok(None)` however few retries the caller allows.
    ///
    /// Mutation (run): `read` returning `Ok(None)` after the last retry →
    /// the mid-write read is `Ok(None)` → fails. Mutation (run): `read`
    /// checking for an odd sequence before the `before == 0` check, and
    /// returning `Err(Contended)` for zero → fails.
    #[test]
    fn a_read_tells_never_written_from_contended() {
        let mut writer = SeqWriter::<u64>::new();
        let reader = writer.reader();
        assert_eq!(reader.read(0), Ok(None));
        assert_eq!(reader.read(5), Ok(None));
        writer.write(7);
        // What a write leaves between its odd mark and its closing store.
        reader.cell[0].store(3, Ordering::Relaxed);
        assert_eq!(reader.read(0), Err(Contended));
        assert_eq!(reader.read(5), Err(Contended));
        reader.cell[0].store(2, Ordering::Relaxed);
        assert_eq!(reader.read(0), Ok(Some(7)), "settled again");
    }

    /// A concurrent reader never accepts a copy mixing two writes. Every
    /// write stores one value in all elements, so a mix shows as unequal
    /// elements. A stress run, not a proof: the proof is the loom model.
    ///
    /// Mutation (run): `read` returning `copy` without the sequence
    /// re-check → mixed copies are accepted → fails.
    #[test]
    #[cfg_attr(miri, ignore = "a threaded stress run; the loom model covers it")]
    fn a_concurrent_reader_never_sees_a_mixed_copy() {
        use std::sync::atomic::AtomicBool;

        let mut writer = SeqWriter::<[u64; 8]>::new();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let cell = writer.reader();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut mixed = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(Some(w)) = cell.read(8) {
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

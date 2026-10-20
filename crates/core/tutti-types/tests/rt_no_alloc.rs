//! Regression gate: the RT capped collection must not allocate on its steady
//! state. A capacity is reserved inline at construction; push/sort/iterate
//! then touch only that inline storage.

use assert_no_alloc::AllocDisabler;
use tutti_types::{AudioThreadCell, RtVec};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `RtVec` reached through `&self`, as `tutti-midi-hardware`'s drain holds it:
/// behind an `AudioThreadCell`. Ported from the gate on `RtEventBuf`, the
/// `&self` wrapper this composition replaced, so the cap stays gated.
///
/// Mutation (run, when the cap lived in a private `Capped` layer since
/// folded into `RtVec`): an uncapped `Vec` in place of the `ArrayVec` → the
/// first block pushes past the warm-up's capacity, grows the `Vec` inside
/// the gate, and aborts.
#[test]
fn rt_vec_steady_state_is_allocation_free() {
    let buf: AudioThreadCell<RtVec<(u32, u32), 64>> = AudioThreadCell::new(RtVec::new());
    // Warm up well within the inline capacity.
    buf.borrow_mut().extend((0..32).map(|i| (32 - i, i)));

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let mut v = buf.borrow_mut();
            v.clear();
            for i in 0..96u32 {
                let _ = v.push((96 - i, i));
            }
            v.as_mut_slice().sort_by_key(|&(k, _)| k);
            let sum: u32 = v.iter().map(|&(_, x)| x).sum();
            core::hint::black_box(sum);
        }
    });
    assert_eq!(buf.borrow().len(), 64);
    assert!(buf.borrow().overflowed());
}

/// Every `AudioThreadCell` borrow path — `borrow_mut`, `borrow`, granted
/// and refused `try_borrow_mut` and `try_borrow`, two shared borrows at once,
/// and each guard's drop — neither allocates nor frees: the always-on check
/// is a compare-exchange and a store or decrement.
///
/// The panic path is left out: it formats a message, and it is a contract
/// violation, not a path the audio thread runs.
///
/// Mutations (run): box the result in `try_acquire_writer` (a
/// `Box::new(self.state.compare_exchange(..))`, unboxed for the test) → the
/// first borrow allocates inside the gate, and `assert_no_alloc` aborts; the
/// same in `try_acquire_reader` (`Ok(_) => return *Box::new(true)`) →
/// aborts at the first `try_borrow`.
#[test]
fn audio_thread_cell_borrows_neither_allocate_nor_free() {
    let cell = AudioThreadCell::new(0u64);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..10_000u64 {
            *cell.borrow_mut() += i;
            let held = cell.try_borrow_mut().expect("nothing else borrows");
            assert!(cell.try_borrow_mut().is_none());
            assert!(cell.try_borrow().is_none());
            drop(held);
            let a = cell.try_borrow().expect("no writer");
            let b = cell.borrow();
            assert!(cell.try_borrow_mut().is_none());
            core::hint::black_box(*a + *b);
        }
    });
    assert_eq!(*cell.borrow(), (0..10_000u64).sum::<u64>());
}

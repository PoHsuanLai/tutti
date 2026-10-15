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
/// Mutation (run): `Capped` over an uncapped `Vec` in place of the
/// `ArrayVec` → the first block pushes past the warm-up's capacity, grows the
/// `Vec` inside the gate, and aborts.
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

/// The whole reader side of `RtPublish` — slot reads, overflow reads past the
/// slot count, and dropping a read of a value that has since been retired —
/// neither allocates nor frees.
///
/// This is a claim about the *code path*, which is deterministic and so can be
/// pinned by a gate: `RtRef::drop` has no branch that runs a destructor. The
/// race-shaped half of the property (a publisher reclaiming concurrently) is
/// the loom model's job, `tests/rt_publish_loom.rs`, not this test's.
///
/// Mutation: make `RtRef::drop` reclaim (call `take_unprotected` and drop the
/// result, in both the slot and the overflow branch). The last `held` drop
/// then frees the retired `Vec` inside the gate, and `assert_no_alloc` aborts.
#[test]
fn rt_publish_reader_side_neither_allocates_nor_frees() {
    use std::sync::Arc;
    use tutti_types::RtPublish;

    let cell = RtPublish::new(vec![1u32; 64]);
    // More live reads than any sane slot count, so the overflow path runs too.
    let mut held = Vec::with_capacity(64);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..32 {
            held.push(cell.read());
        }
    });
    // Retire the value every `held` read points at. `publish` allocates the
    // new value and may grow the retirement list; that is the control side.
    cell.publish(Arc::new(vec![2u32; 64]));
    assert!(held.iter().all(|r| r[0] == 1));

    assert_no_alloc::assert_no_alloc(|| {
        // A fresh read sees the new value, and dropping the last readers of
        // the retired one frees nothing.
        assert_eq!(cell.read()[0], 2);
        // Newest first, so the overflow readers go before the slot readers
        // and the last drop is the one a reclaiming drop would free on.
        while let Some(r) = held.pop() {
            drop(r);
        }
    });
}

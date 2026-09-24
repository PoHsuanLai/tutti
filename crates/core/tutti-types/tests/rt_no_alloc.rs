//! Regression gate: the RT event collector must not allocate on its steady
//! state. A capacity is reserved inline at construction; refill/push/sort/
//! for_each then touch only that inline storage.

use assert_no_alloc::AllocDisabler;
use tutti_types::RtEventBuf;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[test]
fn rt_event_buf_steady_state_is_allocation_free() {
    let buf: RtEventBuf<(u32, u32), 64> = RtEventBuf::new();
    // Warm up well within the inline capacity.
    buf.refill((0..32).map(|i| (32 - i, i)));

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            buf.clear();
            for i in 0..32u32 {
                let _ = buf.push((32 - i, i));
            }
            buf.sort_by_key(|&(k, _)| k);
            let mut sum = 0u32;
            buf.for_each(|&(_, v)| sum += v);
            core::hint::black_box(sum);
        }
    });
    assert_eq!(buf.len(), 32);
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

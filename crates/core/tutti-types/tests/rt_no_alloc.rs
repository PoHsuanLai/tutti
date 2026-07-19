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

extern crate std;
use super::*;
use std::vec::Vec;

#[test]
fn push_fills_then_refuses_at_the_cap() {
    let mut v: RtVec<u32, 3> = RtVec::new();
    assert!(v.push(1));
    assert!(v.push(2));
    assert!(v.push(3));
    assert!(!v.push(4), "push past N must be refused");
    assert_eq!(v.as_slice(), &[1, 2, 3]);
    assert_eq!(v.len(), 3);
    assert!(v.is_full());
    assert_eq!(v.remaining(), 0);
}

#[test]
fn overflow_is_reported_and_latches_for_the_block() {
    let mut v: RtVec<u32, 2> = RtVec::new();
    assert!(!v.is_overflowed());
    v.push(1);
    v.push(2);
    assert!(!v.is_overflowed(), "at capacity but nothing dropped yet");
    v.push(3);
    assert!(v.is_overflowed(), "a refused push latches the flag");
    assert!(
        v.is_overflowed(),
        "and it stays set for the rest of the block"
    );
}

#[test]
fn clear_resets_contents_and_the_flag() {
    let mut v: RtVec<u32, 2> = RtVec::new();
    v.extend(0..10);
    assert!(v.is_overflowed());
    v.clear();
    assert!(v.is_empty());
    assert!(
        !v.is_overflowed(),
        "the flag describes the current block, not history"
    );
}

#[test]
fn extend_reports_how_many_it_wrote() {
    let mut v: RtVec<u32, 4> = RtVec::new();
    assert_eq!(v.extend(0..2), 2, "everything fits");
    assert!(!v.is_overflowed());
    assert_eq!(v.extend(100..200), 2, "only the remaining room");
    assert!(v.is_overflowed());
    assert_eq!(v.as_slice(), &[0, 1, 100, 101]);
}

#[test]
fn as_slice_serves_a_borrowed_return() {
    // The shape the plugin-host pools need: fill, then hand `&[T]` back.
    fn lend(v: &RtVec<u32, 8>) -> &[u32] {
        v.as_slice()
    }
    let mut v: RtVec<u32, 8> = RtVec::new();
    v.extend([7, 8, 9]);
    assert_eq!(lend(&v), &[7, 8, 9]);
}

#[test]
fn as_mut_slice_allows_in_place_fixups() {
    let mut v: RtVec<u32, 8> = RtVec::new();
    v.extend([3, 1, 2]);
    v.as_mut_slice().sort_unstable();
    assert_eq!(v.as_slice(), &[1, 2, 3]);
}

#[test]
fn deref_and_iter_read_the_filled_run_only() {
    let mut v: RtVec<u32, 8> = RtVec::new();
    v.extend([4, 5]);
    assert_eq!(v.len(), 2, "not the inline capacity");
    assert_eq!(v.first(), Some(&4), "slice methods via Deref");
    let seen: Vec<u32> = v.iter().copied().collect();
    assert_eq!(seen, std::vec![4, 5]);
}

#[test]
fn refilling_past_the_cap_every_block_stays_at_the_cap() {
    // The regression this type exists to prevent: driven past `N` on every
    // block, the collection must never grow. Checked via `remaining`,
    // which `no_std` can assert without a heap probe.
    let mut v: RtVec<u32, 4> = RtVec::new();
    for _ in 0..1_000 {
        v.clear();
        v.extend(0..64);
        assert_eq!(v.len(), 4);
        assert_eq!(v.remaining(), 0);
        assert!(v.is_overflowed());
    }
}

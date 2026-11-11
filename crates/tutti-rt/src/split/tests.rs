use super::*;

/// Exclusive claims exclude, and release on drop.
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

/// The same for a write: a full `Held` refuses an exclusive claim before
/// taking it.
#[test]
fn a_full_held_leaves_no_write_claim_behind() {
    let mut v = [0u8; 2];
    let mut t = ClaimTable::new(2);
    let s = SplitRw::new(&mut v, 1, &mut t);
    let h = Held::new(1);
    let mut view = s.view(&h);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        view.split(&[(0, ()), (1, ())], |()| true, |_, _| {}, |_, _| {});
    }));
    assert!(r.is_err(), "the second claim does not fit");
    drop(view);
    let mut view = s.view(&h);
    assert_eq!(view.try_get_mut(1).map(|c| c.len()), Ok(1));
}

/// `split`'s reads and writes land on the right chunks.
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

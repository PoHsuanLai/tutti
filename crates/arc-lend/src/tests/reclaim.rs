//! What a publish or a reclaim frees, and what a loan keeps.

use super::*;

#[test]
fn a_lend_sees_the_published_value() {
    let cell = Arc::new(ArcLend::new(vec![1u32, 2, 3]));
    let mut reader = cell.reader();
    assert_eq!(&*reader.lend(), &[1, 2, 3]);

    cell.publish(Arc::new(vec![4, 5]));
    assert_eq!(&*reader.lend(), &[4, 5]);
}

#[test]
fn a_held_loan_keeps_seeing_its_own_snapshot() {
    // The iteration-scoped contract in miniature: a reader that took a value
    // before a publish keeps working with it, rather than tearing
    // mid-iteration.
    let cell = Arc::new(ArcLend::new(vec![1u32]));
    let (mut a, mut b) = (cell.reader(), cell.reader());
    let held = a.lend();

    cell.publish(Arc::new(vec![9]));

    assert_eq!(&*held, &[1], "the loan still sees the snapshot it took");
    assert_eq!(&*b.lend(), &[9], "a fresh lend sees the new value");
}

#[test]
fn publish_frees_the_retired_value_on_the_publishing_thread() {
    // The property the whole type exists for. `dropped` flips when the
    // retired payload is freed; it must be observable immediately after
    // `publish` returns, i.e. on this thread, never deferred to whichever
    // thread happens to read next.
    struct Tattle(Arc<StdAtomicBool>);
    impl Drop for Tattle {
        fn drop(&mut self) {
            self.0.store(true, StdOrdering::SeqCst);
        }
    }

    let dropped = Arc::new(StdAtomicBool::new(false));
    let cell = Arc::new(ArcLend::new(Tattle(dropped.clone())));
    let _reader = cell.reader();

    assert!(!dropped.load(StdOrdering::SeqCst));
    cell.publish(Arc::new(Tattle(Arc::new(StdAtomicBool::new(false)))));
    assert!(
        dropped.load(StdOrdering::SeqCst),
        "retired value must be freed by publish(), on the publishing thread"
    );
}

/// A reader that lets go of a retired value does not free it: the free is
/// deferred to the next publish, on the publisher's thread. This is the
/// property an `ArcSwap` cannot promise: there, the last holder of a settled
/// debt runs the destructor wherever it is.
///
/// Mutation: make `Reader::drop` reclaim (`self.cell.reclaim();` before its
/// store). The reader thread then frees value 0, appears in `sites`, and the
/// first assertion after the join fails.
#[test]
fn a_reader_releasing_a_retired_value_never_frees_it() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));

    let (took_tx, took_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let mut reader = cell.reader();
    let reader = std::thread::spawn(move || {
        let held = reader.lend();
        assert_eq!(held.id, 0);
        took_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        // The value is retired by now; dropping the only loan of it is
        // exactly the moment an owning handle would free.
        drop(held);
        // The reader goes too; the cell outlives it (we hold a handle).
        drop(reader);
        std::thread::current().id()
    });

    took_rx.recv().unwrap();
    cell.publish(make(1));
    assert!(
        sites.lock().unwrap().is_empty(),
        "value 0 is held by the reader and must not be freed yet"
    );
    assert_eq!(cell.retired_len(), 1);

    release_tx.send(()).unwrap();
    let reader_thread = reader.join().unwrap();
    assert!(
        sites.lock().unwrap().is_empty(),
        "releasing the loan must not free anything, on any thread"
    );

    // The next publish reclaims it, here.
    cell.publish(make(2));
    let here = std::thread::current().id();
    assert_eq!(*sites.lock().unwrap(), vec![(0, here), (1, here)]);
    assert_ne!(here, reader_thread);
    assert_eq!(cell.retired_len(), 0);
}

/// A reader holding a loan protects exactly its own value: a publish frees
/// every other retired value, and keeps that one.
///
/// Mutation: in `Control::scan`, drop the announcement check
/// (`is_announced`). Value 0 is then freed while `held` still points at it,
/// and `sites` gains it at the first check.
#[test]
fn a_loan_protects_only_the_value_it_announced() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();

    let held = reader.lend();
    cell.publish(make(1));
    cell.publish(make(2));
    assert_eq!(
        freed(&sites),
        vec![1],
        "1 was never lent, so it goes; 0 stays"
    );
    assert_eq!(held.id, 0);
    drop(held);
    drop(reader);

    drop(Arc::into_inner(cell).unwrap());
    let mut ids = freed(&sites);
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![0, 1, 2],
        "dropping the cell frees the rest, once each"
    );
}

/// A parked loan pins at most one value per registered reader, however many
/// publishes pass. Nesting takes one reader per level, and each
/// level pins only the value it lent.
///
/// Mutation: in `Slots::read`, make an announcement pin everything
/// (`floor = 0` in its arm), as a reader-count-blind epoch would. The retired
/// list then grows by one per publish and the bound fails on the first loop
/// iteration.
#[test]
fn parked_loans_pin_one_value_per_reader() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut readers: Vec<_> = (0..12).map(|_| cell.reader()).collect();

    // A staircase: each reader lends, then a publish retires what it holds.
    let mut parked = Vec::new();
    for (id, reader) in (1..).zip(readers.iter_mut()) {
        parked.push(reader.lend());
        cell.publish(make(id));
    }
    assert_eq!(cell.retired_len(), 12, "one value per parked loan");
    for (id, loan) in parked.iter().enumerate() {
        assert_eq!(loan.id, id);
    }

    for id in 13..=200 {
        cell.publish(make(id));
        assert_eq!(cell.retired_len(), 12, "publish {id}: still one per loan");
    }
    // Everything between went, oldest first, on this thread.
    assert_eq!(freed(&sites), (12..200).collect::<Vec<_>>());

    drop(parked);
    assert_eq!(cell.reclaim(), 0, "and they go once the loans drop");
    let mut ids = freed(&sites);
    ids.sort_unstable();
    assert_eq!(ids, (0..200).collect::<Vec<_>>());
}

/// `reclaim` frees what a `publish` would, without publishing, and reports
/// what is still pinned.
///
/// Mutation: make `reclaim` return `0` without scanning. The first
/// assertion fails (it reports 0 with value 1 still held), and the second
/// (value 0 must be freed).
#[test]
fn reclaim_frees_without_publishing_and_reports_what_is_pinned() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let (mut a, mut b) = (cell.reader(), cell.reader());

    let held_0 = a.lend();
    cell.publish(make(1));
    let held_1 = b.lend();
    cell.publish(make(2));
    drop(held_0);

    assert_eq!(cell.reclaim(), 1, "1 is still lent");
    assert_eq!(freed(&sites), vec![0]);
    assert_eq!(held_1.id, 1);
    drop(held_1);
    assert_eq!(cell.reclaim(), 0);
    assert_eq!(freed(&sites), vec![0, 1]);
}

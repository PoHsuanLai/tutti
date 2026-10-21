use super::*;

use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering};
use std::sync::mpsc;
use std::time::Duration;

/// Records the thread that dropped it, so a test can say *where* a value was
/// freed, not just that it was.
struct DropSite {
    id: usize,
    sites: Arc<std::sync::Mutex<Vec<(usize, std::thread::ThreadId)>>>,
}
impl Drop for DropSite {
    fn drop(&mut self) {
        self.sites
            .lock()
            .unwrap()
            .push((self.id, std::thread::current().id()));
    }
}

type Sites = Arc<std::sync::Mutex<Vec<(usize, std::thread::ThreadId)>>>;

fn sites() -> (Sites, impl Fn(usize) -> Arc<DropSite>) {
    let sites: Sites = Arc::default();
    let make = {
        let sites = sites.clone();
        move |id| {
            Arc::new(DropSite {
                id,
                sites: sites.clone(),
            })
        }
    };
    (sites, make)
}

fn freed(sites: &Sites) -> Vec<usize> {
    sites.lock().unwrap().iter().map(|&(id, _)| id).collect()
}

/// Puts `reader`'s slot in the state a lend is in between its claim and its
/// announcement, as if the reader were preempted there: the first half of
/// `Reader::lend`, without the load or the announcement.
fn claim_only<T>(reader: &mut Reader<T>) {
    let claim = reader.next_claim();
    reader.slot().word.store(claim, Ordering::Release);
}

/// Clears a slot left claimed by [`claim_only`], as the rest of that lend and
/// its loan's drop would.
fn finish<T>(reader: &Reader<T>) {
    reader.slot().word.store(FREE, Ordering::Release);
}

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

/// Autoref specialization: the inherent const on `Wrap<T>` wins over the
/// trait's default only when the bound holds, so `IS_*` resolves to `false`
/// exactly when the type does *not* have the trait. A plain
/// `assert_not_send::<T>()` cannot express this (there is no stable negative
/// bound).
///
/// `const` blocks rather than `assert!`: both operands are compile-time
/// constants, so the property fails the *build*, not just a test run.
struct Wrap<T>(PhantomData<T>);
trait NotSend {
    const IS_SEND: bool = false;
}
impl<T> NotSend for Wrap<T> {}
impl<T: Send> Wrap<T> {
    const IS_SEND: bool = true;
}
trait NotSync {
    const IS_SYNC: bool = false;
}
impl<T> NotSync for Wrap<T> {}
impl<T: Sync> Wrap<T> {
    const IS_SYNC: bool = true;
}

/// A loan must not escape the thread that took it: that is what makes "moved
/// to another thread" a compile error rather than a convention.
///
/// Mutation: `unsafe impl<T: Sync> Send for Loan<'_, T> {}`; the first
/// assertion stops compiling.
#[test]
fn a_loan_is_not_send() {
    const {
        assert!(!<Wrap<Loan<'static, Vec<u32>>>>::IS_SEND);
    }
    // Control: the same machinery reports `true` for a type that IS `Send`,
    // so the assertion above measures something.
    const {
        assert!(<Wrap<Vec<u32>>>::IS_SEND);
    }
}

/// A reader moves to the thread that lends (it is made on the control side),
/// and is not shared: one reader, one owner, one slot.
///
/// Mutations: delete the `unsafe impl Send for Reader`; the first assertion
/// stops compiling. Add `unsafe impl<T: Send + Sync> Sync for Reader<T> {}`;
/// the second does.
#[test]
fn a_reader_is_send_but_not_sync() {
    const {
        assert!(<Wrap<Reader<Vec<u32>>>>::IS_SEND);
        assert!(!<Wrap<Reader<Vec<u32>>>>::IS_SYNC);
        // Controls.
        assert!(<Wrap<Vec<u32>>>::IS_SYNC);
        assert!(!<Wrap<Cell<u32>>>::IS_SYNC);
    }
    // And only for a payload the cell itself may share: a reader of a
    // `!Sync` payload would hand `&T` to another thread.
    const {
        assert!(!<Wrap<Reader<Cell<u32>>>>::IS_SEND);
    }
}

/// The cell itself must stay shareable: every reader holds an
/// `Arc<ArcLend<_>>` across threads. The raw pointer inside would make it
/// neither `Send` nor `Sync` by default; `PhantomData<Arc<T>>` plus the
/// atomics is what gives it `Arc<T>`'s auto traits back without an
/// `unsafe impl`.
///
/// Mutation: replace `_owns: PhantomData<Arc<T>>` with
/// `PhantomData<*const T>`; this stops compiling.
#[test]
fn the_cell_is_send_and_sync_for_send_sync_payloads() {
    fn assert_send_sync<X: Send + Sync>() {}
    assert_send_sync::<ArcLend<Vec<u32>>>();
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
/// Mutation: in `scan`, drop the announcement check (`binary_search`). Value
/// 0 is then freed while `held` still points at it, and `sites` gains it at
/// the first check.
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
/// Mutation: in `scan`, make an announcement pin everything (`floor = 0` in
/// its branch), as a reader-count-blind epoch would. The retired list then
/// grows by one per publish and the bound fails on the first loop iteration.
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

/// The publisher never waits on a reader caught between claiming its slot
/// and announcing. It keeps everything that reader might still load (each
/// value current at or after the last scan before the claim) and checks
/// again on the next publish.
///
/// The publishes run on another thread with a deadline, so a publisher that
/// waited for the claim fails by timeout rather than hanging the suite.
///
/// Mutation: in `scan`, wait while a slot holds a claim
/// (`while word & 1 == 1 { yield_now(); word = slot.0.word.load(Acquire) }`).
/// The publishes never return and the `recv_timeout` fails.
#[test]
#[cfg_attr(miri, ignore = "waits on a wall-clock deadline")]
fn the_publisher_never_waits_on_a_reader_mid_lend() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();
    claim_only(&mut reader);

    let (done_tx, done_rx) = mpsc::channel();
    let publisher = {
        let (cell, make) = (cell.clone(), make);
        std::thread::spawn(move || {
            for id in 1..=3 {
                cell.publish(make(id));
            }
            done_tx.send(()).unwrap();
            make
        })
    };
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("publish waited on a reader mid-lend");
    let make = publisher.join().unwrap();

    // The claim was made before the first scan, so the reader may load
    // anything from value 0 on: nothing retired can go.
    assert_eq!(cell.retired_len(), 3);
    assert!(freed(&sites).is_empty());

    // Once the lend is done (and its loan dropped), the next publish frees
    // them all.
    finish(&reader);
    cell.publish(make(4));
    assert_eq!(freed(&sites), vec![0, 1, 2, 3]);
    assert_eq!(cell.retired_len(), 0);
}

/// A claim the publisher has already seen keeps the floor it had then: the
/// reader may have been preempted before its load ever since, so it can
/// still load the oldest value current since that scan.
///
/// Mutation: in `scan`, always take `scanned_at` as a claim's floor (forget
/// that the same claim was seen before). The second publish then frees value
/// 0, which the claim may still load, and the first assertion fails. (This is
/// a use-after-free in a real interleaving; the loom model catches the same
/// mutation, in `one_reader_two_publishes`.)
#[test]
fn a_claim_seen_before_keeps_its_floor() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();

    claim_only(&mut reader);
    cell.publish(make(1)); // sees the claim first: floor 0
    cell.publish(make(2)); // the same claim: still floor 0
    assert!(freed(&sites).is_empty(), "0 and 1 may still be loaded");
    assert_eq!(cell.retired_len(), 2);
    finish(&reader);
}

/// A *new* claim (a later generation) pins only from the last scan on: the
/// last scan read the slot before the claim, so the reader loads a value no
/// older than the one current then. This is what the generation in a claim
/// is for; without it a reader that keeps lending would look mid-lend
/// forever and pin every value retired since its first claim.
///
/// Mutation: make `next_claim` return `1` (no generation). The second
/// publish then mistakes the new claim for the old one, keeps floor 0, and
/// value 0 is not freed: the first assertion fails.
#[test]
fn a_new_claim_pins_only_from_the_last_scan() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();

    claim_only(&mut reader);
    cell.publish(make(1)); // floor 0: keeps 0
                           // The reader finishes that lend, drops it, and claims again before the
                           // next scan reads its slot.
    finish(&reader);
    claim_only(&mut reader);
    cell.publish(make(2)); // a new claim: floor 1, keeps 1, frees 0
    assert_eq!(freed(&sites), vec![0]);
    assert_eq!(cell.retired_len(), 1);

    finish(&reader);
    assert_eq!(cell.reclaim(), 0);
    assert_eq!(freed(&sites), vec![0, 1]);
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

/// Slots of dropped readers are reused, so the slot list stays at the
/// high-water mark of readers registered at once, and grows a block at a
/// time past it. A clone is a registration of its own.
///
/// Mutation: make `Reader::drop` do nothing. The re-registrations then take
/// new slots and the count after them is two blocks, not one.
#[test]
fn slots_are_reused_and_the_list_keeps_its_high_water_mark() {
    let cell = Arc::new(ArcLend::new(0u32));
    let first: Vec<_> = (0..BLOCK).map(|_| cell.reader()).collect();
    assert_eq!(cell.slot_count(), BLOCK);
    drop(first);
    for _ in 0..3 {
        let again: Vec<_> = (0..BLOCK).map(|_| cell.reader()).collect();
        assert_eq!(cell.slot_count(), BLOCK, "the dropped readers' slots");
        drop(again);
    }

    // Past a block, the list grows by one, and every reader lends its own
    // slot.
    let one = cell.reader();
    let mut many: Vec<_> = (0..BLOCK).map(|_| one.clone()).collect();
    assert_eq!(cell.slot_count(), 2 * BLOCK);
    let loans: Vec<_> = many.iter_mut().map(Reader::lend).collect();
    cell.publish(Arc::new(1));
    assert!(loans.iter().all(|l| **l == 0));
    assert_eq!(cell.retired_len(), 1);
    drop(loans);
    drop((one, many));
    assert_eq!(cell.reclaim(), 0);
    assert_eq!(cell.slot_count(), 2 * BLOCK, "the high-water mark stays");
}

/// A loan passed to `mem::forget` leaves its announcement in the slot, which
/// pins that one value until the reader lends again or its slot is handed to
/// a new reader. It never pins more.
///
/// Mutation: delete `slot.word.store(FREE, ..)` in `Control::take`. The
/// forgotten announcement then survives the slot's re-registration and the
/// last `reclaim` reports 1.
#[test]
fn a_forgotten_loan_pins_one_value_until_its_slot_is_reused() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));

    let mut reader = cell.reader();
    std::mem::forget(reader.lend());
    for id in 1..=10 {
        cell.publish(make(id));
    }
    assert_eq!(cell.retired_len(), 1, "only the forgotten value");
    assert_eq!(freed(&sites), (1..10).collect::<Vec<_>>());

    // The reader lends again: its slot moves on, and the value is released.
    assert_eq!(reader.lend().id, 10);
    assert_eq!(cell.reclaim(), 0);

    // Forgotten again, then the reader is dropped and its slot reused.
    std::mem::forget(reader.lend());
    cell.publish(make(11));
    drop(reader);
    assert_eq!(cell.reclaim(), 1, "the slot still announces 10");
    // Registering a block's worth of readers hands every slot on, that one
    // included.
    let _next: Vec<_> = (0..BLOCK).map(|_| cell.reader()).collect();
    assert_eq!(cell.slot_count(), BLOCK);
    assert_eq!(cell.reclaim(), 0, "a new reader starts from a free slot");
}

/// A retired value's destructor runs *after* the control lock is released,
/// so a destructor that panics neither poisons the lock nor leaves the
/// retired list half-updated; the cell keeps working.
///
/// (The other panic site the lock guards against, a failed `reserve`, cannot
/// be provoked here without an allocator that fails on demand; it happens
/// before the swap, which is what makes it harmless.)
///
/// Mutation: move `self.free(freeable)` inside the locked block in `publish`
/// (drop the values while the guard is alive). The panic then unwinds
/// through the guard and the lock is poisoned.
#[test]
fn a_panicking_destructor_does_not_poison_the_cell() {
    struct Bomb(bool);
    impl Drop for Bomb {
        fn drop(&mut self) {
            if self.0 {
                panic!("boom");
            }
        }
    }

    let cell = Arc::new(ArcLend::new(Bomb(true)));
    let mut reader = cell.reader();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cell.publish(Arc::new(Bomb(false)));
    }));
    assert!(result.is_err(), "the retired value's drop panicked");
    assert!(!cell.control.is_poisoned());
    assert_eq!(cell.retired_len(), 0, "it left the list before it ran");

    cell.publish(Arc::new(Bomb(false)));
    assert!(!reader.lend().0);
    assert_eq!(cell.retired_len(), 0);
}

/// The publisher's buffers are reused: a publish that frees a value leaves
/// the emptied buffer for the next, so a steady stream of publishes stops
/// allocating one per publish.
///
/// Mutation: make `free` drop the buffer instead of returning it. `spare`
/// then has no capacity after a publish and the assertion fails.
#[test]
fn a_publish_hands_its_buffer_to_the_next() {
    let cell = Arc::new(ArcLend::new(0u32));
    cell.publish(Arc::new(1));
    assert!(cell.lock().spare.capacity() > 0);
}

/// Shared by the two concurrent tests: readers check every value they lend
/// is whole and never freed, a publisher hammers the cell, and at the end
/// every generation was dropped exactly once and never on a reader thread.
fn readers_against_a_publisher(readers: usize, publishes: usize, nest: bool) {
    const LEN: usize = 16;
    const POISON: usize = usize::MAX;

    struct Gen {
        v: Vec<usize>,
        sites: Sites,
    }
    impl Drop for Gen {
        fn drop(&mut self) {
            let g = self.v[0];
            self.v.iter_mut().for_each(|x| *x = POISON);
            self.sites
                .lock()
                .unwrap()
                .push((g, std::thread::current().id()));
        }
    }

    let sites: Sites = Arc::default();
    let make = |g| Gen {
        v: vec![g; LEN],
        sites: sites.clone(),
    };
    let cell = Arc::new(ArcLend::new(make(0)));
    let done = Arc::new(StdAtomicBool::new(false));

    let handles: Vec<_> = (0..readers)
        .map(|_| {
            let done = done.clone();
            // Registered here, on the "control side", and moved in.
            let mut reader = cell.reader();
            let mut nested = cell.reader();
            std::thread::spawn(move || {
                let mut last = 0;
                let mut reads = 0usize;
                while !done.load(StdOrdering::Relaxed) || reads == 0 {
                    let r = reader.lend();
                    let g = r.v[0];
                    assert_ne!(g, POISON, "read a freed value");
                    assert!(r.v.iter().all(|&x| x == g), "torn value {:?}", r.v);
                    assert!(g >= last, "went back in time: {g} after {last}");
                    last = g;
                    reads += 1;
                    // Now and then, nest a second reader inside the first.
                    if nest && reads.is_multiple_of(64) {
                        let n = nested.lend();
                        assert!(n.v[0] >= g && n.v[0] != POISON);
                    }
                }
                drop((reader, nested));
                std::thread::current().id()
            })
        })
        .collect();

    for g in 1..=publishes {
        cell.publish(Arc::new(make(g)));
    }
    done.store(true, StdOrdering::Relaxed);
    let reader_threads: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    drop(Arc::into_inner(cell).expect("readers are joined and dropped"));
    let sites = sites.lock().unwrap();
    let mut ids: Vec<_> = sites.iter().map(|&(g, _)| g).collect();
    ids.sort_unstable();
    assert_eq!(ids, (0..=publishes).collect::<Vec<_>>(), "each freed once");
    assert!(
        sites.iter().all(|(_, t)| !reader_threads.contains(t)),
        "a reader thread freed a published value"
    );
}

/// Concurrent readers against a publisher hammering the cell.
///
/// Mutation: in `scan`, drop the announcement check; readers see poisoned
/// vectors and the whole-value assertion fails.
#[test]
#[cfg_attr(miri, ignore = "threaded stress test; miri runs the small one below")]
fn concurrent_readers_and_a_publisher_hammering() {
    readers_against_a_publisher(3, 20_000, true);
}

/// The same, small enough for miri, whose weak-memory emulation is the
/// second check on the protocol beside the loom model.
///
/// Mutations: remove either `SeqCst` fence; miri reports a data race on the
/// freed value on some seeds (CI runs 16). A plain x86 run does not, which is
/// why the loom model exists.
#[test]
fn concurrent_readers_and_a_publisher_small() {
    readers_against_a_publisher(2, 40, true);
}

/// `Debug` formats the value with the cell's lock released, so a payload
/// whose `fmt` publishes to the same cell (a value that logs a correction
/// of itself) does not deadlock.
///
/// Mutation (run): format under the lock (`let _control = self.lock();`
/// held across `f.debug_tuple(..)`). `fmt`'s publish blocks on the lock the
/// formatting thread holds, and the `recv_timeout` fails.
#[test]
#[cfg_attr(miri, ignore = "waits on a wall-clock deadline")]
fn debug_formats_outside_the_lock() {
    struct Reentrant {
        cell: std::sync::OnceLock<std::sync::Weak<ArcLend<Reentrant>>>,
    }
    impl std::fmt::Debug for Reentrant {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if let Some(cell) = self.cell.get().and_then(std::sync::Weak::upgrade) {
                cell.publish(Arc::new(Reentrant {
                    cell: std::sync::OnceLock::new(),
                }));
            }
            f.write_str("Reentrant")
        }
    }

    let first = Arc::new(Reentrant {
        cell: std::sync::OnceLock::new(),
    });
    let cell = Arc::new(ArcLend::from_arc(Arc::clone(&first)));
    first.cell.set(Arc::downgrade(&cell)).expect("set once");
    drop(first);

    let (done_tx, done_rx) = mpsc::channel();
    let formatter = {
        let cell = Arc::clone(&cell);
        std::thread::spawn(move || {
            let text = format!("{cell:?}");
            done_tx.send(text).unwrap();
        })
    };
    let text = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("Debug deadlocked on a payload that publishes");
    formatter.join().unwrap();
    assert_eq!(text, "ArcLend(Reentrant)");
    assert_eq!(cell.retired_len(), 0, "the formatted value was freed after");
}

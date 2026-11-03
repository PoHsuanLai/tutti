//! The publishing side's own guarantees: its lock, its buffers, `Debug`.

use super::*;

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

/// `current` is the value published last, as the same allocation the
/// publisher handed in, whether it published a `T` or an `Arc<T>`.
///
/// Mutation: make `publish` return before the swap (`return;` after the
/// reservations), keeping the old value. The second assertion fails.
#[test]
fn current_is_the_value_published_last() {
    let first = Arc::new(1u32);
    let cell = Arc::new(ArcLend::from_arc(Arc::clone(&first)));
    assert!(Arc::ptr_eq(&cell.current(), &first));

    let second = Arc::new(2u32);
    cell.publish(Arc::clone(&second));
    assert!(
        Arc::ptr_eq(&cell.current(), &second),
        "an `Arc` it was given"
    );

    cell.publish(3);
    assert_eq!(*cell.current(), 3, "a `T` it wrapped");
}

/// A snapshot from `current` owns its value: a publish retires the cell's
/// own count as usual and pins nothing for it, and the value is freed where
/// the snapshot's last `Arc` drops, here on another thread.
///
/// Mutation: in `Current::to_arc`, drop `Arc::increment_strong_count(ptr)`.
/// The snapshot then shares the cell's count, the publish frees value 0
/// under it, and the first assertion fails.
#[test]
fn a_snapshot_owns_its_value_and_frees_where_it_drops() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));

    let snapshot = cell.current();
    cell.publish(make(1));
    assert!(freed(&sites).is_empty(), "the snapshot still owns value 0");
    assert_eq!(cell.retired_len(), 0, "and the cell does not pin it");
    assert_eq!(snapshot.id, 0);

    let there = std::thread::spawn(move || {
        drop(snapshot);
        std::thread::current().id()
    })
    .join()
    .unwrap();
    assert_eq!(*sites.lock().unwrap(), vec![(0, there)]);
}

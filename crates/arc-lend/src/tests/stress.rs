//! Readers on threads against a publisher hammering the cell.

use super::*;

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
#[test]
#[cfg_attr(miri, ignore = "threaded stress test; miri runs the small one below")]
fn concurrent_readers_and_a_publisher_hammering() {
    readers_against_a_publisher(3, 20_000, true);
}

/// The same, small enough for miri, whose weak-memory emulation is the
/// second check on the protocol beside the loom model.
#[test]
fn concurrent_readers_and_a_publisher_small() {
    readers_against_a_publisher(2, 40, true);
}

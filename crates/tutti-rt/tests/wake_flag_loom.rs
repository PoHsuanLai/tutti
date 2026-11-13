//! A `loom` model of [`WakeFlag`]: the shipped code, whose atomic is loom's
//! under the flag (see `src/wake_flag.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test wake_flag_loom
//! ```
//!
//! One thread raises, once, after writing a datum; the main thread takes
//! twice while it runs, then once more after the raiser joins. Over every
//! interleaving:
//!
//! - **a raise is never lost**: the takes together return `true`;
//! - **a raise is never invented**: with one raise, exactly one take returns
//!   `true`;
//! - **a take that sees the raise sees what came before it**: the datum;
//! - **an unraised flag is never taken** (the second test).

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::thread;
use tutti_rt::WakeFlag;

#[test]
fn a_raise_racing_a_take_is_seen_once_by_this_take_or_the_next() {
    loom::model(|| {
        let flag = Arc::new(WakeFlag::new());
        let datum = Arc::new(AtomicU32::new(0));
        let raiser = {
            let (flag, datum) = (flag.clone(), datum.clone());
            thread::spawn(move || {
                datum.store(7, Ordering::Relaxed);
                flag.raise();
            })
        };

        let mut seen = 0;
        for _ in 0..2 {
            if flag.take() {
                seen += 1;
                assert_eq!(
                    datum.load(Ordering::Relaxed),
                    7,
                    "took the wake without the datum"
                );
            }
        }
        raiser.join().unwrap();
        if flag.take() {
            seen += 1;
        }
        assert_eq!(seen, 1, "one raise is taken exactly once");
    });
}

#[test]
fn an_unraised_flag_is_never_taken() {
    loom::model(|| {
        let flag = Arc::new(WakeFlag::new());
        let other = flag.clone();
        let t = thread::spawn(move || assert!(!other.take()));
        assert!(!flag.take());
        t.join().unwrap();
    });
}

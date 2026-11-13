//! A `loom` model of [`DirtySet`]: the shipped code, whose atomics are loom's
//! under the flag (see `src/dirty_set.rs`'s `sync` module).
//!
//! ```text
//! cargo xtask loom dirty_set_loom   # cargo test -p tutti-rt --release --test dirty_set_loom, with --cfg loom
//! ```
//!
//! A setter thread writes a datum (a `Relaxed` store to a second atomic) and
//! then sets a flag, for flags 1 and 2 (both in word 0) and 65 (word 1), each
//! once. The main thread takes twice while the setter runs, then after it
//! joins takes a last time. Over every interleaving:
//!
//! - **a set during a take is never lost**: every flag set is yielded by some
//!   take, this one or a later one;
//! - **each set is yielded once**: no flag appears in two takes, because a set
//!   happens once and a take clears;
//! - **a set orders what came before it**: a take that yields a flag also
//!   sees the datum written before that flag was set;
//! - **a flag set twice is at least one take** (`a_flag_set_twice_is_not_lost`):
//!   coalescing may merge the two sets but never drops both.
//!
//! # The mutation record
//!
//! Each was made in `src/dirty_set.rs`, run at this model's default bound, and
//! reverted:
//!
//! - `swap(0, Acquire)` in `Taken::next` replaced by `load(Acquire)` then
//!   `store(0, Release)` (a set between the two is wiped): fails both models,
//!   "a flag lost, or taken twice" and a flag set twice taken no time;
//! - the same `swap` replaced by a `load` alone (nothing is cleared): fails
//!   `every_set_is_taken_once` ("a flag lost, or taken twice": a flag is
//!   taken by every take);
//! - `fetch_or(.., Release)` in `set` weakened to `Relaxed`: fails
//!   `every_set_is_taken_once` ("saw the flag without the datum");
//! - `swap(0, Acquire)` weakened to `Relaxed`: the same failure.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::thread;
use tutti_rt::DirtySet;

const FLAGS: [usize; 3] = [1, 2, 65];

#[test]
fn every_set_is_taken_once() {
    loom::model(|| {
        let set = Arc::new(DirtySet::new(70));
        let data = Arc::new([AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)]);
        let setter = {
            let (set, data) = (set.clone(), data.clone());
            thread::spawn(move || {
                for (n, flag) in FLAGS.into_iter().enumerate() {
                    data[n].store(7, Ordering::Relaxed);
                    set.set(set.index(flag).unwrap());
                }
            })
        };

        let record = |seen: &mut [u8; 3]| {
            for flag in set.take() {
                let n = FLAGS
                    .iter()
                    .position(|&f| f == flag.get())
                    .expect("a flag nobody set");
                seen[n] += 1;
                assert_eq!(
                    data[n].load(Ordering::Relaxed),
                    7,
                    "saw the flag without the datum"
                );
            }
        };
        let mut seen = [0u8; 3];
        record(&mut seen);
        record(&mut seen);
        setter.join().unwrap();
        record(&mut seen);
        assert_eq!(seen, [1, 1, 1], "a flag lost, or taken twice");
    });
}

#[test]
fn a_flag_set_twice_is_not_lost() {
    loom::model(|| {
        let set = Arc::new(DirtySet::new(64));
        let setter = {
            let set = set.clone();
            thread::spawn(move || {
                let flag = set.index(5).unwrap();
                set.set(flag);
                set.set(flag);
            })
        };
        let mut taken = set.take().count();
        setter.join().unwrap();
        taken += set.take().count();
        assert!((1..=2).contains(&taken), "taken {taken} times");
    });
}

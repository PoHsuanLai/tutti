//! A `loom` model of [`SplitMut`] / [`SplitRw`]'s claim protocol — the
//! shipped code, whose atomics are loom's under the flag (see `rt/split.rs`'s
//! `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test split_loom
//! ```
//!
//! Every element is a `loom::cell::UnsafeCell`, and each access through a
//! claim goes through `with` (shared) or `with_mut` (exclusive). loom reports
//! any two accesses to one cell that are not ordered by happens-before, so
//! each model below asserts, over every interleaving:
//!
//! - **no aliasing** — two claims that would both reach an element at once
//!   cannot both succeed (a concurrent `with_mut`/`with` pair would be a
//!   loom data-race report);
//! - **release → acquire** — whatever a claim's holder wrote is visible to the
//!   next claimer (a missing `Acquire`/`Release` is a causality violation);
//! - **a refused claim leaves the word as it found it** — after every thread
//!   is done, each element can be claimed exclusively again.
//!
//! # Mutation record
//!
//! Each was applied to `rt/split.rs` and a model failed
//! (`LOOM_MAX_PREEMPTIONS=3`):
//!
//! - `release_exclusive`'s `Release` weakened to `Relaxed` →
//!   `a_write_is_seen_by_the_next_reader`, `two_writers_never_overlap`
//!   (causality violations);
//! - `claim_exclusive`'s success ordering weakened to `Relaxed` → the same
//!   two;
//! - `claim_shared`'s `fetch_add` weakened to `Relaxed` →
//!   `a_write_is_seen_by_the_next_reader`;
//! - `claim_shared` made not to undo its increment on a refusal →
//!   `a_write_is_seen_by_the_next_reader` (the final exclusive claim is
//!   refused);
//! - `upgrade` made to succeed from any shared count (`compare_exchange(1,
//!   ..)` replaced by a plain `fetch_or(WRITE)`) →
//!   `an_upgrade_excludes_another_reader` (a write races a read).

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::thread;
use tutti_types::{ClaimTable, Held, SplitMut, SplitRw};

struct Elem(UnsafeCell<u32>);

// SAFETY (test): the splits share elements across threads only under a
// claim, which is exactly what the model checks: loom flags every access
// pair not ordered by happens-before.
unsafe impl Sync for Elem {}

impl Elem {
    fn new() -> Self {
        Self(UnsafeCell::new(0))
    }
    fn read(&self) -> u32 {
        // SAFETY (test): a shared access loom checks.
        self.0.with(|p| unsafe { *p })
    }
    fn bump(&self) {
        // SAFETY (test): an exclusive access loom checks.
        self.0.with_mut(|p| unsafe { *p += 1 })
    }
}

fn leak<T>(v: T) -> &'static mut T {
    Box::leak(Box::new(v))
}

/// A writer bumps element 0; a reader takes a shared claim, and if it gets
/// one, reads 0 or 1 — never a torn or unordered value.
#[test]
fn a_write_is_seen_by_the_next_reader() {
    loom::model(|| {
        let data: &'static mut [Elem] = leak([Elem::new(), Elem::new()]);
        let table = leak(ClaimTable::new(2));
        let split: &'static SplitRw<'static, Elem> = leak(SplitRw::new(data, 1, table));
        let w = thread::spawn(move || {
            let held = Held::new(2);
            let mut v = split.view(&held);
            if let Ok(c) = v.try_get_mut(0) {
                c[0].bump();
            }
        });
        let held = Held::new(2);
        let got = {
            let v = split.view(&held);
            v.try_get(0).ok().map(|c| c[0].read())
        };
        w.join().unwrap();
        if let Some(x) = got {
            assert!(x <= 1);
        }
        // Every word released: an exclusive claim succeeds now.
        let mut v = split.view(&held);
        assert!(v.try_get_mut(0).is_ok());
        assert!(v.try_get_mut(1).is_ok());
    });
}

/// Two threads each claim element 0 exclusively and bump it; each bump that
/// happened is counted exactly once and never overlaps the other.
#[test]
fn two_writers_never_overlap() {
    loom::model(|| {
        let data: &'static mut [Elem] = leak([Elem::new()]);
        let table = leak(ClaimTable::new(1));
        let split: &'static SplitMut<'static, Elem> = leak(SplitMut::new(data, table));
        let t = thread::spawn(move || match split.try_claim(0) {
            Ok(e) => {
                e.bump();
                1
            }
            Err(_) => 0,
        });
        let mine = match split.try_claim(0) {
            Ok(e) => {
                e.bump();
                1
            }
            Err(_) => 0,
        };
        let theirs = t.join().unwrap();
        assert!(mine + theirs >= 1, "a claim on a free word was refused");
        assert_eq!(split.claim(0).read(), mine + theirs);
    });
}

/// A view that holds the only shared claim may upgrade it; while another view
/// also reads the chunk, the upgrade is refused.
#[test]
fn an_upgrade_excludes_another_reader() {
    loom::model(|| {
        let data: &'static mut [Elem] = leak([Elem::new()]);
        let table = leak(ClaimTable::new(1));
        let split: &'static SplitRw<'static, Elem> = leak(SplitRw::new(data, 1, table));
        let r = thread::spawn(move || {
            let held = Held::new(1);
            let v = split.view(&held);
            if let Ok(c) = v.try_get(0) {
                assert!(c[0].read() <= 1);
            }
        });
        let held = Held::new(1);
        let mut v = split.view(&held);
        if v.try_get(0).is_ok() {
            if let Ok(c) = v.try_get_mut(0) {
                c[0].bump();
            }
        }
        drop(v);
        r.join().unwrap();
    });
}

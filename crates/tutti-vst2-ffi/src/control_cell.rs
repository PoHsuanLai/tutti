//! A cell whose one accessor is whoever holds its key.
//!
//! [`ControlCell`] is shared between threads (it sits in an `Arc` both halves of an
//! activation hold), but only the holder of its [`ControlKey`] can reach the value, so the
//! type system, not a convention, says which thread that is.
//!
//! It lives here and not in `tutti-rt` because it is a type-level capability around an
//! `UnsafeCell`, not a lock-free structure: there is no protocol between threads (no atomic
//! but the id counter, which orders nothing), so there is nothing for a loom model to explore.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hands every cell an identity, so a key opens only its own.
static NEXT_CELL: AtomicU64 = AtomicU64::new(1);

/// A value that is reached only through its [`ControlKey`] (or by value or `&mut`, when the
/// cell is owned).
///
/// `Sync` when `T: Send`: sharing the cell shares nothing but the right to ask for the value,
/// and the key is the only way to ask. The key is `Send` and not `Sync`, so the thread that
/// holds it is one thread at a time, and `get_mut` needs the key `&mut`.
///
/// A cell is not `Sync` for a `T` that is not `Send`:
///
/// ```compile_fail,E0277
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<tutti_vst2_ffi::control_cell::ControlCell<std::rc::Rc<u8>>>();
/// ```
///
/// Mutation (run): drop the `T: Send` bound from the `unsafe impl Sync`: the doctest compiles
/// and fails.
#[derive(Debug)]
pub struct ControlCell<T> {
    id: u64,
    value: UnsafeCell<T>,
}

// SAFETY: a shared `&ControlCell<T>` gives no access to the value; `get` and `get_mut` need
// the cell's own `ControlKey`, which exists once (it is made by `new`, is not `Clone`) and is
// not `Sync`, so at most one thread at a time can hold a reference derived from it. `T: Send`
// is what moving the value to that thread (or dropping it on another) needs.
unsafe impl<T: Send> Sync for ControlCell<T> {}

/// The one key to a [`ControlCell`]. `Send`, not `Sync`, not `Clone`.
#[derive(Debug)]
pub struct ControlKey {
    id: u64,
    _not_sync: PhantomData<Cell<()>>,
}

impl<T> ControlCell<T> {
    /// A cell holding `value`, and the key to it.
    pub fn new(value: T) -> (Self, ControlKey) {
        let id = NEXT_CELL.fetch_add(1, Ordering::Relaxed);
        (
            Self {
                id,
                value: UnsafeCell::new(value),
            },
            ControlKey {
                id,
                _not_sync: PhantomData,
            },
        )
    }

    /// The value, shared, for as long as `key` is borrowed.
    ///
    /// # Panics
    ///
    /// If `key` is another cell's.
    pub fn get<'a>(&'a self, key: &'a ControlKey) -> &'a T {
        assert_eq!(key.id, self.id, "a ControlKey opens only its own cell");
        // SAFETY: `key` is this cell's only key (checked above), so no other thread reaches
        // the value; it is borrowed shared for `'a`, so `get_mut` (which needs it `&mut`)
        // cannot run meanwhile.
        unsafe { &*self.value.get() }
    }

    /// The value, exclusively, for as long as `key` is borrowed.
    ///
    /// # Panics
    ///
    /// If `key` is another cell's.
    #[allow(clippy::mut_from_ref)]
    pub fn get_mut<'a>(&'a self, key: &'a mut ControlKey) -> &'a mut T {
        assert_eq!(key.id, self.id, "a ControlKey opens only its own cell");
        // SAFETY: as `get`; the key is borrowed `&mut` for `'a`, so this is the only
        // reference to the value.
        unsafe { &mut *self.value.get() }
    }

    /// The value, when the cell is owned: no key is needed.
    pub fn get_mut_owned(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key opens its cell and no other.
    ///
    /// Mutation (run): drop the `assert_eq!` in `get`: the second `get` reads the
    /// other cell's value and the `should_panic` test fails.
    #[test]
    #[should_panic(expected = "opens only its own cell")]
    fn a_key_opens_only_its_own_cell() {
        let (a, key_a) = ControlCell::new(1);
        let (b, _key_b) = ControlCell::new(2);
        assert_eq!(*a.get(&key_a), 1);
        let _ = b.get(&key_a);
    }

    /// The value is changed through the key and seen through it.
    #[test]
    fn the_key_holder_reads_and_writes() {
        let (cell, mut key) = ControlCell::new(vec![1]);
        cell.get_mut(&mut key).push(2);
        assert_eq!(cell.get(&key), &vec![1, 2]);
    }

    /// The cell crosses threads inside an `Arc` while the key stays on one.
    #[test]
    fn the_cell_is_shared_and_the_key_is_not() {
        fn sync<T: Sync>() {}
        fn send<T: Send>() {}
        sync::<ControlCell<Vec<u8>>>();
        send::<ControlKey>();
    }
}

//! [`Retire`] — an owning box for anything that crosses to the audio thread
//! and must come back to be freed.
//!
//! Doc 013 §4: a graph edit ships to the audio thread as a box; the audio
//! thread swaps pointers at a block boundary and sends the *old* contents
//! (retired units, the previous plan, the previous arena) back in the same box,
//! so every free happens on the control thread. The rule that makes that true
//! is "nothing in the box is dropped on the audio thread", and this type
//! checks it: in debug builds, dropping a non-empty `Retire` while the current
//! thread is marked with [`AudioThread::enter`] panics.
//!
//! # How the contents come back
//!
//! There is no `Retire::into_inner` on the audio side. The contents leave a
//! `Retire` only through [`reclaim`](Retire::reclaim), which is itself checked
//! to run off the audio thread. The return *path* is the caller's: in
//! `tutti-graph`'s phase 1 it is the value `Executor::apply` returns, and the
//! control side's credit count (at most N edits in flight, one box each) is
//! what guarantees the phase-2 return ring always has room — back-pressure
//! lands on the control side, never as a failed push on the audio side.
//!
//! # Mutable access, and why there is no `DerefMut`
//!
//! A `DerefMut` would reopen the hole this type closes: `*r = other` or
//! `mem::take(&mut *r)` frees the old contents in place, on whatever thread
//! runs it, and `Retire`'s own drop check never sees it. So `&mut T` is
//! available only through [`get_mut`](Retire::get_mut), and only for contents
//! that implement [`Guarded`]: types whose `&mut` cannot be used to free them
//! unnoticed.
//!
//! ```compile_fail
//! use tutti_types::Retire;
//! let mut r = Retire::new(vec![1.0f32; 64]);
//! *r = Vec::new(); // would free the old buffer wherever this runs
//! ```
//!
//! # What it does not do in release builds
//!
//! The check is a `debug_assertions` check. A release build that breaks the
//! rule frees on the audio thread rather than aborting a live performance;
//! the debug check plus the tests that run under it are the gate.

use core::ops::Deref;

use super::audio_thread::AudioThread;

/// Contents a [`Retire`] may hand out `&mut` to.
///
/// The contract: holding `&mut Self` must not let a caller free part of it
/// without a debug check firing on the audio thread. Two kinds of type meet
/// it, and only the crate that owns the type can say which:
///
/// - **a trait object** (`dyn Node`): unsized, so it cannot be assigned,
///   swapped or taken out of in safe Rust at all;
/// - **a type whose own `Drop` checks [`AudioThread`]** (see
///   [`AudioThread::check_not_current`]), and whose fields are private, so
///   the only code that can touch them is the type's own.
pub trait Guarded {}

/// An owning box that must not be dropped on the audio thread. See the
/// [module docs](self).
#[must_use = "a Retire must travel back to the control thread to be reclaimed"]
pub struct Retire<T: ?Sized> {
    inner: Option<Box<T>>,
}

impl<T> Retire<T> {
    /// Box `value` for a trip to the audio thread and back. Control thread:
    /// allocates.
    pub fn new(value: T) -> Self {
        Self::from_box(Box::new(value))
    }
}

impl<T: ?Sized> Retire<T> {
    /// Take ownership of an existing box — for unsized contents
    /// (`Box<dyn Node>`).
    pub fn from_box(value: Box<T>) -> Self {
        Self { inner: Some(value) }
    }

    /// Give up the contents, on the control thread.
    ///
    /// # Panics
    ///
    /// In debug builds, when called on a thread marked as the audio thread:
    /// the caller would free the box right there.
    pub fn reclaim(mut self) -> Box<T> {
        debug_assert!(
            !AudioThread::is_current(),
            "Retire<{}> reclaimed on the audio thread",
            core::any::type_name::<T>()
        );
        self.inner.take().expect("a Retire is full until reclaimed")
    }
}

impl<T: ?Sized> Deref for Retire<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner
            .as_deref()
            .expect("a Retire is full until reclaimed")
    }
}

impl<T: ?Sized + Guarded> Retire<T> {
    /// Mutable access to [`Guarded`] contents. See the
    /// [module docs](self) for why this is not a `DerefMut`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner
            .as_deref_mut()
            .expect("a Retire is full until reclaimed")
    }
}

impl<T: ?Sized> Drop for Retire<T> {
    fn drop(&mut self) {
        if cfg!(debug_assertions) && self.inner.is_some() && AudioThread::is_current() {
            // Not while already unwinding: a second panic would abort and hide
            // the first.
            if !std::thread::panicking() {
                panic!(
                    "Retire<{}> dropped on the audio thread: its contents must go back \
                     to the control thread to be freed",
                    core::any::type_name::<T>()
                );
            }
        }
    }
}

impl<T: ?Sized + core::fmt::Debug> core::fmt::Debug for Retire<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Retire").field(&self.inner).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off the audio thread a `Retire` behaves as a box.
    #[test]
    fn a_retire_drops_and_reclaims_on_the_control_thread() {
        let r = Retire::new(vec![1, 2, 3]);
        assert_eq!(r.len(), 3);
        drop(r);
        let r: Retire<dyn core::fmt::Debug> = Retire::from_box(Box::new(7));
        assert_eq!(format!("{:?}", r.reclaim()), "7");
    }

    /// The whole point: dropping one inside an audio-thread scope panics in
    /// debug builds.
    ///
    /// Mutation: delete the `AudioThread::is_current()` condition in `Drop`
    /// → every drop panics, and the control-thread test above fails;
    /// delete the panic → this test fails.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "dropped on the audio thread")]
    fn dropping_on_the_audio_thread_panics_in_debug() {
        let r = Retire::new(vec![0u8; 16]);
        let _rt = AudioThread::enter();
        drop(r);
    }

    /// Reclaiming inside an audio-thread scope is the same mistake by another
    /// route.
    ///
    /// Mutation: delete the `debug_assert!` in `reclaim` → no panic → fails.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "reclaimed on the audio thread")]
    fn reclaiming_on_the_audio_thread_panics_in_debug() {
        let r = Retire::new(1u32);
        let _rt = AudioThread::enter();
        let _ = r.reclaim();
    }

    /// `get_mut` reaches a `Guarded` value; a guarded type whose `Drop`
    /// checks the marker catches the `*r = other` replacement a `DerefMut`
    /// would have allowed.
    ///
    /// Mutation: make `Probe::drop` skip `check_not_current` → the
    /// replacement below is silent → fails.
    #[test]
    #[cfg(debug_assertions)]
    fn a_guarded_value_replaced_on_the_audio_thread_panics() {
        struct Probe(u8);
        impl Guarded for Probe {}
        impl Drop for Probe {
            fn drop(&mut self) {
                AudioThread::check_not_current("Probe");
            }
        }
        let mut r = Retire::new(Probe(1));
        r.get_mut().0 = 2; // field mutation is fine
        assert_eq!(r.0, 2);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _rt = AudioThread::enter();
            *r.get_mut() = Probe(3); // drops the old Probe here
        }));
        assert!(caught.is_err());
        let _ = r.reclaim();
    }

    /// Moving a `Retire` around on the audio thread is fine — only freeing is
    /// not. This is the audio side's whole job: swap pointers, send the box on.
    #[test]
    fn moving_on_the_audio_thread_is_allowed() {
        let mut slots: [Option<Retire<u64>>; 2] = [Some(Retire::new(5u64)), None];
        {
            let _rt = AudioThread::enter();
            // A pointer swap between two slots, as `apply` does: nothing is
            // dropped, so nothing panics.
            slots.swap(0, 1);
        }
        let [empty, full] = slots;
        assert!(empty.is_none());
        assert_eq!(*full.unwrap().reclaim(), 5);
    }
}

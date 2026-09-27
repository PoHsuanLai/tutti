//! [`Retire`], an owning box for anything that crosses to the audio thread
//! and must come back to be freed.

use core::ops::Deref;

use super::audio_thread::AudioThread;

/// An owning box that must not be dropped on the audio thread.
///
/// A graph edit ships to the audio thread as a box; the audio thread swaps
/// pointers at a block boundary and sends the *old* contents (retired nodes,
/// the previous plan, the previous arena) back in the same box, so every free
/// happens on the control thread. The rule that makes that true is "nothing in
/// the box is dropped on the audio thread", and this type checks it: in debug
/// builds, dropping a non-empty `Retire` while the current thread is marked
/// with [`AudioThread::enter`] panics.
///
/// # How the contents come back
///
/// There is no way to take the contents out on the audio side. They leave a
/// `Retire` only through [`reclaim`](Retire::reclaim), which is itself checked
/// to run off the audio thread. The return *path* is the caller's: in
/// `tutti-graph` it is the value `Executor::apply` returns, and the control
/// side bounds the number of edits in flight so the return path always has
/// room. Back-pressure lands on the control side, never as a failed push on
/// the audio side.
///
/// # No mutable access at all
///
/// A `DerefMut` would reopen the hole this type closes: `*r = other` or
/// `mem::take(&mut *r)` frees the old contents in place, on whatever thread
/// runs it, and `Retire`'s own drop check never sees it. So a `Retire` is
/// read-only and move-only.
///
/// A crate whose audio side must *mutate* what it was handed keeps its own
/// crate-private box instead, whose `Drop` calls
/// [`AudioThread::check_not_current`].
///
/// ```compile_fail
/// use tutti_types::Retire;
/// let mut r = Retire::new(vec![1.0f32; 64]);
/// *r = Vec::new(); // would free the old buffer wherever this runs
/// ```
///
/// # Interior mutability is outside the guarantee
///
/// `Retire` refuses `&mut`, but a `&T` can still free through interior
/// mutability: a `Retire<RefCell<Vec<f32>>>` lets
/// `r.borrow_mut().clear(); r.borrow_mut().shrink_to_fit()` free the buffer
/// on whatever thread runs it, and a `Mutex` or `Cell<Option<Box<_>>>` does the
/// same. Do not put interior-mutable owners in a `Retire`; nothing here can
/// check it.
///
/// # Release builds
///
/// The check is a `debug_assertions` check. A release build that breaks the
/// rule frees on the audio thread rather than aborting a live performance.
///
/// # Examples
///
/// ```
/// use tutti_types::Retire;
///
/// let boxed = Retire::new(vec![0.0f32; 64]); // control thread: allocates
/// assert_eq!(boxed.len(), 64); // read-only access anywhere
/// let back: Box<Vec<f32>> = boxed.reclaim(); // control thread again
/// drop(back);
/// ```
#[must_use = "a Retire must travel back to the control thread to be reclaimed"]
pub struct Retire<T: ?Sized> {
    inner: Option<Box<T>>,
}

impl<T> Retire<T> {
    /// Boxes `value` for a trip to the audio thread and back.
    ///
    /// Allocates, so call it on the control thread.
    pub fn new(value: T) -> Self {
        Self::from_box(Box::new(value))
    }
}

impl<T: ?Sized> Retire<T> {
    /// Takes ownership of an existing box, for unsized contents such as a
    /// `Box<dyn Trait>`.
    pub fn from_box(value: Box<T>) -> Self {
        Self { inner: Some(value) }
    }

    /// Gives up the contents, on the control thread.
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
